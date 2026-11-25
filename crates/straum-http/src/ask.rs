//! The JSON around an ask: its query and the values for its `$n`
//! parameters. `read` takes the bytes of a `POST /ask` body, parses them
//! with `json.rs` and checks each field, collecting the errors it finds;
//! `routes.rs` then hands the query to the `ReadAsk` the binary gave.
//! Fields Straum doesn't know are errors, never ignored.
//!
//! ```json
//! {
//!   "query": "SELECT borough, trips FROM trips WHERE vendor_id = $1 AND bucket >= $2 AND bucket < $3 GROUP BY borough",
//!   "parameters": [2, "2024-01-01T00:00:00Z", 1706745600]
//! }
//! ```
//!
//! - `query` is required; what it may say is up to the `ReadAsk` `serve`
//!   is given.
//! - `parameters`: numbers, strings and booleans, `$1` first. A number
//!   with a fraction stays exactly as written, `20.5` being 205 tenths.

use std::fmt;

use straum_domain::{Literal, MAX_SCALE};

use crate::json::{Json, JsonError};

/// An ask as sent, before the `ReadAsk` reads its query.
#[derive(Debug, PartialEq)]
pub(crate) struct AskBody {
    pub(crate) query: String,
    pub(crate) parameters: Vec<Literal>,
}

/// Where a parameter sits in `parameters`, counted from 0; it fills
/// `$n` with n one more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParameterIndex(pub(crate) usize);

/// One thing wrong with an ask's body, naming the field it is in.
#[derive(Debug)]
pub(crate) enum BodyError {
    Json(JsonError),
    NotAnObject {
        kind: &'static str,
    },
    QueryNotText {
        kind: &'static str,
    },
    ParametersNotArray {
        kind: &'static str,
    },
    UnknownField(String),
    QueryMissing,
    /// A parameter that is not a number, a string or a boolean.
    ParameterType {
        index: ParameterIndex,
        kind: &'static str,
    },
    /// A number with more digits than an `i128` holds.
    NumberTooLong {
        index: ParameterIndex,
        number: String,
    },
    /// How many errors came past the first [`MAX_ERRORS`].
    More(usize),
}

impl fmt::Display for BodyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BodyError::Json(error) => error.fmt(formatter),
            BodyError::NotAnObject { kind } => write!(formatter, "an ask is an object, not {kind}"),
            BodyError::QueryNotText { kind } => {
                write!(formatter, "\"query\" must be a string, not {kind}")
            }
            BodyError::ParametersNotArray { kind } => {
                write!(formatter, "\"parameters\" must be an array, not {kind}")
            }
            BodyError::UnknownField(name) => write!(formatter, "unknown field {name:?}"),
            BodyError::QueryMissing => formatter.write_str("\"query\" is missing"),
            BodyError::ParameterType { index, kind } => write!(
                formatter,
                "\"parameters[{}]\" (${}) must be a number, a string or a boolean, not {kind}",
                index.0,
                index.0 + 1
            ),
            BodyError::NumberTooLong { index, number } => write!(
                formatter,
                "\"parameters[{}]\" (${}) {number} has more digits than Straum keeps exactly",
                index.0,
                index.0 + 1
            ),
            BodyError::More(count) => write!(formatter, "and {count} more errors"),
        }
    }
}

/// The most errors a body keeps. A 1 MiB body of nulls holds half a
/// million, and printing each made a 17 MB reply; the rest are counted.
const MAX_ERRORS: usize = 50;

/// A body's first [`MAX_ERRORS`] errors in the order met, and how many
/// came after them.
#[derive(Default)]
struct Errors {
    kept: Vec<BodyError>,
    more: usize,
}

impl Errors {
    fn push(&mut self, error: BodyError) {
        if self.kept.len() == MAX_ERRORS {
            self.more += 1;
            return;
        }
        self.kept.push(error);
    }

    /// The errors kept, then the count of the rest, if any came.
    fn into_list(mut self) -> Vec<BodyError> {
        if self.more > 0 {
            self.kept.push(BodyError::More(self.more));
        }
        self.kept
    }
}

