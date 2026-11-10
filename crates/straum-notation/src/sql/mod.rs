//! Walks the `sqlparser` AST of a straum SQL spec into dims and facts, and
//! of an ask into an `Ask`.
//!
//! Deny by default: every `match` lists what straum SQL accepts and rejects the
//! rest by name, and accepted structs are destructured without `..`, so a
//! field added by a `sqlparser` upgrade fails to compile until it is handled
//! here.

use std::fmt;

use sqlparser::ast as sql;
use sqlparser::ast::Spanned;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use straum_domain::{Cond, Dim, Fact, Ident, KeyPath, Literal};

mod ask;
mod view;

pub(crate) use ask::parse_ask;

/// Longest SQL text quoted in an error.
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

pub(crate) fn parse(src: &str) -> Result<(Vec<Dim>, Vec<Fact>), Vec<SqlError>> {
    let statements = statements(src)?;
    let mut walker = Walker::default();
    for statement in &statements {
        match statement {
            sql::Statement::CreateView(view) => walker.view(view),
            other => walker.reject(line(other), "statement", other),
        }
    }
    if walker.errors.is_empty() {
        Ok((walker.dims, walker.facts))
    } else {
        Err(walker.errors)
    }
}

fn line(node: &impl Spanned) -> u64 {
    node.span().start.line
}

#[derive(Default)]
struct Walker {
    dims: Vec<Dim>,
    facts: Vec<Fact>,
    errors: Vec<SqlError>,
}

impl Walker {
    fn error(&mut self, line: u64, msg: String) {
        self.errors.push(SqlError { line, msg });
    }

    fn reject(&mut self, line: u64, what: &str, node: &impl fmt::Display) {
        let text = node.to_string();
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let text = match text.char_indices().nth(QUOTE_LEN) {
            Some((end, _)) => format!("{}...", &text[..end]),
            None => text,
        };
        self.not_in_subset(line, &format!("{what} `{text}`"));
    }

    /// Rejects a clause straum SQL has no use for when it is present.
    fn absent(&mut self, line: u64, present: bool, clause: &str) {
        if present {
            self.not_in_subset(line, clause);
        }
    }

    /// Every rejection says straum SQL is straum's own subset, so no one
    /// takes it for PostgreSQL's error.
    fn not_in_subset(&mut self, line: u64, what: &str) {
        self.error(
            line,
            format!("{what} is not in straum SQL, the PostgreSQL subset made only for straum"),
        );
    }

    /// An unquoted name, folded to lower case as PostgreSQL does.
    fn ident(&mut self, id: &sql::Ident) -> Option<Ident> {
        let sql::Ident {
            value,
            quote_style,
            span,
        } = id;
        if quote_style.is_some() {
            self.reject(span.start.line, "quoted identifier", id);
            return None;
        }
        let ident = Ident::new(&value.to_lowercase());
        if ident.is_none() {
            self.error(span.start.line, format!("bad identifier {value}"));
        }
        ident
    }

    fn name(&mut self, name: &sql::ObjectName, parts: usize, what: &str) -> Option<Vec<Ident>> {
        let sql::ObjectName(list) = name;
        if list.len() != parts {
            let shape = if parts == 1 {
                "a bare name"
            } else {
                "schema.table"
            };
            self.error(line(name), format!("{what} {name} must be {shape}"));
            return None;
        }
        let mut idents = Vec::new();
        for part in list {
            match part {
                sql::ObjectNamePart::Identifier(id) => idents.push(self.ident(id)?),
                other => self.reject(line(name), "name part", other),
            }
        }
        (idents.len() == parts).then_some(idents)
    }

