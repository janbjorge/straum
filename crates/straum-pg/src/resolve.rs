//! Refs resolved for streamed fact rows: one keyed lookup per ref and
//! commit on a plain connection, so a fact row carries its attributes as a
//! loaded row does and straum holds no dim row. The lookup reads the dims
//! as they are now, not as they were at the commit, which is what a stream
//! engine's lookup join does too: a dim change shows in the rows after it,
//! and never rewrites the cells before it.

use std::collections::{BTreeSet, HashMap};
use std::iter;

use postgres_protocol::escape::{escape_identifier, escape_literal};
use straum_app::{Change, Commit};
use straum_domain::{ColumnType, Datum, Ident, Spec};

use crate::catalog::Tables;
use crate::error::Error;
use crate::value::read;
use crate::wire::Conn;

/// Keys per statement: a bound on the statement's length, not on the rows
/// a transaction may change.
const KEYS_PER_QUERY: usize = 1_000;

pub(crate) struct Resolver {
    facts: Vec<FactRefs>,
}

struct FactRefs {
    name: Ident,
    /// Columns a row has as the stream brings it, before its attributes.
    width: usize,
    refs: Vec<Lookup>,
}

/// One ref's lookup: `SELECT key, attrs FROM dim WHERE (key) IN (...)`.
struct Lookup {
    /// The statement up to the key list.
    sql: String,
    /// `schema.table` of the dim, for the error a value that doesn't read
    /// gives.
    qualified: String,
    /// Where the via columns sit in a fact row.
    via: Vec<usize>,
    via_types: Vec<ColumnType>,
    /// The dim columns the statement selects: its key, `key_width` of
    /// them, then the attributes.
    columns: Vec<(Ident, ColumnType)>,
    key_width: usize,
}

impl Resolver {
    pub(crate) fn new(spec: &Spec, tables: &Tables) -> Resolver {
        let facts = (spec.facts().iter())
            .map(|fact| {
                let table = tables.fact(&fact.name);
                let columns = fact.columns();
                let refs = (fact.refs.iter())
                    .map(|r| {
                        let dim = tables.dim(&r.dim);
                        let key = &dim.columns[..dim.key_width];
                        let dim_columns: Vec<(Ident, ColumnType)> = (key.iter().chain(&r.attrs))
                            .map(|c| (c.clone(), dim.column_type(c)))
                            .collect();
                        let selected: Vec<String> = (dim_columns.iter())
                            .map(|(c, _)| escape_identifier(c.as_str()))
                            .collect();
                        let keys: Vec<String> =
                            key.iter().map(|c| escape_identifier(c.as_str())).collect();
                        Lookup {
                            sql: format!(
                                "SELECT {} FROM {}.{} WHERE ({}) IN ",
                                selected.join(", "),
                                escape_identifier(dim.schema.as_str()),
                                escape_identifier(dim.table.as_str()),
                                keys.join(", ")
                            ),
                            qualified: dim.qualified(),
                            via: (r.via.iter())
                                .map(|v| {
                                    (columns.iter().position(|c| c == v))
                                        .expect("Fact::columns holds every via column")
                                })
                                .collect(),
                            via_types: r.via.iter().map(|v| table.column_type(v)).collect(),
                            columns: dim_columns,
                            key_width: key.len(),
                        }
                    })
                    .collect();
                FactRefs {
                    name: fact.name.clone(),
                    width: columns.len(),
                    refs,
                }
            })
            .collect();
        Resolver { facts }
    }

    /// Appends each fact row's attributes, looked up as the dims are now.
    /// A key with no row, or with a null in it, gives nulls, as the view's
    /// `LEFT JOIN` does. A row of the wrong width is left for the store to
    /// refuse.
    pub(crate) fn resolve(&self, conn: &mut Conn, commit: &mut Commit) -> Result<(), Error> {
        for fact in &self.facts {
            if fact.refs.is_empty() {
                continue;
            }
            let mut rows: Vec<&mut Vec<Datum>> = Vec::new();
            for change in &mut commit.changes {
                match change {
                    Change::Insert { table, row } if *table == fact.name => rows.push(row),
                    Change::Update { table, old, new } if *table == fact.name => {
                        rows.push(old);
                        rows.push(new);
                    }
                    Change::Delete { table, old } if *table == fact.name => rows.push(old),
                    _ => {}
                }
            }
            rows.retain(|row| row.len() == fact.width);
            if rows.is_empty() {
                continue;
            }
            for lookup in &fact.refs {
                let keys: BTreeSet<Vec<Datum>> =
                    rows.iter().filter_map(|r| lookup.key(r)).collect();
                let found = lookup.fetch(conn, &keys)?;
                for row in &mut rows {
                    match lookup.key(row).and_then(|key| found.get(&key)) {
                        Some(attrs) => row.extend_from_slice(attrs),
                        None => row.extend(iter::repeat_n(
                            Datum::Null,
                            lookup.columns.len() - lookup.key_width,
                        )),
                    }
                }
            }
        }
        Ok(())
    }
}

