use std::fmt;

use sqlparser::ast as sql;
use straum_domain::{
    Attribute, ByKey, Condition, Dim, Fact, Identifier, Measure, Ref, RefKind, Table, Time, Via,
    Width,
};

use super::{Names, SqlError, Walker, line, statements};
use measures::is_date_bin;

mod columns;
mod dims;
mod joins;
mod measures;

pub(crate) fn parse(src: &str) -> Result<(Vec<Dim>, Vec<Fact>), Vec<SqlError>> {
    let statements = statements(src)?;
    let mut walker = Walker::default();
    let mut dims = Vec::new();
    let mut facts = Vec::new();
    for statement in &statements {
        match statement {
            sql::Statement::CreateView(view) => {
                facts.extend(walker.view(view, &mut dims));
            }
            other => walker.refuse(line(other), "statement", other),
        }
    }
    walker.finish(Some((dims, facts)))
}

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
    is_null: Option<bool>,
    /// Written `alias.column::text`.
    cast: bool,
    /// The column as written, so a second spelling can be named.
    spelled: String,
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
    Attribute { through: usize, column: Identifier },
}

struct AliasedTable {
    table: Table,
    alias: Identifier,
}

struct Qualified {
    alias: Identifier,
    column: Identifier,
}

struct Reference {
    alias: Identifier,
    column: Identifier,
    cast: bool,
}

struct KeyRead {
    column: ResolvedColumn,
    is_null: Option<bool>,
    cast: bool,
}

struct Equalities {
    key: Vec<Identifier>,
    via: Vec<Via>,
}

struct Equality {
    key: Identifier,
    via: Via,
}

struct NamedItem<'a> {
    expr: &'a sql::Expr,
    alias: &'a sql::Ident,
}

struct FromClause<'a> {
    table: Table,
    scope: Scope,
    conditions: Vec<&'a sql::Expr>,
}

struct Call<'a> {
    args: &'a [sql::FunctionArg],
    distinct: bool,
    within_group: &'a [sql::OrderByExpr],
}

#[derive(Clone, Copy)]
enum KeyKind {
    Value,
    NullNess,
}

impl Walker {
    fn view(&mut self, view: &sql::CreateView, dims: &mut Vec<Dim>) -> Option<Fact> {
        let before = self.errors.len();
        let at = self.view_clauses(view);
        let name = &view.name;
        let query = &view.query;
        let options = &view.options;

        let fact_name = self.bare_name(name, "view");
        let buckets = self.options(at, options);
        if let (Some(name), None) = (&fact_name, &buckets) {
            self.error(
                at,
                format!(
                    "view {name} needs WITH (buckets = '<n> <unit>, ...'): every width \
                     Straum keeps, the date_bin width first"
                ),
            );
        }
        let body = (self.one_select(query, "view body")).and_then(|select| self.select(select));
        // An empty bucket list is always an error already reported.
        let (Some(name), Some(body), Some([first, wider @ ..])) =
            (fact_name, body, buckets.as_deref())
        else {
            return None;
        };
        if self.errors.len() > before {
            return None;
        }
        let Body {
            table,
            mut time,
            refs,
            by_keys: by,
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
            return None;
        }
        time.wider = wider.to_vec();
        let refs = self.add_dims(at, dims, refs);
        Some(Fact {
            name,
            table,
            time,
            refs,
            by_keys: by,
            measures,
            filter,
            as_text,
        })
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
        self.absent(at, *or_alter, "OR ALTER");
        self.absent(at, *materialized, "MATERIALIZED");
        self.absent(at, *secure, "SECURE");
        self.absent(at, !columns.is_empty(), "a view column list");
        self.absent(at, !cluster_by.is_empty(), "CLUSTER BY");
        self.absent(at, comment.is_some(), "COMMENT");
        self.absent(at, *with_no_schema_binding, "WITH NO SCHEMA BINDING");
        self.absent(at, *if_not_exists, "IF NOT EXISTS");
        self.absent(at, *temporary, "TEMPORARY");
        self.absent(at, *copy_grants, "COPY GRANTS");
        self.absent(at, to.is_some(), "TO");
        self.absent(at, params.is_some(), "ALGORITHM, DEFINER or SQL SECURITY");
        at
    }

