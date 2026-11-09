//! Loads: one `GROUP BY` per fact, so the rollup reads cells instead of
//! rows, and a plain `SELECT` per dim.

use std::iter;
use std::num::NonZeroU64;

use postgres_protocol::escape::escape_identifier;
use straum_domain::{Datum, DimTable, Fact, Ident, Rollup, Sum};

use crate::catalog::Table;
use crate::error::Error;
use crate::value::{Kind, read_sum};
use crate::wire::Conn;

/// Groups by [`Rollup::grouped`] with the time column as its bucket, and
/// returns the row count and, per [`Rollup::summed`] column, its sum and
/// count of non-null values.
pub(crate) fn fact_sql(fact: &Fact, table: &Table, rollup: &Rollup) -> String {
    let column = |c: &Ident| format!("t.{}", escape_identifier(c.as_str()));
    let mut grouped = rollup.grouped();
    let time = grouped.next().expect("the time column comes first");
    let bucket = format!(
        "extract(epoch FROM date_bin('{} seconds', {}, timestamptz 'epoch'))::int8",
        fact.time.grain.0,
        column(time)
    );
    let select: Vec<String> = iter::once(bucket)
        .chain(grouped.map(column))
        .chain(iter::once("count(*)".to_owned()))
        .chain(rollup.summed().flat_map(|c| {
            let c = column(c);
            [format!("sum({c})"), format!("count({c})")]
        }))
        .collect();
    let group_by: Vec<String> = (1..=rollup.grouped().count())
        .map(|i| i.to_string())
        .collect();
    format!(
        "SELECT {} FROM {} t GROUP BY {}",
        select.join(", "),
        from(table),
        group_by.join(", ")
    )
}

pub(crate) fn dim_sql(table: &Table) -> String {
    let columns: Vec<String> = table
        .columns
        .iter()
        .map(|c| escape_identifier(c.as_str()))
        .collect();
    format!("SELECT {} FROM {}", columns.join(", "), from(table))
}

fn from(table: &Table) -> String {
    format!(
        "{}.{}",
        escape_identifier(table.schema.as_str()),
        escape_identifier(table.table.as_str())
    )
}

pub(crate) fn fact(
    conn: &mut Conn,
    fact: &Fact,
    table: &Table,
    rollup: &mut Rollup,
) -> Result<(), Error> {
    let sql = fact_sql(fact, table, rollup);
    let column_scales: Vec<u32> = (rollup.columns().iter())
        .map(|c| table.kind(c).scale())
        .collect();
    rollup.set_scales(&column_scales).map_err(Error::Row)?;
    // The bucket comes back as an integer.
    let kinds: Vec<Kind> = iter::once(Kind::Int)
        .chain(rollup.grouped().skip(1).map(|c| table.kind(c)))
        .collect();
    let scales: Vec<u32> = rollup.summed().map(|c| table.kind(c).scale()).collect();
    let width = kinds.len() + 1 + 2 * scales.len();
    conn.query(&sql, |fields| {
        if fields.len() != width {
            return Err(Error::Protocol(format!(
                "a load row of {} columns, not {width}",
                fields.len()
            )));
        }
        let (values, rest) = fields.split_at(kinds.len());
        let values = read_row(&kinds, values)?;
        let rows = count(rest[0])
            .and_then(NonZeroU64::new)
            .ok_or_else(|| Error::Protocol("a load group without rows".into()))?;
        let sums = scales
            .iter()
            .zip(rest[1..].chunks(2))
            .map(|(&scale, pair)| {
                let sum = pair[0].map_or(Ok(0), |s| read_sum(s, scale))?;
                let count = count(pair[1])
                    .ok_or_else(|| Error::Protocol("a load count that isn't one".into()))?;
                Ok(Sum { sum, count })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        rollup.add_group(&values, rows, &sums).map_err(Error::Row)
    })
}

pub(crate) fn dim(conn: &mut Conn, table: &Table, rows: &mut DimTable) -> Result<(), Error> {
    let scales: Vec<u32> = table.kinds.iter().map(|k| k.scale()).collect();
    rows.set_scales(&scales).map_err(Error::Row)?;
    conn.query(&dim_sql(table), |fields| {
        let row = read_row(&table.kinds, fields)?;
        rows.upsert(&row).map_err(Error::Row)
    })
}

fn read_row(kinds: &[Kind], fields: &[Option<&str>]) -> Result<Vec<Datum>, Error> {
    kinds
        .iter()
        .zip(fields)
        .map(|(kind, field)| match field {
            None => Ok(Datum::Null),
            Some(text) => kind.read(text).map_err(Error::Value),
        })
        .collect()
}

fn count(field: Option<&str>) -> Option<u64> {
    field?.parse().ok()
}
