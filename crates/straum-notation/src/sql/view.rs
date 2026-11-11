use std::fmt;

use sqlparser::ast as sql;
use straum_domain::{
    Agg, Attr, By, Cond, Dim, Fact, Grain, Ident, Literal, Measure, Origin, Ref, RefKind, Table,
    Time, Via,
};

use super::{Names, SqlError, Walker, and_parts, function_name, line, statements};

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
            other => walker.reject(line(other), "statement", other),
        }
    }
    if walker.errors.is_empty() {
        Ok((dims, facts))
    } else {
        Err(walker.errors)
    }
}

struct Body {
    table: Table,
    time: Time,
    refs: Vec<JoinedRef>,
    by: Vec<By>,
    measures: Vec<Measure>,
    filter: Vec<Cond>,
    as_text: Vec<Ident>,
}

/// The tables one view reads: the fact under `alias`, and one ref per join;
/// and its keys so far, which a FILTER reaches through their columns.
pub(super) struct Scope {
    alias: Ident,
    refs: Vec<JoinedRef>,
    keys: Vec<ScopedKey>,
}

/// A key the view selects, by what it reads: a fact column, a fact
/// column's null-ness, or a dim column through its ref.
struct ScopedKey {
    name: Ident,
    column: Column,
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
    key: Vec<Ident>,
    /// The dim columns this view reads as `::text`.
    as_text: Vec<Ident>,
}

/// What a column reference names. A dim's key column names the fact
/// column joined to it, since that is where its value is.
#[derive(PartialEq, Eq)]
enum Column {
    Fact(Ident),
    Attr { through: usize, column: Ident },
}

impl Walker {
    fn view(&mut self, view: &sql::CreateView, dims: &mut Vec<Dim>) -> Option<Fact> {
        let sql::CreateView {
            or_alter,
            or_replace: _,
            materialized,
            secure,
            name,
            // Only says where IF NOT EXISTS was written, which is rejected.
            name_before_not_exists: _,
            columns,
            query,
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
        let before = self.errors.len();
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

        let fact_name = self.bare_name(name, "view");
        let buckets = self.options(at, options);
        if let (Some(name), None) = (&fact_name, &buckets) {
            self.error(
                at,
                format!(
                    "view {name} needs WITH (buckets = '<n> <unit>, ...'): every width \
                     straum keeps, the date_bin width first"
                ),
            );
        }
        let body = self.query(query);
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
            by,
            measures,
            filter,
            as_text,
        } = body;
        // Every width is said once, in the view's `WITH`; the body's `date_bin`
        // must agree with the first, so a reader finds the whole list in one
        // place and the body still runs in PostgreSQL.
        if *first != time.grain {
            self.error(
                at,
                format!(
                    "view {name}: buckets starts with {first} but date_bin is {}: list \
                     every width, the date_bin width first",
                    time.grain
                ),
            );
            return None;
        }
        time.buckets = wider.to_vec();
        let refs = self.add_dims(at, dims, refs);
        Some(Fact {
            name,
            table,
            time,
            refs,
            by,
            measures,
            filter,
            as_text,
        })
    }

    fn add_dims(&mut self, at: u64, dims: &mut Vec<Dim>, refs: Vec<JoinedRef>) -> Vec<Ref> {
        // A dim a later join reads its key from carries that column too.
        let mut carried: Vec<(usize, Ident)> = Vec::new();
        for joined in &refs {
            for via in &joined.reference.via {
                if let Via::Attr { through, column } = via {
                    carried.push((*through, column.clone()));
                }
            }
        }
        let mut fact_refs = Vec::new();
        for (index, mut joined) in refs.into_iter().enumerate() {
            if let Some(dim) = self.add_dim(at, dims, &mut joined) {
                for (through, column) in &carried {
                    if *through == index && !dim.attrs.contains(column) {
                        dim.attrs.push(column.clone());
                    }
                }
            }
            fact_refs.push(joined.reference);
        }
        fact_refs
    }