    /// `WITH (buckets = '1 hour, 1 day, 7 days')`.
    fn options(&mut self, at: u64, options: &sql::CreateTableOptions) -> Option<Vec<Width>> {
        let mut buckets = None;
        let list = match options {
            sql::CreateTableOptions::With(list) => list.as_slice(),
            sql::CreateTableOptions::None => &[],
            other => {
                self.refuse(at, "view options", other);
                &[]
            }
        };
        for option in list {
            let sql::SqlOption::KeyValue { key: name, value } = option else {
                self.refuse(line(option), "view option", option);
                continue;
            };
            let at = line(option);
            let Some(name) = self.read_identifier(name) else {
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
        match buckets {
            None => None,
            Some(
                value @ sql::Expr::Value(sql::ValueWithSpan {
                    value: sql::Value::SingleQuotedString(text),
                    span: _,
                }),
            ) => {
                let mut buckets = Vec::new();
                for width in text.split(',').map(str::trim) {
                    match parse_width(width) {
                        Some(parsed) => buckets.push(parsed),
                        None => self.error(
                            line(value),
                            format!("bucket width '{width}': use '<n> seconds|minutes|hours|days'"),
                        ),
                    }
                }
                Some(buckets)
            }
            Some(other) => {
                self.refuse(line(other), "buckets, use '<n> <unit>, ...',", other);
                Some(Vec::new())
            }
        }
    }

    fn select(&mut self, select: &sql::Select) -> Option<Body> {
        let at = self.select_clauses(select);
        let [from] = select.from.as_slice() else {
            self.error(
                at,
                "SELECT needs exactly one FROM table; join with LEFT OUTER JOIN".into(),
            );
            return None;
        };
        let FromClause {
            table,
            mut scope,
            conditions,
        } = self.from(from, select.selection.as_ref())?;
        let named = self.named_items(&select.projection);
        // Keys first, then measures, so a FILTER can reach any key. A
        // position comes from the item's shape, not its walk, so a bad key
        // column is reported once and not again by GROUP BY.
        let mut positions = Vec::new();
        let mut buckets = Vec::new();
        let mut by = Vec::new();
        let mut attributes: Vec<Vec<Attribute>> = vec![Vec::new(); scope.refs.len()];
        for (index, &NamedItem { expr, alias }) in named.iter().enumerate() {
            match expr {
                sql::Expr::Function(function) if is_date_bin(function) => {
                    positions.push(index + 1);
                    buckets.extend(self.bucket(&scope, function, alias));
                }
                sql::Expr::Function(_) => {}
                _ => {
                    positions.push(index + 1);
                    self.key(&mut scope, expr, alias, &mut by, &mut attributes);
                }
            }
        }
        let measures = self.measures(&scope, &named);
        self.group_by(at, &select.group_by, &positions);
        let mut buckets = buckets.into_iter();
        let time = buckets.next();
        for _ in buckets {
            self.error(at, "SELECT has more than one date_bin bucket".into());
        }
        self.ref_attributes(at, &mut scope.refs, attributes);
        let as_text = cast_columns(&mut scope);
        let filter = self.read_filter(&scope, conditions)?;
        if measures_share_filter(&measures) {
            self.error(
                at,
                "every measure has the same FILTER; write it as the view's WHERE".into(),
            );
            return None;
        }

        let Some(time) = time else {
            self.error(
                at,
                "SELECT needs a bucket: date_bin('<n> <unit>', <column>, timestamptz 'epoch'), \
                 or timestamp 'epoch' for a column without a zone"
                    .into(),
            );
            return None;
        };
        Some(Body {
            table,
            time,
            refs: scope.refs,
            by_keys: by,
            measures,
            filter,
            as_text,
        })
    }

    fn named_items<'a>(&mut self, projection: &'a [sql::SelectItem]) -> Vec<NamedItem<'a>> {
        let mut named = Vec::new();
        for item in projection {
            match item {
                sql::SelectItem::ExprWithAlias { expr, alias } => {
                    named.push(NamedItem { expr, alias });
                }
                sql::SelectItem::UnnamedExpr(expr) => self.error(
                    line(item),
                    format!("name every SELECT item: {expr} AS <name>"),
                ),
                other => self.refuse(line(other), "SELECT item", other),
            }
        }
        named
    }

