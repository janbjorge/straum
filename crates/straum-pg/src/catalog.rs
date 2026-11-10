//! The setup check: does PostgreSQL hold the spec's tables in a shape straum
//! can read and follow? Read from the catalog on the connection that loads,
//! so it sees the tables as the load does.

use postgres_protocol::escape::escape_literal;
use straum_domain::{By, ColumnType, Dim, Fact, Ident, Key, Literal, Origin, Spec};

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
    /// How many of `columns`, from the first, make a dim's key. 0 for a
    /// fact.
    pub(crate) key_width: usize,
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

/// The spec's tables as checked: the facts, which straum loads and
/// follows, and the dims, which it only looks rows up in.
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Relation {
    pub(crate) schema: String,
    pub(crate) name: String,
    /// `pg_class.relkind`: `r` for a plain table.
    pub(crate) relkind: String,
    /// `pg_class.relreplident`: `d`efault, `f`ull, `i`ndex or `n`othing.
    pub(crate) identity: String,
    pub(crate) select: bool,
    /// The columns of each valid unique index without a predicate or an
    /// expression.
    pub(crate) unique_indexes: Vec<Vec<String>>,
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
                (SELECT string_agg(u.columns, ';')
                   FROM (SELECT string_agg(a.attname, ',' ORDER BY k.ord) AS columns
                           FROM pg_index i
                          CROSS JOIN LATERAL unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord)
                           JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = k.attnum
                          WHERE i.indrelid = c.oid AND k.ord <= i.indnkeyatts
                            AND i.indisunique AND i.indisvalid
                            AND i.indpred IS NULL AND i.indexprs IS NULL
                          GROUP BY i.indexrelid) u),
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
            unique_indexes,
            published,
            row_filter,
        ] = row.try_into().map_err(|_| shape("relations"))?;
        let list = |s: Option<String>| -> Vec<String> {
            s.map(|s| s.split(',').map(str::to_owned).collect())
                .unwrap_or_default()
        };
        let lists = |s: Option<String>| -> Vec<Vec<String>> {
            s.map(|s| s.split(';').map(|l| list(Some(l.to_owned()))).collect())
                .unwrap_or_default()
        };
        catalog.relations.push(Relation {
            schema: schema.unwrap_or_default(),
            name: name.unwrap_or_default(),
            relkind: relkind.unwrap_or_default(),
            identity: identity.unwrap_or_default(),
            select: select.as_deref() == Some("t"),
            unique_indexes: lists(unique_indexes),
            published: published.map(|p| list(Some(p))),
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

/// How straum uses a table, which decides what the setup owes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    /// A fact: loaded whole and followed through the publication.
    Streamed,
    /// A dim: rows looked up by key, in the load and per commit. Never
    /// followed, so the publication owes it nothing.
    LookedUp,
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
        let roles = dim.columns().map(|c| (c, Role::Key)).collect();
        let checked = check_table(
            &dim.table,
            &dim.name,
            dim.key.len(),
            Access::LookedUp,
            roles,
            catalog,
            publication,
            &mut problems,
        );
        if let Some((table, relation)) = checked {
            check_dim(dim, &table, relation, &mut problems);
            tables.dims.push(table);
        }
    }
    for fact in spec.facts() {
        let roles = fact_roles(fact);
        let checked = check_table(
            &fact.table,
            &fact.name,
            0,
            Access::Streamed,
            roles,
            catalog,
            publication,
            &mut problems,
        );
        if let Some((table, relation)) = checked {
            if relation.identity != "f" {
                problems.push(format!(
                    "fact {}: straum needs the whole old row of every update and delete: \
                     ALTER TABLE {} REPLICA IDENTITY FULL",
                    fact.name,
                    table.qualified()
                ));
            }
            tables.facts.push(table);
        }
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
        By::Column(column) => (column, Role::Key),
        By::IsNull { column, .. } => (column, Role::Nullness),
    }));
    // A ref's via columns are read for the lookup, kept or not.
    for r in &fact.refs {
        roles.extend(r.via.iter().map(|v| (v, Role::Key)));
    }
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

