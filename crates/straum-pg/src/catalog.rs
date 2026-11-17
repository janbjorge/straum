//! The setup check: does PostgreSQL hold the spec's tables in a shape
//! Straum can read and follow? `query` reads PostgreSQL's catalog (its
//! system tables: `pg_class`, `pg_attribute`, `pg_type`, `pg_enum`,
//! `pg_publication_tables`) on the connection that loads, so it sees the
//! tables as the load does; `check` compares that with the spec and gives
//! `source.rs` the tables ready to load, or every `Problem` found.

use postgres_protocol::escape::{escape_identifier, escape_literal};
use straum_domain::{
    ByKey, ColumnType, Dim, Fact, Identifier, Key, Literal, Origin, Reads, Spec, Via, decimal,
};

use crate::error::Error;
use crate::problem::{
    ColumnFix, ColumnProblem, Filter, Problem, ServerProblem, TableNames, TableProblem, ViewProblem,
};
use crate::source::Replication;
use crate::unexpected::Unexpected;
use crate::value::{Oid, type_of};
use crate::wire::Connection;

/// `server_version_num` is the major times 10 000, plus the minor.
pub(crate) const VERSION_NUM_PER_MAJOR: u32 = 10_000;
/// The oldest server Straum reads: 17 (ADR-0007).
const OLDEST_VERSION_NUM: u32 = 170_000;
/// From 19, a logical slot turns logical decoding on under `replica`.
const LOGICAL_UNDER_REPLICA_VERSION_NUM: u32 = 190_000;

/// A spec table as PostgreSQL has it, ready to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Checked {
    /// The spec's name for it.
    pub(crate) name: Identifier,
    pub(crate) schema: Identifier,
    pub(crate) table_name: Identifier,
    /// What Straum reads of it, in order: `Fact::columns`, or the dim's
    /// key columns and then its attributes.
    pub(crate) columns: Vec<Identifier>,
    pub(crate) types: Vec<ColumnType>,
    /// The enum types its `::text` columns have, by OID. An enum's OID is
    /// its own, so the stream can't know one from a type Straum doesn't
    /// read without this list.
    pub(crate) enums: Vec<Oid>,
}

impl Checked {
    /// The type Straum reads `column` as; the column is one it reads.
    pub(crate) fn column_type(&self, column: &Identifier) -> ColumnType {
        let mut found = None;
        for (candidate, column_type) in self.columns.iter().zip(&self.types) {
            if candidate == column {
                found = Some(*column_type);
            }
        }
        found.expect("the column is in the table's row")
    }

    /// `schema.table`, unquoted, for messages.
    pub(crate) fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.table_name)
    }

    /// `name`, reading `columns` of `public.table`, none an enum.
    #[cfg(test)]
    pub(crate) fn public(
        name: &str,
        table: &str,
        columns: &[&str],
        types: Vec<ColumnType>,
    ) -> Checked {
        Checked {
            name: Identifier::new(name).unwrap(),
            schema: Identifier::new("public").unwrap(),
            table_name: Identifier::new(table).unwrap(),
            columns: identifiers(columns),
            types,
            enums: Vec::new(),
        }
    }
}

/// Each of `names` as an identifier, which a test knows is valid.
#[cfg(test)]
fn identifiers(names: &[&str]) -> Vec<Identifier> {
    let mut identifiers = Vec::new();
    for name in names {
        identifiers.push(Identifier::new(name).unwrap());
    }
    identifiers
}

/// `"schema"."table"`, quoted for SQL.
pub(crate) fn escaped(schema: &Identifier, table: &Identifier) -> String {
    format!(
        "{}.{}",
        escape_identifier(schema.as_str()),
        escape_identifier(table.as_str())
    )
}

/// The spec's tables as checked, facts and dims, each loaded whole and
/// followed through the publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tables {
    pub(crate) facts: Vec<Checked>,
    pub(crate) dims: Vec<Checked>,
}

