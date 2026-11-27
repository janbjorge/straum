//! Loads: one binary `COPY` of a table for every spec table that reads
//! it, a dim or the views of a fact table, with each column any of them
//! reads sent once, so Straum groups or projects a row as it comes. One
//! thread reads PostgreSQL's copy messages and decodes them (`value.rs`)
//! into batches of rows; the caller's thread hands each spec table its own
//! columns of a batch and adds them to the store, counts the rows and
//! what the stream holds, then checks the stream reading beside it has
//! not failed. A load must not cost Straum the table's rows in memory.
//! Before any copy, the catalog gives each table's estimated size.

use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::{mem, panic, thread};

use postgres_protocol::escape::escape_identifier;
use straum_app::{Estimate, LoadCounts};
use straum_domain::{ColumnType, Datum, Identifier, Store};

use crate::catalog::{Checked, escaped};
use crate::error::Error;
use crate::stream::Stream;
use crate::unexpected::{BinaryCopy, Unexpected};
use crate::value::read_binary;
use crate::wire::{Connection, first_row_value};

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

/// The rows `ANALYZE` or `VACUUM` last counted in `table`, and the bytes
/// of its main fork now. `reltuples` is -1 for a table neither has read
/// since PostgreSQL 14, which is no estimate.
pub(crate) fn estimate(connection: &mut Connection, table: &Checked) -> Result<Estimate, Error> {
    let rows = connection.rows(&format!(
        "SELECT reltuples::bigint, pg_relation_size(oid) FROM pg_class WHERE oid = {}",
        table.oid
    ))?;
    let shape = Error::Protocol(Unexpected::CatalogShape("estimate"));
    let (Some(read), Some(bytes)) = (first_row_value(&rows, 0), first_row_value(&rows, 1)) else {
        return Err(shape);
    };
    let (Ok(read), Ok(bytes)) = (read.parse::<i64>(), bytes.parse::<u64>()) else {
        return Err(shape);
    };
    Ok(Estimate {
        rows: u64::try_from(read).ok(),
        bytes,
    })
}

/// Loads `tables`, spec tables that read one table of PostgreSQL, whole
/// into the store, in the connection's snapshot: one copy of the table
/// carries the columns of them all. The stream, which reads on meanwhile,
/// is checked after each batch, and `counts` takes the batch's rows and
/// what the stream holds.
pub(crate) fn tables(
    connection: &mut Connection,
    tables: &[&Checked],
    store: &mut Store,
    stream: &mut Stream,
    counts: &LoadCounts,
) -> Result<(), Error> {
    let Some(copied) = copy_of(tables) else {
        return Ok(());
    };
    let mut readers = Vec::new();
    for table in tables {
        let name = table.name.as_str();
        store.set_types(name, &table.types).map_err(Error::Row)?;
        readers.push(Reader::of(table, &copied));
    }
    // One more batch beside those in flight, of one reader's columns.
    let mut rows = Vec::new();
    let width = copied.columns.len();
    copy(connection, &copied, |batch| {
        for reader in &readers {
            reader.add(batch, width, &mut rows, store)?;
        }
        let read = u64::try_from(batch.len() / width).unwrap_or(u64::MAX);
        counts.rows.fetch_add(read, Ordering::Relaxed);
        (counts.held_bytes).store(stream.held_bytes(), Ordering::Relaxed);
        stream.check()
    })
}

/// The table as one copy reads it for all of `tables`: every column any
/// of them reads, once each, in the order first read. Its name is the
/// first one's, which a copy doesn't read. `None` when there are none.
fn copy_of(tables: &[&Checked]) -> Option<Checked> {
    let first = tables.first()?;
    let mut copied = Checked {
        columns: Vec::new(),
        types: Vec::new(),
        ..Checked::clone(first)
    };
    for table in tables {
        debug_assert_eq!(table.qualified(), copied.qualified());
        for (column, column_type) in table.columns.iter().zip(&table.types) {
            if position(&copied, column, *column_type).is_none() {
                copied.columns.push(column.clone());
                copied.types.push(*column_type);
            }
        }
    }
    Some(copied)
}

/// Where the rows of `copied` hold `column` read as `column_type`.
fn position(copied: &Checked, column: &Identifier, column_type: ColumnType) -> Option<usize> {
    for (at, (name, read_as)) in copied.columns.iter().zip(&copied.types).enumerate() {
        if name == column && *read_as == column_type {
            return Some(at);
        }
    }
    None
}

/// A spec table of a copy, and where its columns sit in the copy's rows.
struct Reader {
    /// The spec's name for the table.
    name: Identifier,
    /// For each column the table reads, its place in a row of the copy.
    positions: Vec<usize>,
}

impl Reader {
    /// `table` reading from `copied`, which holds each of its columns.
    fn of(table: &Checked, copied: &Checked) -> Reader {
        let mut positions = Vec::new();
        for (column, column_type) in table.columns.iter().zip(&table.types) {
            let at = position(copied, column, *column_type);
            positions.push(at.expect("the copy reads every column of its tables"));
        }
        Reader {
            name: table.name.clone(),
            positions,
        }
    }