/// The checks every table gets. `roles` names each column the spec reads,
/// with what for; a column may come twice. A dim's key columns come first
/// in `roles`, `key_width` of them.
#[allow(clippy::too_many_arguments)]
fn check_table<'a>(
    table: &straum_domain::Table,
    name: &Ident,
    key_width: usize,
    access: Access,
    roles: Vec<(&Ident, Role)>,
    catalog: &'a Catalog,
    publication: &str,
    problems: &mut Vec<String>,
) -> Option<(Table, &'a Relation)> {
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
    let published = match access {
        Access::LookedUp => None,
        Access::Streamed => {
            if relation.row_filter {
                problems.push(format!(
                    "{name}: publication {publication} filters the rows of {qualified}; \
                     straum needs every row"
                ));
            }
            // A missing publication is reported once, not per table.
            match (&relation.published, catalog.publication) {
                (Some(published), _) => Some(published),
                (None, None) => None,
                (None, Some(_)) => {
                    problems.push(format!(
                        "{name}: publication {publication} leaves out {qualified}: \
                         ALTER PUBLICATION {publication} ADD TABLE {qualified}"
                    ));
                    return None;
                }
            }
        }
    };

    let column = |column: &Ident| {
        let found = relation.columns.iter().find(|c| c.name == column.as_str());
        found.ok_or_else(|| format!("{name}: {qualified} has no column {column}"))
    };
    let mut columns: Vec<Ident> = Vec::new();
    let mut types = Vec::new();
    for (name_of, role) in roles {
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
        key_width,
    };
    Some((table, relation))
}

/// A lookup by key finds one row or none only if the key is unique: a
/// primary key or unique index on exactly the dim's key columns, in any
/// order.
fn check_dim(dim: &Dim, table: &Table, relation: &Relation, problems: &mut Vec<String>) {
    let same = |columns: &[String]| {
        columns.len() == dim.key.len()
            && dim
                .key
                .iter()
                .all(|k| columns.iter().any(|c| c == k.as_str()))
    };
    if !relation.unique_indexes.iter().any(|i| same(i)) {
        let key = (dim.key.iter().map(Ident::as_str))
            .collect::<Vec<_>>()
            .join(", ");
        problems.push(format!(
            "dim {}: {} needs a primary key or unique index on {key} alone",
            dim.name,
            table.qualified()
        ));
    }
}

/// Refs join fact columns to dim keys of the same type, and filters compare
/// columns with literals of their type.
fn check_refs(spec: &Spec, tables: &Tables, problems: &mut Vec<String>) {
    for fact in spec.facts() {
        let facts = tables.fact(&fact.name);
        for r in &fact.refs {
            let dim = tables.dim(&r.dim);
            for (via, key) in r.via.iter().zip(&dim.columns) {
                let (via_type, key_type) = (facts.column_type(via), dim.column_type(key));
                if via_type != key_type {
                    problems.push(format!(
                        "fact {}: ref {} joins {via} ({via_type}) to {key} ({key_type}); \
                         they must read alike",
                        fact.name, r.name
                    ));
                }
            }
        }
        for m in &fact.measures {
            for cond in &m.filter {
                let column_type = match fact.resolve(&cond.path).expect("checked by Spec::new") {
                    Key::By(By::Column(c)) => facts.column_type(c),
                    Key::By(By::IsNull { .. }) => ColumnType::Bool,
                    Key::Attr(r, attr) => tables.dim(&r.dim).column_type(attr),
                };
                for value in &cond.values {
                    // The rule asks follow too.
                    if !column_type.fits(value) {
                        problems.push(format!(
                            "fact {}: measure {} compares {} ({column_type}) with {}",
                            fact.name,
                            m.name,
                            cond.path,
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
