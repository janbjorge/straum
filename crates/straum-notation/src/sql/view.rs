//! Walks a spec, a list of `CREATE VIEW` statements, into dims and facts.
//! Each view's `WITH` gives its bucket widths; its `SELECT` reads one fact
//! table and the dims it joins. The walk reads FROM first, so it knows the
//! tables, then the keys, so a FILTER can reach them, then the measures.
//! This file holds the view and its `SELECT`; `joins` reads FROM and the
//! joins, `columns` the keys and the columns they name, `measures` the
//! aggregates and the bucket, and `dims` merges each view's joins into
//! the dims of the spec.

use std::fmt;

use sqlparser::ast as sql;
use straum_domain::{
    Attribute, ByKey, Condition, Dim, Fact, Identifier, Measure, Ref, RefKind, Table, Time, Width,
};

use super::{Names, NullTest, Reported, SqlError, Walker, line, statements};
use measures::is_date_bin;

mod columns;
mod dims;
mod joins;
mod measures;

/// What a spec holds once walked: every dim its views join, and the views.
pub(crate) struct Views {
    pub(crate) dims: Vec<Dim>,
    pub(crate) facts: Vec<Fact>,
}

/// Walks a spec's text into its views and their dims.
pub(crate) fn parse(text: &str) -> Result<Views, Vec<SqlError>> {
    let statements = statements(text)?;
    let mut walker = Walker::default();
    let mut dims = Vec::new();
    let mut facts = Vec::new();
    for statement in &statements {
        let sql::Statement::CreateView(view) = statement else {
            walker.refuse(line(statement), "statement", statement);
            continue;
        };
        if let Ok(fact) = walker.view(view, &mut dims) {
            facts.push(fact);
        }
    }
    walker.finish(Ok(Views { dims, facts }))
}

/// A view's `SELECT`, walked.
struct Body {
    table: Table,
    time: Time,
    refs: Vec<JoinedRef>,
    by_keys: Vec<ByKey>,
    measures: Vec<Measure>,
    filter: Vec<Condition>,
    as_text: Vec<Identifier>,
}

/// The tables one view reads: the fact under `alias`, and one ref per join;
/// and its keys so far, which a FILTER reaches through their columns.
pub(super) struct Scope {
    alias: Identifier,
    refs: Vec<JoinedRef>,
    keys: Vec<ScopedKey>,
}

/// A key the view selects, by what it reads: a fact column, a fact
/// column's null-ness, or a dim column through its ref.
struct ScopedKey {
    name: Identifier,
    column: ResolvedColumn,
    form: KeyForm,
    cast: Cast,
    /// The column as written, so a second spelling can be named.
    written: String,
}

/// What a key reads of its column: the value, or whether it is null.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyForm {
    Value,
    NullNess(NullTest),
}

/// Whether a column is written `alias.column::text`, to read an enum as
/// its label (ADR-0037), or plain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cast {
    Plain,
    AsText,
}

/// A join: `reference.via[i]`, of the fact or of a dim joined before,
/// meets `key[i]` of the dim.
struct JoinedRef {
    reference: Ref,
    table: Table,
    key: Vec<Identifier>,
    /// The dim columns this view reads as `::text`.
    as_text: Vec<Identifier>,
}

/// What a column reference names. A dim's key column names the fact
/// column joined to it, since that is where its value is.
#[derive(PartialEq, Eq)]
enum ResolvedColumn {
    Fact(Identifier),
    /// A dim column; `through` is its ref's place in `Scope::refs`, as in
    /// the domain's `Via::Attribute`.
    Attribute {
        through: usize,
        column: Identifier,
    },
}

/// `alias.column`, as written.
struct Qualified {
    alias: Identifier,
    column: Identifier,
}

/// A named SELECT item: `expression AS alias`.
// Borrows both from the tree, which outlives the walk.
struct NamedItem<'tree> {
    expression: &'tree sql::Expr,
    alias: &'tree sql::Ident,
}

/// What a view's SELECT list names before its measures: the key items'
/// places, the `date_bin` buckets, the fact keys, and per ref the dim
/// columns it reads.
struct SelectedKeys {
    positions: Vec<usize>,
    buckets: Vec<Time>,
    by_keys: Vec<ByKey>,
    attributes: Vec<Vec<Attribute>>,
}