    /// Adds the table's rows of `batch`, rows of the copy `width` columns
    /// each, to the store. `rows` is scratch space, kept between batches.
    fn add(
        &self,
        batch: &[Datum],
        width: usize,
        rows: &mut Vec<Datum>,
        store: &mut Store,
    ) -> Result<(), Error> {
        let name = self.name.as_str();
        // A table that reads the copy's columns as they come, as one
        // loaded alone does, takes the batch as it is.
        if self.positions.iter().copied().eq(0..width) {
            return store.add_rows(name, batch).map_err(Error::Row);
        }
        rows.clear();
        for row in batch.chunks_exact(width) {
            for &position in &self.positions {
                rows.push(row[position].clone());
            }
        }
        store.add_rows(name, rows).map_err(Error::Row)
    }
}

/// Rows per batch the reader thread hands over. A batch starts a thread
/// per wider bucket width (`Rollup::add_rows`), so a small one costs.
pub(crate) const BATCH: usize = 8192;
/// Batches between the threads. With the one being read and the one being
/// added, a load holds six batches besides what it fills: about 1.2 MB per
/// column copied, however big the table. A table several views read holds
/// one more, of one view's columns.
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
        // PostgreSQL sends each row whole in one message, and the header in
        // the first. A row split across two messages fails as cut short.
        while !data.is_empty() {
            if ended {
                return Err(Error::Protocol(Unexpected::Copy(BinaryCopy::DataAfterEnd)));
            }
            let fields = i16::from_be_bytes(take(&mut data)?);
            if fields == -1 {
                ended = true;
                continue;
            }
            if usize::try_from(fields).ok() != Some(types.len()) {
                return Err(Error::Protocol(Unexpected::Copy(BinaryCopy::RowWidth {
                    fields,
                    columns: types.len(),
                })));
            }
            for (name, column_type) in table.columns.iter().zip(types) {
                let length = i32::from_be_bytes(take(&mut data)?);
                batch.push(match usize::try_from(length) {
                    // -1 is null.
                    Err(_) if length == -1 => Datum::Null,
                    Err(_) => {
                        return Err(Error::Protocol(Unexpected::Copy(BinaryCopy::ValueLength(
                            length,
                        ))));
                    }
                    Ok(length) => {
                        let Some((value, rest)) = data.split_at_checked(length) else {
                            return Err(Error::Protocol(Unexpected::Copy(
                                BinaryCopy::ValueCutShort,
                            )));
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
        return Err(Error::Protocol(Unexpected::Copy(BinaryCopy::WithoutEnd)));
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
        return Err(Error::Protocol(Unexpected::Copy(BinaryCopy::NotBinary)));
    };
    let _flags: [u8; 4] = take(&mut data)?;
    let extension = u32::from_be_bytes(take(&mut data)?);
    // An extension past `usize` is past the data too.
    let extension = usize::try_from(extension).unwrap_or(usize::MAX);
    let Some(rest) = data.get(extension..) else {
        return Err(Error::Protocol(Unexpected::Copy(
            BinaryCopy::HeaderCutShort,
        )));
    };
    Ok(rest)
}

/// The next `N` bytes.
fn take<const N: usize>(data: &mut &[u8]) -> Result<[u8; N], Error> {
    let Some((head, rest)) = data.split_first_chunk() else {
        return Err(Error::Protocol(Unexpected::Copy(BinaryCopy::RowCutShort)));
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

    /// `name` reading `columns` of `public.rides`, each an integer.
    fn rides(name: &str, columns: &[&str]) -> Checked {
        Checked::public(name, "rides", columns, vec![ColumnType::Int; columns.len()])
    }

    /// One copy serves every spec table of a table: a column two of them
    /// read is sent once, and each finds its own columns in a row.
    #[test]
    fn one_copy_carries_the_columns_of_every_table_that_reads_it() {
        let trips = rides("trips", &["at", "kind", "fare"]);
        let dropoffs = rides("dropoffs", &["at", "zone", "fare"]);
        let copied = copy_of(&[&trips, &dropoffs]).unwrap();
        assert_eq!(
            copy_sql(&copied),
            "COPY (SELECT \"at\", \"kind\", \"fare\", \"zone\" \
             FROM \"public\".\"rides\") TO STDOUT (FORMAT binary)"
        );
        assert_eq!(Reader::of(&trips, &copied).positions, [0, 1, 2]);
        assert_eq!(Reader::of(&dropoffs, &copied).positions, [0, 3, 2]);
        assert!(copy_of(&[]).is_none());
    }

    /// A column two spec tables read as different types is sent for each,
    /// so neither reads the other's value.
    #[test]
    fn a_column_read_as_two_types_is_copied_for_each() {
        let numbers = rides("numbers", &["at", "kind"]);
        let mut words = rides("words", &["at", "kind"]);
        words.types[1] = ColumnType::Text;
        let copied = copy_of(&[&numbers, &words]).unwrap();
        assert_eq!(copied.columns.len(), 3);
        assert_eq!(
            copied.types,
            [ColumnType::Int, ColumnType::Int, ColumnType::Text]
        );
        assert_eq!(Reader::of(&numbers, &copied).positions, [0, 1]);
        assert_eq!(Reader::of(&words, &copied).positions, [0, 2]);
    }
}