impl Tables {
    /// A table by the spec's name for it, dim or fact.
    pub(crate) fn get(&self, name: &Identifier) -> &Checked {
        if let Some(fact) = named(&self.facts, name) {
            return fact;
        }
        named(&self.dims, name).expect("the spec's tables are checked")
    }

    /// A fact by the spec's name for it.
    pub(crate) fn fact(&self, name: &Identifier) -> &Checked {
        named(&self.facts, name).expect("the spec's facts are checked")
    }

    /// A dim by the spec's name for it.
    pub(crate) fn dim(&self, name: &Identifier) -> &Checked {
        named(&self.dims, name).expect("the spec's dims are checked")
    }
}

/// The table of `tables` the spec calls `name`, if there is one.
// 'a: the table found is borrowed from `tables`, not from `name`.
fn named<'a>(tables: &'a [Checked], name: &Identifier) -> Option<&'a Checked> {
    for table in tables {
        if table.name == *name {
            return Some(table);
        }
    }
    None
}

/// What Straum reads from the catalog; `check` holds every rule.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Catalog {
    pub(crate) server_version: u32,
    pub(crate) wal_level: String,
    pub(crate) full_page_writes: String,
    pub(crate) wal_compression: String,
    /// Whether the publication publishes inserts, updates, deletes and
    /// truncates; `None` when it doesn't exist.
    pub(crate) publication: Option<bool>,
    pub(crate) classes: Vec<Class>,
    /// Every label of every enum type a spec column has, in type order.
    pub(crate) labels: Vec<EnumLabel>,
}

/// One value of an enum type, as `pg_enum` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnumLabel {
    /// The label's own OID, which is what a row on disk stores.
    pub(crate) oid: Oid,
    pub(crate) type_oid: Oid,
    pub(crate) label: String,
}

/// A spec table as `pg_class` (PostgreSQL's list of tables) has it, with
/// what the publication sends of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Class {
    pub(crate) schema: String,
    pub(crate) name: String,
    /// `pg_class.relkind`: `r` for a plain table.
    pub(crate) relkind: String,
    /// Whether another table inherits from it (`pg_inherits`).
    pub(crate) children: bool,
    /// `pg_class.relreplident`: `d`efault, `f`ull, `i`ndex or `n`othing.
    pub(crate) identity: String,
    pub(crate) select: bool,
    /// The columns the publication sends; `None` when it leaves the table
    /// out.
    pub(crate) published: Option<Vec<String>>,
    pub(crate) row_filter: bool,
    pub(crate) columns: Vec<Column>,
}

/// A column of a spec table as `pg_attribute` and `pg_type` have it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Column {
    pub(crate) name: String,
    pub(crate) type_oid: Oid,
    pub(crate) typmod: i32,
    /// As `format_type` prints it, for messages.
    pub(crate) type_name: String,
    /// `pg_type.typtype`: `e` for an enum.
    pub(crate) typtype: String,
}

/// Reads the catalog and checks it against the spec.
pub(crate) fn read(
    connection: &mut Connection,
    spec: &Spec,
    publication: &str,
    replication: Replication,
) -> Result<Tables, Error> {
    let catalog = query(connection, spec, publication)?;
    check(spec, &catalog, publication, replication).map_err(Error::Setup)
}

/// Reads what the setup check needs from the catalog: settings, the spec's
/// tables, their columns and their enums' labels.
pub(crate) fn query(
    connection: &mut Connection,
    spec: &Spec,
    publication: &str,
) -> Result<Catalog, Error> {
    let publication = escape_literal(publication);
    let tables = table_list(spec);
    let mut catalog = settings(connection, &publication)?;
    catalog.classes = classes(connection, &publication, &tables)?;
    read_columns(connection, &tables, &mut catalog.classes)?;
    catalog.labels = labels(connection, &tables)?;
    Ok(catalog)
}