    /// The arguments of a plain call `name(args)`, or `None` after rejecting
    /// anything else about the call.
    fn call<'a>(&mut self, f: &'a sql::Function) -> Option<(&'a [sql::FunctionArg], bool)> {
        let sql::Function {
            name: _,
            uses_odbc_syntax,
            parameters,
            args,
            within_group,
            filter: _,
            null_treatment,
            over,
        } = f;
        let at = line(f);
        self.absent(at, *uses_odbc_syntax, "ODBC call syntax");
        self.absent(
            at,
            !matches!(parameters, sql::FunctionArguments::None),
            "function parameters",
        );
        self.absent(at, !within_group.is_empty(), "WITHIN GROUP");
        self.absent(at, null_treatment.is_some(), "IGNORE or RESPECT NULLS");
        self.absent(at, over.is_some(), "a window function (OVER)");
        let sql::FunctionArguments::List(sql::FunctionArgumentList {
            duplicate_treatment,
            args,
            clauses,
        }) = args
        else {
            self.reject(at, "function call", f);
            return None;
        };
        self.absent(at, !clauses.is_empty(), "ORDER BY or LIMIT inside a call");
        let distinct = match duplicate_treatment {
            None => false,
            Some(sql::DuplicateTreatment::Distinct) => true,
            Some(sql::DuplicateTreatment::All) => {
                self.absent(at, true, "ALL inside a call");
                false
            }
        };
        Some((args, distinct))
    }

    /// The one plain expression argument of a call.
    fn single<'a>(
        &mut self,
        f: &'a sql::Function,
        args: &'a [sql::FunctionArg],
    ) -> Option<&'a sql::Expr> {
        match args {
            [sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(e))] => Some(e),
            _ => {
                self.reject(line(f), "arguments", f);
                None
            }
        }
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
                self.reject(at, &format!("{what}, use one SELECT,"), other);
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
            Some(sql::Distinct::All) => self.absent(at, true, "SELECT ALL"),
            Some(sql::Distinct::Distinct | sql::Distinct::On(_)) => {
                self.absent(at, true, "SELECT DISTINCT");
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
                self.absent(at, true, "FROM before SELECT");
            }
        }
        at
    }

    /// `path = value` and `path IN (values)` joined by `AND`; `column IS
    /// NULL` and `IS NOT NULL` go to `null_test` with whether it is the
    /// `NULL` one, since what they mean depends on the caller.
    fn conds(
        &mut self,
        expr: &sql::Expr,
        conds: &mut Vec<Cond>,
        path: &mut impl FnMut(&mut Self, &sql::Expr) -> Option<KeyPath>,
        value: &mut impl FnMut(&mut Self, &sql::Expr) -> Option<Literal>,
        null_test: &mut impl FnMut(&mut Self, &sql::Expr, &sql::Expr, bool) -> Option<Cond>,
    ) {
        match expr {
            sql::Expr::Nested(inner) => self.conds(inner, conds, path, value, null_test),
            sql::Expr::BinaryOp {
                left,
                op: sql::BinaryOperator::And,
                right,
            } => {
                self.conds(left, conds, path, value, null_test);
                self.conds(right, conds, path, value, null_test);
            }
            sql::Expr::IsNull(column) => conds.extend(null_test(self, expr, column, true)),
            sql::Expr::IsNotNull(column) => conds.extend(null_test(self, expr, column, false)),
            sql::Expr::BinaryOp {
                left,
                op: sql::BinaryOperator::Eq,
                right,
            } => {
                let key = path(self, left);
                let literal = value(self, right);
                if let (Some(path), Some(literal)) = (key, literal) {
                    conds.push(Cond {
                        path,
                        values: vec![literal],
                    });
                }
            }
            sql::Expr::InList {
                expr: inner,
                list,
                negated: false,
            } => {
                let key = path(self, inner);
                let values: Vec<Literal> = list.iter().filter_map(|v| value(self, v)).collect();
                if let Some(path) = key.filter(|_| values.len() == list.len()) {
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

    fn literal(&mut self, expr: &sql::Expr) -> Option<Literal> {
        let (negative, value) = match expr {
            sql::Expr::UnaryOp {
                op: sql::UnaryOperator::Minus,
                expr: inner,
            } => (true, inner.as_ref()),
            other => (false, other),
        };
        let literal = match value {
            sql::Expr::Value(sql::ValueWithSpan { value, span: _ }) => match value {
                sql::Value::Number(n, false) => {
                    let n = if negative { format!("-{n}") } else { n.clone() };
                    n.parse().ok().map(Literal::Int)
                }
                sql::Value::SingleQuotedString(s) if !negative => Some(Literal::Text(s.clone())),
                sql::Value::Boolean(b) if !negative => Some(Literal::Bool(*b)),
                _ => None,
            },
            _ => None,
        };
        if literal.is_none() {
            self.reject(
                line(expr),
                "literal, use an integer, 'text', true or false,",
                expr,
            );
        }
        literal
    }
}

/// The parts of a condition joined by AND.
fn and_parts<'a>(expr: &'a sql::Expr, parts: &mut Vec<&'a sql::Expr>) {
    match expr {
        sql::Expr::Nested(inner) => and_parts(inner, parts),
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

fn function_name(f: &sql::Function) -> Option<String> {
    match f.name.0.as_slice() {
        [sql::ObjectNamePart::Identifier(id)] if id.quote_style.is_none() => {
            Some(id.value.to_lowercase())
        }
        _ => None,
    }
}