impl Walker {
    fn view(&mut self, view: &sql::CreateView, dims: &mut Vec<Dim>) -> Result<Fact, Reported> {
        let before = self.errors.len();
        let at = self.view_clauses(view);
        let fact_name = self.bare_name(&view.name, "view");
        let buckets = self.options(at, &view.options);
        if buckets.is_none()
            && let Ok(name) = &fact_name
        {
            self.error(
                at,
                format!(
                    "view {name} needs WITH (buckets = '<n> <unit>, ...'): every width \
                     Straum keeps, the date_bin width first"
                ),
            );
        }
        let body = self.view_body(&view.query);
        let name = fact_name?;
        let body = body?;
        // An empty bucket list is always an error already reported.
        let Some([first, wider @ ..]) = buckets.as_deref() else {
            return Err(Reported);
        };
        if self.errors.len() > before {
            return Err(Reported);
        }
        let Body {
            table,
            mut time,
            refs,
            by_keys,
            measures,
            filter,
            as_text,
        } = body;
        // Every width is said once, in the view's `WITH`; the body's `date_bin`
        // must agree with the first, so a reader finds the whole list in one
        // place and the body still runs in PostgreSQL.
        if *first != time.width {
            self.error(
                at,
                format!(
                    "view {name}: buckets starts with {first} but date_bin is {}: list \
                     every width, the date_bin width first",
                    time.width
                ),
            );
            return Err(Reported);
        }
        time.wider = wider.to_vec();
        let refs = self.add_dims(at, dims, refs);
        Ok(Fact {
            name,
            table,
            time,
            refs,
            by_keys,
            measures,
            filter,
            as_text,
        })
    }

    fn view_body(&mut self, query: &sql::Query) -> Result<Body, Reported> {
        let select = self.one_select(query, "view body")?;
        self.select(select)
    }

    fn view_clauses(&mut self, view: &sql::CreateView) -> u64 {
        let sql::CreateView {
            or_alter,
            or_replace: _,
            materialized,
            secure,
            name,
            // Only says where IF NOT EXISTS was written, which is rejected.
            name_before_not_exists: _,
            columns,
            query: _,
            options: _,
            cluster_by,
            comment,
            with_no_schema_binding,
            if_not_exists,
            temporary,
            copy_grants,
            to,
            params,
        } = view;
        let at = line(name);
        if *or_alter {
            self.not_in_subset(at, "OR ALTER");
        }
        if *materialized {
            self.not_in_subset(at, "MATERIALIZED");
        }
        if *secure {
            self.not_in_subset(at, "SECURE");
        }
        if !columns.is_empty() {
            self.not_in_subset(at, "a view column list");
        }
        if !cluster_by.is_empty() {
            self.not_in_subset(at, "CLUSTER BY");
        }
        if comment.is_some() {
            self.not_in_subset(at, "COMMENT");
        }
        if *with_no_schema_binding {
            self.not_in_subset(at, "WITH NO SCHEMA BINDING");
        }
        if *if_not_exists {
            self.not_in_subset(at, "IF NOT EXISTS");
        }
        if *temporary {
            self.not_in_subset(at, "TEMPORARY");
        }
        if *copy_grants {
            self.not_in_subset(at, "COPY GRANTS");
        }
        if to.is_some() {
            self.not_in_subset(at, "TO");
        }
        if params.is_some() {
            self.not_in_subset(at, "ALGORITHM, DEFINER or SQL SECURITY");
        }
        at
    }

    /// `WITH (buckets = '1 hour, 1 day, 7 days')`; `None` when the view
    /// gives no buckets, which the caller reports in its own words.
    fn options(&mut self, at: u64, options: &sql::CreateTableOptions) -> Option<Vec<Width>> {
        let list = match options {
            sql::CreateTableOptions::With(list) => list.as_slice(),
            sql::CreateTableOptions::None => &[],
            other => {
                self.refuse(at, "view options", other);
                &[]
            }
        };
        let mut buckets = None;
        for option in list {
            let sql::SqlOption::KeyValue { key: name, value } = option else {
                self.refuse(line(option), "view option", option);
                continue;
            };
            let at = line(option);
            let Ok(name) = self.read_identifier(name) else {
                continue;
            };
            if name.as_str() != "buckets" {
                self.refuse(at, "view option", option);
                continue;
            }
            if buckets.is_some() {
                self.error(at, format!("view option {name} given twice"));
                continue;
            }
            buckets = Some(value);
        }
        Some(self.bucket_widths(buckets?))
    }