/// The spec's tables as an SQL list of `(schema, table)` pairs.
fn table_list(spec: &Spec) -> String {
    let mut pairs = Vec::new();
    for table in spec.tables() {
        pairs.push(format!(
            "({}, {})",
            escape_literal(table.schema.as_str()),
            escape_literal(table.name.as_str())
        ));
    }
    pairs.join(", ")
}

/// The server's settings, in a catalog with nothing else yet.
fn settings(connection: &mut Connection, publication: &str) -> Result<Catalog, Error> {
    let settings = connection.rows(&format!(
        "SELECT current_setting('server_version_num'), current_setting('wal_level'),
                current_setting('full_page_writes'), current_setting('wal_compression'),
                (SELECT pubinsert AND pubupdate AND pubdelete AND pubtruncate
                   FROM pg_publication WHERE pubname = {publication})"
    ))?;
    let Some(row) = settings.into_iter().next() else {
        return Err(shape("settings"));
    };
    let [
        version,
        wal_level,
        full_page_writes,
        wal_compression,
        published,
    ] = row.try_into().map_err(|_| shape("settings"))?;
    let server_version =
        (version.as_deref().unwrap_or_default().parse()).map_err(|_| shape("settings"))?;
    Ok(Catalog {
        server_version,
        wal_level: wal_level.unwrap_or_default(),
        full_page_writes: full_page_writes.unwrap_or_default(),
        wal_compression: wal_compression.unwrap_or_default(),
        publication: published.as_deref().map(is_true),
        ..Catalog::default()
    })
}

/// The spec's tables as `pg_class` has them, with no columns yet.
fn classes(
    connection: &mut Connection,
    publication: &str,
    tables: &str,
) -> Result<Vec<Class>, Error> {
    let rows = connection.rows(&format!(
        "SELECT n.nspname, c.relname, c.relkind, c.relreplident,
                has_table_privilege(c.oid, 'SELECT'),
                array_to_string(p.attnames, ','), p.rowfilter IS NOT NULL,
                EXISTS (SELECT 1 FROM pg_inherits i WHERE i.inhparent = c.oid)
           FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
           LEFT JOIN pg_publication_tables p
                  ON p.pubname = {publication}
                 AND p.schemaname = n.nspname AND p.tablename = c.relname
          WHERE (n.nspname, c.relname) IN (VALUES {tables})"
    ))?;
    let mut classes = Vec::new();
    for row in rows {
        let [
            schema,
            name,
            relkind,
            identity,
            select,
            published,
            row_filter,
            children,
        ] = row.try_into().map_err(|_| shape("relations"))?;
        classes.push(Class {
            schema: schema.unwrap_or_default(),
            name: name.unwrap_or_default(),
            relkind: relkind.unwrap_or_default(),
            children: children.as_deref() == Some("t"),
            identity: identity.unwrap_or_default(),
            select: select.as_deref() == Some("t"),
            published: published.as_deref().map(comma_list),
            row_filter: row_filter.as_deref() == Some("t"),
            columns: Vec::new(),
        });
    }
    Ok(classes)
}

/// PostgreSQL's text for a true boolean.
fn is_true(text: &str) -> bool {
    text == "t"
}

fn comma_list(text: &str) -> Vec<String> {
    text.split(',').map(str::to_owned).collect()
}

