//! The setup check: does PostgreSQL hold the spec's tables in a shape straum
//! can read and follow? Read from the catalog on the connection that loads,
//! so it sees the tables as the load does.

use std::iter;

use postgres_protocol::escape::escape_literal;
use straum_domain::{ColumnType, Dim, Fact, Ident, Key, Literal, Spec};

use crate::error::Error;
use crate::value::type_of;
use crate::wire::Conn;

/// A spec table as PostgreSQL has it, ready to load and stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Table {
    /// The spec's name for it.
    pub(crate) name: Ident,
    pub(crate) schema: Ident,
    pub(crate) table: Ident,
    /// What a row holds, in order: `Fact::columns`, or the dim key and then
    /// its attributes.
    pub(crate) columns: Vec<Ident>,
    pub(crate) kinds: Vec<ColumnType>,
    /// The fact's or dim's key. A fact's key may be of any type; straum only
    /// needs to see that a change carries it instead of the whole row.
    pub(crate) key: Ident,
    pub(crate) key_kind: ColumnType,
}

impl Table {
    pub(crate) fn kind(&self, column: &Ident) -> ColumnType {
        let i = self.columns.iter().position(|c| c == column);
        self.kinds[i.expect("the column is in the table's row")]
    }

    pub(crate) fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.table)
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
    pub(crate) kind: String,
    /// `pg_class.relreplident`: `d`efault, `f`ull, `i`ndex or `n`othing.
    pub(crate) identity: String,
    pub(crate) select: bool,
    /// The columns of the replica identity index, if there is one.
    pub(crate) identity_columns: Vec<String>,
    /// Columns with a single-column unique index of their own.
    pub(crate) unique_columns: Vec<String>,
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
pub(crate) fn read(conn: &mut Conn, spec: &Spec, publication: &str) -> Result<Vec<Table>, Error> {
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
                (SELECT string_agg(a.attname, ',' ORDER BY k.ord)
                   FROM pg_index i
                  CROSS JOIN LATERAL unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord)
                   JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = k.attnum
                  WHERE i.indrelid = c.oid AND k.ord <= i.indnkeyatts
                    AND CASE c.relreplident WHEN 'd' THEN i.indisprimary
                                            WHEN 'i' THEN i.indisreplident
                                            ELSE false END),
                (SELECT string_agg(a.attname, ',')
                   FROM pg_index i
                   JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = i.indkey[0]
                  WHERE i.indrelid = c.oid AND i.indisunique AND i.indisvalid
                    AND i.indnkeyatts = 1 AND i.indpred IS NULL AND i.indexprs IS NULL),
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
            kind,
            identity,
            select,
            identity_columns,
            unique_columns,
            published,
            row_filter,
        ] = row.try_into().map_err(|_| shape("relations"))?;
        let list = |s: Option<String>| -> Vec<String> {
            s.map(|s| s.split(',').map(str::to_owned).collect())
                .unwrap_or_default()
        };
        catalog.relations.push(Relation {
            schema: schema.unwrap_or_default(),
            name: name.unwrap_or_default(),
            kind: kind.unwrap_or_default(),
            identity: identity.unwrap_or_default(),
            select: select.as_deref() == Some("t"),
            identity_columns: list(identity_columns),
            unique_columns: list(unique_columns),
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
    /// The fact's time column.
    Time,
    /// A value straum sums, or keeps the least and greatest of.
    Number,
    /// A value straum groups, joins or counts distinct.
    Key,
}

impl Role {
    fn allows(self, kind: ColumnType) -> bool {
        match self {
            Role::Time => kind == ColumnType::Time,
            Role::Number => matches!(kind, ColumnType::Int | ColumnType::Numeric { .. }),
            Role::Key => kind != ColumnType::Time,
        }
    }

    fn needs(self) -> &'static str {
        match self {
            Role::Time => "timestamptz",
            Role::Number => "an integer type or numeric(p, s) with p at most 18",
            Role::Key => "an integer type, numeric(p, s), boolean or text",
        }
    }
}

