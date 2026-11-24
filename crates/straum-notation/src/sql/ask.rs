//! Walks an ask, one `SELECT` over a view, into an `Ask`: the view, the
//! keys it groups by, the measures, the filter and the time range. The
//! values come from the `$n` parameters passed beside the text.
//!
//! ```sql
//! SELECT hour, borough, trips, fare
//! FROM trips
//! WHERE payment_type IN ($1, $2) AND hour >= $3 AND hour < $4
//! GROUP BY hour, borough
//! ```
//!
//! Every value is a `$n` parameter, so no value is ever spliced into the
//! query text. Keys, measures and the view's time go by the names the view
//! gives them; the walk doesn't know the view, so the spec's check says
//! which name is the time. `buckets(hour, '1 day')` in `SELECT` and
//! `GROUP BY` asks for rows a wider width apart; the width is part of the
//! query, not a parameter, since it sets the answer's shape. Each bound
//! may be left out: an ask without `hour >= $n` reads from the first
//! bucket held, one without `hour < $n` to the last, and one with no
//! `WHERE` reads them all.

use std::fmt;

use jiff::Timestamp;
use sqlparser::ast as sql;
use sqlparser::ast::Spanned;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Location, Token, Tokenizer, Whitespace};
use straum_domain::{
    Ask, AskKey, Comparison, Condition, Identifier, Literal, MeasureCondition, decimal,
};

use super::calls::{BUCKETS, DATE_BIN, is_call};
use super::{
    NOT_IN_STRAUM_SQL, Names, PARENTHESES, Reported, SqlError, Walker, and_parts, byte_offset,
    line, statements, without_leading_zeros,
};
use crate::ask_error::comparison_text;

const MEASURE_CONDITION: &str =
    "HAVING condition, write a measure, = <> < <= > or >=, and $n, joined by AND,";

/// An ask, its values taken from `parameters` by `$n`.
pub fn parse_ask(query: &str, parameters: &[Literal]) -> Result<Ask, Vec<SqlError>> {
    if let Some(error) = extra_tokens(query) {
        return Err(vec![error]);
    }
    let statements = statements(query)?;
    let mut walker = Walker::default();
    let mut parameters = Parameters {
        values: parameters.to_vec(),
        used: vec![false; parameters.len()],
        text_bytes: 0,
    };
    let ask = match statements.as_slice() {
        [sql::Statement::Query(parsed)] => walker.ask(parsed, query, &mut parameters),
        [other] => {
            walker.refuse(line(other), "statement", other);
            Err(Reported)
        }
        _ => {
            walker.error(0, "an ask is one SELECT".into());
            Err(Reported)
        }
    };
    // A refused part may hold the parameter the walk never reached: an
    // unused one is news only in an ask with nothing else wrong.
    let refused = walker.met() > 0;
    for (index, used) in parameters.used.iter().enumerate() {
        if !used && !refused {
            walker.error(0, format!("parameter ${} is not used", index + 1));
        }
    }
    walker.finish(ask)
}

/// The most names an ask's SELECT, or its GROUP BY, may list: more than
/// any view has, since each is named once. Checking each name against the
/// ones before is quadratic in them.
const MAX_NAMES: usize = 256;

/// The most bytes of text an ask's parameters may come to as it uses
/// them: the most an ask's body holds. Each use is a copy, so a long text
/// named many times would grow far past the body.
const MAX_TEXT_BYTES: usize = 1 << 20;

/// The values an ask's `$n` stand for, which of them it used, and the
/// bytes of text its uses came to.
pub(super) struct Parameters {
    values: Vec<Literal>,
    used: Vec<bool>,
    text_bytes: usize,
}

/// Which end of the bucket range a bound is.
enum End {
    From,
    To,
}

impl End {
    /// The comparison a bound on this end writes.
    fn comparison(&self) -> Comparison {
        match self {
            End::From => Comparison::GreaterOrEqual,
            End::To => Comparison::Less,
        }
    }
}

