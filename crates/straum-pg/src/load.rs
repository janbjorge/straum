//! Loads: a binary `COPY` of the columns the spec reads, so PostgreSQL only
//! scans the table and sends each value as it stores it. Straum groups the
//! rows itself as they come; a load must not cost the server a sort or a
//! hash that spills to disk, nor Straum the table's rows in memory.

use std::sync::mpsc;
use std::{mem, panic, thread};

use postgres_protocol::escape::escape_identifier;
use straum_domain::{Datum, DimTable, Ident, Rollup};

use crate::catalog::Table;
use crate::error::Error;
use crate::value::Kind;
use crate::wire::Conn;

/// What starts a binary copy, before its flags and header extension.
const SIGNATURE: &[u8] = b"PGCOPY\n\xff\r\n\0";

pub(crate) fn copy_sql(table: &Table, columns: &[Ident]) -> String {
    let columns: Vec<String> = columns
        .iter()
        .map(|c| escape_identifier(c.as_str()))
        .collect();
    format!(
        "COPY (SELECT {} FROM {}.{}) TO STDOUT (FORMAT binary)",
        columns.join(", "),
        escape_identifier(table.schema.as_str()),
        escape_identifier(table.table.as_str())
    )
}

pub(crate) fn fact(conn: &mut Conn, table: &Table, rollup: &mut Rollup) -> Result<(), Error> {
    let kinds: Vec<Kind> = rollup.columns().iter().map(|c| table.kind(c)).collect();
    let scales: Vec<u32> = kinds.iter().map(|k| k.scale()).collect();
    rollup.set_scales(&scales).map_err(Error::Row)?;
    let sql = copy_sql(table, rollup.columns());
    copy(conn, &sql, &kinds, |rows| {
        rollup.add_rows(rows).map_err(Error::Row)
    })
}

pub(crate) fn dim(conn: &mut Conn, table: &Table, rows: &mut DimTable) -> Result<(), Error> {
    let scales: Vec<u32> = table.kinds.iter().map(|k| k.scale()).collect();
    rows.set_scales(&scales).map_err(Error::Row)?;
    let sql = copy_sql(table, &table.columns);
    copy(conn, &sql, &table.kinds, |batch| {
        for row in batch.chunks_exact(table.kinds.len()) {
            rows.upsert(row).map_err(Error::Row)?;
        }
        Ok(())
    })
}

/// Rows per batch the reading thread hands over. A batch starts a thread
/// per wider bucket width (`Rollup::add_rows`), so a small one costs.
pub(crate) const BATCH: usize = 8192;
/// Batches between the threads. With the one being read and the one being
/// added, a load holds six batches besides what it fills: about 1.2 MB per
/// column copied, however big the table.
const IN_FLIGHT: usize = 4;

/// Runs a binary `COPY` and calls `each` with every batch of rows, laid
/// end to end. A second thread reads and decodes the next batch
/// meanwhile; spent buffers go back to it.
pub(crate) fn copy(
    conn: &mut Conn,
    sql: &str,
    kinds: &[Kind],
    mut each: impl FnMut(&[Datum]) -> Result<(), Error>,
) -> Result<(), Error> {
    // A fact has its time column, a dim its key.
    debug_assert!(!kinds.is_empty());
    let (full, filled) = mpsc::sync_channel::<Vec<Datum>>(IN_FLIGHT);
    let (spent, empty) = mpsc::channel::<Vec<Datum>>();
    thread::scope(|scope| {
        let reader = scope.spawn(move || {
            read(conn, sql, kinds, |batch| {
                let next = empty
                    .try_recv()
                    .unwrap_or_else(|_| Vec::with_capacity(BATCH * kinds.len()));
                // The receiver is gone once `each` has failed.
                full.send(mem::replace(batch, next))
                    .map_err(|_| Error::Stopped)
            })
        });
        let mut result = Ok(());
        for mut batch in &filled {
            result = each(&batch);
            if result.is_err() {
                break;
            }
            batch.clear();
            // The reader may be done.
            let _ = spent.send(batch);
        }
        drop(filled);
        let read = reader.join().unwrap_or_else(|e| panic::resume_unwind(e));
        result.and(read)
    })
}

/// Runs a binary `COPY`, decoding rows into a batch and calling `hand_over`
/// with each full one and the last, which it swaps for an empty one.
fn read(
    conn: &mut Conn,
    sql: &str,
    kinds: &[Kind],
    mut hand_over: impl FnMut(&mut Vec<Datum>) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut batch = Vec::with_capacity(BATCH * kinds.len());
    let mut started = false;
    let mut ended = false;
    conn.copy_out(sql, |mut data| {
        if !started {
            data = header(data)?;
            started = true;
        }
        // PostgreSQL sends a row per message, but nothing here needs it to.
        while !data.is_empty() {
            if ended {
                return Err(copy_error("data after the end of a copy"));
            }
            let fields = i16::from_be_bytes(take(&mut data)?);
            if fields == -1 {
                ended = true;
                continue;
            }
            if usize::try_from(fields).ok() != Some(kinds.len()) {
                return Err(Error::Protocol(format!(
                    "a copy row of {fields} columns, not {}",
                    kinds.len()
                )));
            }
            for kind in kinds {
                let len = i32::from_be_bytes(take(&mut data)?);
                batch.push(match usize::try_from(len) {
                    // -1 is null.
                    Err(_) => Datum::Null,
                    Ok(len) => {
                        let (value, rest) = data
                            .split_at_checked(len)
                            .ok_or_else(|| copy_error("a copy value cut short"))?;
                        data = rest;
                        kind.read_binary(value)?
                    }
                });
            }
            if batch.len() >= BATCH * kinds.len() {
                hand_over(&mut batch)?;
            }
        }
        Ok(())
    })?;
    if !ended {
        return Err(copy_error("a copy without its end"));
    }
    if batch.is_empty() {
        Ok(())
    } else {
        hand_over(&mut batch)
    }
}

/// The data after the signature, flags and header extension.
fn header(data: &[u8]) -> Result<&[u8], Error> {
    let mut data = data
        .strip_prefix(SIGNATURE)
        .ok_or_else(|| copy_error("a copy that isn't binary"))?;
    let _flags: [u8; 4] = take(&mut data)?;
    let extension = u32::from_be_bytes(take(&mut data)?);
    usize::try_from(extension)
        .ok()
        .and_then(|len| data.get(len..))
        .ok_or_else(|| copy_error("a copy header cut short"))
}

/// The next `N` bytes.
fn take<const N: usize>(data: &mut &[u8]) -> Result<[u8; N], Error> {
    let (head, rest) = data
        .split_first_chunk()
        .ok_or_else(|| copy_error("a copy row cut short"))?;
    *data = rest;
    Ok(*head)
}

fn copy_error(what: &str) -> Error {
    Error::Protocol(what.to_owned())
}