/// Checks the catalog against the spec and returns every problem found,
/// each with what to change. On success, the tables ready to read.
pub(crate) fn check(
    spec: &Spec,
    catalog: &Catalog,
    publication: &str,
) -> Result<Vec<Table>, Vec<String>> {
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

    let mut tables = Vec::new();
    for dim in spec.dims() {
        let roles = iter::once(&dim.key)
            .chain(&dim.attrs)
            .map(|c| (c, Role::Key))
            .collect();
        let checked = check_table(
            &dim.table,
            &dim.name,
            None,
            roles,
            catalog,
            publication,
            &mut problems,
        );
        if let Some((table, relation)) = checked {
            check_dim(dim, &table, relation, &mut problems);
            tables.push(table);
        }
    }
    for fact in spec.facts() {
        let roles = fact_roles(fact);
        let checked = check_table(
            &fact.table,
            &fact.name,
            Some(&fact.key),
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
            tables.push(table);
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
    let mut roles = vec![(&fact.time.column, Role::Time)];
    roles.extend(fact.by.iter().map(|c| (c, Role::Key)));
    roles.extend(fact.refs.iter().map(|r| (&r.via, Role::Key)));
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
/// with what for; a column may come twice. A dim's key comes first in
/// `roles`; a fact's `key` is apart, since it may be of any type.
fn check_table<'a>(
    table: &straum_domain::Table,
    name: &Ident,
    key: Option<&Ident>,
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
    if relation.kind != "r" {
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
            "{name}: publication {publication} filters the rows of {qualified}; straum needs every row"
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
    let mut kinds = Vec::new();
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
                        "{name}: column {name_of} of {qualified} is {}; straum needs {} there",
                        c.type_name,
                        role.needs()
                    )
                })
        });
        match found {
            Err(problem) => problems.push(problem),
            Ok(_) if columns.contains(name_of) => {}
            Ok(kind) => {
                columns.push(name_of.clone());
                kinds.push(kind);
            }
        }
    }
    // A fact's key reads as text when straum has no kind for its type.
    let key_kind = match key {
        None => kinds.first().copied(),
        Some(key) => match column(key) {
            Ok(c) => Some(type_of(c.type_oid, c.typmod).unwrap_or(ColumnType::Text)),
            Err(problem) => {
                problems.push(problem);
                None
            }
        },
    };
    if problems.len() > before {
        return None;
    }
    let table = Table {
        name: name.clone(),
        schema: table.schema.clone(),
        table: table.name.clone(),
        key: key.unwrap_or(&columns[0]).clone(),
        columns,
        kinds,
        key_kind: key_kind.expect("no problem, so the key was found"),
    };
    Some((table, relation))
}

fn check_dim(dim: &Dim, table: &Table, relation: &Relation, problems: &mut Vec<String>) {
    let qualified = table.qualified();
    let key = dim.key.as_str();
    match relation.identity.as_str() {
        "f" if !relation.unique_columns.iter().any(|c| c == key) => problems.push(format!(
            "dim {}: {qualified} needs a primary key or unique index on {key} alone",
            dim.name
        )),
        "f" => {}
        _ if relation.identity_columns != [key] => problems.push(format!(
            "dim {}: the replica identity of {qualified} must be {key} alone, \
             so a delete names the row straum holds: a primary key on {key}, \
             REPLICA IDENTITY USING INDEX on a unique index of {key}, or REPLICA IDENTITY FULL",
            dim.name
        )),
        _ => {}
    }
}

/// Refs join fact columns to dim keys of the same kind, and filters compare
/// columns with literals of their kind.
fn check_refs(spec: &Spec, tables: &[Table], problems: &mut Vec<String>) {
    let table = |name: &Ident| tables.iter().find(|t| t.name == *name).expect("checked");
    for fact in spec.facts() {
        let facts = table(&fact.name);
        for r in &fact.refs {
            let dim = table(&r.dim);
            let (via, key) = (facts.kind(&r.via), dim.kinds[0]);
            if via != key {
                problems.push(format!(
                    "fact {}: ref {} joins {} ({via}) to {} ({key}); they must read alike",
                    fact.name, r.name, r.via, dim.key
                ));
            }
        }
        for m in &fact.measures {
            for cond in &m.filter {
                let kind = match spec
                    .resolve(fact, &cond.path)
                    .expect("checked by Spec::new")
                {
                    Key::Column(c) => facts.kind(c),
                    Key::Ref(r) => facts.kind(&r.via),
                    Key::Attr(r, attr) => table(&r.dim).kind(attr),
                };
                for value in &cond.values {
                    // The rule asks follow too.
                    if !kind.fits(value) {
                        problems.push(format!(
                            "fact {}: measure {} compares {} ({kind}) with {}",
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
