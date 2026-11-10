//! Walks the `sqlparser` AST of a straum SQL file into dims and facts.
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
use straum_domain::{Cond, Dim, Fact, Ident, KeyPath, Literal, Table};

mod view;

/// Longest SQL text quoted in an error.
const QUOTE_LEN: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlError {
    /// 1-based; 0 when `sqlparser` puts the position in `msg`.
    pub line: u64,
    pub msg: String,
}

pub(crate) fn parse(src: &str) -> Result<(Vec<Dim>, Vec<Fact>), Vec<SqlError>> {
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, src).map_err(|e| {
        vec![SqlError {
            line: 0,
            msg: e.to_string(),
        }]
    })?;
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
    /// Dim attributes the current view reads.
    attrs: Vec<(Table, Ident)>,
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
        self.error(
            line,
            format!("{what} `{text}` is not in straum SQL, the PostgreSQL subset straum specs use"),
        );
    }

    /// Rejects a clause straum SQL has no use for when it is present.
    fn absent(&mut self, line: u64, present: bool, clause: &str) {
        if present {
            self.error(
                line,
                format!("{clause} is not in straum SQL, the PostgreSQL subset straum specs use"),
            );
        }
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

    fn conds(
        &mut self,
        expr: &sql::Expr,
        conds: &mut Vec<Cond>,
        path: &mut impl FnMut(&mut Self, &sql::Expr) -> Option<KeyPath>,
    ) {
        match expr {
            sql::Expr::Nested(inner) => self.conds(inner, conds, path),
            sql::Expr::BinaryOp {
                left,
                op: sql::BinaryOperator::And,
                right,
            } => {
                self.conds(left, conds, path);
                self.conds(right, conds, path);
            }
            sql::Expr::BinaryOp {
                left,
                op: sql::BinaryOperator::Eq,
                right,
            } => {
                let key = path(self, left);
                let value = self.literal(right);
                if let (Some(path), Some(value)) = (key, value) {
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
                let key = path(self, inner);
                let values: Vec<Literal> = list.iter().filter_map(|v| self.literal(v)).collect();
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

fn function_name(f: &sql::Function) -> Option<String> {
    match f.name.0.as_slice() {
        [sql::ObjectNamePart::Identifier(id)] if id.quote_style.is_none() => {
            Some(id.value.to_lowercase())
        }
        _ => None,
    }
}
