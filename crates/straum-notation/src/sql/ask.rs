//! Walks an ask, one `SELECT` over a view, into an `Ask`: the view, the
//! keys it groups by, the measures, the filter and the bucket range. The
//! values come from the `$n` parameters passed beside the text.
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

use super::{Names, Reported, SqlError, Walker, and_parts, line, statements};

/// An ask, its values taken from `parameters` by `$n`.
pub fn parse_ask(query: &str, parameters: &[Literal]) -> Result<Ask, Vec<SqlError>> {
    let statements = statements(query)?;
    let mut walker = Walker::default();
    let mut parameters = Parameters {
        values: parameters.to_vec(),
        used: vec![false; parameters.len()],
    };
    let ask = match statements.as_slice() {
        [sql::Statement::Query(query)] => walker.ask(query, &mut parameters),
        [other] => {
            walker.refuse(line(other), "statement", other);
            Err(Reported)
        }
        _ => {
            walker.error(0, "an ask is one SELECT".into());
            Err(Reported)
        }
    };
    for (index, used) in parameters.used.iter().enumerate() {
        if !used {
            walker.error(0, format!("parameter ${} is not used", index + 1));
        }
    }
    walker.finish(ask)
}

/// The values an ask's `$n` stand for, and which of them it used.
pub(super) struct Parameters {
    values: Vec<Literal>,
    used: Vec<bool>,
}

/// Which end of the bucket range a bound is.
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

/// What an ask's WHERE holds: the filter and both bounds.
struct Where {
    filter: Vec<Condition>,
    from: Bound,
    to: Bound,
}

/// A bucket bound as the ask writes it; `Bad` is one already reported.
enum Bound {
    Missing,
    Bad,
    At { seconds: i64 },
}

impl Bound {
    /// The bound in Unix seconds; a missing or bad one is already reported.
    fn seconds(&self) -> Result<i64, Reported> {
        match self {
            Bound::At { seconds } => Ok(*seconds),
            Bound::Missing | Bound::Bad => Err(Reported),
        }
    }
}

/// `bucket >= value` or `bucket < value`, as written.
// Borrows the value from the tree, which outlives the walk.
struct BucketBound<'tree> {
    end: End,
    value: &'tree sql::Expr,
}

impl Walker {
    fn ask(&mut self, query: &sql::Query, parameters: &mut Parameters) -> Result<Ask, Reported> {
        let select = self.one_select(query, "ask")?;
        let at = self.select_clauses(select);
        let view = if let [sql::TableWithJoins { relation, joins }] = select.from.as_slice() {
            for join in joins {
                self.refuse(line(join), "join in an ask, read FROM one view,", join);
            }
            self.ask_view(relation)
        } else {
            self.error(at, "an ask reads FROM one view".into());
            Err(Reported)
        };
        let grouped = self.ask_group_by(at, &select.group_by);
        let Selected { keys, measures } = self.ask_selected(&select.projection, &grouped);
        for key in &grouped {
            if !keys.contains(key) {
                self.error(at, format!("GROUP BY {key} is not selected"));
            }
        }
        let Where { filter, from, to } = self.ask_where(at, select.selection.as_ref(), parameters);
        Ok(Ask {
            view: view?,
            measures,
            keys,
            filter,
            from: from.seconds()?,
            to: to.seconds()?,
        })
    }

    /// The SELECT list: the grouped keys first, then the measures.
    fn ask_selected(&mut self, projection: &[sql::SelectItem], grouped: &[AskKey]) -> Selected {
        let mut keys = Vec::new();
        let mut measures = Vec::new();
        for item in projection {
            let sql::SelectItem::UnnamedExpr(expression) = item else {
                self.refuse(line(item), "SELECT item", item);
                continue;
            };
            let Ok(key) = self.ask_key(expression) else {
                continue;
            };
            if grouped.contains(&key) {
                if !measures.is_empty() {
                    let message = format!("select {expression} before the measures");
                    self.error(line(expression), message);
                }
                keys.push(key);
                continue;
            }
            match key {
                AskKey::Key(name) => measures.push(name),
                AskKey::Bucket => self.error(
                    line(expression),
                    format!("{expression} is selected but not in GROUP BY"),
                ),
            }
        }
        Selected { keys, measures }
    }

    fn ask_view(&mut self, relation: &sql::TableFactor) -> Result<Identifier, Reported> {
        let named = self.table_factor(relation)?;
        if let Some(alias) = named.alias {
            self.refuse(
                line(relation),
                "alias in an ask, read FROM the view by name,",
                alias,
            );
        }
        self.bare_name(named.name, "view")
    }

    fn ask_group_by(&mut self, at: u64, group_by: &sql::GroupByExpr) -> Vec<AskKey> {
        let Ok(expressions) = self.group_by_expressions(at, group_by) else {
            return Vec::new();
        };
        let mut keys = Vec::new();
        for expression in expressions {
            let Ok(key) = self.ask_key(expression) else {
                continue;
            };
            if keys.contains(&key) {
                self.error(
                    line(expression),
                    format!("GROUP BY {expression} given twice"),
                );
                continue;
            }
            keys.push(key);
        }
        keys
    }