/// Adds each class's columns, dropped ones left out.
fn read_columns(
    connection: &mut Connection,
    tables: &str,
    classes: &mut [Class],
) -> Result<(), Error> {
    let rows = connection.rows(&format!(
        "SELECT n.nspname, c.relname, a.attname, a.atttypid, a.atttypmod,
                format_type(a.atttypid, a.atttypmod), t.typtype
           FROM pg_attribute a
           JOIN pg_class c ON c.oid = a.attrelid
           JOIN pg_namespace n ON n.oid = c.relnamespace
           JOIN pg_type t ON t.oid = a.atttypid
          WHERE a.attnum > 0 AND NOT a.attisdropped
            AND (n.nspname, c.relname) IN (VALUES {tables})
          ORDER BY a.attnum"
    ))?;
    for row in rows {
        // None of these is ever null.
        let [schema, table, name, type_oid, typmod, type_name, typtype] =
            <[Option<String>; 7]>::try_from(row)
                .map_err(|_| shape("columns"))?
                .map(Option::unwrap_or_default);
        let Some(class) = class_mut(classes, &schema, &table) else {
            return Err(shape("columns"));
        };
        class.columns.push(Column {
            name,
            type_oid: type_oid.parse().map_err(|_| shape("columns"))?,
            typmod: typmod.parse().map_err(|_| shape("columns"))?,
            type_name,
            typtype,
        });
    }
    Ok(())
}

/// The class of `classes` that is `schema.table`, to add columns to.
// 'a: the class found is borrowed from `classes`, not from the names.
fn class_mut<'a>(classes: &'a mut [Class], schema: &str, table: &str) -> Option<&'a mut Class> {
    for class in classes {
        if class.schema == schema && class.name == table {
            return Some(class);
        }
    }
    None
}

/// Every label of every enum type a column of `tables`, an SQL list of
/// `(schema, table)` pairs, has.
pub(crate) fn labels(connection: &mut Connection, tables: &str) -> Result<Vec<EnumLabel>, Error> {
    let rows = connection.rows(&format!(
        "SELECT e.oid, e.enumtypid, e.enumlabel
           FROM pg_enum e
          WHERE e.enumtypid IN (SELECT a.atttypid
                                  FROM pg_attribute a
                                  JOIN pg_class c ON c.oid = a.attrelid
                                  JOIN pg_namespace n ON n.oid = c.relnamespace
                                 WHERE (n.nspname, c.relname) IN (VALUES {tables}))
          ORDER BY e.enumtypid, e.enumsortorder"
    ))?;
    let mut labels = Vec::new();
    for row in rows {
        let [oid, type_oid, label] = <[Option<String>; 3]>::try_from(row)
            .map_err(|_| shape("labels"))?
            .map(Option::unwrap_or_default);
        labels.push(EnumLabel {
            oid: oid.parse().map_err(|_| shape("labels"))?,
            type_oid: type_oid.parse().map_err(|_| shape("labels"))?,
            label,
        });
    }
    Ok(labels)
}

fn shape(query: &'static str) -> Error {
    Error::Protocol(Unexpected::CatalogShape(query))
}

/// What a column is for, which decides the types it may have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// The fact's time column, of the type its `date_bin` origin has.
    Time(Origin),
    /// A value Straum sums, or keeps the least and greatest of.
    Number,
    /// A value Straum groups, joins or counts distinct.
    Key,
    /// A value Straum takes the `bool_and` or `bool_or` of.
    Boolean,
    /// A column whose null-ness is a key or is counted: any type Straum
    /// reads.
    Nullness,
}

impl Purpose {
    fn allows(self, column_type: ColumnType) -> bool {
        match self {
            Purpose::Time(origin) => column_type == origin.column_type(),
            Purpose::Number => matches!(column_type, ColumnType::Int | ColumnType::Numeric { .. }),
            Purpose::Key => !matches!(column_type, ColumnType::Time { .. }),
            Purpose::Boolean => column_type == ColumnType::Bool,
            Purpose::Nullness => true,
        }
    }

    /// What to change, after what the column is.
    pub(crate) fn needs(self) -> String {
        match self {
            Purpose::Time(origin) => {
                let other = match origin {
                    Origin::Timestamptz => Origin::Timestamp,
                    Origin::Timestamp => Origin::Timestamptz,
                };
                format!(
                    "the date_bin origin {origin} needs {} there; for {} column, write {other}",
                    origin.column_type(),
                    other.column_type()
                )
            }
            Purpose::Number => {
                "Straum needs an integer type or numeric(p, s) with p at most 18 there".into()
            }
            Purpose::Key => {
                "Straum needs an integer type, numeric(p, s), boolean or text there".into()
            }
            Purpose::Boolean => "Straum needs boolean there".into(),
            Purpose::Nullness => {
                "straum needs a type it reads there: an integer type, numeric(p, s), \
                               boolean, text, timestamptz or timestamp"
                    .into()
            }
        }
    }
}

