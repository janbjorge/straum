//! The setup check: does PostgreSQL hold the spec's tables in a shape Straum
//! can read and follow? Read from the catalog on the connection that loads,
//! so it sees the tables as the load does.

use postgres_protocol::escape::{escape_identifier, escape_literal};
use straum_domain::{
    ByKey, ColumnType, Dim, Fact, Identifier, Key, Literal, Origin, Reads, Spec, Via,
};

use crate::error::Error;
use crate::value::type_of;
use crate::wire::Connection;

/// A spec table as PostgreSQL has it, ready to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Checked {
    /// The spec's name for it.
    pub(crate) name: Identifier,
    pub(crate) schema: Identifier,
    pub(crate) table: Identifier,
    /// What Straum reads of it, in order: `Fact::columns`, or the dim's
    /// key columns and then its attributes.
    pub(crate) columns: Vec<Identifier>,
    pub(crate) types: Vec<ColumnType>,
    /// The enum types its `::text` columns have, by OID. An enum's OID is
    /// its own, so the stream can't know one from a type Straum doesn't
    /// read without this list.
    pub(crate) enums: Vec<u32>,
}

impl Checked {
    pub(crate) fn column_type(&self, column: &Identifier) -> ColumnType {
        let i = self.columns.iter().position(|c| c == column);
        self.types[i.expect("the column is in the table's row")]
    }

    pub(crate) fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.table)
    }
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
        (self
            .facts
            .iter()
            .chain(&self.dims)
            .find(|t| t.name == *name))
        .expect("the spec's tables are checked")
    }

    pub(crate) fn fact(&self, name: &Identifier) -> &Checked {
        (self.facts.iter().find(|t| t.name == *name)).expect("the spec's facts are checked")
    }

    pub(crate) fn dim(&self, name: &Identifier) -> &Checked {
        (self.dims.iter().find(|t| t.name == *name)).expect("the spec's dims are checked")
    }
}

