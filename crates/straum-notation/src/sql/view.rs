use sqlparser::ast as sql;
use straum_domain::{
    Agg, Attr, By, Cond, Dim, Fact, Grain, Ident, Literal, Measure, Origin, Ref, RefKind, Table,
    Time, Via,
};

use super::{SqlError, Walker, and_parts, function_name, line};

/// What a view's `WITH` declares: every bucket width the view keeps, the
/// `date_bin` width first, or nothing when the `WITH` is missing.
struct ViewOptions {
    buckets: Option<Vec<Grain>>,
}

/// What a view's `SELECT` declares.
struct Body {
    table: Table,
    time: Time,
    refs: Vec<JoinedRef>,
    by: Vec<By>,
    measures: Vec<Measure>,
    filter: Vec<Cond>,
}

/// The tables one view reads: the fact under `alias`, and one ref per join;
/// and its keys so far, which a FILTER reaches through their columns.
struct Scope {
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
}

/// A join: `r.via[i]`, of the fact or of a dim joined before, meets
/// `key[i]` of the dim.
struct JoinedRef {
    r: Ref,
    table: Table,
    key: Vec<Ident>,
}

/// What a column reference names. A dim's key column names the fact
/// column joined to it, since that is where its value is.
#[derive(PartialEq, Eq)]
enum Column {
    Fact(Ident),
    Attr(usize, Ident),
}

/// A `SELECT` item, as the rollup sees it.
enum Item {
    Bucket(Time),
    By(By),
    /// A dim attribute, by its ref.
    Attr(usize, Attr),
    Measure(Measure),
}

impl Walker {
    pub(super) fn view(&mut self, view: &sql::CreateView) {
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

        let fact_name = self.name(name, 1, "view").map(|mut n| n.remove(0));
        let ViewOptions { buckets } = self.options(at, options);
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
        let (Some(name), Some(body)) = (fact_name, body) else {
            return;
        };
        if self.errors.len() > before {
            return;
        }
        let Body {
            table,
            mut time,
            refs,
            by,
            measures,
            filter,
        } = body;
        // Every width is said once, in the view's `WITH`; the body's `date_bin`
        // must agree with the first, so a reader finds the whole list in one
        // place and the body still runs in PostgreSQL.
        match buckets.unwrap_or_default().split_first() {
            Some((first, wider)) if *first == time.grain => time.buckets = wider.to_vec(),
            Some((first, _)) => {
                self.error(
                    at,
                    format!(
                        "view {name}: buckets starts with {first} but date_bin is {}: list \
                         every width, the date_bin width first",
                        time.grain
                    ),
                );
                return;
            }
            None => return,
        }
        // A dim a later join reads its key from carries that column too.
        let mut carried: Vec<(usize, Ident)> = Vec::new();
        for j in &refs {
            for via in &j.r.via {
                if let Via::Attr { through, column } = via {
                    carried.push((*through, column.clone()));
                }
            }
        }
        let refs = refs
            .into_iter()
            .enumerate()
            .map(|(i, mut j)| {
                self.add_dim(at, &mut j);
                let dim = (self.dims.iter_mut())
                    .find(|d| d.table == j.table)
                    .expect("added just now");
                for (through, column) in &carried {
                    if *through == i && !dim.attrs.contains(column) {
                        dim.attrs.push(column.clone());
                    }
                }
                j.r
            })
            .collect();
        self.facts.push(Fact {
            name,
            table,
            time,
            refs,
            by,
            measures,
            filter,
        });
    }