    /// The widths a `buckets` option lists; a bad one is reported and
    /// left out.
    fn bucket_widths(&mut self, value: &sql::Expr) -> Vec<Width> {
        let sql::Expr::Value(sql::ValueWithSpan {
            value: sql::Value::SingleQuotedString(text),
            span: _,
        }) = value
        else {
            self.refuse(line(value), "buckets, use '<n> <unit>, ...',", value);
            return Vec::new();
        };
        let mut widths = Vec::new();
        for width in text.split(',').map(str::trim) {
            let Some(parsed) = parse_width(width) else {
                self.error(
                    line(value),
                    format!("bucket width '{width}': use '<n> seconds|minutes|hours|days'"),
                );
                continue;
            };
            widths.push(parsed);
        }
        widths
    }

    fn select(&mut self, select: &sql::Select) -> Result<Body, Reported> {
        let at = self.select_clauses(select);
        let [from] = select.from.as_slice() else {
            self.error(
                at,
                "SELECT needs exactly one FROM table; join with LEFT OUTER JOIN".into(),
            );
            return Err(Reported);
        };
        let joins::FromClause {
            table,
            mut scope,
            conditions,
        } = self.from(from, select.selection.as_ref())?;
        let named = self.named_items(&select.projection);
        let SelectedKeys {
            positions,
            buckets,
            by_keys,
            attributes,
        } = self.selected_keys(&mut scope, &named);
        let measures = self.measures(&scope, &named);
        self.group_by(at, &select.group_by, &positions);
        let mut buckets = buckets.into_iter();
        let time = buckets.next();
        for _ in buckets {
            self.error(at, "SELECT has more than one date_bin bucket".into());
        }
        self.ref_attributes(at, &mut scope.refs, attributes);
        let as_text = cast_columns(&mut scope);
        let filter = self.read_filter(&scope, &conditions)?;
        if measures_share_filter(&measures) {
            self.error(
                at,
                "every measure has the same FILTER; write it as the view's WHERE".into(),
            );
            return Err(Reported);
        }
        let Some(time) = time else {
            self.error(
                at,
                "SELECT needs a bucket: date_bin('<n> <unit>', <column>, timestamptz 'epoch'), \
                 or timestamp 'epoch' for a column without a zone"
                    .into(),
            );
            return Err(Reported);
        };
        Ok(Body {
            table,
            time,
            refs: scope.refs,
            by_keys,
            measures,
            filter,
            as_text,
        })
    }

    /// Keys first, then measures, so a FILTER can reach any key. A
    /// position comes from the item's shape, not its walk, so a bad key
    /// column is reported once and not again by GROUP BY.
    fn selected_keys(&mut self, scope: &mut Scope, named: &[NamedItem]) -> SelectedKeys {
        let mut selected = SelectedKeys {
            positions: Vec::new(),
            buckets: Vec::new(),
            by_keys: Vec::new(),
            attributes: vec![Vec::new(); scope.refs.len()],
        };
        for (index, item) in named.iter().enumerate() {
            let position = index + 1;
            match item.expression {
                sql::Expr::Function(function) if is_date_bin(function) => {
                    selected.positions.push(position);
                    if let Ok(time) = self.bucket(scope, function, item.alias) {
                        selected.buckets.push(time);
                    }
                }
                sql::Expr::Function(_) => {}
                expression => {
                    selected.positions.push(position);
                    self.key(scope, expression, item.alias, &mut selected);
                }
            }
        }
        selected
    }