/// Checks the catalog against the spec and returns every problem found,
/// each with what to change. On success, the tables ready to read.
pub(crate) fn check(
    spec: &Spec,
    catalog: &Catalog,
    publication: &str,
    replication: Replication,
) -> Result<Tables, Vec<Problem>> {
    let mut problems = Vec::new();
    check_server(catalog, publication, replication, &mut problems);
    let mut tables = Tables {
        facts: Vec::new(),
        dims: Vec::new(),
    };
    // Both kinds are loaded whole and followed through the publication.
    for dim in spec.dims() {
        let member = Member {
            name: dim.name.clone(),
            kind: "dim",
            table: dim.table.clone(),
            uses: dim_uses(dim),
            as_text: dim.as_text.clone(),
        };
        let checked = check_table(member, catalog, publication, replication, &mut problems);
        tables.dims.extend(checked);
    }
    for fact in spec.facts() {
        let member = Member {
            name: fact.name.clone(),
            kind: "fact",
            table: fact.table.clone(),
            uses: fact_uses(fact),
            as_text: fact.as_text.clone(),
        };
        let checked = check_table(member, catalog, publication, replication, &mut problems);
        tables.facts.extend(checked);
    }
    if problems.is_empty() {
        check_refs(spec, &tables, catalog, &mut problems);
    }
    if problems.is_empty() {
        Ok(tables)
    } else {
        Err(problems)
    }
}

/// The server's version and settings, and the publication.
fn check_server(
    catalog: &Catalog,
    publication: &str,
    replication: Replication,
    problems: &mut Vec<Problem>,
) {
    // ADR-0007.
    if catalog.server_version < OLDEST_VERSION_NUM {
        problems.push(Problem::Server(ServerProblem::TooOld {
            major: catalog.server_version / VERSION_NUM_PER_MAJOR,
        }));
    }
    // From 19, a logical slot turns logical decoding on under replica.
    // The physical WAL holds every change from replica on (ADR-0008).
    let enough: &'static [&'static str] = match replication {
        Replication::Logical if catalog.server_version < LOGICAL_UNDER_REPLICA_VERSION_NUM => {
            &["logical"]
        }
        _ => &["replica", "logical"],
    };
    if !enough.contains(&catalog.wal_level.as_str()) {
        problems.push(Problem::Server(ServerProblem::WalLevel {
            wal_level: catalog.wal_level.clone(),
            enough,
        }));
    }
    let publication = publication.to_owned();
    match replication {
        Replication::Logical => match catalog.publication {
            None => problems.push(Problem::Server(ServerProblem::NoPublication {
                publication,
            })),
            Some(false) => problems.push(Problem::Server(ServerProblem::PublicationActions {
                publication,
            })),
            Some(true) => {}
        },
        Replication::Physical { .. } => {
            // The first change to a page after a checkpoint must bring
            // the page whole: deletes and updates name only a slot on it.
            if catalog.full_page_writes != "on" {
                problems.push(Problem::Server(ServerProblem::FullPageWrites {
                    setting: catalog.full_page_writes.clone(),
                }));
            }
            if catalog.wal_compression == "zstd" {
                problems.push(Problem::Server(ServerProblem::WalCompressionZstd));
            }
        }
    }
}

/// A column the spec reads, and what for.
struct ColumnUse {
    column: Identifier,
    purpose: Purpose,
}

/// Every column the dim reads, the key columns first, each a key.
fn dim_uses(dim: &Dim) -> Vec<ColumnUse> {
    let mut uses = Vec::new();
    for column in dim.columns() {
        uses.push(ColumnUse {
            column: column.clone(),
            purpose: Purpose::Key,
        });
    }
    uses
}