/// Reads an ask's body, or says its first [`MAX_ERRORS`] errors and how
/// many more it has.
pub(crate) fn read(body: &[u8]) -> Result<AskBody, Vec<BodyError>> {
    let json = match Json::parse(body) {
        Ok(json) => json,
        Err(error) => return Err(vec![BodyError::Json(error)]),
    };
    let Json::Object(fields) = json else {
        let kind = json.kind();
        return Err(vec![BodyError::NotAnObject { kind }]);
    };
    let mut errors = Errors::default();
    let mut query = None;
    let mut parameters = Vec::new();
    let mut saw_query = false;
    for field in fields {
        saw_query |= field.name == "query";
        match (field.name.as_str(), field.value) {
            ("query", Json::Text(text)) => query = Some(text),
            ("query", other) => {
                let kind = other.kind();
                errors.push(BodyError::QueryNotText { kind });
            }
            ("parameters", Json::Array(items)) => {
                read_parameters(&items, &mut parameters, &mut errors);
            }
            ("parameters", other) => {
                let kind = other.kind();
                errors.push(BodyError::ParametersNotArray { kind });
            }
            (other, _) => errors.push(BodyError::UnknownField(other.to_owned())),
        }
    }
    if !saw_query {
        errors.push(BodyError::QueryMissing);
    }
    match query {
        Some(query) if errors.kept.is_empty() => Ok(AskBody { query, parameters }),
        _ => Err(errors.into_list()),
    }
}

fn read_parameters(items: &[Json], parameters: &mut Vec<Literal>, errors: &mut Errors) {
    for (position, item) in items.iter().enumerate() {
        match literal(ParameterIndex(position), item) {
            Ok(literal) => parameters.push(literal),
            Err(error) => errors.push(error),
        }
    }
}

fn literal(index: ParameterIndex, value: &Json) -> Result<Literal, BodyError> {
    match value {
        Json::Int(integer) => Ok(Literal::Int(*integer)),
        Json::Text(text) => Ok(Literal::Text(text.clone())),
        Json::Bool(boolean) => Ok(Literal::Bool(*boolean)),
        Json::Number(number) => exact(number).ok_or_else(|| BodyError::NumberTooLong {
            index,
            number: number.clone(),
        }),
        // `= NULL` matches nothing in SQL, so it can only be a mistake.
        other => Err(BodyError::ParameterType {
            index,
            kind: other.kind(),
        }),
    }
}

/// A JSON number as a decimal, exactly: the digits as written, the point
/// moved by the exponent, trailing zeros after it dropped. `None` past
/// what an `i128` holds, or past [`MAX_SCALE`] digits after the point.
fn exact(number: &str) -> Option<Literal> {
    let (mantissa, exponent) = match number.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i64>().ok()?),
        None => (number, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let fraction = fraction.trim_end_matches('0');
    let mut units: i128 = format!("{whole}{fraction}").parse().ok()?;
    let mut scale = i64::try_from(fraction.len()).ok()?.checked_sub(exponent)?;
    // `10e-1` is 1, as `1.0` is. Zero keeps its scale, so that
    // `0e-100000000` is still past MAX_SCALE.
    while scale > 0 && units != 0 && units % 10 == 0 {
        units /= 10;
        scale -= 1;
    }
    if scale >= 0 {
        let scale = u32::try_from(scale).ok()?;
        // Printing a scale past it, as an error does, wrote every zero of
        // `1e-100000000`.
        if scale > MAX_SCALE {
            return None;
        }
        return Some(whole_or_decimal(units, scale));
    }
    let places = u32::try_from(scale.unsigned_abs()).ok()?;
    let units = if units == 0 {
        0
    } else {
        units.checked_mul(10_i128.checked_pow(places)?)?
    };
    Some(whole_or_decimal(units, 0))
}

