//! An ask: one `SELECT` over a view.
//!
//! ```sql
//! SELECT bucket, pickup.borough, trips, fare
//! FROM trips
//! WHERE payment_type IN ($1, $2) AND bucket >= $3 AND bucket < $4
//! GROUP BY bucket, pickup.borough
//! ```
//!
//! Every value is a `$n` parameter, so no value is ever spliced into the
//! query text. `bucket` is the view's time bucket; a spec can't name a
//! column or ref `bucket`, so it never clashes.

use jiff::Timestamp;
use sqlparser::ast as sql;
use straum_domain::{Ask, AskKey, Cond, KeyPath, Literal, TimeKey};

use super::{SqlError, Walker, line, statements};

pub(crate) fn parse_ask(query: &str, parameters: &[Literal]) -> Result<Ask, Vec<SqlError>> {
    let statements = statements(query)?;
    let mut walker = Walker::default();
    let mut params = Params {
        values: parameters,
        used: vec![false; parameters.len()],
    };
    let ask = match statements.as_slice() {
        [sql::Statement::Query(query)] => walker.ask(query, &mut params),
        [other] => {
            walker.reject(line(other), "statement", other);
            None
        }
        _ => {
            walker.error(0, "an ask is one SELECT".into());
            None
        }
    };
    for (i, used) in params.used.iter().enumerate() {
        if !used {
            walker.error(0, format!("parameter ${} is not used", i + 1));
        }
    }
    match ask {
        Some(ask) if walker.errors.is_empty() => Ok(ask),
        _ => Err(walker.errors),
    }
}

struct Params<'a> {
    values: &'a [Literal],
    used: Vec<bool>,
}

/// Which end of the range a `bucket` comparison sets.
enum End {
    From,
    To,
}

impl Walker {
    fn ask(&mut self, query: &sql::Query, params: &mut Params) -> Option<Ask> {
        let select = self.one_select(query, "ask")?;
        let at = self.select_clauses(select);

        let fact = match select.from.as_slice() {
            [
                sql::TableWithJoins {
                    relation:
                        sql::TableFactor::Table {
                            name,
                            alias: None,
                            args: None,
                            with_hints,
                            version: None,
                            with_ordinality: false,
                            partitions,
                            json_path: None,
                            sample: None,
                            index_hints,
                        },
                    joins,
                },
            ] if joins.is_empty()
                && with_hints.is_empty()
                && partitions.is_empty()
                && index_hints.is_empty() =>
            {
                self.name(name, 1, "view").map(|mut n| n.remove(0))
            }
            _ => {
                self.error(
                    at,
                    "an ask reads FROM one view by name, with no alias or join".into(),
                );
                None
            }
        };

        let grouped = self.ask_group_by(at, &select.group_by);
        let mut by = Vec::new();
        let mut measures = Vec::new();
        for item in &select.projection {
            let sql::SelectItem::UnnamedExpr(expr) = item else {
                self.reject(line(item), "SELECT item", item);
                continue;
            };
            let Some(key) = self.ask_key(expr) else {
                continue;
            };
            if grouped.contains(&key) {
                if !measures.is_empty() {
                    self.error(line(expr), format!("select {expr} before the measures"));
                }
                by.push(key);
                continue;
            }
            match key {
                AskKey::Key(KeyPath { head, attr: None }) => measures.push(head),
                _ => self.error(
                    line(expr),
                    format!("{expr} is selected but not in GROUP BY"),
                ),
            }
        }
        for key in &grouped {
            if !by.contains(key) {
                self.error(at, format!("GROUP BY {} is not selected", key_name(key)));
            }
        }

        let (filter, from, to) = self.ask_where(at, select.selection.as_ref(), params);
        Some(Ask {
            fact: fact?,
            measures,
            by,
            filter,
            from: from?,
            to: to?,
        })
    }

    fn ask_group_by(&mut self, at: u64, group_by: &sql::GroupByExpr) -> Vec<AskKey> {
        let exprs = match group_by {
            sql::GroupByExpr::Expressions(exprs, modifiers) => {
                self.absent(at, !modifiers.is_empty(), "ROLLUP, CUBE or GROUPING SETS");
                exprs
            }
            sql::GroupByExpr::All(_) => {
                self.absent(at, true, "GROUP BY ALL");
                return Vec::new();
            }
        };
        let mut keys = Vec::new();
        for expr in exprs {
            match self.ask_key(expr) {
                Some(key) if keys.contains(&key) => {
                    self.error(line(expr), format!("GROUP BY {expr} given twice"));
                }
                Some(key) => keys.push(key),
                None => {}
            }
        }
        keys
    }