/// Every column the fact reads, with what it is for.
fn fact_uses(fact: &Fact) -> Vec<ColumnUse> {
    let mut uses = vec![ColumnUse {
        column: fact.time.column.clone(),
        purpose: Purpose::Time(fact.time.origin),
    }];
    for by in &fact.by_keys {
        uses.push(match by {
            ByKey::Column { column, .. } => ColumnUse {
                column: column.clone(),
                purpose: Purpose::Key,
            },
            ByKey::IsNull { column, .. } => ColumnUse {
                column: column.clone(),
                purpose: Purpose::Nullness,
            },
        });
    }
    // A ref's fact-side via columns key the cells, selected or not.
    for column in fact.fact_vias() {
        uses.push(ColumnUse {
            column: column.clone(),
            purpose: Purpose::Key,
        });
    }
    for measure in &fact.measures {
        let Some(reads) = measure.aggregate.reads() else {
            continue;
        };
        let Some(column) = measure.aggregate.input() else {
            continue;
        };
        let purpose = match reads {
            Reads::Numbers => Purpose::Number,
            Reads::Values => Purpose::Key,
            Reads::Nullness => Purpose::Nullness,
            Reads::Booleans => Purpose::Boolean,
        };
        uses.push(ColumnUse {
            column: column.clone(),
            purpose,
        });
    }
    uses
}

/// A spec table as the spec reads it: what it is called, which columns it
/// reads for what, and which of them are written `::text`.
struct Member {
    name: Identifier,
    /// "fact" or "dim", for problems.
    kind: &'static str,
    table: straum_domain::Table,
    /// Every column read, with what it is for; a column may come twice.
    uses: Vec<ColumnUse>,
    /// The columns written `::text`: enums, read as their labels
    /// (ADR-0004).
    as_text: Vec<Identifier>,
}

/// The checks every table gets.
fn check_table(
    Member {
        name,
        kind,
        table,
        uses,
        as_text,
    }: Member,
    catalog: &Catalog,
    publication: &str,
    replication: Replication,
    problems: &mut Vec<Problem>,
) -> Option<Checked> {
    let logical = replication == Replication::Logical;
    let names = TableNames {
        name: name.clone(),
        qualified: format!("{}.{}", table.schema, table.name),
    };
    let Some(class) = catalog.class(table.schema.as_str(), table.name.as_str()) else {
        problems.push(names.table_problem(TableProblem::Missing));
        return None;
    };
    let before = problems.len();
    if class.relkind != "r" {
        problems.push(names.table_problem(TableProblem::NotPlain));
        return None;
    }
    // A query of the table reads its children's rows too, the load
    // included, but their changes come under their own names.
    if class.children {
        problems.push(names.table_problem(TableProblem::Children));
    }
    if !class.select {
        problems.push(names.table_problem(TableProblem::NoSelect));
    }
    if logical && class.row_filter {
        problems.push(names.table_problem(TableProblem::RowFilter {
            publication: publication.to_owned(),
        }));
    }
    // A missing publication is reported once, not per table.
    let published = match (&class.published, catalog.publication) {
        // The physical WAL holds every table, every column.
        _ if !logical => None,
        (Some(published), _) => Some(published.as_slice()),
        (None, None) => None,
        (None, Some(_)) => {
            problems.push(names.table_problem(TableProblem::NotPublished {
                publication: publication.to_owned(),
            }));
            return None;
        }
    };
    let reader = ColumnReader {
        table: &names,
        publication,
        class,
        published,
        as_text: &as_text,
    };
    let read = reader.read(uses, problems);
    if problems.len() > before {
        return None;
    }
    // The physical stream keeps old rows from the pages.
    if logical && class.identity != "f" {
        problems.push(names.table_problem(TableProblem::ReplicaIdentity { kind }));
    }
    Some(Checked {
        name,
        schema: table.schema.clone(),
        table_name: table.name.clone(),
        columns: read.columns,
        types: read.types,
        enums: read.enums,
    })
}

