//! Loads: a binary `COPY` of the columns the spec reads of each table, dim
//! or fact, so Straum groups or projects a row as it comes. One thread
//! reads PostgreSQL's copy messages and decodes them (`value.rs`) into
//! batches of rows; the caller's thread adds each batch to the store. A
//! load must not cost Straum the table's rows in memory.

use std::sync::mpsc;
use std::{mem, panic, thread};

use postgres_protocol::escape::escape_identifier;
use straum_domain::{Datum, Store};

use crate::catalog::{Checked, escaped};
use crate::error::Error;
use crate::unexpected::Unexpected;
use crate::value::read_binary;
use crate::wire::Connection;

/// What starts a binary copy, before its flags and header extension.
const SIGNATURE: &[u8] = b"PGCOPY\n\xff\r\n\0";

/// The columns Straum reads of the table, as the catalog checked them.
pub(crate) fn copy_sql(table: &Checked) -> String {
    let mut select = Vec::new();
    for column in &table.columns {
        select.push(escape_identifier(column.as_str()));
    }
    format!(
        "COPY (SELECT {} FROM {}) TO STDOUT (FORMAT binary)",
        select.join(", "),
        escaped(&table.schema, &table.table_name)
    )
}

/// Loads the whole table into the store, in the connection's snapshot.
pub(crate) fn table(
    connection: &mut Connection,
    table: &Checked,
    store: &mut Store,
) -> Result<(), Error> {
    let name = table.name.as_str();
    store.set_types(name, &table.types).map_err(Error::Row)?;
    copy(connection, table, |rows| {
        store.add_rows(name, rows).map_err(Error::Row)
    })
}

/// Rows per batch the reader thread hands over. A batch starts a thread
/// per wider bucket width (`Rollup::add_rows`), so a small one costs.
pub(crate) const BATCH: usize = 8192;
/// Batches between the threads. With the one being read and the one being
/// added, a load holds six batches besides what it fills: about 1.2 MB per
/// column copied, however big the table.
const IN_FLIGHT: usize = 4;

/// Runs a binary `COPY` of the table and calls `each` with every batch of
/// rows, laid end to end. A second thread reads and decodes the next batch
/// meanwhile; spent buffers go back to it.
// `impl FnMut`: the caller's code runs on each batch as it comes, so the
// table never sits in memory whole.
pub(crate) fn copy(
    connection: &mut Connection,
    table: &Checked,
    mut each: impl FnMut(&[Datum]) -> Result<(), Error>,
) -> Result<(), Error> {
    // A fact has its time column, a dim its key.
    debug_assert_ne!(table.types.len(), 0);
    let width = table.types.len();
    let (full, filled) = mpsc::sync_channel::<Vec<Datum>>(IN_FLIGHT);
    let (spent, empty) = mpsc::channel::<Vec<Datum>>();
    thread::scope(|scope| {
        let reader = scope.spawn(move || {
            read(connection, table, |batch| {
                let next = match empty.try_recv() {
                    Ok(spent) => spent,
                    Err(_) => Vec::with_capacity(BATCH * width),
                };
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
        let read = match reader.join() {
            Ok(read) => read,
            Err(payload) => panic::resume_unwind(payload),
        };
        result.and(read)
    })
}

/// Runs a binary `COPY` of the table, decoding rows into a batch and
/// calling `hand_over` with each full one and the last, which it swaps for
/// an empty one.
// `impl FnMut`: the batch goes to the other thread as soon as it is full.
fn read(
    connection: &mut Connection,
    table: &Checked,
    mut hand_over: impl FnMut(&mut Vec<Datum>) -> Result<(), Error>,
) -> Result<(), Error> {
    let types = &table.types;
    let mut batch = Vec::with_capacity(BATCH * types.len());
    let mut started = false;
    let mut ended = false;
    connection.copy_out(&copy_sql(table), |mut data| {
        if !started {
            data = header(data)?;
            started = true;
        }
        // PostgreSQL sends a row per message, but nothing here needs it to.
        while !data.is_empty() {
            if ended {
                return Err(Error::Protocol(Unexpected::DataAfterCopy));
            }
            let fields = i16::from_be_bytes(take(&mut data)?);
            if fields == -1 {
                ended = true;
                continue;
            }
            if usize::try_from(fields).ok() != Some(types.len()) {
                return Err(Error::Protocol(Unexpected::CopyRowWidth {
                    fields,
                    columns: types.len(),
                }));
            }
            for (name, column_type) in table.columns.iter().zip(types) {
                let length = i32::from_be_bytes(take(&mut data)?);
                batch.push(match usize::try_from(length) {
                    // -1 is null.
                    Err(_) => Datum::Null,
                    Ok(length) => {
                        let Some((value, rest)) = data.split_at_checked(length) else {
                            return Err(Error::Protocol(Unexpected::CopyValueCutShort));
                        };
                        data = rest;
                        read_binary(*column_type, value)
                            .map_err(|error| error.at(&table.qualified(), name))?
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
        return Err(Error::Protocol(Unexpected::CopyWithoutEnd));
    }
    if batch.is_empty() {
        Ok(())
    } else {
        hand_over(&mut batch)
    }
}

/// The data after the signature, flags and header extension.
fn header(data: &[u8]) -> Result<&[u8], Error> {
    let Some(mut data) = data.strip_prefix(SIGNATURE) else {
        return Err(Error::Protocol(Unexpected::CopyNotBinary));
    };
    let _flags: [u8; 4] = take(&mut data)?;
    let extension = u32::from_be_bytes(take(&mut data)?);
    // An extension past `usize` is past the data too.
    let extension = usize::try_from(extension).unwrap_or(usize::MAX);
    let Some(rest) = data.get(extension..) else {
        return Err(Error::Protocol(Unexpected::CopyHeaderCutShort));
    };
    Ok(rest)
}

/// The next `N` bytes.
fn take<const N: usize>(data: &mut &[u8]) -> Result<[u8; N], Error> {
    let Some((head, rest)) = data.split_first_chunk() else {
        return Err(Error::Protocol(Unexpected::CopyRowCutShort));
    };
    *data = rest;
    Ok(*head)
}

#[cfg(test)]
mod tests {
    use straum_domain::ColumnType;

    use super::*;

    /// A load copies each table alone, the columns the spec reads of it.
    #[test]
    fn copies_the_columns_read() {
        let tasks = Checked::public(
            "tasks",
            "tasks",
            &["created", "organization_id", "taskgroup_id", "location_id"],
            vec![
                ColumnType::Time { zoned: false },
                ColumnType::Int,
                ColumnType::Int,
                ColumnType::Numeric { scale: 1 },
            ],
        );
        assert_eq!(
            copy_sql(&tasks),
            "COPY (SELECT \"created\", \"organization_id\", \"taskgroup_id\", \"location_id\" \
             FROM \"public\".\"tasks\") TO STDOUT (FORMAT binary)"
        );
    }
}
