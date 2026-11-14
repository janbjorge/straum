//! Walks the `sqlparser` AST of a Straum SQL spec into dims and facts, and
//! of an ask into an `Ask`.
//!
//! Deny by default: every `match` lists what Straum SQL accepts and rejects the
//! rest by name, and accepted structs are destructured without `..`, so a
//! field added by a `sqlparser` upgrade fails to compile until it is handled
//! here.

use std::fmt;

use sqlparser::ast as sql;
use sqlparser::ast::Spanned;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use straum_domain::{Condition, Identifier, Literal};

mod ask;
mod view;

use ask::Params;
pub use ask::parse_ask;
use view::Scope;
pub(crate) use view::parse;

const QUOTE_LEN: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlError {
    /// 1-based; 0 when `sqlparser` puts the position in `msg`.
    pub line: u64,
    pub msg: String,
}

fn statements(src: &str) -> Result<Vec<sql::Statement>, Vec<SqlError>> {
    Parser::parse_sql(&PostgreSqlDialect {}, src).map_err(|e| {
        vec![SqlError {
            line: 0,
            msg: e.to_string(),
        }]
    })
}

fn line(node: &impl Spanned) -> u64 {
    node.span().start.line
}

const CONDITION: &str = "filter condition, write = or IN joined by AND,";
const PARENTHESES: &str = "parentheses, write the condition without them,";

#[derive(Default)]
struct Walker {
    errors: Vec<SqlError>,
}

/// What a condition's keys and values are read against: a view's scope,
/// whose columns reach the keys that read them, or an ask's parameters,
/// the keys named outright.
enum Names<'n, 'p> {
    View(&'n Scope),
    Ask(&'n mut Params<'p>),
}

impl Walker {
    fn error(&mut self, line: u64, msg: String) {
        self.errors.push(SqlError { line, msg });
    }

    fn finish<T>(self, value: Option<T>) -> Result<T, Vec<SqlError>> {
        match value {
            Some(value) if self.errors.is_empty() => Ok(value),
            _ => Err(self.errors),
        }
    }

    fn refuse(&mut self, line: u64, what: &str, node: &impl fmt::Display) {
        let text = node.to_string();
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let text = match text.char_indices().nth(QUOTE_LEN) {
            Some((end, _)) => format!("{}...", &text[..end]),
            None => text,
        };
        self.not_in_subset(line, &format!("{what} `{text}`"));
    }

    /// Rejects a clause Straum SQL has no use for when it is present.
    fn absent(&mut self, line: u64, present: bool, clause: &str) {
        if present {
            self.not_in_subset(line, clause);
        }
    }