/// What a table's columns are checked against, and named by in problems.
// Borrows the catalog's class and the member's lists for one table's check,
// which nothing outlives.
struct ColumnReader<'a> {
    table: &'a TableNames,
    publication: &'a str,
    class: &'a Class,
    /// The columns the publication sends; `None` when it sends all.
    published: Option<&'a [String]>,
    as_text: &'a [Identifier],
}

/// The columns Straum reads of a table, each once, with their types, and
/// the enums among them.
struct ReadColumns {
    columns: Vec<Identifier>,
    types: Vec<ColumnType>,
    enums: Vec<Oid>,
}

impl ColumnReader<'_> {
    /// Each column of `uses` once, the problems of all of them in
    /// `problems`.
    fn read(&self, uses: Vec<ColumnUse>, problems: &mut Vec<Problem>) -> ReadColumns {
        let mut read = ReadColumns {
            columns: Vec::new(),
            types: Vec::new(),
            enums: Vec::new(),
        };
        for ColumnUse { column, purpose } in uses {
            match self.column_type(&column, purpose, &mut read.enums) {
                Err(problem) => problems.push(problem),
                Ok(_) if read.columns.contains(&column) => {}
                Ok(column_type) => {
                    read.columns.push(column);
                    read.types.push(column_type);
                }
            }
        }
        read
    }

    /// The type Straum reads `column_name` as for `purpose`, or what is
    /// wrong with it. An enum it reads goes in `enums`.
    fn column_type(
        &self,
        column_name: &Identifier,
        purpose: Purpose,
        enums: &mut Vec<Oid>,
    ) -> Result<ColumnType, Problem> {
        let Some(column) = self.class.column(column_name) else {
            return Err(self.problem(column_name, ColumnProblem::Missing));
        };
        if let Some(published) = self.published
            && !published.contains(&column.name)
        {
            let publication = self.publication.to_owned();
            let problem = ColumnProblem::NotPublished { publication };
            return Err(self.problem(column_name, problem));
        }
        let column_type = match (self.as_text.contains(column_name), column.typtype == "e") {
            (true, true) => {
                if !enums.contains(&column.type_oid) {
                    enums.push(column.type_oid);
                }
                Some(ColumnType::Text)
            }
            (true, false) => return Err(self.is(column_name, column, ColumnFix::DropText)),
            (false, true) => return Err(self.is(column_name, column, ColumnFix::WriteText)),
            (false, false) => type_of(column.type_oid, column.typmod),
        };
        match column_type {
            Some(column_type) if purpose.allows(column_type) => Ok(column_type),
            _ => Err(self.is(column_name, column, ColumnFix::Needs(purpose))),
        }
    }

    /// A problem with a column's type, and how to fix it.
    fn is(&self, column_name: &Identifier, column: &Column, fix: ColumnFix) -> Problem {
        let type_name = column.type_name.clone();
        self.problem(column_name, ColumnProblem::Type { type_name, fix })
    }

    fn problem(&self, column: &Identifier, problem: ColumnProblem) -> Problem {
        Problem::Column {
            table: self.table.clone(),
            column: column.clone(),
            problem,
        }
    }
}

/// Refs join fact columns to dim keys of the same type, and filters compare
/// columns with literals of their type, an enum's with one of its labels.
fn check_refs(spec: &Spec, tables: &Tables, catalog: &Catalog, problems: &mut Vec<Problem>) {
    for fact in spec.facts() {
        check_joins(fact, tables, problems);
        check_filters(fact, tables, catalog, problems);
    }
}

