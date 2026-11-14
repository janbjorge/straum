//! An ask: one `SELECT` over a view.
//!
//! ```sql
//! SELECT bucket, borough, trips, fare
//! FROM trips
//! WHERE payment_type IN ($1, $2) AND bucket >= $3 AND bucket < $4
//! GROUP BY bucket, borough
//! ```
//!
//! Every value is a `$n` parameter, so no value is ever spliced into the
//! query text. Keys and measures go by the names the view gives them.
//! `bucket` is the view's time bucket; a spec can't name a key or measure
//! `bucket`, so it never clashes. Both bounds are written: an ask says
//! what it reads.

use jiff::Timestamp;
use sqlparser::ast as sql;
use straum_domain::{Ask, AskKey, Condition, Identifier, Literal};

use super::{Names, SqlError, Walker, and_parts, line, statements};

/// An ask, its values taken from `parameters` by `$n`.
pub fn parse_ask(query: &str, parameters: &[Literal]) -> Result<Ask, Vec<SqlError>> {
    let statements = statements(query)?;
    let mut walker = Walker::default();
    let mut params = Params {
        values: parameters,
        used: vec![false; parameters.len()],
    };
    let ask = match statements.as_slice() {
        [sql::Statement::Query(query)] => walker.ask(query, &mut params),
        [other] => {
            walker.refuse(line(other), "statement", other);
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
    walker.finish(ask)
}

pub(super) struct Params<'a> {
    values: &'a [Literal],
    used: Vec<bool>,
}

enum End {
    From,
    To,
}

impl End {
    fn spelling(&self) -> &'static str {
        match self {
            End::From => ">=",
            End::To => "<",
        }
    }
}

/// A bucket bound as the ask writes it; `Bad` is one already reported.
enum Bound {
    Missing,
    Bad,
    At { seconds: i64 },
}

impl Bound {
    fn seconds(&self) -> Option<i64> {
        match self {
            Bound::At { seconds } => Some(*seconds),
            Bound::Missing | Bound::Bad => None,
        }
    }
}

impl Walker {
    fn ask(&mut self, query: &sql::Query, params: &mut Params) -> Option<Ask> {
        let select = self.one_select(query, "ask")?;
        let at = self.select_clauses(select);

        let fact = match select.from.as_slice() {
            [sql::TableWithJoins { relation, joins }] => {
                for join in joins {
                    self.refuse(line(join), "join in an ask, read FROM one view,", join);
                }
                self.ask_view(relation)
            }
            _ => {
                self.error(at, "an ask reads FROM one view".into());
                None
            }
        };

        let grouped = self.ask_group_by(at, &select.group_by);
        let mut by = Vec::new();
        let mut measures = Vec::new();
        for item in &select.projection {
            let sql::SelectItem::UnnamedExpr(expr) = item else {
                self.refuse(line(item), "SELECT item", item);
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
                AskKey::Key(name) => measures.push(name),
                AskKey::Bucket => self.error(
                    line(expr),
                    format!("{expr} is selected but not in GROUP BY"),
                ),
            }
        }
        for key in &grouped {
            if !by.contains(key) {
                self.error(at, format!("GROUP BY {key} is not selected"));
            }
        }

        let (filter, from, to) = self.ask_where(at, select.selection.as_ref(), params);
        Some(Ask {
            view: fact?,
            measures,
            keys: by,
            filter,
            from: from?,
            to: to?,
        })
    }

    fn ask_view(&mut self, relation: &sql::TableFactor) -> Option<Identifier> {
        let (name, alias) = self.table_factor(relation)?;
        if let Some(alias) = alias {
            self.refuse(
                line(relation),
                "alias in an ask, read FROM the view by name,",
                alias,
            );
        }
        self.bare_name(name, "view")
    }

    fn ask_group_by(&mut self, at: u64, group_by: &sql::GroupByExpr) -> Vec<AskKey> {
        let Some(exprs) = self.group_by_exprs(at, group_by) else {
            return Vec::new();
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

    /// A key or measure by the name the view gives it, or `bucket`.
    fn ask_key(&mut self, expr: &sql::Expr) -> Option<AskKey> {
        match expr {
            sql::Expr::Identifier(id) => {
                let name = self.ident(id)?;
                if name.as_str() == "bucket" {
                    return Some(AskKey::Bucket);
                }
                Some(AskKey::Key(name))
            }
            other => {
                self.refuse(
                    line(other),
                    "expression, write the name the view gives the key or measure,",
                    other,
                );
                None
            }
        }
    }

    /// The conditions, and the range from `bucket >= $n AND bucket < $m`,
    /// both bounds required.
    fn ask_where(
        &mut self,
        at: u64,
        selection: Option<&sql::Expr>,
        params: &mut Params,
    ) -> (Vec<Condition>, Option<i64>, Option<i64>) {
        let mut parts = Vec::new();
        if let Some(expr) = selection {
            and_parts(expr, &mut parts);
        }
        let mut conds = Vec::new();
        let (mut from, mut to) = (Bound::Missing, Bound::Missing);
        for part in parts {
            if let Some((end, value)) = bucket_bound(part) {
                let bound = match end {
                    End::From => &mut from,
                    End::To => &mut to,
                };
                if !matches!(bound, Bound::Missing) {
                    let op = end.spelling();
                    self.error(line(part), format!("bucket {op} given twice"));
                    continue;
                }
                *bound = match self.instant(value, params) {
                    Some(seconds) => Bound::At { seconds },
                    None => Bound::Bad,
                };
                continue;
            }
            self.conds(part, &mut conds, &mut Names::Ask(params));
        }
        for (end, bound) in [(End::From, &from), (End::To, &to)] {
            if matches!(bound, Bound::Missing) {
                let op = end.spelling();
                self.error(
                    at,
                    format!("an ask writes both bounds; bucket {op} $n is missing"),
                );
            }
        }
        (conds, from.seconds(), to.seconds())
    }

    pub(super) fn ask_name(&mut self, expr: &sql::Expr) -> Option<Identifier> {
        match self.ask_key(expr)? {
            AskKey::Key(name) => Some(name),
            AskKey::Bucket => {
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
    pub(super) fn parameter(&mut self, expr: &sql::Expr, params: &mut Params) -> Option<Literal> {
        let sql::Expr::Value(sql::ValueWithSpan {
            value: sql::Value::Placeholder(name),
            span: _,
        }) = expr
        else {
            self.refuse(line(expr), "value, use a parameter such as $1,", expr);
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