/// What Straum reads from the catalog; `check` holds every rule.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Catalog {
    pub(crate) server_version: u32,
    pub(crate) wal_level: String,
    /// Whether the publication publishes inserts, updates, deletes and
    /// truncates; `None` when it doesn't exist.
    pub(crate) publication: Option<bool>,
    pub(crate) classes: Vec<Class>,
    /// Every label of every enum type a spec column has, in type order.
    pub(crate) labels: Vec<EnumLabel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnumLabel {
    pub(crate) type_oid: u32,
    pub(crate) label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Class {
    pub(crate) schema: String,
    pub(crate) name: String,
    /// `pg_class.relkind`: `r` for a plain table.
    pub(crate) relkind: String,
    /// `pg_class.relreplident`: `d`efault, `f`ull, `i`ndex or `n`othing.
    pub(crate) identity: String,
    pub(crate) select: bool,
    /// The columns the publication sends; `None` when it leaves the table
    /// out.
    pub(crate) published: Option<Vec<String>>,
    pub(crate) row_filter: bool,
    pub(crate) columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Column {
    pub(crate) name: String,
    pub(crate) type_oid: u32,
    pub(crate) typmod: i32,
    /// As `format_type` prints it, for messages.
    pub(crate) type_name: String,
    /// `pg_type.typtype`: `e` for an enum.
    pub(crate) typtype: String,
}

/// Reads the catalog and checks it against the spec.
pub(crate) fn read(conn: &mut Connection, spec: &Spec, publication: &str) -> Result<Tables, Error> {
    let catalog = query(conn, spec, publication)?;
    check(spec, &catalog, publication).map_err(Error::Setup)
}

pub(crate) fn query(
    conn: &mut Connection,
    spec: &Spec,
    publication: &str,
) -> Result<Catalog, Error> {
    let publication = escape_literal(publication);
    let tables = (spec.tables().iter())
        .map(|t| {
            format!(
                "({}, {})",
                escape_literal(t.schema.as_str()),
                escape_literal(t.name.as_str())
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut catalog = Catalog::default();

    let settings = conn.rows(&format!(
        "SELECT current_setting('server_version_num'), current_setting('wal_level'),
                (SELECT pubinsert AND pubupdate AND pubdelete AND pubtruncate
                   FROM pg_publication WHERE pubname = {publication})"
    ))?;
    let [version, wal_level, published] = settings
        .into_iter()
        .next()
        .and_then(|r| r.try_into().ok())
        .ok_or_else(|| shape("settings"))?;
    catalog.server_version = version
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| shape("settings"))?;
    catalog.wal_level = wal_level.unwrap_or_default();
    catalog.publication = published.map(|p| p == "t");

    let classes = conn.rows(&format!(
        "SELECT n.nspname, c.relname, c.relkind, c.relreplident,
                has_table_privilege(c.oid, 'SELECT'),
                (SELECT array_to_string(p.attnames, ',') FROM pg_publication_tables p
                  WHERE p.pubname = {publication}
                    AND p.schemaname = n.nspname AND p.tablename = c.relname),
                (SELECT p.rowfilter IS NOT NULL FROM pg_publication_tables p
                  WHERE p.pubname = {publication}
                    AND p.schemaname = n.nspname AND p.tablename = c.relname)
           FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE (n.nspname, c.relname) IN (VALUES {tables})"
    ))?;
    for row in classes {
        let [
            schema,
            name,
            relkind,
            identity,
            select,
            published,
            row_filter,
        ] = row.try_into().map_err(|_| shape("relations"))?;
        let list = |s: &str| -> Vec<String> { s.split(',').map(str::to_owned).collect() };
        catalog.classes.push(Class {
            schema: schema.unwrap_or_default(),
            name: name.unwrap_or_default(),
            relkind: relkind.unwrap_or_default(),
            identity: identity.unwrap_or_default(),
            select: select.as_deref() == Some("t"),
            published: published.as_deref().map(list),
            row_filter: row_filter.as_deref() == Some("t"),
            columns: Vec::new(),
        });
    }

    let columns = conn.rows(&format!(
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
    for row in columns {
        // None of these is ever null.
        let [schema, table, name, type_oid, typmod, type_name, typtype] =
            <[Option<String>; 7]>::try_from(row)
                .map_err(|_| shape("columns"))?
                .map(Option::unwrap_or_default);
        let class = catalog
            .classes
            .iter_mut()
            .find(|r| r.schema == schema && r.name == table)
            .ok_or_else(|| shape("columns"))?;
        class.columns.push(Column {
            name,
            type_oid: type_oid.parse().map_err(|_| shape("columns"))?,
            typmod: typmod.parse().map_err(|_| shape("columns"))?,
            type_name,
            typtype,
        });
    }

    let labels = conn.rows(&format!(
        "SELECT e.enumtypid, e.enumlabel
           FROM pg_enum e
          WHERE e.enumtypid IN (SELECT a.atttypid
                                  FROM pg_attribute a
                                  JOIN pg_class c ON c.oid = a.attrelid
                                  JOIN pg_namespace n ON n.oid = c.relnamespace
                                 WHERE (n.nspname, c.relname) IN (VALUES {tables}))
          ORDER BY e.enumtypid, e.enumsortorder"
    ))?;
    for row in labels {
        let [type_oid, label] = <[Option<String>; 2]>::try_from(row)
            .map_err(|_| shape("labels"))?
            .map(Option::unwrap_or_default);
        catalog.labels.push(EnumLabel {
            type_oid: type_oid.parse().map_err(|_| shape("labels"))?,
            label,
        });
    }
    Ok(catalog)
}

fn shape(query: &str) -> Error {
    Error::Protocol(format!("a catalog {query} result of an unexpected shape"))
}

/// What a column is for, which decides the types it may have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// The fact's time column, of the type its `date_bin` origin has.
    Time(Origin),
    /// A value Straum sums, or keeps the least and greatest of.
    Number,
    /// A value Straum groups, joins or counts distinct.
    Key,
    /// A column whose null-ness is a key: any type Straum reads.
    Nullness,
}

impl Purpose {
    fn allows(self, column_type: ColumnType) -> bool {
        match self {
            Purpose::Time(origin) => column_type == origin.column_type(),
            Purpose::Number => matches!(column_type, ColumnType::Int | ColumnType::Numeric { .. }),
            Purpose::Key => !matches!(column_type, ColumnType::Time { .. }),
            Purpose::Nullness => true,
        }
    }

    /// What to change, after what the column is.
    fn needs(self) -> String {
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
) -> Result<Tables, Vec<String>> {
    let mut problems = Vec::new();
    // From 19, a logical slot turns logical decoding on under replica.
    let enough: &[&str] = if catalog.server_version >= 190_000 {
        &["replica", "logical"]
    } else {
        &["logical"]
    };
    if !enough.contains(&catalog.wal_level.as_str()) {
        problems.push(format!(
            "wal_level is {}; Straum needs {}, which takes a restart",
            catalog.wal_level,
            enough.join(" or ")
        ));
    }
    match catalog.publication {
        None => problems.push(format!(
            "publication {publication} does not exist; run the SQL straum plan prints"
        )),
        Some(false) => problems.push(format!(
            "publication {publication} must publish insert, update, delete and truncate"
        )),
        Some(true) => {}
    }

    let mut tables = Tables {
        facts: Vec::new(),
        dims: Vec::new(),
    };
    // Both kinds are loaded whole and followed through the publication.
    for dim in spec.dims() {
        let checked = check_table(Member::Dim(dim), catalog, publication, &mut problems);
        tables.dims.extend(checked);
    }
    for fact in spec.facts() {
        let checked = check_table(Member::Fact(fact), catalog, publication, &mut problems);
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

/// Every column the fact reads, with what it is for.
fn fact_purposes(fact: &Fact) -> Vec<(&Identifier, Purpose)> {
    let mut purposes = vec![(&fact.time.column, Purpose::Time(fact.time.origin))];
    purposes.extend(fact.by_keys.iter().map(|by| match by {
        ByKey::Column { column, .. } => (column, Purpose::Key),
        ByKey::IsNull { column, .. } => (column, Purpose::Nullness),
    }));
    // A ref's fact-side via columns key the cells, selected or not.
    purposes.extend(fact.fact_vias().map(|v| (v, Purpose::Key)));
    purposes.extend(fact.measures.iter().filter_map(|m| {
        let purpose = match m.aggregate.reads()? {
            Reads::Numbers => Purpose::Number,
            Reads::Values => Purpose::Key,
        };
        Some((m.aggregate.input()?, purpose))
    }));
    purposes
}

/// A spec table as the spec reads it: what it is called, which columns it
/// reads for what, and which of them are written `::text`.
#[derive(Clone, Copy)]
enum Member<'a> {
    Dim(&'a Dim),
    Fact(&'a Fact),
}

impl<'a> Member<'a> {
    fn name(self) -> &'a Identifier {
        match self {
            Member::Dim(dim) => &dim.name,
            Member::Fact(fact) => &fact.name,
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Member::Dim(_) => "dim",
            Member::Fact(_) => "fact",
        }
    }

    fn table(self) -> &'a straum_domain::Table {
        match self {
            Member::Dim(dim) => &dim.table,
            Member::Fact(fact) => &fact.table,
        }
    }

    /// Every column read, with what it is for; a column may come twice.
    fn purposes(self) -> Vec<(&'a Identifier, Purpose)> {
        match self {
            // The key columns come first.
            Member::Dim(dim) => dim.columns().map(|c| (c, Purpose::Key)).collect(),
            Member::Fact(fact) => fact_purposes(fact),
        }
    }

    /// The columns written `::text`: enums, read as their labels
    /// (ADR-0037).
    fn as_text(self) -> &'a [Identifier] {
        match self {
            Member::Dim(dim) => &dim.as_text,
            Member::Fact(fact) => &fact.as_text,
        }
    }
}

/// The checks every table gets.
fn check_table(
    member: Member<'_>,
    catalog: &Catalog,
    publication: &str,
    problems: &mut Vec<String>,
) -> Option<Checked> {
    let (name, kind, table) = (member.name(), member.kind(), member.table());
    let (purposes, as_text) = (member.purposes(), member.as_text());
    let qualified = format!("{}.{}", table.schema, table.name);
    let Some(class) = catalog
        .classes
        .iter()
        .find(|r| r.schema == table.schema.as_str() && r.name == table.name.as_str())
    else {
        problems.push(format!("{name}: table {qualified} does not exist"));
        return None;
    };
    let before = problems.len();
    if class.relkind != "r" {
        problems.push(format!("{name}: {qualified} is not a plain table"));
        return None;
    }
    if !class.select {
        problems.push(format!(
            "{name}: Straum's role can't read {qualified}: GRANT SELECT ON {qualified}"
        ));
    }
    if class.row_filter {
        problems.push(format!(
            "{name}: publication {publication} filters the rows of {qualified}; \
             Straum needs every row"
        ));
    }
    // A missing publication is reported once, not per table.
    let published = match (&class.published, catalog.publication) {
        (Some(published), _) => Some(published),
        (None, None) => None,
        (None, Some(_)) => {
            problems.push(format!(
                "{name}: publication {publication} leaves out {qualified}: \
                 ALTER PUBLICATION {publication} ADD TABLE {qualified}"
            ));
            return None;
        }
    };

    let column = |column: &Identifier| {
        let found = class.columns.iter().find(|c| c.name == column.as_str());
        found.ok_or_else(|| format!("{name}: {qualified} has no column {column}"))
    };
    let mut columns: Vec<Identifier> = Vec::new();
    let mut types = Vec::new();
    let mut enums = Vec::new();
    for (name_of, purpose) in purposes {
        let found = column(name_of).and_then(|c| {
            if published.is_some_and(|p| !p.contains(&c.name)) {
                return Err(format!(
                    "{name}: publication {publication} leaves out column {name_of} of {qualified}"
                ));
            }
            let is = |what: &str| {
                format!(
                    "{name}: column {name_of} of {qualified} is {}{what}",
                    c.type_name
                )
            };
            let column_type = match (as_text.contains(name_of), c.typtype == "e") {
                (true, true) => {
                    if !enums.contains(&c.type_oid) {
                        enums.push(c.type_oid);
                    }
                    Some(ColumnType::Text)
                }
                (true, false) => return Err(is(", not an enum; drop the ::text")),
                (false, true) => {
                    return Err(is(&format!(", an enum; write {name_of}::text")));
                }
                (false, false) => type_of(c.type_oid, c.typmod),
            };
            (column_type.filter(|k| purpose.allows(*k)))
                .ok_or_else(|| is(&format!("; {}", purpose.needs())))
        });
        match found {
            Err(problem) => problems.push(problem),
            Ok(_) if columns.contains(name_of) => {}
            Ok(column_type) => {
                columns.push(name_of.clone());
                types.push(column_type);
            }
        }
    }
    if problems.len() > before {
        return None;
    }
    if class.identity != "f" {
        problems.push(format!(
            "{kind} {name}: Straum needs the whole old row of every update and delete: \
             ALTER TABLE {qualified} REPLICA IDENTITY FULL"
        ));
    }
    Some(Checked {
        name: name.clone(),
        schema: table.schema.clone(),
        table: table.name.clone(),
        columns,
        types,
        enums,
    })
}

/// Refs join fact columns to dim keys of the same type, and filters compare
/// columns with literals of their type, an enum's with one of its labels.
fn check_refs(spec: &Spec, tables: &Tables, catalog: &Catalog, problems: &mut Vec<String>) {
    for fact in spec.facts() {
        let facts = tables.fact(&fact.name);
        for r in &fact.refs {
            let dim = tables.dim(&r.dim);
            for (via, key) in r.via.iter().zip(&dim.columns) {
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
                    problems.push(format!(
                        "view {}: join {} joins {via_name} ({via_type}) to {key} ({key_type}); \
                         they must read alike",
                        fact.name, r.alias
                    ));
                }
            }
        }
        let filters = (fact.measures.iter())
            .map(|m| (format!("measure {}", m.name), &m.filter))
            .chain(std::iter::once(("WHERE".to_owned(), &fact.filter)));
        for (what, conds) in filters {
            for cond in conds {
                let (table, column) = match fact.resolve(&cond.key).expect("checked by Spec::new") {
                    Key::ByKey(ByKey::Column { column, .. }) => (facts, column),
                    Key::ByKey(ByKey::IsNull { .. }) => (facts, &fact.time.column),
                    Key::Attribute(r, attr) => (tables.dim(&r.dim), &attr.column),
                };
                let column_type = match fact.resolve(&cond.key).expect("checked by Spec::new") {
                    Key::ByKey(ByKey::IsNull { .. }) => ColumnType::Bool,
                    Key::ByKey(ByKey::Column { .. }) | Key::Attribute(..) => {
                        table.column_type(column)
                    }
                };
                let labels = catalog.labels(table, column);
                for value in &cond.values {
                    // The rule asks follow too.
                    if !column_type.fits(value) {
                        problems.push(format!(
                            "view {}: {what} compares {} ({column_type}) with {}",
                            fact.name,
                            cond.key,
                            literal(value)
                        ));
                    } else if let (Some((type_name, labels)), Literal::Text(text)) =
                        (&labels, value)
                        && !labels.contains(&text.as_str())
                    {
                        problems.push(format!(
                            "view {}: {what} compares {} with {}, which is no label of {type_name}",
                            fact.name,
                            cond.key,
                            literal(value)
                        ));
                    }
                }
            }
        }
    }
}

impl Catalog {
    /// An enum column's type name and labels; `None` for any other column.
    fn labels(&self, table: &Checked, column: &Identifier) -> Option<(&str, Vec<&str>)> {
        let class = (self.classes.iter())
            .find(|r| r.schema == table.schema.as_str() && r.name == table.table.as_str())?;
        let column = class.columns.iter().find(|c| c.name == column.as_str())?;
        if column.typtype != "e" {
            return None;
        }
        let labels = (self.labels.iter())
            .filter(|l| l.type_oid == column.type_oid)
            .map(|l| l.label.as_str())
            .collect();
        Some((&column.type_name, labels))
    }
}

fn literal(value: &Literal) -> String {
    match value {
        Literal::Int(n) => n.to_string(),
        Literal::Text(s) => escape_literal(s),
        Literal::Bool(b) => b.to_string(),
    }
}