/// Each join's key columns read alike on both sides.
fn check_joins(fact: &Fact, tables: &Tables, problems: &mut Vec<Problem>) {
    let facts = tables.fact(&fact.name);
    for reference in &fact.refs {
        let dim = tables.dim(&reference.dim);
        for (via, key) in reference.via.iter().zip(&dim.columns) {
            let (via_name, via_type) = match via {
                Via::Fact(column) => (column.to_string(), facts.column_type(column)),
                Via::Attribute { through, column } => {
                    let parent = &fact.refs[*through];
                    let name = format!("{}.{column}", parent.alias);
                    (name, tables.dim(&parent.dim).column_type(column))
                }
            };
            let key_type = dim.column_type(key);
            if via_type != key_type {
                problems.push(Problem::View {
                    view: fact.name.clone(),
                    problem: ViewProblem::JoinTypes {
                        join: reference.alias.clone(),
                        via: via_name,
                        via_type,
                        key: key.clone(),
                        key_type,
                    },
                });
            }
        }
    }
}

/// Each filter's literals fit their column, and an enum's are its labels.
fn check_filters(fact: &Fact, tables: &Tables, catalog: &Catalog, problems: &mut Vec<Problem>) {
    let facts = tables.fact(&fact.name);
    let mut filters = Vec::new();
    for measure in &fact.measures {
        filters.push((Filter::Measure(measure.name.clone()), &measure.filter));
    }
    filters.push((Filter::Where, &fact.filter));
    for (what, filter) in filters {
        for condition in filter {
            let (table, column, column_type) = match fact
                .resolve(&condition.key)
                .expect("checked by Spec::new")
            {
                Key::ByKey(ByKey::Column { column, .. }) => {
                    (facts, column, facts.column_type(column))
                }
                Key::ByKey(ByKey::IsNull { .. }) => (facts, &fact.time.column, ColumnType::Bool),
                Key::Attribute(reference, attribute) => {
                    let dim = tables.dim(&reference.dim);
                    (dim, &attribute.column, dim.column_type(&attribute.column))
                }
            };
            let labels = catalog.labels(table, column);
            for value in &condition.values {
                // The rule asks follow too.
                let problem = if !column_type.fits(value) {
                    ViewProblem::FilterType {
                        filter: what.clone(),
                        key: condition.key.clone(),
                        column_type,
                        literal: literal(value),
                    }
                } else if let (Some((type_name, labels)), Literal::Text(text)) = (&labels, value)
                    && !labels.contains(&text.as_str())
                {
                    ViewProblem::NotALabel {
                        filter: what.clone(),
                        key: condition.key.clone(),
                        literal: literal(value),
                        type_name: (*type_name).to_owned(),
                    }
                } else {
                    continue;
                };
                problems.push(Problem::View {
                    view: fact.name.clone(),
                    problem,
                });
            }
        }
    }
}

impl Catalog {
    /// An enum column's type name and labels; `None` for any other column.
    fn labels(&self, table: &Checked, column: &Identifier) -> Option<(&str, Vec<&str>)> {
        let class = self.class(table.schema.as_str(), table.table_name.as_str())?;
        let column = class.column(column)?;
        if column.typtype != "e" {
            return None;
        }
        let mut labels = Vec::new();
        for label in &self.labels {
            if label.type_oid == column.type_oid {
                labels.push(label.label.as_str());
            }
        }
        Some((&column.type_name, labels))
    }

    fn class(&self, schema: &str, name: &str) -> Option<&Class> {
        for class in &self.classes {
            if class.schema == schema && class.name == name {
                return Some(class);
            }
        }
        None
    }
}

impl Class {
    /// The column named `name`, if the table has one.
    fn column(&self, name: &Identifier) -> Option<&Column> {
        for column in &self.columns {
            if column.name == name.as_str() {
                return Some(column);
            }
        }
        None
    }
}

fn literal(value: &Literal) -> String {
    match value {
        Literal::Int(number) => number.to_string(),
        Literal::Text(text) => escape_literal(text),
        Literal::Bool(boolean) => boolean.to_string(),
        Literal::Decimal { units, scale } => decimal(*units, *scale),
    }
}