    /// A key or measure by the name the view gives it, or `bucket`.
    fn ask_key(&mut self, expression: &sql::Expr) -> Result<AskKey, Reported> {
        let sql::Expr::Identifier(identifier) = expression else {
            self.refuse(
                line(expression),
                "expression, write the name the view gives the key or measure,",
                expression,
            );
            return Err(Reported);
        };
        let name = self.read_identifier(identifier)?;
        if name.as_str() == "bucket" {
            return Ok(AskKey::Bucket);
        }
        Ok(AskKey::Key(name))
    }

    /// The conditions, and the range from `bucket >= $n AND bucket < $m`,
    /// both bounds required.
    fn ask_where(
        &mut self,
        at: u64,
        selection: Option<&sql::Expr>,
        parameters: &mut Parameters,
    ) -> Where {
        let mut parts = Vec::new();
        if let Some(expression) = selection {
            and_parts(expression, &mut parts);
        }
        let mut filter = Vec::new();
        let mut from = Bound::Missing;
        let mut to = Bound::Missing;
        for part in parts {
            let Some(BucketBound { end, value }) = bucket_bound(part) else {
                self.read_conditions(part, &mut filter, &mut Names::Ask(parameters));
                continue;
            };
            let bound = match end {
                End::From => &mut from,
                End::To => &mut to,
            };
            if !matches!(bound, Bound::Missing) {
                let operator = end.spelling();
                self.error(line(part), format!("bucket {operator} given twice"));
                continue;
            }
            *bound = match self.instant(value, parameters) {
                Ok(seconds) => Bound::At { seconds },
                Err(Reported) => Bound::Bad,
            };
        }
        if let Bound::Missing = from {
            self.missing_bound(at, &End::From);
        }
        if let Bound::Missing = to {
            self.missing_bound(at, &End::To);
        }
        Where { filter, from, to }
    }

    fn missing_bound(&mut self, at: u64, end: &End) {
        let operator = end.spelling();
        self.error(
            at,
            format!("an ask writes both bounds; bucket {operator} $n is missing"),
        );
    }

    /// A key by the name the view gives it; `bucket` takes only the range.
    pub(super) fn ask_name(&mut self, expression: &sql::Expr) -> Result<Identifier, Reported> {
        match self.ask_key(expression)? {
            AskKey::Key(name) => Ok(name),
            AskKey::Bucket => {
                self.error(
                    line(expression),
                    "bucket takes only bucket >= $n AND bucket < $m".into(),
                );
                Err(Reported)
            }
        }
    }

    /// `$n`, the n-th parameter. Values written into the query are
    /// rejected, so callers never build a query out of their values.
    pub(super) fn parameter(
        &mut self,
        expression: &sql::Expr,
        parameters: &mut Parameters,
    ) -> Result<Literal, Reported> {
        let sql::Expr::Value(sql::ValueWithSpan {
            value: sql::Value::Placeholder(name),
            span: _,
        }) = expression
        else {
            self.refuse(
                line(expression),
                "value, use a parameter such as $1,",
                expression,
            );
            return Err(Reported);
        };
        let count = parameters.values.len();
        let Some(index) = parameter_index(name, count) else {
            self.error(
                line(expression),
                format!("{name} is not a parameter: there are {count}, $1 on"),
            );
            return Err(Reported);
        };
        parameters.used[index] = true;
        Ok(parameters.values[index].clone())
    }

    /// Unix seconds, from an integer parameter or an RFC 3339 one.
    fn instant(
        &mut self,
        expression: &sql::Expr,
        parameters: &mut Parameters,
    ) -> Result<i64, Reported> {
        let instant = match self.parameter(expression, parameters)? {
            Literal::Int(seconds) => Ok(seconds),
            Literal::Text(text) => rfc3339_seconds(&text),
            Literal::Bool(_) => Err("is a boolean".into()),
        };
        match instant {
            Ok(seconds) => Ok(seconds),
            Err(why) => {
                self.error(
                    line(expression),
                    format!("bucket bound {expression} must be RFC 3339 or Unix seconds: {why}"),
                );
                Err(Reported)
            }
        }
    }
}

/// What an ask's SELECT list holds once read.
struct Selected {
    keys: Vec<AskKey>,
    measures: Vec<Identifier>,
}

/// The 0-based index of `$n`, if there is an n-th parameter.
fn parameter_index(name: &str, count: usize) -> Option<usize> {
    let number: usize = name.strip_prefix('$')?.parse().ok()?;
    if number < 1 || number > count {
        return None;
    }
    Some(number - 1)
}

fn rfc3339_seconds(text: &str) -> Result<i64, String> {
    match text.parse::<Timestamp>() {
        Ok(at) if at.subsec_nanosecond() != 0 => Err(format!("{text} is not a whole second")),
        Ok(at) => Ok(at.as_second()),
        Err(error) => Err(format!("{text:?}: {error}")),
    }
}

/// `bucket >= value` or `bucket < value`; `None` for any other condition.
fn bucket_bound(expression: &sql::Expr) -> Option<BucketBound<'_>> {
    let sql::Expr::BinaryOp { left, op, right } = expression else {
        return None;
    };
    let sql::Expr::Identifier(identifier) = left.as_ref() else {
        return None;
    };
    if identifier.quote_style.is_some() || !identifier.value.eq_ignore_ascii_case("bucket") {
        return None;
    }
    let end = match op {
        sql::BinaryOperator::GtEq => End::From,
        sql::BinaryOperator::Lt => End::To,
        _ => return None,
    };
    Some(BucketBound { end, value: right })
}
