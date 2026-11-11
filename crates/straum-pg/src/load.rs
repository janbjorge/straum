//! Loads: a binary `COPY` of the columns the spec reads, each fact joined
//! to its dims in PostgreSQL, so a row comes whole and straum groups it as
//! it comes. A load must not cost straum the table's rows in memory; what
//! the join costs PostgreSQL is the planner's, as it is for the view.

use std::sync::mpsc;
use std::{mem, panic, thread};

use postgres_protocol::escape::escape_identifier;
use straum_domain::{ColumnType, Datum, Fact, Ident, Store};

use crate::catalog::{Table, Tables};
use crate::error::Error;
use crate::value::read_binary;
use crate::wire::Conn;

/// What starts a binary copy, before its flags and header extension.
const SIGNATURE: &[u8] = b"PGCOPY\n\xff\r\n\0";

fn qualified(table: &Table) -> String {
    format!(
        "{}.{}",
        escape_identifier(table.schema.as_str()),
        escape_identifier(table.table.as_str())
    )
}

/// The fact's row as the spec joins it: `t0` is the fact and `t1` on its
/// refs, the aliases plain since no identifier straum accepts is quoted.
pub(crate) fn copy_sql(tables: &Tables, fact: &Fact) -> String {
    let fact_table = tables.fact(&fact.name);
    let mut select: Vec<String> = (fact.columns().iter())
        .map(|c| format!("t0.{}", escape_identifier(c.as_str())))
        .collect();
    let mut joins = String::new();
    for (i, r) in fact.refs.iter().enumerate() {
        let alias = format!("t{}", i + 1);
        let dim = tables.dim(&r.dim);
        select.extend(
            (r.attrs.iter()).map(|a| format!("{alias}.{}", escape_identifier(a.column.as_str()))),
        );
        let on: Vec<String> = (dim.columns[..dim.key_width].iter().zip(&r.via))
            .map(|(key, via)| {
                format!(
                    "{alias}.{} = t0.{}",
                    escape_identifier(key.as_str()),
                    escape_identifier(via.as_str())
                )
            })
            .collect();
        joins.push_str(&format!(
            " LEFT JOIN {} AS {alias} ON {}",
            qualified(dim),
            on.join(" AND ")
        ));
    }
    format!(
        "COPY (SELECT {} FROM {} AS t0{joins}) TO STDOUT (FORMAT binary)",
        select.join(", "),
        qualified(fact_table)
    )
}

/// The columns a copy reads, named so a value that doesn't read says
/// where it was.
pub(crate) struct Columns {
    pub(crate) table: String,
    pub(crate) names: Vec<Ident>,
    pub(crate) types: Vec<ColumnType>,
}

/// The types of a fact's row: its columns', then its attributes', which
/// are the dim columns'.
pub(crate) fn fact_types(tables: &Tables, fact: &Fact) -> Vec<ColumnType> {
    let table = tables.fact(&fact.name);
    (fact.columns().iter().map(|c| table.column_type(c)))
        .chain(
            fact.attrs()
                .map(|(r, a)| tables.dim(&r.dim).column_type(&a.column)),
        )
        .collect()
}

pub(crate) fn fact(
    conn: &mut Conn,
    tables: &Tables,
    fact: &Fact,
    store: &mut Store,
) -> Result<(), Error> {
    let name = fact.name.as_str();
    let types = fact_types(tables, fact);
    store.set_fact_types(name, &types).map_err(Error::Row)?;
    let names = (fact.columns().into_iter())
        .chain(fact.attrs().map(|(_, a)| a.column.clone()))
        .collect();
    let columns = Columns {
        table: tables.fact(&fact.name).qualified(),
        names,
        types,
    };
    copy(conn, &copy_sql(tables, fact), &columns, |rows| {
        store.add_fact_rows(name, rows).map_err(Error::Row)
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
    columns: &Columns,
    mut each: impl FnMut(&[Datum]) -> Result<(), Error>,
) -> Result<(), Error> {
    // A fact has its time column.
    debug_assert!(!columns.types.is_empty());
    let width = columns.types.len();
    let (full, filled) = mpsc::sync_channel::<Vec<Datum>>(IN_FLIGHT);
    let (spent, empty) = mpsc::channel::<Vec<Datum>>();
    thread::scope(|scope| {
        let reader = scope.spawn(move || {
            read(conn, sql, columns, |batch| {
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
    columns: &Columns,
    mut hand_over: impl FnMut(&mut Vec<Datum>) -> Result<(), Error>,
) -> Result<(), Error> {
    let types = &columns.types;
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
                return Err(copy_error("data after the end of a copy"));
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
            for (name, column_type) in columns.names.iter().zip(types) {
                let len = i32::from_be_bytes(take(&mut data)?);
                batch.push(match usize::try_from(len) {
                    // -1 is null.
                    Err(_) => Datum::Null,
                    Ok(len) => {
                        let (value, rest) = data
                            .split_at_checked(len)
                            .ok_or_else(|| copy_error("a copy value cut short"))?;
                        data = rest;
                        read_binary(*column_type, value).map_err(|e| e.at(&columns.table, name))?
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