    /// A dim is its table. Every view that joins it must join on the same
    /// key columns, in any order, and its attributes are every column any
    /// view reads, a column another join reads its key from included. The
    /// join's via columns are put in the dim's key order, so a ref's via
    /// and its dim's key pair up by position.
    fn add_dim<'d>(
        &mut self,
        at: u64,
        dims: &'d mut Vec<Dim>,
        joined: &mut JoinedRef,
    ) -> Option<&'d mut Dim> {
        let Some(index) = dims.iter().position(|dim| dim.table == joined.table) else {
            dims.push(Dim {
                name: joined.reference.dim.clone(),
                table: joined.table.clone(),
                key: joined.key.clone(),
                attrs: (joined.reference.attrs.iter())
                    .map(|attr| attr.column.clone())
                    .collect(),
                as_text: joined.as_text.clone(),
            });
            return dims.last_mut();
        };
        let dim = &mut dims[index];
        let same = dim.key.len() == joined.key.len()
            && dim.key.iter().all(|column| joined.key.contains(column));
        if !same {
            self.error(
                at,
                format!(
                    "{}.{} is joined on {} here and on {} elsewhere",
                    joined.table.schema,
                    joined.table.name,
                    comma_list(&joined.key),
                    comma_list(&dim.key)
                ),
            );
            return None;
        }
        let via: Vec<Via> = (dim.key.iter())
            .map(|column| {
                let position = (joined.key.iter())
                    .position(|joined_column| joined_column == column)
                    .expect("the key sets are equal");
                joined.reference.via[position].clone()
            })
            .collect();
        joined.reference.via = via;
        joined.key.clone_from(&dim.key);
        // A column is one spelling in every view, since it is one type.
        for attr in &joined.reference.attrs {
            let column = &attr.column;
            let cast = joined.as_text.contains(column);
            let read_before = dim.attrs.contains(column);
            let cast_before = dim.as_text.contains(column);
            if read_before && cast != cast_before {
                let (here, there) = if cast {
                    ("::text", "without it")
                } else {
                    ("without ::text", "with it")
                };
                self.error(
                    at,
                    format!(
                        "{}.{}.{column} is read {here} here and {there} in another view; \
                         write it alike",
                        joined.table.schema, joined.table.name
                    ),
                );
            }
            if !read_before {
                dim.attrs.push(column.clone());
            }
            if cast && !cast_before {
                dim.as_text.push(column.clone());
            }
        }
        Some(dim)
    }

    /// `WITH (buckets = '1 hour, 1 day, 7 days')`. The `key` option of
    /// earlier specs is refused by name, since a spec that still has it was
    /// written for a straum that needed it.
    fn options(&mut self, at: u64, options: &sql::CreateTableOptions) -> Option<Vec<Grain>> {
        let mut buckets = None;
        let list = match options {
            sql::CreateTableOptions::With(list) => list.as_slice(),
            sql::CreateTableOptions::None => &[],
            other => {
                self.reject(at, "view options", other);
                &[]
            }
        };
        for option in list {
            let sql::SqlOption::KeyValue { key: name, value } = option else {
                self.reject(line(option), "view option", option);
                continue;
            };
            let at = line(option);
            let Some(name) = self.ident(name) else {
                continue;
            };
            match name.as_str() {
                "buckets" => {}
                "key" => {
                    self.error(
                        at,
                        format!(
                            "view option {option} is gone: straum reads the whole old row \
                             under REPLICA IDENTITY FULL, so drop it"
                        ),
                    );
                    continue;
                }
                _ => {
                    self.reject(at, "view option", option);
                    continue;
                }
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
                    value: sql::Value::SingleQuotedString(s),
                    span: _,
                }),
            ) => {
                let mut buckets = Vec::new();
                for width in s.split(',').map(str::trim) {
                    match grain(width) {
                        Some(g) => buckets.push(g),
                        None => self.error(
                            line(value),
                            format!("bucket width '{width}': use '<n> seconds|minutes|hours|days'"),
                        ),
                    }
                }
                Some(buckets)
            }
            Some(other) => {
                self.reject(line(other), "buckets, use '<n> <unit>, ...',", other);
                Some(Vec::new())
            }
        }
    }

    fn query(&mut self, query: &sql::Query) -> Option<Body> {
        let select = self.one_select(query, "view body")?;
        self.select(select)
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
        let (table, mut scope, conditions) = self.from(from, select.selection.as_ref())?;

        let mut named = Vec::new();
        for item in &select.projection {
            match item {
                sql::SelectItem::ExprWithAlias { expr, alias } => named.push((expr, alias)),
                sql::SelectItem::UnnamedExpr(expr) => self.error(
                    line(item),
                    format!("name every SELECT item: {expr} AS <name>"),
                ),
                other => self.reject(line(other), "SELECT item", other),
            }
        }
        // Keys first, then measures, so a FILTER can reach any key. A
        // position comes from the item's shape, not its walk, so a bad key
        // column is reported once and not again by GROUP BY.
        let mut positions = Vec::new();
        let mut buckets = Vec::new();
        let mut by = Vec::new();
        let mut attrs: Vec<Vec<Attr>> = vec![Vec::new(); scope.refs.len()];
        for (index, &(expr, alias)) in named.iter().enumerate() {
            match expr {
                sql::Expr::Function(function) if is_date_bin(function) => {
                    positions.push(index + 1);
                    buckets.extend(self.bucket(&scope, function, alias));
                }
                sql::Expr::Function(_) => {}
                _ => {
                    positions.push(index + 1);
                    self.key(&mut scope, expr, alias, &mut by, &mut attrs);
                }
            }
        }
        let mut measures = Vec::new();
        for &(expr, alias) in &named {
            if let sql::Expr::Function(function) = expr
                && !is_date_bin(function)
            {
                measures.extend(self.measure(&scope, function, alias));
            }
        }
        self.group_by(at, &select.group_by, &positions);
        let mut buckets = buckets.into_iter();
        let time = buckets.next();
        for _ in buckets {
            self.error(at, "SELECT has more than one date_bin bucket".into());
        }
        self.ref_attrs(at, &mut scope.refs, attrs);
        let mut as_text = Vec::new();
        for key in &scope.keys {
            let (read, column) = match &key.column {
                _ if !key.cast => continue,
                Column::Fact(column) => (&mut as_text, column),
                Column::Attr { through, column } => (&mut scope.refs[*through].as_text, column),
            };
            if !read.contains(column) {
                read.push(column.clone());
            }
        }
        let filter = self.key_conds(&scope, conditions)?;
        // A FILTER every measure shares is the view's WHERE in disguise; a
        // lone measure shares with nobody.
        if let [first, rest @ ..] = &measures[..]
            && !first.filter.is_empty()
            && !rest.is_empty()
            && rest.iter().all(|measure| measure.filter == first.filter)
        {
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
            by,
            measures,
            filter,
            as_text,
        })
    }

    /// A ref reads the dim columns the view selects, through the via
    /// columns that key its cells. A join that selects nothing would do
    /// nothing; a semi or anti join selects nothing by nature.
    fn ref_attrs(&mut self, at: u64, refs: &mut [JoinedRef], attrs: Vec<Vec<Attr>>) {
        for (joined, attrs) in refs.iter_mut().zip(attrs) {
            let reference = &mut joined.reference;
            let selects_none = attrs.is_empty();
            match reference.kind {
                RefKind::Left | RefKind::Inner if selects_none => self.error(
                    at,
                    format!(
                        "{} is {} and selects no column of it; select one or drop the join",
                        reference.name, reference.kind
                    ),
                ),
                RefKind::Semi | RefKind::Anti if !selects_none => self.error(
                    at,
                    format!(
                        "{} is {} and has no columns to select; select none of it",
                        reference.name, reference.kind
                    ),
                ),
                _ => {}
            }
            reference.attrs = attrs;
        }
    }

    /// The view's WHERE and a measure's FILTER alike: `=`, `IN`, `IS NULL`
    /// and `IS NOT NULL` on columns, joined by AND, each reaching the key
    /// that reads the column. `None` once a condition fails.
    fn key_conds<'e>(
        &mut self,
        scope: &Scope,
        conditions: impl IntoIterator<Item = &'e sql::Expr>,
    ) -> Option<Vec<Cond>> {
        let mut conds = Vec::new();
        for condition in conditions {
            let before = self.errors.len();
            self.conds(condition, &mut conds, &mut Names::View(scope));
            if self.errors.len() > before {
                return None;
            }
        }
        Some(conds)
    }

    /// A key item: a fact column, its null-ness, or a dim column through its
    /// ref. Recorded in `scope` too, so a FILTER can reach it.
    fn key(
        &mut self,
        scope: &mut Scope,
        expr: &sql::Expr,
        alias: &sql::Ident,
        by: &mut Vec<By>,
        attrs: &mut [Vec<Attr>],
    ) {
        let name = self.ident(alias);
        let (spelled, read) = match null_test(expr) {
            Some((inner, negated)) => (
                inner,
                (self.fact_column(scope, inner, "the IS NULL column"))
                    .map(|(column, cast)| (Column::Fact(column), Some(!negated), cast)),
            ),
            None => (
                expr,
                (self.column(scope, expr)).map(|(column, cast)| (column, None, cast)),
            ),
        };
        let (Some(name), Some((column, is_null, cast))) = (name, read) else {
            return;
        };
        self.same_spelling(scope, spelled, &column, cast);
        match (&column, is_null) {
            (Column::Fact(fact_column), None) => by.push(By::Column {
                column: fact_column.clone(),
                name: name.clone(),
            }),
            (Column::Fact(fact_column), Some(is_null)) => by.push(By::IsNull {
                column: fact_column.clone(),
                name: name.clone(),
                negated: !is_null,
            }),
            (
                Column::Attr {
                    through,
                    column: attr_column,
                },
                _,
            ) => attrs[*through].push(Attr {
                name: name.clone(),
                column: attr_column.clone(),
            }),
        }
        scope.keys.push(ScopedKey {
            name,
            column,
            is_null,
            cast,
            spelled: spelling(spelled),
        });
    }

    /// A column is one spelling throughout a view: with `::text` or
    /// without, as its type decides.
    fn same_spelling(&mut self, scope: &Scope, expr: &sql::Expr, column: &Column, cast: bool) {
        let other = (scope.keys.iter()).find(|k| k.column == *column && k.cast != cast);
        if let Some(other) = other {
            self.error(
                line(expr),
                format!(
                    "{}: SELECT spells the column {}; write it alike",
                    spelling(expr),
                    other.spelled
                ),
            );
        }
    }

    /// A column that can't be `::text`: the cast goes on a key or a
    /// FILTER column, where the text is what straum keeps.
    fn uncast(&mut self, expr: &sql::Expr, cast: bool, what: &str) -> Option<()> {
        if cast {
            self.error(
                line(expr),
                format!(
                    "{what} {} can't be ::text; it goes on a key or a FILTER column",
                    spelling(expr)
                ),
            );
            return None;
        }
        Some(())
    }

    fn measure(
        &mut self,
        scope: &Scope,
        function: &sql::Function,
        alias: &sql::Ident,
    ) -> Option<Measure> {
        let name = self.ident(alias);
        let (agg, filter) = self.aggregate(scope, function)?;
        Some(Measure {
            name: name?,
            agg,
            filter,
        })
    }

    /// `EXISTS (SELECT 1 FROM schema.dim AS alias WHERE alias.key =
    /// near.column AND ...)`, or `NOT EXISTS`: a semi or anti join, which
    /// keeps or drops the fact row and reads nothing of the dim.
    fn exists(
        &mut self,
        scope: &Scope,
        expr: &sql::Expr,
        subquery: &sql::Query,
        negated: bool,
    ) -> Option<JoinedRef> {
        let at = line(expr);
        let kind = if negated {
            RefKind::Anti
        } else {
            RefKind::Semi
        };
        let select = self.one_select(subquery, "EXISTS")?;
        self.select_clauses(select);
        let one = matches!(
            select.projection.as_slice(),
            [sql::SelectItem::UnnamedExpr(sql::Expr::Value(sql::ValueWithSpan {
                value: sql::Value::Number(n, false),
                ..
            }))] if n == "1"
        );
        if !one {
            self.reject(at, "EXISTS, write EXISTS (SELECT 1 FROM ...),", expr);
            return None;
        }
        self.absent(
            at,
            !matches!(select.group_by, sql::GroupByExpr::Expressions(ref e, _) if e.is_empty()),
            "GROUP BY inside EXISTS",
        );
        let [from] = select.from.as_slice() else {
            self.reject(at, "EXISTS, write EXISTS (SELECT 1 FROM one table),", expr);
            return None;
        };
        self.absent(at, !from.joins.is_empty(), "a join inside EXISTS");
        let (table, alias) = self.table(&from.relation)?;
        let Some(on) = &select.selection else {
            self.reject(at, "EXISTS without WHERE alias.key = ...,", expr);
            return None;
        };
        self.joined(scope, kind, table, alias, on)
    }

    fn joined(
        &mut self,
        scope: &Scope,
        kind: RefKind,
        table: Table,
        alias: Ident,
        on: &sql::Expr,
    ) -> Option<JoinedRef> {
        let (key, via) = self.equalities(scope, &alias, on)?;
        Some(JoinedRef {
            reference: Ref {
                name: alias,
                dim: table.name.clone(),
                kind,
                via,
                attrs: Vec::new(),
            },
            table,
            key,
            as_text: Vec::new(),
        })
    }

    fn push_ref(&mut self, scope: &mut Scope, at: u64, joined: JoinedRef) {
        let name = &joined.reference.name;
        if *name == scope.alias || (scope.refs.iter()).any(|other| other.reference.name == *name) {
            self.error(at, format!("alias {name} used twice"));
            return;
        }
        scope.refs.push(joined);
    }

    /// The key equalities of a join: `alias.key = near.column` joined by
    /// `AND`, the joined table's column on the left, the other the fact's
    /// or a column of a dim joined before.
    fn equalities(
        &mut self,
        scope: &Scope,
        alias: &Ident,
        on: &sql::Expr,
    ) -> Option<(Vec<Ident>, Vec<Via>)> {
        let mut parts = Vec::new();
        and_parts(on, &mut parts);
        let mut key = Vec::new();
        let mut via = Vec::new();
        for equality in parts {
            let pair = match equality {
                sql::Expr::BinaryOp {
                    left,
                    op: sql::BinaryOperator::Eq,
                    right,
                } => (self.qualified(left).zip(self.qualified(right)))
                    .filter(|(l, _)| l.0 == *alias)
                    .and_then(|(l, r)| Some((l.1, self.near(scope, r)?))),
                _ => None,
            };
            let Some((k, v)) = pair else {
                self.error(
                    line(on),
                    format!(
                        "join ON {alias}.<key> = {}.<column>, the joined table's column on \
                         the left, one or more joined by AND; found {on}",
                        scope.alias
                    ),
                );
                return None;
            };
            key.push(k);
            via.push(v);
        }
        Some((key, via))
    }

    /// The near side of a join equality: a fact column, or a column of a
    /// dim joined before, which then carries it.
    fn near(&mut self, scope: &Scope, (alias, column): (Ident, Ident)) -> Option<Via> {
        if alias == scope.alias {
            return Some(Via::Fact(column));
        }
        let through = (scope.refs.iter()).position(|joined| joined.reference.name == alias)?;
        let joined = &scope.refs[through];
        match joined.reference.kind {
            RefKind::Left | RefKind::Inner => {}
            RefKind::Semi | RefKind::Anti => return None,
        }
        // A key column of that dim is the column it was joined on.
        if let Some(position) = joined.key.iter().position(|key| *key == column)
            && let Via::Fact(_) = &joined.reference.via[position]
        {
            return Some(joined.reference.via[position].clone());
        }
        Some(Via::Attr { through, column })
    }

    /// The fact, its joins, and the semi and anti joins in WHERE; the rest
    /// of WHERE waits for the keys, which it names.
    fn from<'a>(
        &mut self,
        from: &sql::TableWithJoins,
        selection: Option<&'a sql::Expr>,
    ) -> Option<(Table, Scope, Vec<&'a sql::Expr>)> {
        let sql::TableWithJoins { relation, joins } = from;
        let (table, alias) = self.table(relation)?;
        let mut scope = Scope {
            alias,
            refs: Vec::new(),
            keys: Vec::new(),
        };
        for join in joins {
            if let Some(joined) = self.join(&scope, join) {
                self.push_ref(&mut scope, line(join), joined);
            }
        }
        let mut parts = Vec::new();
        if let Some(selection) = selection {
            and_parts(selection, &mut parts);
        }
        let mut conditions = Vec::new();
        for part in parts {
            match part {
                sql::Expr::Exists { subquery, negated } => {
                    if let Some(joined) = self.exists(&scope, part, subquery, *negated) {
                        self.push_ref(&mut scope, line(part), joined);
                    }
                }
                other => conditions.push(other),
            }
        }
        Some((table, scope, conditions))
    }

    /// `schema.table AS alias`, every part written: nothing defaults to
    /// `public` or to the table's name.
    fn table(&mut self, factor: &sql::TableFactor) -> Option<(Table, Ident)> {
        let (name, alias) = self.table_factor(factor)?;
        let at = line(factor);
        let table = self.schema_table(name);
        let alias = match alias {
            Some(sql::TableAlias {
                explicit: true,
                name: alias,
                columns,
                at: at_time,
            }) => {
                self.absent(at, !columns.is_empty(), "an alias column list");
                self.absent(at, at_time.is_some(), "AT");
                Some(self.ident(alias)?)
            }
            Some(sql::TableAlias {
                explicit: false, ..
            })
            | None => {
                self.error(
                    line(name),
                    format!("table {name} must be {name} AS <alias>"),
                );
                None
            }
        };
        Some((table?, alias?))
    }

    /// `LEFT OUTER JOIN schema.dim AS alias ON alias.key = near.column` or
    /// `INNER JOIN ...`, spelled whole, with more such equalities joined by
    /// `AND` for a key of several columns, the dim's column on the left of
    /// each; `near` is the fact or a dim joined before.
    fn join(&mut self, scope: &Scope, join: &sql::Join) -> Option<JoinedRef> {
        let sql::Join {
            relation,
            global,
            join_operator,
        } = join;
        let at = line(join);
        self.absent(at, *global, "GLOBAL");
        let (kind, constraint) = match join_operator {
            sql::JoinOperator::LeftOuter(c) => (RefKind::Left, c),
            sql::JoinOperator::Inner(c) => (RefKind::Inner, c),
            sql::JoinOperator::Left(_) => {
                self.reject(at, "LEFT JOIN, write LEFT OUTER JOIN,", join);
                return None;
            }
            sql::JoinOperator::Join(_) => {
                self.reject(at, "JOIN, write INNER JOIN,", join);
                return None;
            }
            _ => {
                self.reject(at, "join, write INNER JOIN or LEFT OUTER JOIN,", join);
                return None;
            }
        };
        let (table, alias) = self.table(relation)?;
        let on = match constraint {
            sql::JoinConstraint::On(on) => on,
            _ => {
                self.reject(at, "join condition, write ON,", join);
                return None;
            }
        };
        self.joined(scope, kind, table, alias, on)
    }

    /// `alias.column`, or `alias.column::text` to read an enum column as
    /// the label PostgreSQL prints (ADR-0037). `true` when cast.
    fn reference(&mut self, expr: &sql::Expr) -> Option<(Ident, Ident, bool)> {
        let sql::Expr::Cast {
            kind,
            expr: inner,
            data_type,
            format,
        } = expr
        else {
            return self
                .qualified(expr)
                .map(|(alias, column)| (alias, column, false));
        };
        let at = line(expr);
        self.absent(at, format.is_some(), "CAST ... FORMAT");
        match kind {
            sql::CastKind::DoubleColon => {}
            sql::CastKind::Cast => {
                self.reject(at, "CAST, write alias.column::text,", expr);
                return None;
            }
            sql::CastKind::TryCast | sql::CastKind::SafeCast => {
                self.reject(at, "cast", expr);
                return None;
            }
        }
        if !matches!(data_type, sql::DataType::Text) {
            self.reject(at, &format!("::{data_type}, write ::text,"), expr);
            return None;
        }
        let (alias, column) = self.qualified(inner)?;
        Some((alias, column, true))
    }

    /// `alias.column`, the fact's included: no table is anyone's default.
    fn qualified(&mut self, expr: &sql::Expr) -> Option<(Ident, Ident)> {
        match expr {
            sql::Expr::Cast { .. } => {
                self.reject(line(expr), "::text outside a key or a FILTER column,", expr);
                None
            }
            sql::Expr::CompoundIdentifier(ids) => {
                let [alias, column] = ids.as_slice() else {
                    self.reject(line(expr), "column, write alias.column,", expr);
                    return None;
                };
                Some((self.ident(alias)?, self.ident(column)?))
            }
            other => {
                self.reject(line(other), "column, write alias.column,", other);
                None
            }
        }
    }

    fn column(&mut self, scope: &Scope, expr: &sql::Expr) -> Option<(Column, bool)> {
        let (alias, column, cast) = self.reference(expr)?;
        if alias == scope.alias {
            return Some((Column::Fact(column), cast));
        }
        let Some(through) = (scope.refs.iter()).position(|joined| joined.reference.name == alias)
        else {
            self.error(line(expr), format!("unknown table alias {alias}"));
            return None;
        };
        let joined = &scope.refs[through];
        let position = joined.key.iter().position(|key| *key == column);
        let column = match position.map(|position| &joined.reference.via[position]) {
            Some(Via::Fact(via)) => Column::Fact(via.clone()),
            Some(Via::Attr { .. }) | None => Column::Attr { through, column },
        };
        Some((column, cast))
    }

    fn fact_column(
        &mut self,
        scope: &Scope,
        expr: &sql::Expr,
        what: &str,
    ) -> Option<(Ident, bool)> {
        match self.column(scope, expr)? {
            (Column::Fact(column), cast) => Some((column, cast)),
            (Column::Attr { .. }, _) => {
                self.error(
                    line(expr),
                    format!("{what} must be a {} column", scope.alias),
                );
                None
            }
        }
    }

    /// `FILTER (WHERE alias.column = ...)` reaches the key that reads the
    /// column, by the column, as PostgreSQL reads the view: an output name
    /// is no column there.
    pub(super) fn key_of(&mut self, scope: &Scope, expr: &sql::Expr) -> Option<Ident> {
        let (column, cast) = self.column(scope, expr)?;
        let key = (scope.keys.iter()).find(|k| k.column == column && k.is_null.is_none());
        let Some(key) = key else {
            self.error(
                line(expr),
                format!("FILTER on {expr} needs {expr} AS <name> in SELECT"),
            );
            return None;
        };
        self.same_spelling(scope, expr, &column, cast);
        Some(key.name.clone())
    }

    /// `FILTER (WHERE alias.column IS NULL)` counts through the null-ness
    /// key over that column, which holds for the rows the test picks out.
    pub(super) fn null_cond(
        &mut self,
        scope: &Scope,
        test: &sql::Expr,
        column: &sql::Expr,
        is_null: bool,
    ) -> Option<Cond> {
        let expr = column;
        let (column, cast) = self.fact_column(scope, expr, "the IS NULL column")?;
        let column = Column::Fact(column);
        let key = (scope.keys.iter()).find(|k| k.column == column && k.is_null.is_some());
        let Some(key) = key else {
            self.error(
                line(test),
                format!("FILTER (WHERE {test}) needs the key ({expr} IS NULL) AS <name> in SELECT"),
            );
            return None;
        };
        self.same_spelling(scope, expr, &column, cast);
        Some(Cond {
            key: key.name.clone(),
            values: vec![Literal::Bool(
                is_null == key.is_null.expect("a null-ness key"),
            )],
        })
    }

    /// `date_bin('15 minutes', f.column, timestamptz 'epoch')`, or
    /// `timestamp 'epoch'` for a column without a zone. The origin is fixed
    /// so buckets start on midnight, as straum's do; which type it has, the
    /// catalog checks against the column.
    fn bucket(
        &mut self,
        scope: &Scope,
        function: &sql::Function,
        alias: &sql::Ident,
    ) -> Option<Time> {
        // Walked for its errors: the name is fixed.
        self.ident(alias);
        if alias.value != "bucket" {
            self.error(
                alias.span.start.line,
                format!("the date_bin bucket is named AS bucket, not AS {alias}"),
            );
        }
        let at = line(function);
        self.absent(at, function.filter.is_some(), "FILTER on date_bin");
        let (args, distinct) = self.call(function)?;
        self.absent(at, distinct, "DISTINCT on date_bin");
        let [
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(width)),
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(column_expr)),
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(origin)),
        ] = args
        else {
            self.reject(at, "date_bin arguments", function);
            return None;
        };
        let grain = match width {
            sql::Expr::Value(sql::ValueWithSpan {
                value: sql::Value::SingleQuotedString(s),
                span: _,
            }) => grain(s),
            _ => None,
        };
        if grain.is_none() {
            self.error(
                line(width),
                format!("date_bin width {width}: use '<n> seconds|minutes|hours|days'"),
            );
        }
        let epoch = match origin {
            sql::Expr::TypedString(sql::TypedString {
                data_type: sql::DataType::Timestamp(None, zone),
                value:
                    sql::ValueWithSpan {
                        value: sql::Value::SingleQuotedString(s),
                        span: _,
                    },
                uses_odbc_syntax: false,
            }) if s == "epoch" => match zone {
                sql::TimezoneInfo::Tz => Some(Origin::Timestamptz),
                sql::TimezoneInfo::None => Some(Origin::Timestamp),
                sql::TimezoneInfo::WithTimeZone | sql::TimezoneInfo::WithoutTimeZone => None,
            },
            _ => None,
        };
        if epoch.is_none() {
            self.error(
                line(origin),
                format!("date_bin origin {origin}: use timestamptz 'epoch' or timestamp 'epoch'"),
            );
        }
        let (column, cast) = self.fact_column(scope, column_expr, "the date_bin column")?;
        self.uncast(column_expr, cast, "the date_bin column")?;
        Some(Time {
            column,
            grain: grain?,
            buckets: Vec::new(),
            origin: epoch?,
        })
    }

    fn aggregate(&mut self, scope: &Scope, function: &sql::Function) -> Option<(Agg, Vec<Cond>)> {
        let at = line(function);
        let name = function_name(function).map(str::to_ascii_lowercase);
        let (args, distinct) = self.call(function)?;
        let star = matches!(
            args,
            [sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Wildcard)]
        );
        let agg: fn(Ident) -> Agg = match (name.as_deref(), distinct) {
            (Some("count"), false) if star => {
                let filter = self.key_conds(scope, function.filter.as_deref())?;
                return Some((Agg::Count, filter));
            }
            (Some("count"), false) => {
                self.reject(
                    at,
                    "count, use count(*) or count(DISTINCT column),",
                    function,
                );
                return None;
            }
            (Some("count"), true) => Agg::CountDistinct,
            (Some("sum"), false) => Agg::Sum,
            (Some("min"), false) => Agg::Min,
            (Some("max"), false) => Agg::Max,
            (Some("avg"), false) => Agg::Avg,
            (Some("sum" | "min" | "max" | "avg"), true) => {
                self.reject(at, "DISTINCT outside count", function);
                return None;
            }
            _ => {
                self.reject(at, "aggregate", function);
                return None;
            }
        };
        let arg = self.single(function, args)?;
        let (column, cast) = self.fact_column(scope, arg, "an aggregated column")?;
        self.uncast(arg, cast, "an aggregated column")?;
        Some((
            agg(column),
            self.key_conds(scope, function.filter.as_deref())?,
        ))
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
                    value: sql::Value::Number(n, false),
                    span: _,
                }) => n.parse::<usize>().ok(),
                _ => None,
            };
            let Some(position) = position else {
                self.reject(
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

/// `column IS NULL` or `column IS NOT NULL`, in any parentheses, and
/// whether it is the `NOT` one.
fn null_test(expr: &sql::Expr) -> Option<(&sql::Expr, bool)> {
    match expr {
        sql::Expr::Nested(inner) => null_test(inner),
        sql::Expr::IsNull(column) => Some((column, false)),
        sql::Expr::IsNotNull(column) => Some((column, true)),
        _ => None,
    }
}

fn grain(s: &str) -> Option<Grain> {
    let [count, unit] = s.split_whitespace().collect::<Vec<_>>()[..] else {
        return None;
    };
    let scale = match unit.strip_suffix('s').unwrap_or(unit) {
        "second" => 1,
        "minute" => 60,
        "hour" => 3_600,
        "day" => 86_400,
        _ => return None,
    };
    count
        .parse::<u32>()
        .ok()?
        .checked_mul(scale)
        .map(|seconds| Grain { seconds })
}

/// A column reference as straum SQL spells it: `sqlparser` prints the cast
/// as `::TEXT`.
fn spelling(expr: &sql::Expr) -> String {
    match expr {
        sql::Expr::Cast { expr: inner, .. } => format!("{inner}::text"),
        other => other.to_string(),
    }
}

fn is_date_bin(function: &sql::Function) -> bool {
    function_name(function).is_some_and(|name| name.eq_ignore_ascii_case("date_bin"))
}

fn comma_list(items: &[impl fmt::Display]) -> String {
    (items.iter().map(ToString::to_string))
        .collect::<Vec<_>>()
        .join(", ")
}