/// What an ask's WHERE holds: the filter, the bounds written and the
/// name they compare.
struct Where {
    filter: Vec<Condition>,
    from: Bound,
    to: Bound,
    bounded: Option<Identifier>,
}

/// A bucket bound as the ask writes it; `Bad` is one already reported.
enum Bound {
    Open,
    Bad,
    At { seconds: i64 },
}

impl Bound {
    /// The bound in Unix seconds, `None` for an open end; a bad one is
    /// already reported.
    fn seconds(&self) -> Result<Option<i64>, Reported> {
        match self {
            Bound::At { seconds } => Ok(Some(*seconds)),
            Bound::Open => Ok(None),
            Bound::Bad => Err(Reported),
        }
    }
}

/// `name >= value` or `name < value`, as written.
// Borrows the name and value from the tree, which outlives the walk.
struct BucketBound<'tree> {
    name: &'tree sql::Ident,
    end: End,
    value: &'tree sql::Expr,
}

/// Why a bucket bound is no instant.
enum InstantError {
    Boolean,
    NotWholeSecond {
        text: String,
    },
    NotRfc3339 {
        text: String,
        error: jiff::Error,
    },
    /// Read by jiff, but not written as RFC 3339's one form here.
    NotItsForm {
        text: String,
    },
    /// A zero offset written other than `Z`.
    ZeroOffset {
        text: String,
    },
}

impl fmt::Display for InstantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstantError::Boolean => formatter.write_str("is a boolean"),
            InstantError::NotWholeSecond { text } => {
                write!(formatter, "{text} is not a whole second")
            }
            InstantError::NotRfc3339 { text, error } => write!(formatter, "{text:?}: {error}"),
            InstantError::NotItsForm { text } => write!(
                formatter,
                "{text:?}: write YYYY-MM-DDTHH:MM:SS, then Z or an offset such as +01:00"
            ),
            InstantError::ZeroOffset { text } => {
                write!(formatter, "{text:?}: write Z for a zero offset")
            }
        }
    }
}

