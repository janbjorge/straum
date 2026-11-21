//! Walks a spec, a list of `CREATE VIEW` statements, into dims and facts.
//! Each view's `SELECT` reads one fact table and the dims it joins, and
//! names its time with `buckets(column, widths...)`. The walk reads FROM first, so it knows the
//! tables, then the keys, so a FILTER can reach them, then the measures.
//! This file holds the view and its `SELECT`; `joins` reads FROM and the
//! joins, `columns` the keys and the columns they name, `measures` the
//! aggregates and the `buckets` item, and `dims` merges each view's joins into
//! the dims of the spec.

use sqlparser::ast as sql;
use straum_domain::{
    Attribute, ByKey, Condition, Dim, Fact, Identifier, Measure, Ref, Table, Time, Via,
};

use super::calls::{BUCKETS, DATE_BIN, is_call};
use super::{
    Clause, Names, NullTest, Reported, SqlError, Walker, and_parts, comma_list, line, statements,
    without_leading_zeros, written_digits,
};

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
    if statements.is_empty() {
        walker.error(
            0,
            "a spec is one or more CREATE VIEW statements; this has none".into(),
        );
    }
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
    column: Via,
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
/// its label (ADR-0004), or plain.
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
/// places, the `buckets` items, the fact keys, and per ref the dim
/// columns it reads.
struct SelectedKeys {
    positions: Vec<usize>,
    /// Each `buckets` item, an `Err` one already reported.
    buckets: Vec<Result<Time, Reported>>,
    by_keys: Vec<ByKey>,
    attributes: Vec<Vec<Attribute>>,
}

impl Walker {
    fn view(&mut self, view: &sql::CreateView, dims: &mut Vec<Dim>) -> Result<Fact, Reported> {
        let before = self.met();
        let at = self.view_clauses(view);
        let fact_name = self.bare_name(&view.name, "view");
        let body = self.view_body(&view.query);
        let name = fact_name?;
        let body = body?;
        if self.met() > before {
            return Err(Reported);
        }
        let Body {
            table,
            time,
            refs,
            by_keys,
            measures,
            filter,
            as_text,
        } = body;
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
            or_replace,
            materialized,
            secure,
            name,
            // Only says where IF NOT EXISTS was written, which is rejected.
            name_before_not_exists: _,
            columns,
            query: _,
            options,
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
        self.reject_if_present(at, *or_alter, "OR ALTER");
        self.reject_if_present(at, *or_replace, "OR REPLACE");
        // The widths a view keeps are in its `buckets` item.
        if *options != sql::CreateTableOptions::None {
            self.refuse(at, "view options", options);
        }
        self.reject_if_present(at, *materialized, "MATERIALIZED");
        self.reject_if_present(at, *secure, "SECURE");
        self.reject_if_present(at, !columns.is_empty(), "a view column list");
        self.reject_if_present(at, !cluster_by.is_empty(), "CLUSTER BY");
        self.reject_if_present(at, comment.is_some(), "COMMENT");
        self.reject_if_present(at, *with_no_schema_binding, "WITH NO SCHEMA BINDING");
        self.reject_if_present(at, *if_not_exists, "IF NOT EXISTS");
        self.reject_if_present(at, *temporary, "TEMPORARY");
        self.reject_if_present(at, *copy_grants, "COPY GRANTS");
        self.reject_if_present(at, to.is_some(), "TO");
        self.reject_if_present(at, params.is_some(), "ALGORITHM, DEFINER or SQL SECURITY");
        at
    }

    fn select(&mut self, select: &sql::Select) -> Result<Body, Reported> {
        let at = self.select_clauses(select);
        self.reject_if_present(at, select.having.is_some(), "HAVING");
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
            self.error(at, "SELECT has more than one buckets item".into());
        }
        // A ref reads the dim columns the view selects; `Spec::new` checks
        // that a join's kind fits what it reads.
        for (joined, reads) in scope.refs.iter_mut().zip(attributes) {
            joined.reference.reads = reads;
        }
        let as_text = cast_columns(&mut scope);
        let filter = self.read_filter(&scope, &conditions, Clause::Where)?;
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
                "SELECT needs its buckets: buckets(<column>, '<n> <unit>', ...) AS <name>, \
                 every width kept, narrowest first"
                    .into(),
            );
            return Err(Reported);
        };
        let time = time?;
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
                sql::Expr::Function(function) if is_call(function, BUCKETS) => {
                    selected.positions.push(position);
                    let time = self.bucket(scope, function, item.alias);
                    selected.buckets.push(time);
                }
                // PostgreSQL's spelling of the time, taken as the view's
                // time so the error names it alone.
                sql::Expr::Function(function) if is_call(function, DATE_BIN) => {
                    selected.positions.push(position);
                    self.error(
                        line(function),
                        "date_bin in a view: write buckets(<alias.column>, '<width>', ...) AS <name>"
                            .into(),
                    );
                    selected.buckets.push(Err(Reported));
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
            if is_call(function, BUCKETS) || is_call(function, DATE_BIN) {
                continue;
            }
            if let Ok(measure) = self.measure(scope, function, item.alias) {
                measures.push(measure);
            }
        }
        measures
    }

    /// The view's WHERE and a measure's FILTER alike: `=`, `IN`, `IS NULL`
    /// and `IS NOT NULL` on columns, joined by AND, each reaching the key
    /// that reads the column, none written twice. Stops at the first
    /// condition that fails.
    fn read_filter(
        &mut self,
        scope: &Scope,
        conditions: &[&sql::Expr],
        clause: Clause,
    ) -> Result<Vec<Condition>, Reported> {
        let mut parts = Vec::new();
        for condition in conditions {
            and_parts(condition, &mut parts);
        }
        let before = self.met();
        let parts = self.without_repeats(parts);
        if self.met() > before {
            return Err(Reported);
        }
        let mut filter = Vec::new();
        for condition in parts {
            let before = self.met();
            self.read_conditions(condition, &mut filter, &mut Names::View(scope, clause));
            if self.met() > before {
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
            if let Some(digits) = written_digits(expression)
                && let Some(position) = without_leading_zeros(digits)
            {
                self.error(
                    line(expression),
                    format!("GROUP BY {digits}: write {position}"),
                );
                return;
            }
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
    let digits = written_digits(expression)?;
    // `01` is `1` written a second way.
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    digits.parse().ok()
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
            Via::Fact(column) => push_once(&mut as_text, column),
            Via::Attribute { through, column } => {
                push_once(&mut scope.refs[through.0].as_text, column);
            }
        }
    }
    as_text
}

pub(super) fn push_once(columns: &mut Vec<Identifier>, column: &Identifier) {
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