    /// A dim is its table. Every view that joins it must join on the same
    /// key columns, in any order, and its attributes are every column any
    /// view reads, a column another join reads its key from included. The
    /// join's via columns are put in the dim's key order, so a ref's via
    /// and its dim's key pair up by position.
    fn add_dim(&mut self, at: u64, j: &mut JoinedRef) {
        let Some(dim) = self.dims.iter_mut().find(|d| d.table == j.table) else {
            self.dims.push(Dim {
                name: j.r.dim.clone(),
                table: j.table.clone(),
                key: j.key.clone(),
                attrs: j.r.attrs.iter().map(|a| a.column.clone()).collect(),
            });
            return;
        };
        let same = dim.key.len() == j.key.len() && dim.key.iter().all(|k| j.key.contains(k));
        if !same {
            let list = |key: &[Ident]| {
                (key.iter().map(ToString::to_string))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let msg = format!(
                "{}.{} is joined on {} here and on {} elsewhere",
                j.table.schema,
                j.table.name,
                list(&j.key),
                list(&dim.key)
            );
            self.errors.push(SqlError { line: at, msg });
            return;
        }
        let via: Vec<Via> = (dim.key.iter())
            .map(|k| {
                let i = j
                    .key
                    .iter()
                    .position(|c| c == k)
                    .expect("the key sets are equal");
                j.r.via[i].clone()
            })
            .collect();
        j.r.via = via;
        j.key.clone_from(&dim.key);
        for attr in &j.r.attrs {
            if !dim.attrs.contains(&attr.column) {
                dim.attrs.push(attr.column.clone());
            }
        }
    }

    /// `WITH (buckets = '1 hour, 1 day, 7 days')`. The `key` option of
    /// earlier specs is refused by name, since a spec that still has it was
    /// written for a straum that needed it.
    fn options(&mut self, at: u64, options: &sql::CreateTableOptions) -> ViewOptions {
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
        let buckets = match buckets {
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
        };
        ViewOptions { buckets }
    }

    fn query(&mut self, query: &sql::Query) -> Option<Body> {
        let select = self.one_select(query, "view body")?;
        self.select(select)
    }

    fn select(&mut self, select: &sql::Select) -> Option<Body> {
        let at = self.select_clauses(select);
        let (projection, from, group_by) = (&select.projection, &select.from, &select.group_by);

        let [from] = from.as_slice() else {
            self.error(
                at,
                "SELECT needs exactly one FROM table; join with LEFT OUTER JOIN".into(),
            );
            return None;
        };
        let (table, mut scope) = self.from(from)?;
        // Semi and anti joins live in WHERE; the rest of it waits for the
        // keys, which it names.
        let mut conditions = Vec::new();
        if let Some(selection) = &select.selection {
            and_parts(selection, &mut conditions);
        }
        let (subqueries, conditions): (Vec<_>, Vec<_>) =
            (conditions.into_iter()).partition(|e| matches!(e, sql::Expr::Exists { .. }));
        for subquery in subqueries {
            if let Some(j) = self.exists(&scope, subquery) {
                if j.r.name == scope.alias || scope.refs.iter().any(|o| o.r.name == j.r.name) {
                    self.error(line(subquery), format!("alias {} used twice", j.r.name));
                    continue;
                }
                scope.refs.push(j);
            }
        }

        let mut selected = Vec::new();
        for item in projection {
            let (expr, alias) = match item {
                sql::SelectItem::ExprWithAlias { expr, alias } => (expr, alias),
                sql::SelectItem::UnnamedExpr(expr) => {
                    self.error(
                        line(item),
                        format!("name every SELECT item: {expr} AS <name>"),
                    );
                    continue;
                }
                other => {
                    self.reject(line(other), "SELECT item", other);
                    continue;
                }
            };
            // Decided before walking the item, so a bad key column is
            // reported once and not again by GROUP BY.
            let is_key = match expr {
                sql::Expr::Function(f) => function_name(f).as_deref() == Some("date_bin"),
                _ => true,
            };
            selected.push((is_key, expr, alias));
        }
        // The keys' positions in SELECT, which GROUP BY must list.
        let positions: Vec<usize> = (selected.iter().enumerate())
            .filter(|(_, (is_key, ..))| *is_key)
            .map(|(i, _)| i + 1)
            .collect();
        // Keys first, then measures, so a FILTER can reach any key.
        selected.sort_by_key(|&(is_key, ..)| !is_key);
        let mut items = Vec::new();
        for (_, expr, alias) in selected {
            if let Some(it) = self.item(&scope, expr, alias) {
                match &it {
                    Item::By(By::Column { column, name }) => scope.keys.push(ScopedKey {
                        name: name.clone(),
                        column: Column::Fact(column.clone()),
                        is_null: None,
                    }),
                    Item::By(By::IsNull {
                        column,
                        name,
                        negated,
                    }) => scope.keys.push(ScopedKey {
                        name: name.clone(),
                        column: Column::Fact(column.clone()),
                        is_null: Some(!negated),
                    }),
                    Item::Attr(i, attr) => scope.keys.push(ScopedKey {
                        name: attr.name.clone(),
                        column: Column::Attr(*i, attr.column.clone()),
                        is_null: None,
                    }),
                    Item::Bucket(_) | Item::Measure(_) => {}
                }
                items.push(it);
            }
        }
        self.group_by(at, group_by, &positions);

        let mut time = None;
        let mut by: Vec<By> = Vec::new();
        let mut measures = Vec::new();
        let mut attrs: Vec<Vec<Attr>> = vec![Vec::new(); scope.refs.len()];
        for it in items {
            match it {
                Item::Bucket(t) => {
                    if time.replace(t).is_some() {
                        self.error(at, "SELECT has more than one date_bin bucket".into());
                    }
                }
                Item::By(b) => by.push(b),
                Item::Attr(i, attr) => attrs[i].push(attr),
                Item::Measure(m) => measures.push(m),
            }
        }
        // A ref reads the dim columns the view selects, through the via
        // columns that key its cells. A join that selects nothing would do
        // nothing; a semi or anti join selects nothing by nature.
        for (j, attrs) in scope.refs.iter_mut().zip(attrs) {
            match j.r.kind {
                RefKind::Left | RefKind::Inner if attrs.is_empty() => self.error(
                    at,
                    format!(
                        "{} {} selects no column of it; select one or drop the join",
                        join_name(j.r.kind),
                        j.r.name
                    ),
                ),
                RefKind::Semi | RefKind::Anti if !attrs.is_empty() => self.error(
                    at,
                    format!(
                        "{} is {} and has no columns to select; select none of it",
                        j.r.name, j.r.kind
                    ),
                ),
                _ => {}
            }
            j.r.attrs = attrs;
        }
        // The view's WHERE, on the keys like a FILTER.
        let mut filter = Vec::new();
        for condition in conditions {
            let before = self.errors.len();
            self.conds(
                condition,
                &mut filter,
                &mut |w, e| w.key_of(&scope, e),
                &mut |w, e| w.literal(e),
                &mut |w, test, column, is_null| w.null_cond(&scope, test, column, is_null),
            );
            if self.errors.len() > before {
                return None;
            }
        }
        // A FILTER every measure shares is the view's WHERE in disguise; a
        // lone measure shares with nobody.
        if let [first, rest @ ..] = &measures[..]
            && !first.filter.is_empty()
            && !rest.is_empty()
            && rest.iter().all(|m| m.filter == first.filter)
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
        })
    }

    /// `EXISTS (SELECT 1 FROM schema.dim AS alias WHERE alias.key =
    /// near.column AND ...)`, or `NOT EXISTS`: a semi or anti join, which
    /// keeps or drops the fact row and reads nothing of the dim.
    fn exists(&mut self, scope: &Scope, expr: &sql::Expr) -> Option<JoinedRef> {
        let sql::Expr::Exists { subquery, negated } = expr else {
            unreachable!("only EXISTS comes here");
        };
        let at = line(expr);
        let kind = if *negated {
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
        let (key, via) = self.equalities(scope, &alias, on)?;
        Some(JoinedRef {
            r: Ref {
                name: alias,
                dim: table.name.clone(),
                kind,
                via,
                attrs: Vec::new(),
            },
            table,
            key,
        })
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
        let through = scope.refs.iter().position(|j| j.r.name == alias)?;
        let j = &scope.refs[through];
        match j.r.kind {
            RefKind::Left | RefKind::Inner => {}
            RefKind::Semi | RefKind::Anti => return None,
        }
        // A key column of that dim is the column it was joined on.
        if let Some(k) = j.key.iter().position(|k| *k == column)
            && let Via::Fact(_) = &j.r.via[k]
        {
            return Some(j.r.via[k].clone());
        }
        Some(Via::Attr { through, column })
    }

    fn from(&mut self, from: &sql::TableWithJoins) -> Option<(Table, Scope)> {
        let sql::TableWithJoins { relation, joins } = from;
        let (table, alias) = self.table(relation)?;
        let mut scope = Scope {
            alias,
            refs: Vec::new(),
            keys: Vec::new(),
        };
        for join in joins {
            if let Some(j) = self.join(&scope, join) {
                if j.r.name == scope.alias || scope.refs.iter().any(|o| o.r.name == j.r.name) {
                    self.error(line(join), format!("alias {} used twice", j.r.name));
                    continue;
                }
                scope.refs.push(j);
            }
        }
        Some((table, scope))
    }

    /// `schema.table AS alias`, every part written: nothing defaults to
    /// `public` or to the table's name.
    fn table(&mut self, factor: &sql::TableFactor) -> Option<(Table, Ident)> {
        let sql::TableFactor::Table {
            name,
            alias,
            args,
            with_hints,
            version,
            with_ordinality,
            partitions,
            json_path,
            sample,
            index_hints,
        } = factor
        else {
            self.reject(line(factor), "table", factor);
            return None;
        };
        let at = line(factor);
        self.absent(at, args.is_some(), "a table function");
        self.absent(at, !with_hints.is_empty(), "a table hint");
        self.absent(at, version.is_some(), "a table version");
        self.absent(at, *with_ordinality, "WITH ORDINALITY");
        self.absent(at, !partitions.is_empty(), "PARTITION");
        self.absent(at, json_path.is_some(), "a JSON path");
        self.absent(at, sample.is_some(), "TABLESAMPLE");
        self.absent(at, !index_hints.is_empty(), "an index hint");
        let parts = self.name(name, 2, "table");
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
        let mut parts = parts?;
        let name = parts.pop()?;
        let schema = parts.pop()?;
        Some((Table { schema, name }, alias?))
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
        let (key, via) = self.equalities(scope, &alias, on)?;
        Some(JoinedRef {
            r: Ref {
                name: alias,
                dim: table.name.clone(),
                kind,
                via,
                attrs: Vec::new(),
            },
            table,
            key,
        })
    }

    /// `alias.column`, the fact's included: no table is anyone's default.
    fn qualified(&mut self, expr: &sql::Expr) -> Option<(Ident, Ident)> {
        match expr {
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

    fn column(&mut self, scope: &Scope, expr: &sql::Expr) -> Option<Column> {
        let (alias, column) = self.qualified(expr)?;
        if alias == scope.alias {
            return Some(Column::Fact(column));
        }
        let Some(i) = scope.refs.iter().position(|j| j.r.name == alias) else {
            self.error(line(expr), format!("unknown table alias {alias}"));
            return None;
        };
        let j = &scope.refs[i];
        Some(match j.key.iter().position(|k| *k == column) {
            Some(k) => match &j.r.via[k] {
                Via::Fact(via) => Column::Fact(via.clone()),
                Via::Attr { .. } => Column::Attr(i, column),
            },
            None => Column::Attr(i, column),
        })
    }

    fn fact_column(&mut self, scope: &Scope, expr: &sql::Expr, what: &str) -> Option<Ident> {
        match self.column(scope, expr)? {
            Column::Fact(c) => Some(c),
            Column::Attr(..) => {
                self.error(
                    line(expr),
                    format!("{what} must be a {} column", scope.alias),
                );
                None
            }
        }
    }

    fn item(&mut self, scope: &Scope, expr: &sql::Expr, alias: &sql::Ident) -> Option<Item> {
        let name = self.ident(alias);
        if let sql::Expr::Function(f) = expr {
            if function_name(f).as_deref() == Some("date_bin") {
                if alias.value != "bucket" {
                    self.error(
                        alias.span.start.line,
                        format!("the date_bin bucket is named AS bucket, not AS {alias}"),
                    );
                }
                return self.bucket(scope, f).map(Item::Bucket);
            }
            let (agg, filter) = self.aggregate(scope, f)?;
            return Some(Item::Measure(Measure {
                name: name?,
                agg,
                filter,
            }));
        }
        if let Some((inner, negated)) = null_test(expr) {
            let column = self.fact_column(scope, inner, "the IS NULL column")?;
            return Some(Item::By(By::IsNull {
                column,
                name: name?,
                negated,
            }));
        }
        Some(match self.column(scope, expr)? {
            Column::Fact(column) => Item::By(By::Column {
                column,
                name: name?,
            }),
            Column::Attr(i, column) => Item::Attr(
                i,
                Attr {
                    name: name?,
                    column,
                },
            ),
        })
    }

    /// `FILTER (WHERE alias.column = ...)` reaches the key that reads the
    /// column, by the column, as PostgreSQL reads the view: an output name
    /// is no column there.
    fn key_of(&mut self, scope: &Scope, expr: &sql::Expr) -> Option<Ident> {
        let column = self.column(scope, expr)?;
        let key = (scope.keys.iter()).find(|k| k.column == column && k.is_null.is_none());
        let Some(key) = key else {
            self.error(
                line(expr),
                format!("FILTER on {expr} needs {expr} AS <name> in SELECT"),
            );
            return None;
        };
        Some(key.name.clone())
    }

    /// `FILTER (WHERE alias.column IS NULL)` counts through the null-ness
    /// key over that column, which holds for the rows the test picks out.
    fn null_cond(
        &mut self,
        scope: &Scope,
        test: &sql::Expr,
        column: &sql::Expr,
        is_null: bool,
    ) -> Option<Cond> {
        let column = self.fact_column(scope, column, "the IS NULL column")?;
        let key = (scope.keys.iter())
            .find(|k| k.column == Column::Fact(column.clone()) && k.is_null.is_some());
        let Some(key) = key else {
            self.error(
                line(test),
                format!(
                    "FILTER (WHERE {test}) needs the key ({}.{column} IS NULL) AS <name> in SELECT",
                    scope.alias
                ),
            );
            return None;
        };
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
    fn bucket(&mut self, scope: &Scope, f: &sql::Function) -> Option<Time> {
        let at = line(f);
        self.absent(at, f.filter.is_some(), "FILTER on date_bin");
        let (args, distinct) = self.call(f)?;
        self.absent(at, distinct, "DISTINCT on date_bin");
        let [
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(width)),
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(column)),
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(origin)),
        ] = args
        else {
            self.reject(at, "date_bin arguments", f);
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
        let column = self.fact_column(scope, column, "the date_bin column")?;
        Some(Time {
            column,
            grain: grain?,
            buckets: Vec::new(),
            origin: epoch?,
        })
    }

    fn aggregate(&mut self, scope: &Scope, f: &sql::Function) -> Option<(Agg, Vec<Cond>)> {
        let at = line(f);
        let name = function_name(f);
        let (args, distinct) = self.call(f)?;
        let agg: fn(Ident) -> Agg = match name.as_deref() {
            Some("count") => {
                if !distinct {
                    if !matches!(
                        args,
                        [sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Wildcard)]
                    ) {
                        self.reject(at, "count, use count(*) or count(DISTINCT column),", f);
                        return None;
                    }
                    return Some((Agg::Count, self.filter(scope, f)?));
                }
                Agg::CountDistinct
            }
            Some("sum") => Agg::Sum,
            Some("min") => Agg::Min,
            Some("max") => Agg::Max,
            Some("avg") => Agg::Avg,
            _ => {
                self.reject(at, "aggregate", f);
                return None;
            }
        };
        if distinct && name.as_deref() != Some("count") {
            self.reject(at, "DISTINCT outside count", f);
            return None;
        }
        let arg = self.single(f, args)?;
        let column = self.fact_column(scope, arg, "an aggregated column")?;
        Some((agg(column), self.filter(scope, f)?))
    }

    /// `FILTER (WHERE ...)`: `=`, `IN`, `IS NULL` and `IS NOT NULL` on
    /// columns, joined by AND.
    fn filter(&mut self, scope: &Scope, f: &sql::Function) -> Option<Vec<Cond>> {
        let mut conds = Vec::new();
        if let Some(expr) = &f.filter {
            let before = self.errors.len();
            self.conds(
                expr,
                &mut conds,
                &mut |w, e| w.key_of(scope, e),
                &mut |w, e| w.literal(e),
                &mut |w, test, column, is_null| w.null_cond(scope, test, column, is_null),
            );
            if self.errors.len() > before {
                return None;
            }
        }
        Some(conds)
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
            let list = |positions: &[usize]| {
                (positions.iter().map(ToString::to_string))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            self.error(
                at,
                format!(
                    "GROUP BY {} must list the keys' positions in SELECT: GROUP BY {}",
                    list(&grouped),
                    list(positions)
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

/// `'15 minutes'`: a count and a unit, singular or plural.
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
    count.parse::<u32>().ok()?.checked_mul(scale).map(Grain)
}

/// A join kind as its spelling.
fn join_name(kind: RefKind) -> &'static str {
    match kind {
        RefKind::Left => "LEFT OUTER JOIN",
        RefKind::Inner => "INNER JOIN",
        RefKind::Semi => "EXISTS",
        RefKind::Anti => "NOT EXISTS",
    }
}