/// A whole number an `i64` holds as an integer, however it was written:
/// `1.0` and `1e3` are integers, as `1` and `1000` are.
fn whole_or_decimal(units: i128, scale: u32) -> Literal {
    if scale == 0
        && let Ok(integer) = i64::try_from(units)
    {
        return Literal::Int(integer);
    }
    Literal::Decimal { units, scale }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_str(body: &str) -> AskBody {
        read(body.as_bytes()).unwrap()
    }

    /// The errors a body gives, as the reply says them.
    fn errors_of(body: &str) -> Vec<String> {
        let errors = read(body.as_bytes()).unwrap_err();
        let mut lines = Vec::new();
        for error in errors {
            lines.push(error.to_string());
        }
        lines
    }

    #[test]
    fn reads_the_query_and_its_parameters() {
        assert_eq!(
            read_str(r#"{"parameters": [2, "x", false], "query": "SELECT"}"#),
            AskBody {
                query: "SELECT".to_owned(),
                parameters: vec![
                    Literal::Int(2),
                    Literal::Text("x".into()),
                    Literal::Bool(false)
                ]
            }
        );
        assert_eq!(
            read_str(r#"{"query": "SELECT"}"#),
            AskBody {
                query: "SELECT".to_owned(),
                parameters: Vec::new()
            }
        );
    }

    #[test]
    fn numbers_stay_as_written() {
        let cases = [
            ("20.5", 205, 1),
            ("-0.05", -5, 2),
            ("20.50", 205, 1),
            ("25E-1", 25, 1),
            ("9223372036854775808", 9_223_372_036_854_775_808, 0),
            ("1e38", 10_i128.pow(38), 0),
            ("1e-38", 1, 38),
            ("0.00000000000000000000000000000000000001", 1, 38),
            ("15e-1", 15, 1),
            ("1.50e-1", 15, 2),
            ("92233720368547758080e-1", 9_223_372_036_854_775_808, 0),
        ];
        for (number, units, scale) in cases {
            let body = format!(r#"{{"query": "SELECT", "parameters": [{number}]}}"#);
            assert_eq!(
                read_str(&body).parameters,
                [Literal::Decimal { units, scale }],
                "{number}"
            );
        }
    }

    /// A whole number an `i64` holds is an integer, as `1` is, however
    /// it is written.
    #[test]
    fn whole_numbers_are_integers() {
        for (number, integer) in [
            ("1.0", 1),
            ("2.5e3", 2500),
            ("1704067200.000", 1_704_067_200),
            ("0e5", 0),
            ("-3.00", -3),
            ("9.223372036854775807e18", i64::MAX),
            ("10e-1", 1),
            ("100e-2", 1),
            ("1704067200000e-3", 1_704_067_200),
        ] {
            let body = format!(r#"{{"query": "SELECT", "parameters": [{number}]}}"#);
            assert_eq!(
                read_str(&body).parameters,
                [Literal::Int(integer)],
                "{number}"
            );
        }
    }

    #[test]
    fn numbers_past_an_i128_are_refused() {
        let body = r#"{"query": "SELECT", "parameters": [1.5, 1e39, 1e99999999999999999999, 340282366920938463463374607431768211456, 1e-39, 0e-100000000, 1e-100000000]}"#;
        assert_eq!(
            errors_of(body),
            [
                r#""parameters[1]" ($2) 1e+39 has more digits than Straum keeps exactly"#,
                r#""parameters[2]" ($3) 1e+99999999999999999999 has more digits than Straum keeps exactly"#,
                r#""parameters[3]" ($4) 340282366920938463463374607431768211456 has more digits than Straum keeps exactly"#,
                r#""parameters[4]" ($5) 1e-39 has more digits than Straum keeps exactly"#,
                r#""parameters[5]" ($6) 0e-100000000 has more digits than Straum keeps exactly"#,
                r#""parameters[6]" ($7) 1e-100000000 has more digits than Straum keeps exactly"#,
            ]
        );
    }

    /// A body of `count` null parameters.
    fn nulls(count: usize) -> String {
        let parameters = vec!["null"; count].join(", ");
        format!(r#"{{"query": "SELECT", "parameters": [{parameters}]}}"#)
    }

    #[test]
    fn keeps_the_first_50_errors_and_counts_the_rest() {
        let errors = errors_of(&nulls(256));
        assert_eq!(errors.len(), 51);
        assert_eq!(
            errors[49],
            r#""parameters[49]" ($50) must be a number, a string or a boolean, not null"#
        );
        assert_eq!(errors[50], "and 206 more errors");
    }

    #[test]
    fn fifty_errors_have_no_count() {
        let errors = errors_of(&nulls(50));
        assert_eq!(errors.len(), 50);
        assert!(
            errors[49].starts_with(r#""parameters[49]""#),
            "{}",
            errors[49]
        );
    }

    #[test]
    fn every_error_names_its_field() {
        let cases = [
            ("[]", vec!["an ask is an object, not an array"]),
            ("{", vec!["EOF while parsing an object at line 1 column 1"]),
            ("{}", vec![r#""query" is missing"#]),
            (
                r#"{"query": 1, "parameters": {}, "fact": "trips"}"#,
                vec![
                    r#""query" must be a string, not an integer"#,
                    r#""parameters" must be an array, not an object"#,
                    r#"unknown field "fact""#,
                ],
            ),
            (
                r#"{"query": "SELECT", "parameters": [1, null, [], {}]}"#,
                vec![
                    r#""parameters[1]" ($2) must be a number, a string or a boolean, not null"#,
                    r#""parameters[2]" ($3) must be a number, a string or a boolean, not an array"#,
                    r#""parameters[3]" ($4) must be a number, a string or a boolean, not an object"#,
                ],
            ),
        ];
        for (body, expected) in cases {
            assert_eq!(errors_of(body), expected, "{body}");
        }
    }
}
