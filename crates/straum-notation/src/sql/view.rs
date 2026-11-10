use sqlparser::ast as sql;
use straum_domain::{
    Agg, By, Cond, Dim, Fact, Grain, Ident, KeyPath, Literal, Measure, Origin, Ref, Table, Time,
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
}

/// The tables one view reads: the fact under `alias`, and one ref per join;
/// and its null-ness keys, which a FILTER's `IS NULL` goes through.
struct Scope {
    alias: Ident,
    refs: Vec<JoinedRef>,
    nulls: Vec<NullKey>,
}

/// `(column IS NULL) AS name`, or `IS NOT NULL` when `negated`.
#[derive(Clone)]
struct NullKey {
    column: Ident,
    name: Ident,
    negated: bool,
}

/// A join: `r.via[i]` of the fact meets `key[i]` of the dim.
struct JoinedRef {
    r: Ref,
    table: Table,
    key: Vec<Ident>,
}

/// What a column reference names. A dim's key column names the fact
/// column joined to it, since that is where its value is.
enum Column {
    Fact(Ident),
    Attr(usize, Ident),
}

/// A `SELECT` item, as the rollup sees it.
enum Item {
    Bucket(Time),
    Column(Ident),
    IsNull(NullKey),
    /// A dim attribute, by its ref.
    Attr(usize, Ident),
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
        let refs = refs
            .into_iter()
            .map(|mut j| {
                self.add_dim(at, &mut j);
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
        });
    }

    /// A dim is its table. Every view that joins it must join on the same
    /// key columns, in any order, and its attributes are every column any
    /// view reads. The join's via columns are put in the dim's key order,
    /// so a ref's via and its dim's key pair up by position.
    fn add_dim(&mut self, at: u64, j: &mut JoinedRef) {
        let Some(dim) = self.dims.iter_mut().find(|d| d.table == j.table) else {
            self.dims.push(Dim {
                name: j.r.dim.clone(),
                table: j.table.clone(),
                key: j.key.clone(),
                attrs: j.r.attrs.clone(),
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
        let via: Vec<Ident> = (dim.key.iter())
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
            if !dim.attrs.contains(attr) {
                dim.attrs.push(attr.clone());
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
        self.absent(
            at,
            select.selection.is_some(),
            "WHERE, filter measures instead,",
        );

        let [from] = from.as_slice() else {
            self.error(
                at,
                "SELECT needs exactly one FROM table; join with LEFT JOIN".into(),
            );
            return None;
        };
        let (table, mut scope) = self.from(from)?;

        let mut selected = Vec::new();
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
            selected.push((is_key, expr, alias));
        }
        // Keys first, then measures, so a FILTER can name a null-ness key
        // selected after it.
        selected.sort_by_key(|&(is_key, ..)| !is_key);
        let mut items = Vec::new();
        let mut keys = Vec::new();
        for (is_key, expr, alias) in selected {
            if is_key {
                keys.push(key_text(expr));
            }
            if let Some(it) = self.item(&scope, expr, alias) {
                if let Item::IsNull(key) = &it {
                    scope.nulls.push(key.clone());
                }
                items.push(it);
            }
        }
        self.group_by(at, group_by, projection, &keys);

        let mut time = None;
        let mut by: Vec<By> = Vec::new();
        let mut measures = Vec::new();
        let mut attrs: Vec<Vec<Ident>> = vec![Vec::new(); scope.refs.len()];
        for it in items {
            match it {
                Item::Bucket(t) => {
                    if time.replace(t).is_some() {
                        self.error(at, "SELECT has more than one date_bin bucket".into());
                    }
                }
                Item::Column(c) => by.push(By::Column(c)),
                Item::IsNull(NullKey {
                    column,
                    name,
                    negated,
                }) => by.push(By::IsNull {
                    column,
                    name,
                    negated,
                }),
                Item::Attr(i, attr) => {
                    if !attrs[i].contains(&attr) {
                        attrs[i].push(attr);
                    }
                }
                Item::Measure(m) => measures.push(m),
            }
        }
        // A ref reads the dim columns the view selects: they come with each
        // fact row and key its cells. A join that selects nothing would do
        // nothing.
        for (j, attrs) in scope.refs.iter_mut().zip(attrs) {
            if attrs.is_empty() {
                self.error(
                    at,
                    format!(
                        "LEFT JOIN {} selects no column of it; select one or drop the join",
                        j.r.name
                    ),
                );
            }
            j.r.attrs = attrs;
        }
        // A FILTER names parts of the rollup key only, so an attribute it
        // tests must be selected too.
        for cond in measures.iter().flat_map(|m| &m.filter) {
            let Some(attr) = &cond.path.attr else {
                continue;
            };
            let selected =
                (scope.refs.iter()).any(|j| j.r.name == cond.path.head && j.r.attrs.contains(attr));
            if !selected {
                self.error(
                    at,
                    format!("FILTER on {} needs {} in SELECT", cond.path, cond.path),
                );
            }
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
        })
    }

    fn from(&mut self, from: &sql::TableWithJoins) -> Option<(Table, Scope)> {
        let sql::TableWithJoins { relation, joins } = from;
        let (table, alias) = self.table(relation)?;
        let mut scope = Scope {
            alias,
            refs: Vec::new(),
            nulls: Vec::new(),
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

    /// `schema.table alias`. The schema defaults to `public` and the alias
    /// to the table's name, as PostgreSQL resolves them.
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
        let parts = self.table_name(name);
        let alias = match alias {
            Some(sql::TableAlias {
                explicit: _,
                name: alias,
                columns,
                at: at_time,
            }) => {
                self.absent(at, !columns.is_empty(), "an alias column list");
                self.absent(at, at_time.is_some(), "AT");
                Some(self.ident(alias)?)
            }
            None => None,
        };
        let (schema, name) = parts?;
        let alias = alias.unwrap_or_else(|| name.clone());
        Some((Table { schema, name }, alias))
    }

    /// `schema.table` or `table`, which is `public.table`.
    fn table_name(&mut self, name: &sql::ObjectName) -> Option<(Ident, Ident)> {
        let sql::ObjectName(list) = name;
        if list.len() > 2 {
            self.error(line(name), format!("table {name} must be schema.table"));
            return None;
        }
        let mut parts = self.name(name, list.len(), "table")?;
        let name = parts.pop()?;
        let schema = match parts.pop() {
            Some(schema) => schema,
            None => Ident::new("public")?,
        };
        Some((schema, name))
    }

    /// `LEFT JOIN schema.dim alias ON alias.key = fact.column`, with more
    /// such equalities joined by `AND` for a key of several columns. LEFT,
    /// so a fact row with no dim row keeps null attributes, as in straum.
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
        let mut equalities = Vec::new();
        and_parts(on, &mut equalities);
        let mut key = Vec::new();
        let mut via = Vec::new();
        for equality in equalities {
            let pair = match equality {
                sql::Expr::BinaryOp {
                    left,
                    op: sql::BinaryOperator::Eq,
                    right,
                } => self
                    .qualified(&scope.alias, left)
                    .zip(self.qualified(&scope.alias, right))
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
            let Some((k, v)) = pair else {
                self.error(
                    line(on),
                    format!(
                        "join on {alias}.<key> = {}.<column>, one or more joined by AND; found {on}",
                        scope.alias
                    ),
                );
                return None;
            };
            key.push(k);
            via.push(v);
        }
        let dim = table.name.clone();
        Some(JoinedRef {
            r: Ref {
                name: alias,
                dim,
                via,
                attrs: Vec::new(),
            },
            table,
            key,
        })
    }

    /// `alias.column`, or `column` of the fact, since every other table is
    /// reached through its alias.
    fn qualified(&mut self, fact: &Ident, expr: &sql::Expr) -> Option<(Ident, Ident)> {
        match expr {
            sql::Expr::CompoundIdentifier(ids) => {
                let [alias, column] = ids.as_slice() else {
                    self.reject(line(expr), "column, use alias.column,", expr);
                    return None;
                };
                Some((self.ident(alias)?, self.ident(column)?))
            }
            sql::Expr::Identifier(id) => Some((fact.clone(), self.ident(id)?)),
            other => {
                self.reject(line(other), "column", other);
                None
            }
        }
    }

    fn column(&mut self, scope: &Scope, expr: &sql::Expr) -> Option<Column> {
        let (alias, column) = self.qualified(&scope.alias, expr)?;
        if alias == scope.alias {
            return Some(Column::Fact(column));
        }
        let Some(i) = scope.refs.iter().position(|j| j.r.name == alias) else {
            self.error(line(expr), format!("unknown table alias {alias}"));
            return None;
        };
        let j = &scope.refs[i];
        Some(match j.key.iter().position(|k| *k == column) {
            Some(k) => Column::Fact(j.r.via[k].clone()),
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
        if let Some((inner, negated)) = null_test(expr) {
            let Some(alias) = alias else {
                self.error(line(expr), format!("name the key: {expr} AS <name>"));
                return None;
            };
            let name = self.ident(alias);
            let column = self.fact_column(scope, inner, "the IS NULL column")?;
            return Some(Item::IsNull(NullKey {
                column,
                name: name?,
                negated,
            }));
        }
        if let Some(alias) = alias {
            self.reject(alias.span.start.line, "alias on a key column", alias);
            return None;
        }
        Some(match self.column(scope, expr)? {
            Column::Fact(c) => Item::Column(c),
            Column::Attr(i, attr) => Item::Attr(i, attr),
        })
    }

    /// `FILTER (WHERE f.column IS NULL)` counts through the null-ness key
    /// over that column, which holds for the rows the test picks out. The
    /// view body stays what PostgreSQL runs, since the key's name is no
    /// column there.
    fn null_cond(
        &mut self,
        scope: &Scope,
        test: &sql::Expr,
        column: &sql::Expr,
        is_null: bool,
    ) -> Option<Cond> {
        let column = self.fact_column(scope, column, "the IS NULL column")?;
        let Some(key) = scope.nulls.iter().find(|k| k.column == column) else {
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
            path: KeyPath {
                head: key.name.clone(),
                attr: None,
            },
            values: vec![Literal::Bool(is_null != key.negated)],
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

    /// `FILTER (WHERE ...)`: `=` and `IN` joined by AND.
    fn filter(&mut self, scope: &Scope, f: &sql::Function) -> Option<Vec<Cond>> {
        let mut conds = Vec::new();
        if let Some(expr) = &f.filter {
            let before = self.errors.len();
            self.conds(
                expr,
                &mut conds,
                &mut |w, e| w.path(scope, e),
                &mut |w, e| w.literal(e),
                &mut |w, test, column, is_null| w.null_cond(scope, test, column, is_null),
            );
            if self.errors.len() > before {
                return None;
            }
        }
        Some(conds)
    }

    fn path(&mut self, scope: &Scope, expr: &sql::Expr) -> Option<KeyPath> {
        Some(match self.column(scope, expr)? {
            Column::Fact(head) => KeyPath { head, attr: None },
            Column::Attr(i, attr) => KeyPath {
                head: scope.refs[i].r.name.clone(),
                attr: Some(attr),
            },
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
        let Some(exprs) = self.group_by_exprs(at, group_by) else {
            return;
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
                | sql::Expr::Function(_)
                | sql::Expr::Nested(_)
                | sql::Expr::IsNull(_)
                | sql::Expr::IsNotNull(_) => None,
                other => {
                    self.reject(line(other), "GROUP BY item", other);
                    return;
                }
            };
            let text = match position {
                None => key_text(expr),
                Some(n) => match n.and_then(|n| projection.get(n.wrapping_sub(1))) {
                    Some(sql::SelectItem::UnnamedExpr(e))
                    | Some(sql::SelectItem::ExprWithAlias { expr: e, alias: _ }) => key_text(e),
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

/// A key as GROUP BY matches it: parentheses don't count, so `(f.c IS
/// NULL) AS k` is grouped by `f.c IS NULL` or `(f.c IS NULL)` alike.
fn key_text(expr: &sql::Expr) -> String {
    match expr {
        sql::Expr::Nested(inner) => key_text(inner),
        other => other.to_string(),
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
