//! Loads: a binary `COPY` of the columns the spec reads of each table, dim
//! or fact, so straum groups or projects a row as it comes. A load must
//! not cost straum the table's rows in memory.

use std::sync::mpsc;
use std::{mem, panic, thread};

use postgres_protocol::escape::escape_identifier;
use straum_domain::{Datum, Store};

use crate::catalog::{Table, escaped};
use crate::error::Error;
use crate::value::read_binary;
use crate::wire::Conn;

/// What starts a binary copy, before its flags and header extension.
const SIGNATURE: &[u8] = b"PGCOPY\n\xff\r\n\0";

/// The columns straum reads of the table, as the catalog checked them.
pub(crate) fn copy_sql(table: &Table) -> String {
    let select: Vec<String> = (table.columns.iter())
        .map(|c| escape_identifier(c.as_str()))
        .collect();
    format!(
        "COPY (SELECT {} FROM {}) TO STDOUT (FORMAT binary)",
        select.join(", "),
        escaped(&table.schema, &table.table)
    )
}

pub(crate) fn table(conn: &mut Conn, table: &Table, store: &mut Store) -> Result<(), Error> {
    let name = table.name.as_str();
    store.set_types(name, &table.types).map_err(Error::Row)?;
    copy(conn, table, |rows| {
        store.add_rows(name, rows).map_err(Error::Row)
    })
}

/// Rows per batch the reading thread hands over. A batch starts a thread
/// per wider bucket width (`Rollup::add_rows`), so a small one costs.
pub(crate) const BATCH: usize = 8192;
/// Batches between the threads. With the one being read and the one being
/// added, a load holds six batches besides what it fills: about 1.2 MB per
/// column copied, however big the table.
const IN_FLIGHT: usize = 4;

/// Runs a binary `COPY` of the table and calls `each` with every batch of
/// rows, laid end to end. A second thread reads and decodes the next batch
/// meanwhile; spent buffers go back to it.
pub(crate) fn copy(
    conn: &mut Conn,
    table: &Table,
    mut each: impl FnMut(&[Datum]) -> Result<(), Error>,
) -> Result<(), Error> {
    // A fact has its time column, a dim its key.
    debug_assert!(!table.types.is_empty());
    let width = table.types.len();
    let sql = copy_sql(table);
    let (full, filled) = mpsc::sync_channel::<Vec<Datum>>(IN_FLIGHT);
    let (spent, empty) = mpsc::channel::<Vec<Datum>>();
    thread::scope(|scope| {
        let reader = scope.spawn(move || {
            read(conn, &sql, table, |batch| {
                let next = empty
                    .try_recv()
                    .unwrap_or_else(|_| Vec::with_capacity(BATCH * width));
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
    table: &Table,
    mut hand_over: impl FnMut(&mut Vec<Datum>) -> Result<(), Error>,
) -> Result<(), Error> {
    let types = &table.types;
    let mut batch = Vec::with_capacity(BATCH * types.len());
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
                return Err(Error::protocol("data after the end of a copy"));
            }
            let fields = i16::from_be_bytes(take(&mut data)?);
            if fields == -1 {
                ended = true;
                continue;
            }
            if usize::try_from(fields).ok() != Some(types.len()) {
                return Err(Error::Protocol(format!(
                    "a copy row of {fields} columns, not {}",
                    types.len()
                )));
            }
            for (name, column_type) in table.columns.iter().zip(types) {
                let len = i32::from_be_bytes(take(&mut data)?);
                batch.push(match usize::try_from(len) {
                    // -1 is null.
                    Err(_) => Datum::Null,
                    Ok(len) => {
                        let (value, rest) = data
                            .split_at_checked(len)
                            .ok_or_else(|| Error::protocol("a copy value cut short"))?;
                        data = rest;
                        read_binary(*column_type, value)
                            .map_err(|e| e.at(&table.qualified(), name))?
                    }
                });
            }
            if batch.len() >= BATCH * types.len() {
                hand_over(&mut batch)?;
            }
        }
        Ok(())
    })?;
    if !ended {
        return Err(Error::protocol("a copy without its end"));
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
        .ok_or_else(|| Error::protocol("a copy that isn't binary"))?;
    let _flags: [u8; 4] = take(&mut data)?;
    let extension = u32::from_be_bytes(take(&mut data)?);
    usize::try_from(extension)
        .ok()
        .and_then(|len| data.get(len..))
        .ok_or_else(|| Error::protocol("a copy header cut short"))
}

/// The next `N` bytes.
fn take<const N: usize>(data: &mut &[u8]) -> Result<[u8; N], Error> {
    let (head, rest) = data
        .split_first_chunk()
        .ok_or_else(|| Error::protocol("a copy row cut short"))?;
    *data = rest;
    Ok(*head)
}