    /// `column`, `ref`, `ref.attribute` or `bucket`.
    fn ask_key(&mut self, expr: &sql::Expr) -> Option<AskKey> {
        match expr {
            sql::Expr::Identifier(id) => {
                let name = self.ident(id)?;
                if name.as_str() == "bucket" {
                    return Some(AskKey::Time(TimeKey::Bucket));
                }
                Some(AskKey::Key(KeyPath {
                    head: name,
                    attr: None,
                }))
            }
            sql::Expr::CompoundIdentifier(ids) => {
                let [head, attr] = ids.as_slice() else {
                    self.reject(line(expr), "name, use column, ref or ref.attribute,", expr);
                    return None;
                };
                let (head, attr) = (self.ident(head), self.ident(attr));
                Some(AskKey::Key(KeyPath {
                    head: head?,
                    attr: Some(attr?),
                }))
            }
            other => {
                self.reject(
                    line(other),
                    "expression, use column, ref or ref.attribute,",
                    other,
                );
                None
            }
        }
    }

    /// The conditions, and the range from `bucket >= $n AND bucket < $m`.
    fn ask_where(
        &mut self,
        at: u64,
        selection: Option<&sql::Expr>,
        params: &mut Params,
    ) -> (Vec<Cond>, Option<i64>, Option<i64>) {
        let mut parts = Vec::new();
        if let Some(expr) = selection {
            and_parts(expr, &mut parts);
        }
        let mut conds = Vec::new();
        // Outer `None`: not given. Inner `None`: given, and already an error.
        let (mut from, mut to): (Option<Option<i64>>, Option<Option<i64>>) = (None, None);
        for part in parts {
            if let Some((end, value)) = bucket_bound(part) {
                let (slot, op) = match end {
                    End::From => (&mut from, ">="),
                    End::To => (&mut to, "<"),
                };
                if slot.is_some() {
                    self.error(line(part), format!("bucket {op} given twice"));
                    continue;
                }
                *slot = Some(self.instant(value, params));
                continue;
            }
            self.conds(part, &mut conds, &mut |w, e| w.ask_path(e), &mut |w, e| {
                w.parameter(e, params)
            });
        }
        if from.is_none() || to.is_none() {
            self.error(at, "an ask needs WHERE bucket >= $n AND bucket < $m".into());
        }
        (conds, from.flatten(), to.flatten())
    }

    fn ask_path(&mut self, expr: &sql::Expr) -> Option<KeyPath> {
        match self.ask_key(expr)? {
            AskKey::Key(path) => Some(path),
            AskKey::Time(_) => {
                self.error(
                    line(expr),
                    "bucket takes only bucket >= $n AND bucket < $m".into(),
                );
                None
            }
        }
    }

    /// `$n`, the n-th parameter. Values written into the query are
    /// rejected, so callers never build a query out of their values.
    fn parameter(&mut self, expr: &sql::Expr, params: &mut Params) -> Option<Literal> {
        let sql::Expr::Value(sql::ValueWithSpan {
            value: sql::Value::Placeholder(name),
            span: _,
        }) = expr
        else {
            self.reject(line(expr), "value, use a parameter such as $1,", expr);
            return None;
        };
        let n = name.strip_prefix('$').and_then(|n| n.parse::<usize>().ok());
        let Some(i) = n.filter(|&n| n >= 1 && n <= params.values.len()) else {
            let count = params.values.len();
            self.error(
                line(expr),
                format!("{name} is not a parameter: there are {count}, $1 on"),
            );
            return None;
        };
        params.used[i - 1] = true;
        Some(params.values[i - 1].clone())
    }

    /// Unix seconds, from an integer parameter or an RFC 3339 one.
    fn instant(&mut self, expr: &sql::Expr, params: &mut Params) -> Option<i64> {
        let at = match self.parameter(expr, params)? {
            Literal::Int(seconds) => Ok(seconds),
            Literal::Text(s) => match s.parse::<Timestamp>() {
                Ok(at) if at.subsec_nanosecond() != 0 => Err(format!("{s} is not a whole second")),
                Ok(at) => Ok(at.as_second()),
                Err(e) => Err(format!("{s:?}: {e}")),
            },
            Literal::Bool(_) => Err("is a boolean".into()),
        };
        at.map_err(|e| {
            self.error(
                line(expr),
                format!("bucket bound {expr} must be RFC 3339 or Unix seconds: {e}"),
            );
        })
        .ok()
    }
}

/// The parts of a WHERE joined by AND.
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

fn bucket_bound(expr: &sql::Expr) -> Option<(End, &sql::Expr)> {
    let sql::Expr::BinaryOp { left, op, right } = expr else {
        return None;
    };
    let sql::Expr::Identifier(id) = left.as_ref() else {
        return None;
    };
    if id.quote_style.is_some() || !id.value.eq_ignore_ascii_case("bucket") {
        return None;
    }
    match op {
        sql::BinaryOperator::GtEq => Some((End::From, right)),
        sql::BinaryOperator::Lt => Some((End::To, right)),
        _ => None,
    }
}

fn key_name(key: &AskKey) -> String {
    match key {
        AskKey::Key(path) => path.to_string(),
        AskKey::Time(t) => t.to_string(),
    }
}