impl Walker {
    fn ask(
        &mut self,
        query: &sql::Query,
        text: &str,
        parameters: &mut Parameters,
    ) -> Result<Ask, Reported> {
        let select = self.one_select(query, "ask")?;
        let at = self.select_clauses(select);
        let grouped = match &select.group_by {
            sql::GroupByExpr::Expressions(expressions, _) => expressions.len(),
            sql::GroupByExpr::All(_) => 0,
        };
        if select.projection.len() > MAX_NAMES || grouped > MAX_NAMES {
            self.error(
                at,
                format!("an ask selects and groups by at most {MAX_NAMES} names each"),
            );
            return Err(Reported);
        }
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
        let Where {
            filter,
            from,
            to,
            bounded,
        } = self.ask_where(select.selection.as_ref(), parameters);
        let having = self.ask_having(select.having.as_ref(), text, parameters);
        Ok(Ask {
            view: view?,
            measures,
            keys,
            filter,
            having,
            from: from.seconds()?,
            to: to.seconds()?,
            bounded,
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
                AskKey::WiderBucket { .. } => self.error(
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

    /// A key or measure by the name the view gives it, or
    /// `buckets(name, '<width>')` for the view's time at a wider width.
    fn ask_key(&mut self, expression: &sql::Expr) -> Result<AskKey, Reported> {
        if let sql::Expr::Function(function) = expression {
            if is_call(function, BUCKETS) {
                return self.wider_bucket(function);
            }
            // PostgreSQL's spelling, which a reader of SQL reaches for.
            if is_call(function, DATE_BIN) {
                self.error(
                    line(function),
                    "date_bin in an ask: write buckets(<time>, '<width>')".into(),
                );
                return Err(Reported);
            }
        }
        let sql::Expr::Identifier(identifier) = expression else {
            self.refuse(
                line(expression),
                "expression, write the name the view gives the key or measure,",
                expression,
            );
            return Err(Reported);
        };
        Ok(AskKey::Key(self.read_identifier(identifier)?))
    }

    /// `buckets(name, '<width>')`: the view's time at a width the spec
    /// must keep, as the view's own `buckets` item lists it. The spec says
    /// whether `name` is the view's time.
    fn wider_bucket(&mut self, function: &sql::Function) -> Result<AskKey, Reported> {
        let at = line(function);
        let arguments = self.buckets_arguments(function)?;
        let [
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(column)),
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(width)),
        ] = arguments
        else {
            self.refuse(
                at,
                "buckets arguments in an ask, write buckets(<time>, '<width>'),",
                function,
            );
            return Err(Reported);
        };
        if self.refused_keep(width) {
            return Err(Reported);
        }
        let width = self.bucket_width(width);
        let sql::Expr::Identifier(identifier) = column else {
            let message = format!("buckets in an ask reads the view's time by name, not {column}");
            self.error(line(column), message);
            return Err(Reported);
        };
        let name = self.read_identifier(identifier)?;
        Ok(AskKey::WiderBucket {
            name,
            width: width?,
        })
    }

    /// The conditions, and the range from `time >= $n AND time < $m`, a
    /// bound not written leaving its end open. Conditions take `=` and
    /// `IN`, so `>=` and `<` always bound the range; the spec says whether
    /// the name is the view's time.
    fn ask_where(&mut self, selection: Option<&sql::Expr>, parameters: &mut Parameters) -> Where {
        let mut parts = Vec::new();
        if let Some(expression) = selection {
            and_parts(expression, &mut parts);
        }
        let mut filter = Vec::new();
        let mut from = Bound::Open;
        let mut to = Bound::Open;
        let mut bounded: Option<Identifier> = None;
        let mut conditions = Vec::new();
        for part in parts {
            let Some(BucketBound { name, end, value }) = bucket_bound(part) else {
                conditions.push(part);
                continue;
            };
            let Ok(name) = self.read_identifier(name) else {
                continue;
            };
            match &bounded {
                Some(first) if *first != name => {
                    let message = format!("the range bounds one time, not {first} and {name}");
                    self.error(line(part), message);
                    continue;
                }
                Some(_) => {}
                None => bounded = Some(name.clone()),
            }
            let bound = match end {
                End::From => &mut from,
                End::To => &mut to,
            };
            if !matches!(bound, Bound::Open) {
                let operator = comparison_text(end.comparison());
                self.error(line(part), format!("{name} {operator} given twice"));
                continue;
            }
            *bound = match self.instant(value, parameters) {
                Ok(seconds) => Bound::At { seconds },
                Err(Reported) => Bound::Bad,
            };
        }
        for condition in self.without_repeats(conditions) {
            self.read_conditions(condition, &mut filter, &mut Names::Ask(parameters));
        }
        Where {
            filter,
            from,
            to,
            bounded,
        }
    }

    /// `measure op $n` joined by AND, `op` one of `= <> < <= > >=`.
    /// The spec decides whether the name is a measure.
    fn ask_having(
        &mut self,
        having: Option<&sql::Expr>,
        text: &str,
        parameters: &mut Parameters,
    ) -> Vec<MeasureCondition> {
        let mut parts = Vec::new();
        if let Some(expression) = having {
            and_parts(expression, &mut parts);
        }
        let mut conditions = Vec::with_capacity(parts.len());
        for part in self.without_repeats(parts) {
            let sql::Expr::BinaryOp { left, op, right } = part else {
                let what = match part {
                    sql::Expr::Nested(_) => PARENTHESES,
                    _ => MEASURE_CONDITION,
                };
                self.refuse(line(part), what, part);
                continue;
            };
            let Some(comparison) = comparison(op) else {
                self.refuse(line(part), MEASURE_CONDITION, part);
                continue;
            };
            // Both sides are walked before either is judged, so each
            // reports its own errors and the parameter counts as used.
            let measure = self.ask_name(left);
            let value = self.parameter(right, parameters);
            // sqlparser reads != as <>, and prints it so; the text tells.
            if comparison == Comparison::NotEqual
                && spelled_with_bang(written_between(text, left.span().end, right.span().start))
            {
                self.error(line(part), "write <> for not equal, not !=".into());
                continue;
            }
            if let Ok(measure) = measure
                && let Ok(value) = value
            {
                conditions.push(MeasureCondition {
                    measure,
                    comparison,
                    value,
                });
            }
        }
        conditions
    }

    /// A key or measure by the name the view gives it; `buckets` goes
    /// only in SELECT and GROUP BY.
    pub(super) fn ask_name(&mut self, expression: &sql::Expr) -> Result<Identifier, Reported> {
        match self.ask_key(expression)? {
            AskKey::Key(name) => Ok(name),
            AskKey::WiderBucket { .. } => {
                self.error(
                    line(expression),
                    "buckets goes only in SELECT and GROUP BY".into(),
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
        if let Some(digits) = name.strip_prefix('$').and_then(without_leading_zeros) {
            self.error(line(expression), format!("{name}: write ${digits}"));
            return Err(Reported);
        }
        let Some(index) = parameter_index(name, count) else {
            self.error(
                line(expression),
                format!("{name} is not a parameter: there are {count}, $1 on"),
            );
            return Err(Reported);
        };
        parameters.used[index] = true;
        let value = &parameters.values[index];
        if let Literal::Text(text) = value {
            let before = parameters.text_bytes;
            parameters.text_bytes = before.saturating_add(text.len());
            if parameters.text_bytes > MAX_TEXT_BYTES {
                // Said once, at the use that went over.
                if before <= MAX_TEXT_BYTES {
                    self.error(
                        line(expression),
                        format!(
                            "the text parameters, as the ask uses them, come to more than \
                             {MAX_TEXT_BYTES} bytes; use each once"
                        ),
                    );
                }
                return Err(Reported);
            }
        }
        Ok(value.clone())
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
            Literal::Bool(_) => Err(InstantError::Boolean),
            Literal::Decimal { units, scale } => Err(InstantError::NotWholeSecond {
                text: decimal(units, scale),
            }),
        };
        match instant {
            Ok(seconds) => Ok(seconds),
            Err(why) => {
                self.error(
                    line(expression),
                    format!("range bound {expression} must be RFC 3339 or Unix seconds: {why}"),
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

fn rfc3339_seconds(text: &str) -> Result<i64, InstantError> {
    if !rfc3339_form(text) {
        return Err(InstantError::NotItsForm {
            text: text.to_owned(),
        });
    }
    // RFC 3339 reads `-00:00` as an unknown local offset, and `+00:00` is
    // `Z` written a second way.
    if text.ends_with("+00:00") || text.ends_with("-00:00") {
        return Err(InstantError::ZeroOffset {
            text: text.to_owned(),
        });
    }
    match text.parse::<Timestamp>() {
        Ok(at) => Ok(at.as_second()),
        Err(error) => Err(InstantError::NotRfc3339 {
            text: text.to_owned(),
            error,
        }),
    }
}

/// Whether `text` is written `YYYY-MM-DDTHH:MM:SS`, then `Z` or
/// `+HH:MM`/`-HH:MM`, with a second below 60: the one form of RFC 3339
/// an ask takes. jiff reads more, such as `T09Z`, a fraction of a second
/// or a leap second, which it moves to the next minute.
fn rfc3339_form(text: &str) -> bool {
    const SHAPE: &[u8] = b"dddd-dd-ddTdd:dd:dd";
    let bytes = text.as_bytes();
    let Some((head, rest)) = bytes.split_at_checked(SHAPE.len()) else {
        return false;
    };
    for (&byte, &shape) in head.iter().zip(SHAPE) {
        let fits = match shape {
            b'd' => byte.is_ascii_digit(),
            other => byte == other,
        };
        if !fits {
            return false;
        }
    }
    if head[17] >= b'6' {
        return false;
    }
    match rest {
        [b'Z'] => true,
        [
            b'+' | b'-',
            hour_tens,
            hour_ones,
            b':',
            minute_tens,
            minute_ones,
        ] => {
            hour_tens.is_ascii_digit()
                && hour_ones.is_ascii_digit()
                && minute_tens.is_ascii_digit()
                && minute_ones.is_ascii_digit()
        }
        _ => false,
    }
}

/// The comparison `operator` writes, if HAVING takes it.
fn comparison(operator: &sql::BinaryOperator) -> Option<Comparison> {
    match operator {
        sql::BinaryOperator::Eq => Some(Comparison::Equal),
        sql::BinaryOperator::NotEq => Some(Comparison::NotEqual),
        sql::BinaryOperator::Lt => Some(Comparison::Less),
        sql::BinaryOperator::LtEq => Some(Comparison::LessOrEqual),
        sql::BinaryOperator::Gt => Some(Comparison::Greater),
        sql::BinaryOperator::GtEq => Some(Comparison::GreaterOrEqual),
        _ => None,
    }
}

/// The text from `start` to `end`, located as sqlparser does.
fn written_between(text: &str, start: Location, end: Location) -> &str {
    let start = byte_offset(text, start);
    // An end before the start is no text, never a panic.
    let end = byte_offset(text, end).max(start);
    &text[start..end]
}

/// A comment or a trailing `;` in an ask: it is one statement, written
/// one way, so neither has a place in it. A `;` between statements is
/// left to the parse, which says an ask is one `SELECT`. `None` when it
/// holds neither, or when the tokenizer fails, which the parse reports.
fn extra_tokens(query: &str) -> Option<SqlError> {
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, query)
        .tokenize_with_location()
        .ok()?;
    let mut trailing = None;
    for token in tokens {
        match token.token {
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_),
            ) => {
                return Some(refused_token(token.span.start.line, "a comment"));
            }
            Token::Whitespace(_) => {}
            Token::SemiColon => trailing = Some(token.span.start.line),
            _ => trailing = None,
        }
    }
    let line = trailing?;
    Some(refused_token(line, "a ;"))
}

/// `what`, at `line`, refused from an ask.
fn refused_token(line: u64, what: &str) -> SqlError {
    SqlError {
        line: Some(line),
        message: format!("{what}, write the ask without it, {NOT_IN_STRAUM_SQL}"),
    }
}

/// Whether the not-equal operator in `between`, the text between a
/// comparison's sides, is written `!=`, which sqlparser reads as `<>`.
fn spelled_with_bang(between: &str) -> bool {
    let tokenized = Tokenizer::new(&PostgreSqlDialect {}, between).tokenize_with_location();
    let Ok(tokens) = tokenized else {
        return false;
    };
    for token in tokens {
        if token.token == Token::Neq {
            return written_between(between, token.span.start, token.span.end) == "!=";
        }
    }
    false
}

/// Whether an ask can say `name` in each place it names a key or a
/// measure: `SELECT`, `WHERE`, `GROUP BY` and `HAVING`. A word SQL
/// reserves, such as `user` or `all`, reads as SQL's own there.
pub(super) fn askable(name: &Identifier) -> bool {
    let ask = format!("SELECT {name} FROM v WHERE {name} = $1 GROUP BY {name} HAVING {name} > $1");
    let Ok(parsed) = parse_ask(&ask, &[Literal::Int(1)]) else {
        return false;
    };
    let name = name.clone();
    parsed.keys == [AskKey::Key(name.clone())]
        && parsed.filter.len() == 1
        && parsed.filter[0].key == name
        && parsed.having.len() == 1
        && parsed.having[0].measure == name
}

/// `name >= value` or `name < value`; `None` for any other condition.
fn bucket_bound(expression: &sql::Expr) -> Option<BucketBound<'_>> {
    let sql::Expr::BinaryOp { left, op, right } = expression else {
        return None;
    };
    let sql::Expr::Identifier(identifier) = left.as_ref() else {
        return None;
    };
    let end = match op {
        sql::BinaryOperator::GtEq => End::From,
        sql::BinaryOperator::Lt => End::To,
        _ => return None,
    };
    Some(BucketBound {
        name: identifier,
        end,
        value: right,
    })
}