    /// The plain GROUP BY list, the only form a view or an ask uses.
    fn group_by_exprs<'a>(
        &mut self,
        at: u64,
        group_by: &'a sql::GroupByExpr,
    ) -> Option<&'a [sql::Expr]> {
        match group_by {
            sql::GroupByExpr::Expressions(exprs, modifiers) => {
                self.absent(at, !modifiers.is_empty(), "ROLLUP, CUBE or GROUPING SETS");
                Some(exprs)
            }
            sql::GroupByExpr::All(_) => {
                self.not_in_subset(at, "GROUP BY ALL");
                None
            }
        }
    }

    /// Every rejection says Straum SQL is Straum's own subset, so no one
    /// takes it for PostgreSQL's error.
    fn not_in_subset(&mut self, line: u64, what: &str) {
        self.error(
            line,
            format!("{what} is not in Straum SQL, the PostgreSQL subset made only for Straum"),
        );
    }

    /// An unquoted name, folded to lower case as PostgreSQL does.
    fn ident(&mut self, id: &sql::Ident) -> Option<Identifier> {
        let sql::Ident {
            value,
            quote_style,
            span,
        } = id;
        if quote_style.is_some() {
            self.refuse(span.start.line, "quoted identifier", id);
            return None;
        }
        let ident = Identifier::new(&value.to_lowercase());
        if ident.is_none() {
            self.error(span.start.line, format!("bad identifier {value}"));
        }
        ident
    }

    fn bare_name(&mut self, name: &sql::ObjectName, what: &str) -> Option<Identifier> {
        let sql::ObjectName(parts) = name;
        let [part] = parts.as_slice() else {
            self.error(line(name), format!("{what} {name} must be a bare name"));
            return None;
        };
        self.name_part(name, part)
    }

    fn name_part(
        &mut self,
        name: &sql::ObjectName,
        part: &sql::ObjectNamePart,
    ) -> Option<Identifier> {
        match part {
            sql::ObjectNamePart::Identifier(id) => self.ident(id),
            other => {
                self.refuse(line(name), "name part", other);
                None
            }
        }
    }

    /// A table by name, after rejecting every other part of a table
    /// reference by name; what the alias may be is the caller's.
    fn table_factor<'a>(
        &mut self,
        factor: &'a sql::TableFactor,
    ) -> Option<(&'a sql::ObjectName, Option<&'a sql::TableAlias>)> {
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
            self.refuse(line(factor), "table", factor);
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
        Some((name, alias.as_ref()))
    }

    /// The one `SELECT` of a query, after rejecting every clause around it.
    fn one_select<'a>(&mut self, query: &'a sql::Query, what: &str) -> Option<&'a sql::Select> {
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
            sql::SetExpr::Select(select) => Some(select),
            other => {
                self.refuse(at, &format!("{what}, use one SELECT,"), other);
                None
            }
        }
    }

    /// Rejects every `SELECT` clause but the list, FROM, WHERE and GROUP
    /// BY, which each caller reads its own way. Returns its line.
    fn select_clauses(&mut self, select: &sql::Select) -> u64 {
        let sql::Select {
            select_token,
            optimizer_hints,
            distinct,
            select_modifiers,
            top,
            // Only says where TOP was written, which is rejected.
            top_before_distinct: _,
            projection: _,
            exclude,
            into,
            from: _,
            lateral_views,
            prewhere,
            selection: _,
            connect_by,
            group_by: _,
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
        match distinct {
            None => {}
            Some(sql::Distinct::All) => self.not_in_subset(at, "SELECT ALL"),
            Some(sql::Distinct::Distinct | sql::Distinct::On(_)) => {
                self.not_in_subset(at, "SELECT DISTINCT");
            }
        }
        self.absent(at, select_modifiers.is_some(), "a SELECT modifier");
        self.absent(at, top.is_some(), "TOP");
        self.absent(at, exclude.is_some(), "EXCLUDE");
        self.absent(at, into.is_some(), "SELECT INTO");
        self.absent(at, !lateral_views.is_empty(), "LATERAL VIEW");
        self.absent(at, prewhere.is_some(), "PREWHERE");
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
                self.not_in_subset(at, "FROM before SELECT");
            }
        }
        at
    }

    /// `key = value` and `key IN (values)` joined by `AND`, and in a view
    /// `column IS NULL` and `IS NOT NULL`, which count through a null-ness
    /// key; an ask names that key outright, `key = $n`.
    fn conds(&mut self, expr: &sql::Expr, conds: &mut Vec<Condition>, names: &mut Names<'_, '_>) {
        let mut parts = Vec::new();
        and_parts(expr, &mut parts);
        for part in parts {
            match part {
                sql::Expr::IsNull(column) | sql::Expr::IsNotNull(column) => {
                    let is_null = matches!(part, sql::Expr::IsNull(_));
                    match names {
                        Names::View(scope) => {
                            conds.extend(self.null_cond(scope, part, column, is_null));
                        }
                        Names::Ask(_) => self.refuse(line(part), CONDITION, part),
                    }
                }
                sql::Expr::BinaryOp {
                    left,
                    op: sql::BinaryOperator::Eq,
                    right,
                } => {
                    let key = self.cond_key(names, left);
                    let literal = self.cond_value(names, right);
                    if let (Some(key), Some(literal)) = (key, literal) {
                        conds.push(Condition {
                            key,
                            values: vec![literal],
                        });
                    }
                }
                sql::Expr::InList {
                    expr: inner,
                    list,
                    negated: false,
                } => {
                    if list.len() < 2 {
                        self.refuse(line(part), "IN with one value, write =,", part);
                        continue;
                    }
                    let key = self.cond_key(names, inner);
                    let values: Vec<Literal> = (list.iter())
                        .filter_map(|item| self.cond_value(names, item))
                        .collect();
                    if let Some(key) = key.filter(|_| values.len() == list.len()) {
                        conds.push(Condition { key, values });
                    }
                }
                sql::Expr::Nested(_) => self.refuse(line(part), PARENTHESES, part),
                other => self.refuse(line(other), CONDITION, other),
            }
        }
    }

    fn cond_key(&mut self, names: &mut Names<'_, '_>, expr: &sql::Expr) -> Option<Identifier> {
        match names {
            Names::View(scope) => self.key_of(scope, expr),
            Names::Ask(_) => self.ask_name(expr),
        }
    }

    fn cond_value(&mut self, names: &mut Names<'_, '_>, expr: &sql::Expr) -> Option<Literal> {
        match names {
            Names::View(_) => self.literal(expr),
            Names::Ask(params) => self.parameter(expr, params),
        }
    }
}

/// The parts of a condition joined by AND. A parenthesis is a part of its
/// own, which every caller refuses: AND needs none.
fn and_parts<'a>(expr: &'a sql::Expr, parts: &mut Vec<&'a sql::Expr>) {
    match expr {
        sql::Expr::BinaryOp {
            left,
            op: sql::BinaryOperator::And,
            right,
        } => {
            and_parts(left, parts);
            and_parts(right, parts);
        }
        other => parts.push(other),
    }
}
