use sqlparser::ast as sql;
use straum_domain::{
    Agg, Cond, Dim, Fact, Grain, Ident, KeyPath, Literal, Measure, Ref, Table, Time,
};

use super::{SqlError, Walker, function_name, line};

/// What a view's `WITH` declares.
struct ViewOptions {
    key: Option<Ident>,
    buckets: Vec<Grain>,
}

/// What a view's `SELECT` declares.
struct Body {
    table: Table,
    time: Time,
    refs: Vec<JoinedRef>,
    by: Vec<Ident>,
    measures: Vec<Measure>,
}

/// The tables one view reads: the fact under `alias`, and one ref per join.
struct Scope {
    alias: Ident,
    refs: Vec<JoinedRef>,
}

struct JoinedRef {
    r: Ref,
    table: Table,
    key: Ident,
}

/// What a column reference names.
enum Column {
    Fact(Ident),
    /// A ref's id: its via column, or the dim key it joins on.
    Ref(usize),
    Attr(usize, Ident),
}

/// A `SELECT` item, as the rollup sees it.
enum Item {
    Bucket(Time),
    Column(Ident),
    Ref(usize),
    Attr,
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
        self.attrs.clear();
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
        let ViewOptions { key, buckets } = self.options(at, name, options);
        let body = self.query(query);
        let (Some(name), Some(key), Some(body)) = (fact_name, key, body) else {
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
        } = body;
        time.buckets = buckets;
        let refs = refs
            .into_iter()
            .map(|j| {
                self.add_dim(at, &j);
                j.r
            })
            .collect();
        self.facts.push(Fact {
            name,
            table,
            key,
            time,
            refs,
            by,
            measures,
        });
    }

    /// A dim is its table. Every view that joins it must join on the same
    /// key, and its attributes are every column any view reads.
    fn add_dim(&mut self, at: u64, j: &JoinedRef) {
        let attrs = self
            .attrs
            .iter()
            .filter(|(table, _)| *table == j.table)
            .map(|(_, attr)| attr.clone());
        let Some(dim) = self.dims.iter_mut().find(|d| d.table == j.table) else {
            let mut dim = Dim {
                name: j.r.dim.clone(),
                table: j.table.clone(),
                key: j.key.clone(),
                attrs: Vec::new(),
            };
            for attr in attrs {
                if !dim.attrs.contains(&attr) {
                    dim.attrs.push(attr);
                }
            }
            self.dims.push(dim);
            return;
        };
        if dim.key != j.key {
            let msg = format!(
                "{}.{} is joined on {} here and on {} elsewhere",
                j.table.schema, j.table.name, j.key, dim.key
            );
            self.errors.push(SqlError { line: at, msg });
            return;
        }
        for attr in attrs {
            if !dim.attrs.contains(&attr) {
                dim.attrs.push(attr);
            }
        }
    }

    /// `WITH (key = column, buckets = '1 day, 7 days')`.
    fn options(
        &mut self,
        at: u64,
        view: &sql::ObjectName,
        options: &sql::CreateTableOptions,
    ) -> ViewOptions {
        let mut key = None;
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
            let slot = match name.as_str() {
                "key" => &mut key,
                "buckets" => &mut buckets,
                _ => {
                    self.reject(at, "view option", option);
                    continue;
                }
            };
            if slot.is_some() {
                self.error(at, format!("view option {name} given twice"));
                continue;
            }
            *slot = Some(value);
        }
        let key = match key {
            Some(sql::Expr::Identifier(id)) => self.ident(id),
            Some(other) => {
                self.reject(line(other), "key", other);
                None
            }
            None => {
                self.error(at, format!("view {view} needs WITH (key = <column>)"));
                None
            }
        };
        let buckets = match buckets {
            None => Vec::new(),
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
                buckets
            }
            Some(other) => {
                self.reject(line(other), "buckets, use '<n> <unit>, ...',", other);
                Vec::new()
            }
        };
        ViewOptions { key, buckets }
    }

    fn query(&mut self, query: &sql::Query) -> Option<Body> {
        let sql::Query {
            with,
            body,
            order_by,
            limit_clause,
            fetch,
            locks,
            for_clause,
            settings,
            format_clause,
            pipe_operators,
        } = query;
        let at = line(query);
        self.absent(at, with.is_some(), "WITH (a CTE)");
        self.absent(at, order_by.is_some(), "ORDER BY");
        self.absent(at, limit_clause.is_some(), "LIMIT or OFFSET");
        self.absent(at, fetch.is_some(), "FETCH");
        self.absent(at, !locks.is_empty(), "FOR UPDATE or FOR SHARE");
        self.absent(at, for_clause.is_some(), "FOR");
        self.absent(at, settings.is_some(), "SETTINGS");
        self.absent(at, format_clause.is_some(), "FORMAT");
        self.absent(at, !pipe_operators.is_empty(), "a pipe operator");
        match body.as_ref() {
            sql::SetExpr::Select(select) => self.select(select),
            other => {
                self.reject(at, "view body, use one SELECT,", other);
                None
            }
        }
    }

    fn select(&mut self, select: &sql::Select) -> Option<Body> {
        let sql::Select {
            select_token,
            optimizer_hints,
            distinct,
            select_modifiers,
            top,
            // Only says where TOP was written, which is rejected.
            top_before_distinct: _,
            projection,
            exclude,
            into,
            from,
            lateral_views,
            prewhere,
            selection,
            connect_by,
            group_by,
            cluster_by,
            distribute_by,
            sort_by,
            having,
            named_window,
            qualify,
            // Only says where QUALIFY was written, which is rejected.
            window_before_qualify: _,
            value_table_mode,
            flavor,
        } = select;
        let at = select_token.0.span.start.line;
        self.absent(at, !optimizer_hints.is_empty(), "an optimizer hint");
        self.absent(at, distinct.is_some(), "SELECT DISTINCT");
        self.absent(at, select_modifiers.is_some(), "a SELECT modifier");
        self.absent(at, top.is_some(), "TOP");
        self.absent(at, exclude.is_some(), "EXCLUDE");
        self.absent(at, into.is_some(), "SELECT INTO");
        self.absent(at, !lateral_views.is_empty(), "LATERAL VIEW");
        self.absent(at, prewhere.is_some(), "PREWHERE");
        self.absent(at, selection.is_some(), "WHERE, filter measures instead,");
        self.absent(at, !connect_by.is_empty(), "CONNECT BY");
        self.absent(at, !cluster_by.is_empty(), "CLUSTER BY");
        self.absent(at, !distribute_by.is_empty(), "DISTRIBUTE BY");
        self.absent(at, !sort_by.is_empty(), "SORT BY");
        self.absent(at, having.is_some(), "HAVING");
        self.absent(at, !named_window.is_empty(), "WINDOW");
        self.absent(at, qualify.is_some(), "QUALIFY");
        self.absent(at, value_table_mode.is_some(), "SELECT AS VALUE or STRUCT");
        match flavor {
            sql::SelectFlavor::Standard => {}
            sql::SelectFlavor::FromFirst | sql::SelectFlavor::FromFirstNoSelect => {
                self.absent(at, true, "FROM before SELECT");
            }
        }

        let [from] = from.as_slice() else {
            self.error(
                at,
                "SELECT needs exactly one FROM table; join with LEFT JOIN".into(),
            );
            return None;
        };
        let (table, scope) = self.from(from)?;

        let mut items = Vec::new();
        let mut keys = Vec::new();
        for item in projection {
            let (expr, alias) = match item {
                sql::SelectItem::UnnamedExpr(expr) => (expr, None),
                sql::SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias)),
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
            if is_key {
                keys.push(expr.to_string());
            }
            if let Some(it) = self.item(&scope, expr, alias) {
                items.push(it);
            }
        }
        self.group_by(at, group_by, projection, &keys);

        let mut time = None;
        let mut by = Vec::new();
        let mut measures = Vec::new();
        let mut selected = vec![false; scope.refs.len()];
        for it in items {
            match it {
                Item::Bucket(t) => {
                    if time.replace(t).is_some() {
                        self.error(at, "SELECT has more than one date_bin bucket".into());
                    }
                }
                Item::Column(c) => by.push(c),
                Item::Ref(i) => selected[i] = true,
                Item::Attr => {}
                Item::Measure(m) => measures.push(m),
            }
        }
        for (j, selected) in scope.refs.iter().zip(selected) {
            if !selected {
                self.error(
                    at,
                    format!(
                        "select {}.{} so the view groups by ref {}",
                        scope.alias, j.r.via, j.r.name
                    ),
                );
            }
        }
        let Some(time) = time else {
            self.error(
                at,
                "SELECT needs a bucket: date_bin('<n> <unit>', <column>, timestamptz 'epoch')"
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
        })
    }

    fn from(&mut self, from: &sql::TableWithJoins) -> Option<(Table, Scope)> {
        let sql::TableWithJoins { relation, joins } = from;
        let (table, alias) = self.table(relation)?;
        let mut scope = Scope {
            alias,
            refs: Vec::new(),
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

    /// `schema.table alias`.
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
        let Some(sql::TableAlias {
            explicit: _,
            name: alias,
            columns,
            at: at_time,
        }) = alias
        else {
            self.error(at, format!("table {name} needs an alias"));
            return None;
        };
        self.absent(at, !columns.is_empty(), "an alias column list");
        self.absent(at, at_time.is_some(), "AT");
        let alias = self.ident(alias)?;
        let mut parts = parts?;
        let name = parts.pop()?;
        let schema = parts.pop()?;
        Some((Table { schema, name }, alias))
    }

    /// `LEFT JOIN schema.dim alias ON alias.key = fact.column`. LEFT, so a
    /// fact row with no dim row keeps null attributes, as in straum.
    fn join(&mut self, scope: &Scope, join: &sql::Join) -> Option<JoinedRef> {
        let sql::Join {
            relation,
            global,
            join_operator,
        } = join;
        let at = line(join);
        self.absent(at, *global, "GLOBAL");
        let constraint = match join_operator {
            sql::JoinOperator::Left(c) | sql::JoinOperator::LeftOuter(c) => c,
            _ => {
                self.reject(at, "join, use LEFT JOIN,", join);
                return None;
            }
        };
        let (table, alias) = self.table(relation)?;
        let on = match constraint {
            sql::JoinConstraint::On(on) => on,
            _ => {
                self.reject(at, "join condition, use ON,", join);
                return None;
            }
        };
        let pair = match on {
            sql::Expr::BinaryOp {
                left,
                op: sql::BinaryOperator::Eq,
                right,
            } => self
                .qualified(left)
                .zip(self.qualified(right))
                .and_then(|(l, r)| {
                    if l.0 == alias && r.0 == scope.alias {
                        Some((l.1, r.1))
                    } else if r.0 == alias && l.0 == scope.alias {
                        Some((r.1, l.1))
                    } else {
                        None
                    }
                }),
            _ => None,
        };
        let Some((key, via)) = pair else {
            self.error(
                line(on),
                format!(
                    "join on {alias}.<key> = {}.<column>, found {on}",
                    scope.alias
                ),
            );
            return None;
        };
        let dim = table.name.clone();
        Some(JoinedRef {
            r: Ref {
                name: alias,
                dim,
                via,
            },
            table,
            key,
        })
    }

    /// `alias.column`.
    fn qualified(&mut self, expr: &sql::Expr) -> Option<(Ident, Ident)> {
        match expr {
            sql::Expr::CompoundIdentifier(ids) => {
                let [alias, column] = ids.as_slice() else {
                    self.reject(line(expr), "column, use alias.column,", expr);
                    return None;
                };
                Some((self.ident(alias)?, self.ident(column)?))
            }
            sql::Expr::Identifier(id) => {
                self.error(
                    line(expr),
                    format!("qualify column {id} with its table alias"),
                );
                None
            }
            other => {
                self.reject(line(other), "column", other);
                None
            }
        }
    }

    fn column(&mut self, scope: &Scope, expr: &sql::Expr) -> Option<Column> {
        let (alias, column) = self.qualified(expr)?;
        if alias == scope.alias {
            return Some(match scope.refs.iter().position(|j| j.r.via == column) {
                Some(i) => Column::Ref(i),
                None => Column::Fact(column),
            });
        }
        let Some(i) = scope.refs.iter().position(|j| j.r.name == alias) else {
            self.error(line(expr), format!("unknown table alias {alias}"));
            return None;
        };
        if scope.refs[i].key == column {
            Some(Column::Ref(i))
        } else {
            Some(Column::Attr(i, column))
        }
    }

    fn fact_column(&mut self, scope: &Scope, expr: &sql::Expr, what: &str) -> Option<Ident> {
        match self.column(scope, expr)? {
            Column::Fact(c) => Some(c),
            Column::Ref(i) => Some(scope.refs[i].r.via.clone()),
            Column::Attr(..) => {
                self.error(
                    line(expr),
                    format!("{what} must be a {} column", scope.alias),
                );
                None
            }
        }
    }

    fn item(
        &mut self,
        scope: &Scope,
        expr: &sql::Expr,
        alias: Option<&sql::Ident>,
    ) -> Option<Item> {
        if let sql::Expr::Function(f) = expr {
            if function_name(f).as_deref() == Some("date_bin") {
                return self.bucket(scope, f).map(Item::Bucket);
            }
            let Some(alias) = alias else {
                self.error(line(expr), format!("name the measure: {expr} AS <name>"));
                return None;
            };
            let name = self.ident(alias);
            let (agg, filter) = self.aggregate(scope, f)?;
            return Some(Item::Measure(Measure {
                name: name?,
                agg,
                filter,
            }));
        }
        if let Some(alias) = alias {
            self.reject(alias.span.start.line, "alias on a key column", alias);
            return None;
        }
        Some(match self.column(scope, expr)? {
            Column::Fact(c) => Item::Column(c),
            Column::Ref(i) => Item::Ref(i),
            Column::Attr(i, attr) => {
                self.attr(scope, i, attr);
                Item::Attr
            }
        })
    }

    /// Records a dim attribute the view reads. Attributes live on the dim,
    /// shared by every ref that points at it.
    fn attr(&mut self, scope: &Scope, i: usize, attr: Ident) {
        self.attrs.push((scope.refs[i].table.clone(), attr));
    }

    /// `date_bin('15 minutes', f.column, timestamptz 'epoch')`. The origin is
    /// fixed so buckets start on UTC midnight, as straum's do.
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
        let epoch = matches!(
            origin,
            sql::Expr::TypedString(sql::TypedString {
                data_type: sql::DataType::Timestamp(None, sql::TimezoneInfo::Tz),
                value: sql::ValueWithSpan {
                    value: sql::Value::SingleQuotedString(s),
                    span: _,
                },
                uses_odbc_syntax: false,
            }) if s == "epoch"
        );
        if !epoch {
            self.error(
                line(origin),
                format!("date_bin origin {origin}: use timestamptz 'epoch'"),
            );
        }
        let column = self.fact_column(scope, column, "the date_bin column")?;
        if !epoch {
            return None;
        }
        Some(Time {
            column,
            grain: grain?,
            buckets: Vec::new(),
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

    /// `FILTER (WHERE ...)`: `=` and `IN` joined by AND.
    fn filter(&mut self, scope: &Scope, f: &sql::Function) -> Option<Vec<Cond>> {
        let mut conds = Vec::new();
        if let Some(expr) = &f.filter {
            let before = self.errors.len();
            self.conds(scope, expr, &mut conds);
            if self.errors.len() > before {
                return None;
            }
        }
        Some(conds)
    }

    fn conds(&mut self, scope: &Scope, expr: &sql::Expr, conds: &mut Vec<Cond>) {
        match expr {
            sql::Expr::Nested(inner) => self.conds(scope, inner, conds),
            sql::Expr::BinaryOp {
                left,
                op: sql::BinaryOperator::And,
                right,
            } => {
                self.conds(scope, left, conds);
                self.conds(scope, right, conds);
            }
            sql::Expr::BinaryOp {
                left,
                op: sql::BinaryOperator::Eq,
                right,
            } => {
                let path = self.path(scope, left);
                let value = self.literal(right);
                if let (Some(path), Some(value)) = (path, value) {
                    conds.push(Cond {
                        path,
                        values: vec![value],
                    });
                }
            }
            sql::Expr::InList {
                expr: inner,
                list,
                negated: false,
            } => {
                let path = self.path(scope, inner);
                let values: Vec<Literal> = list.iter().filter_map(|v| self.literal(v)).collect();
                if let Some(path) = path.filter(|_| values.len() == list.len()) {
                    conds.push(Cond { path, values });
                }
            }
            other => self.reject(
                line(other),
                "filter condition, use = or IN joined by AND,",
                other,
            ),
        }
    }

    fn path(&mut self, scope: &Scope, expr: &sql::Expr) -> Option<KeyPath> {
        Some(match self.column(scope, expr)? {
            Column::Fact(head) => KeyPath { head, attr: None },
            Column::Ref(i) => KeyPath {
                head: scope.refs[i].r.name.clone(),
                attr: None,
            },
            Column::Attr(i, attr) => {
                self.attr(scope, i, attr.clone());
                KeyPath {
                    head: scope.refs[i].r.name.clone(),
                    attr: Some(attr),
                }
            }
        })
    }

    /// GROUP BY must name exactly the non-aggregate SELECT items, by
    /// expression or by position, so the view groups like the rollup.
    fn group_by(
        &mut self,
        at: u64,
        group_by: &sql::GroupByExpr,
        projection: &[sql::SelectItem],
        keys: &[String],
    ) {
        let exprs = match group_by {
            sql::GroupByExpr::Expressions(exprs, modifiers) => {
                self.absent(at, !modifiers.is_empty(), "ROLLUP, CUBE or GROUPING SETS");
                exprs
            }
            sql::GroupByExpr::All(_) => {
                self.absent(at, true, "GROUP BY ALL");
                return;
            }
        };
        let mut grouped = Vec::new();
        for expr in exprs {
            let position = match expr {
                sql::Expr::Value(sql::ValueWithSpan {
                    value: sql::Value::Number(n, false),
                    span: _,
                }) => Some(n.parse::<usize>().ok()),
                sql::Expr::Identifier(_)
                | sql::Expr::CompoundIdentifier(_)
                | sql::Expr::Function(_) => None,
                other => {
                    self.reject(line(other), "GROUP BY item", other);
                    return;
                }
            };
            let text = match position {
                None => expr.to_string(),
                Some(n) => match n.and_then(|n| projection.get(n.wrapping_sub(1))) {
                    Some(sql::SelectItem::UnnamedExpr(e))
                    | Some(sql::SelectItem::ExprWithAlias { expr: e, alias: _ }) => e.to_string(),
                    _ => {
                        self.error(
                            line(expr),
                            format!("GROUP BY {expr} is not a SELECT position"),
                        );
                        continue;
                    }
                },
            };
            grouped.push(text);
        }
        for key in keys {
            if !grouped.contains(key) {
                self.error(at, format!("{key} is selected but not in GROUP BY"));
            }
        }
        for g in &grouped {
            if !keys.contains(g) {
                self.error(
                    at,
                    format!("GROUP BY {g} is not a selected key column or bucket"),
                );
            }
        }
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