    // Borrows the items from the tree, which outlives the walk.
    fn named_items<'tree>(
        &mut self,
        projection: &'tree [sql::SelectItem],
    ) -> Vec<NamedItem<'tree>> {
        let mut named = Vec::new();
        for item in projection {
            match item {
                sql::SelectItem::ExprWithAlias { expr, alias } => {
                    named.push(NamedItem {
                        expression: expr,
                        alias,
                    });
                }
                sql::SelectItem::UnnamedExpr(expression) => self.error(
                    line(item),
                    format!("name every SELECT item: {expression} AS <name>"),
                ),
                other => self.refuse(line(other), "SELECT item", other),
            }
        }
        named
    }

    fn measures(&mut self, scope: &Scope, named: &[NamedItem]) -> Vec<Measure> {
        let mut measures = Vec::new();
        for item in named {
            let sql::Expr::Function(function) = item.expression else {
                continue;
            };
            if is_date_bin(function) {
                continue;
            }
            if let Ok(measure) = self.measure(scope, function, item.alias) {
                measures.push(measure);
            }
        }
        measures
    }

    /// A ref reads the dim columns the view selects, through the via
    /// columns that key its cells. A join that selects nothing would do
    /// nothing; a semi or anti join selects nothing by nature.
    fn ref_attributes(&mut self, at: u64, refs: &mut [JoinedRef], attributes: Vec<Vec<Attribute>>) {
        for (index, reads) in attributes.into_iter().enumerate() {
            let reference = &mut refs[index].reference;
            let selects_none = reads.is_empty();
            match reference.kind {
                RefKind::Left | RefKind::Inner if selects_none => self.error(
                    at,
                    format!(
                        "{} is {} and selects no column of it; select one or drop the join",
                        reference.alias, reference.kind
                    ),
                ),
                RefKind::Semi | RefKind::Anti if !selects_none => self.error(
                    at,
                    format!(
                        "{} is {} and has no columns to select; select none of it",
                        reference.alias, reference.kind
                    ),
                ),
                _ => {}
            }
            reference.reads = reads;
        }
    }

    /// The view's WHERE and a measure's FILTER alike: `=`, `IN`, `IS NULL`
    /// and `IS NOT NULL` on columns, joined by AND, each reaching the key
    /// that reads the column. Stops at the first condition that fails.
    fn read_filter(
        &mut self,
        scope: &Scope,
        conditions: &[&sql::Expr],
    ) -> Result<Vec<Condition>, Reported> {
        let mut filter = Vec::new();
        for condition in conditions {
            let before = self.errors.len();
            self.read_conditions(condition, &mut filter, &mut Names::View(scope));
            if self.errors.len() > before {
                return Err(Reported);
            }
        }
        Ok(filter)
    }

    /// GROUP BY lists the keys by their SELECT positions, in order, so the
    /// view groups like the rollup. A position is what PostgreSQL resolves
    /// without a lookup; an output name it may read as an input column.
    fn group_by(&mut self, at: u64, group_by: &sql::GroupByExpr, positions: &[usize]) {
        let Ok(expressions) = self.group_by_expressions(at, group_by) else {
            return;
        };
        let mut grouped = Vec::new();
        for expression in expressions {
            let Some(position) = select_position(expression) else {
                self.refuse(
                    line(expression),
                    "GROUP BY item, write the key's position in SELECT,",
                    expression,
                );
                return;
            };
            grouped.push(position);
        }
        if grouped != positions {
            self.error(
                at,
                format!(
                    "GROUP BY {} must list the keys' positions in SELECT: GROUP BY {}",
                    comma_list(&grouped),
                    comma_list(positions)
                ),
            );
        }
    }
}

/// A GROUP BY item written as a SELECT position, `1` on.
fn select_position(expression: &sql::Expr) -> Option<usize> {
    let sql::Expr::Value(sql::ValueWithSpan {
        value: sql::Value::Number(digits, false),
        span: _,
    }) = expression
    else {
        return None;
    };
    digits.parse().ok()
}

fn parse_width(text: &str) -> Option<Width> {
    let [count, unit] = text.split_whitespace().collect::<Vec<_>>()[..] else {
        return None;
    };
    let scale = match unit.strip_suffix('s').unwrap_or(unit) {
        "second" => 1,
        "minute" => 60,
        "hour" => 3_600,
        "day" => 86_400,
        _ => return None,
    };
    let count: u32 = count.parse().ok()?;
    let seconds = count.checked_mul(scale)?;
    Some(Width { seconds })
}

/// The columns the view reads as `::text`: the fact's, returned, and each
/// dim's, put on its ref.
fn cast_columns(scope: &mut Scope) -> Vec<Identifier> {
    let mut as_text = Vec::new();
    for key in &scope.keys {
        if key.cast == Cast::Plain {
            continue;
        }
        match &key.column {
            ResolvedColumn::Fact(column) => push_once(&mut as_text, column),
            ResolvedColumn::Attribute { through, column } => {
                push_once(&mut scope.refs[*through].as_text, column);
            }
        }
    }
    as_text
}

fn push_once(columns: &mut Vec<Identifier>, column: &Identifier) {
    if !columns.contains(column) {
        columns.push(column.clone());
    }
}

/// A FILTER every measure shares is the view's WHERE in disguise; a
/// lone measure shares with nobody.
fn measures_share_filter(measures: &[Measure]) -> bool {
    let [first, rest @ ..] = measures else {
        return false;
    };
    if first.filter.is_empty() || rest.is_empty() {
        return false;
    }
    for measure in rest {
        if measure.filter != first.filter {
            return false;
        }
    }
    true
}

// Any printable list: column names or SELECT positions.
fn comma_list(items: &[impl fmt::Display]) -> String {
    let mut texts = Vec::new();
    for item in items {
        texts.push(item.to_string());
    }
    texts.join(", ")
}