impl Lookup {
    /// The via values of a row as a key, or `None` when one is null: null
    /// joins nothing.
    fn key(&self, row: &[Datum]) -> Option<Vec<Datum>> {
        (self.via.iter())
            .map(|&v| match &row[v] {
                Datum::Null => None,
                datum => Some(datum.clone()),
            })
            .collect()
    }

    /// The attributes of each key the dim has a row for.
    fn fetch(
        &self,
        conn: &mut Conn,
        keys: &BTreeSet<Vec<Datum>>,
    ) -> Result<HashMap<Vec<Datum>, Vec<Datum>>, Error> {
        let mut found = HashMap::new();
        let keys: Vec<&Vec<Datum>> = keys.iter().collect();
        for chunk in keys.chunks(KEYS_PER_QUERY) {
            let list: Vec<String> = (chunk.iter())
                .map(|key| {
                    let values: Vec<String> = (key.iter().zip(&self.via_types))
                        .map(|(d, t)| literal(d, *t))
                        .collect();
                    format!("({})", values.join(", "))
                })
                .collect();
            let sql = format!("{}({})", self.sql, list.join(", "));
            conn.query(&sql, |fields| {
                let mut key = Vec::with_capacity(fields.len());
                for (field, (name, column_type)) in fields.iter().zip(&self.columns) {
                    key.push(match field {
                        None => Datum::Null,
                        Some(text) => {
                            read(*column_type, text).map_err(|e| e.at(&self.qualified, name))?
                        }
                    });
                }
                let attrs = key.split_off(self.key_width);
                found.insert(key, attrs);
                Ok(())
            })?;
        }
        Ok(found)
    }
}

/// A key value as a SQL literal of its column's type. A null never gets
/// here, since a key with one is skipped; it reads as `NULL`, which
/// matches nothing, all the same.
fn literal(datum: &Datum, column_type: ColumnType) -> String {
    match (datum, column_type) {
        (Datum::Int(n), ColumnType::Numeric { scale }) => decimal(*n, scale),
        (Datum::Int(n), _) => n.to_string(),
        (Datum::Bool(b), _) => b.to_string(),
        (Datum::Text(s), _) => escape_literal(s),
        (Datum::Null, _) => "NULL".to_owned(),
    }
}

/// `-12345` at scale 2 is `-123.45`.
fn decimal(n: i64, scale: u32) -> String {
    if scale == 0 {
        return n.to_string();
    }
    let unit = 10_u64.pow(scale);
    let sign = if n < 0 { "-" } else { "" };
    let (whole, fraction) = (n.unsigned_abs() / unit, n.unsigned_abs() % unit);
    format!("{sign}{whole}.{fraction:0width$}", width = scale as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::tasks_tables as tables;

    #[test]
    fn literals_take_the_columns_type() {
        let cases = [
            (Datum::Int(42), ColumnType::Int, "42"),
            (Datum::Int(-7), ColumnType::Int, "-7"),
            (Datum::Int(1250), ColumnType::Numeric { scale: 2 }, "12.50"),
            (Datum::Int(-5), ColumnType::Numeric { scale: 2 }, "-0.05"),
            (Datum::Int(7), ColumnType::Numeric { scale: 0 }, "7"),
            (Datum::Bool(true), ColumnType::Bool, "true"),
            (Datum::Text("it's".into()), ColumnType::Text, "'it''s'"),
            (Datum::Text("a\\b".into()), ColumnType::Text, " E'a\\\\b'"),
            (Datum::Null, ColumnType::Int, "NULL"),
        ];
        for (datum, column_type, expected) in cases {
            assert_eq!(literal(&datum, column_type), expected, "{datum:?}");
        }
    }

    #[test]
    fn lookups_follow_the_spec() {
        let (spec, tables) = tables();
        let resolver = Resolver::new(&spec, &tables);
        let tasks = &resolver.facts[0];
        assert_eq!((tasks.name.as_str(), tasks.width), ("tasks", 4));
        let [groups, locations] = tasks.refs.as_slice() else {
            panic!("two refs");
        };
        assert_eq!(
            groups.sql,
            "SELECT \"organization_id\", \"taskgroup_id\", \"category\" FROM \"public\".\"taskgroups\" \
             WHERE (\"organization_id\", \"taskgroup_id\") IN "
        );
        assert_eq!(
            (groups.via.as_slice(), groups.qualified.as_str()),
            (&[1, 2][..], "public.taskgroups")
        );
        assert_eq!(
            locations.sql,
            "SELECT \"id\", \"location_type\" FROM \"public\".\"locations\" WHERE (\"id\") IN "
        );
        assert_eq!(locations.via, [3]);
        assert_eq!(locations.via_types, [ColumnType::Numeric { scale: 1 }]);

        // A key with a null joins nothing.
        let row = [Datum::Int(0), Datum::Int(1), Datum::Int(2), Datum::Int(35)];
        assert_eq!(groups.key(&row), Some(vec![Datum::Int(1), Datum::Int(2)]));
        assert_eq!(locations.key(&row), Some(vec![Datum::Int(35)]));
        let mut no_group = row.clone();
        no_group[2] = Datum::Null;
        assert_eq!(groups.key(&no_group), None);
        assert_eq!(locations.key(&no_group), Some(vec![Datum::Int(35)]));
    }
}
