//! The setup check: does PostgreSQL hold the spec's tables in a shape straum
//! can read and follow? Read from the catalog on the connection that loads,
//! so it sees the tables as the load does.

use postgres_protocol::escape::escape_literal;
use straum_domain::{By, ColumnType, Dim, Fact, Ident, Key, Literal, Origin, Spec, Via};

use crate::error::Error;
use crate::value::type_of;
use crate::wire::Conn;

/// A spec table as PostgreSQL has it, ready to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Table {
    /// The spec's name for it.
    pub(crate) name: Ident,
    pub(crate) schema: Ident,
    pub(crate) table: Ident,
    /// What straum reads of it, in order: `Fact::columns`, or the dim's
    /// key columns and then its attributes.
    pub(crate) columns: Vec<Ident>,
    pub(crate) types: Vec<ColumnType>,
}

impl Table {
    pub(crate) fn column_type(&self, column: &Ident) -> ColumnType {
        let i = self.columns.iter().position(|c| c == column);
        self.types[i.expect("the column is in the table's row")]
    }

    pub(crate) fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.table)
    }
}

/// The spec's tables as checked, facts and dims, each loaded whole and
/// followed through the publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tables {
    pub(crate) facts: Vec<Table>,
    pub(crate) dims: Vec<Table>,
}

impl Tables {
    pub(crate) fn fact(&self, name: &Ident) -> &Table {
        (self.facts.iter().find(|t| t.name == *name)).expect("the spec's facts are checked")
    }

    pub(crate) fn dim(&self, name: &Ident) -> &Table {
        (self.dims.iter().find(|t| t.name == *name)).expect("the spec's dims are checked")
    }
}

/// What straum reads from the catalog; `check` holds every rule.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Catalog {
    pub(crate) server_version: u32,
    pub(crate) wal_level: String,
    /// Whether the publication publishes inserts, updates, deletes and
    /// truncates; `None` when it doesn't exist.
    pub(crate) publication: Option<bool>,
    pub(crate) relations: Vec<Relation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Relation {
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
}

/// Reads the catalog and checks it against the spec.
pub(crate) fn read(conn: &mut Conn, spec: &Spec, publication: &str) -> Result<Tables, Error> {
    let catalog = query(conn, spec, publication)?;
    check(spec, &catalog, publication).map_err(Error::Setup)
}