    fn measures(&mut self, scope: &Scope, named: &[NamedItem]) -> Vec<Measure> {
        let mut measures = Vec::new();
        for &NamedItem { expr, alias } in named {
            if let sql::Expr::Function(function) = expr
                && !is_date_bin(function)
            {
                measures.extend(self.measure(scope, function, alias));
            }
        }
        measures
    }

    /// A ref reads the dim columns the view selects, through the via
    /// columns that key its cells. A join that selects nothing would do
    /// nothing; a semi or anti join selects nothing by nature.
    fn ref_attributes(&mut self, at: u64, refs: &mut [JoinedRef], attributes: Vec<Vec<Attribute>>) {
        for (joined, attributes) in refs.iter_mut().zip(attributes) {
            let reference = &mut joined.reference;
            let selects_none = attributes.is_empty();
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
            reference.reads = attributes;
        }
    }

    /// The view's WHERE and a measure's FILTER alike: `=`, `IN`, `IS NULL`
    /// and `IS NOT NULL` on columns, joined by AND, each reaching the key
    /// that reads the column. `None` once a condition fails.
    fn read_filter<'e>(
        &mut self,
        scope: &Scope,
        conditions: impl IntoIterator<Item = &'e sql::Expr>,
    ) -> Option<Vec<Condition>> {
        let mut filter = Vec::new();
        for condition in conditions {
            let before = self.errors.len();
            self.read_conditions(condition, &mut filter, &mut Names::View(scope));
            if self.errors.len() > before {
                return None;
            }
        }
        Some(filter)
    }

    /// GROUP BY lists the keys by their SELECT positions, in order, so the
    /// view groups like the rollup. A position is what PostgreSQL resolves
    /// without a lookup; an output name it may read as an input column.
    fn group_by(&mut self, at: u64, group_by: &sql::GroupByExpr, positions: &[usize]) {
        let Some(exprs) = self.group_by_exprs(at, group_by) else {
            return;
        };
        let mut grouped = Vec::new();
        for expr in exprs {
            let position = match expr {
                sql::Expr::Value(sql::ValueWithSpan {
                    value: sql::Value::Number(digits, false),
                    span: _,
                }) => digits.parse::<usize>().ok(),
                _ => None,
            };
            let Some(position) = position else {
                self.refuse(
                    line(expr),
                    "GROUP BY item, write the key's position in SELECT,",
                    expr,
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

fn cast_columns(scope: &mut Scope) -> Vec<Identifier> {
    let mut as_text = Vec::new();
    for key in &scope.keys {
        if !key.cast {
            continue;
        }
        let (read, column) = match &key.column {
            ResolvedColumn::Fact(column) => (&mut as_text, column),
            ResolvedColumn::Attribute { through, column } => {
                (&mut scope.refs[*through].as_text, column)
            }
        };
        if !read.contains(column) {
            read.push(column.clone());
        }
    }
    as_text
}

/// A FILTER every measure shares is the view's WHERE in disguise; a
/// lone measure shares with nobody.
fn measures_share_filter(measures: &[Measure]) -> bool {
    let [first, rest @ ..] = measures else {
        return false;
    };
    !first.filter.is_empty()
        && !rest.is_empty()
        && rest.iter().all(|measure| measure.filter == first.filter)
}

fn comma_list(items: &[impl fmt::Display]) -> String {
    (items.iter().map(ToString::to_string))
        .collect::<Vec<_>>()
        .join(", ")
}