pub(crate) fn query(conn: &mut Conn, spec: &Spec, publication: &str) -> Result<Catalog, Error> {
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

    let relations = conn.rows(&format!(
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
    for row in relations {
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
        catalog.relations.push(Relation {
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
                format_type(a.atttypid, a.atttypmod)
           FROM pg_attribute a
           JOIN pg_class c ON c.oid = a.attrelid
           JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE a.attnum > 0 AND NOT a.attisdropped
            AND (n.nspname, c.relname) IN (VALUES {tables})
          ORDER BY a.attnum"
    ))?;
    for row in columns {
        // None of these is ever null.
        let [schema, table, name, type_oid, typmod, type_name] =
            <[Option<String>; 6]>::try_from(row)
                .map_err(|_| shape("columns"))?
                .map(Option::unwrap_or_default);
        let relation = catalog
            .relations
            .iter_mut()
            .find(|r| r.schema == schema && r.name == table)
            .ok_or_else(|| shape("columns"))?;
        relation.columns.push(Column {
            name,
            type_oid: type_oid.parse().map_err(|_| shape("columns"))?,
            typmod: typmod.parse().map_err(|_| shape("columns"))?,
            type_name,
        });
    }
    Ok(catalog)
}

fn shape(query: &str) -> Error {
    Error::Protocol(format!("a catalog {query} result of an unexpected shape"))
}

/// What a column is for, which decides the types it may have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The fact's time column, of the type its `date_bin` origin has.
    Time(Origin),
    /// A value straum sums, or keeps the least and greatest of.
    Number,
    /// A value straum groups, joins or counts distinct.
    Key,
    /// A column whose null-ness is a key: any type straum reads.
    Nullness,
}

impl Role {
    fn allows(self, column_type: ColumnType) -> bool {
        match self {
            Role::Time(origin) => column_type == origin.column_type(),
            Role::Number => matches!(column_type, ColumnType::Int | ColumnType::Numeric { .. }),
            Role::Key => !matches!(column_type, ColumnType::Time { .. }),
            Role::Nullness => true,
        }
    }

    /// What to change, after what the column is.
    fn needs(self) -> String {
        match self {
            Role::Time(origin) => {
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
            Role::Number => {
                "straum needs an integer type or numeric(p, s) with p at most 18 there".into()
            }
            Role::Key => {
                "straum needs an integer type, numeric(p, s), boolean or text there".into()
            }
            Role::Nullness => {
                "straum needs a type it reads there: an integer type, numeric(p, s), \
                               boolean, text, timestamptz or timestamp"
                    .into()
            }
        }
    }
}

/// A table of the spec. Both kinds are loaded whole and followed through
/// the publication; a dim's key must name one row.
#[derive(Debug, Clone, Copy)]
enum Member<'s> {
    Dim(&'s Dim),
    Fact(&'s Fact),
}

impl<'s> Member<'s> {
    fn name(self) -> &'s Ident {
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

    fn table(self) -> &'s straum_domain::Table {
        match self {
            Member::Dim(dim) => &dim.table,
            Member::Fact(fact) => &fact.table,
        }
    }

    /// Every column the member reads, with what it is for; a column may
    /// come twice. A dim's key columns come first.
    fn roles(self) -> Vec<(&'s Ident, Role)> {
        match self {
            Member::Dim(dim) => dim.columns().map(|c| (c, Role::Key)).collect(),
            Member::Fact(fact) => fact_roles(fact),
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
            "wal_level is {}; straum needs {}, which takes a restart",
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
    for dim in spec.dims() {
        let checked = check_table(Member::Dim(dim), catalog, publication, &mut problems);
        tables.dims.extend(checked);
    }
    for fact in spec.facts() {
        let checked = check_table(Member::Fact(fact), catalog, publication, &mut problems);
        tables.facts.extend(checked);
    }
    if problems.is_empty() {
        check_refs(spec, &tables, &mut problems);
    }
    if problems.is_empty() {
        Ok(tables)
    } else {
        Err(problems)
    }
}

/// Every column the fact reads, with what it is for.
fn fact_roles(fact: &Fact) -> Vec<(&Ident, Role)> {
    let mut roles = vec![(&fact.time.column, Role::Time(fact.time.origin))];
    roles.extend(fact.by.iter().map(|by| match by {
        By::Column { column, .. } => (column, Role::Key),
        By::IsNull { column, .. } => (column, Role::Nullness),
    }));
    // A ref's fact-side via columns key the cells, selected or not.
    roles.extend(fact.fact_vias().map(|v| (v, Role::Key)));
    for m in &fact.measures {
        let Some(column) = m.agg.input() else {
            continue;
        };
        let role = if m.agg.reads_numbers() {
            Role::Number
        } else {
            Role::Key
        };
        roles.push((column, role));
    }
    roles
}

/// The checks every table gets, then those its kind of member owes.
fn check_table(
    member: Member<'_>,
    catalog: &Catalog,
    publication: &str,
    problems: &mut Vec<String>,
) -> Option<Table> {
    let (name, table) = (member.name(), member.table());
    let qualified = format!("{}.{}", table.schema, table.name);
    let Some(relation) = catalog
        .relations
        .iter()
        .find(|r| r.schema == table.schema.as_str() && r.name == table.name.as_str())
    else {
        problems.push(format!("{name}: table {qualified} does not exist"));
        return None;
    };
    let before = problems.len();
    if relation.relkind != "r" {
        problems.push(format!("{name}: {qualified} is not a plain table"));
        return None;
    }
    if !relation.select {
        problems.push(format!(
            "{name}: straum's role can't read {qualified}: GRANT SELECT ON {qualified}"
        ));
    }
    if relation.row_filter {
        problems.push(format!(
            "{name}: publication {publication} filters the rows of {qualified}; \
             straum needs every row"
        ));
    }
    // A missing publication is reported once, not per table.
    let published = match (&relation.published, catalog.publication) {
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

    let column = |column: &Ident| {
        let found = relation.columns.iter().find(|c| c.name == column.as_str());
        found.ok_or_else(|| format!("{name}: {qualified} has no column {column}"))
    };
    let mut columns: Vec<Ident> = Vec::new();
    let mut types = Vec::new();
    for (name_of, role) in member.roles() {
        let found = column(name_of).and_then(|c| {
            if published.is_some_and(|p| !p.contains(&c.name)) {
                return Err(format!(
                    "{name}: publication {publication} leaves out column {name_of} of {qualified}"
                ));
            }
            type_of(c.type_oid, c.typmod)
                .filter(|k| role.allows(*k))
                .ok_or_else(|| {
                    format!(
                        "{name}: column {name_of} of {qualified} is {}; {}",
                        c.type_name,
                        role.needs()
                    )
                })
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
    let table = Table {
        name: name.clone(),
        schema: table.schema.clone(),
        table: table.name.clone(),
        columns,
        types,
    };
    if relation.identity != "f" {
        problems.push(format!(
            "{} {name}: straum needs the whole old row of every update and delete: \
             ALTER TABLE {} REPLICA IDENTITY FULL",
            member.kind(),
            table.qualified()
        ));
    }
    Some(table)
}

/// Refs join fact columns to dim keys of the same type, and filters compare
/// columns with literals of their type.
fn check_refs(spec: &Spec, tables: &Tables, problems: &mut Vec<String>) {
    for fact in spec.facts() {
        let facts = tables.fact(&fact.name);
        for r in &fact.refs {
            let dim = tables.dim(&r.dim);
            for (via, key) in r.via.iter().zip(&dim.columns) {
                let (via_name, via_type) = match via {
                    Via::Fact(column) => (column.to_string(), facts.column_type(column)),
                    Via::Attr { through, column } => {
                        let parent = &fact.refs[*through];
                        let name = format!("{}.{column}", parent.name);
                        (name, tables.dim(&parent.dim).column_type(column))
                    }
                };
                let key_type = dim.column_type(key);
                if via_type != key_type {
                    problems.push(format!(
                        "fact {}: ref {} joins {via_name} ({via_type}) to {key} ({key_type}); \
                         they must read alike",
                        fact.name, r.name
                    ));
                }
            }
        }
        for m in &fact.measures {
            for cond in &m.filter {
                let column_type = match fact.resolve(&cond.key).expect("checked by Spec::new") {
                    Key::By(By::Column { column, .. }) => facts.column_type(column),
                    Key::By(By::IsNull { .. }) => ColumnType::Bool,
                    Key::Attr(r, attr) => tables.dim(&r.dim).column_type(&attr.column),
                };
                for value in &cond.values {
                    // The rule asks follow too.
                    if !column_type.fits(value) {
                        problems.push(format!(
                            "fact {}: measure {} compares {} ({column_type}) with {}",
                            fact.name,
                            m.name,
                            cond.key,
                            literal(value)
                        ));
                    }
                }
            }
        }
    }
}

fn literal(value: &Literal) -> String {
    match value {
        Literal::Int(n) => n.to_string(),
        Literal::Text(s) => escape_literal(s),
        Literal::Bool(b) => b.to_string(),
    }
}
