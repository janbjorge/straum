//! The JSON around an ask: its query and the values for its `$n`
//! parameters. `read` takes the bytes of a `POST /ask` body, parses them
//! with `json.rs` and checks each field, collecting every error it finds;
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
//! - `parameters`: integers, strings and booleans, `$1` first.

use std::fmt;

use straum_domain::Literal;

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
    /// A parameter that is not an integer, a string or a boolean.
    ParameterType {
        index: ParameterIndex,
        kind: &'static str,
    },
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
                "\"parameters[{}]\" (${}) must be an integer, a string or a boolean, not {kind}",
                index.0,
                index.0 + 1
            ),
        }
    }
}

/// Reads an ask's body, or says every error found.
pub(crate) fn read(body: &[u8]) -> Result<AskBody, Vec<BodyError>> {
    let json = match Json::parse(body) {
        Ok(json) => json,
        Err(error) => return Err(vec![BodyError::Json(error)]),
    };
    let Json::Object(fields) = json else {
        let kind = json.kind();
        return Err(vec![BodyError::NotAnObject { kind }]);
    };
    let mut errors = Vec::new();
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
        Some(query) if errors.is_empty() => Ok(AskBody { query, parameters }),
        _ => Err(errors),
    }
}

fn read_parameters(items: &[Json], parameters: &mut Vec<Literal>, errors: &mut Vec<BodyError>) {
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
        // `= NULL` matches nothing in SQL, so it can only be a mistake.
        other => Err(BodyError::ParameterType {
            index,
            kind: other.kind(),
        }),
    }
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
                r#"{"query": "SELECT", "parameters": [1, null, 2.5, [], {}]}"#,
                vec![
                    r#""parameters[1]" ($2) must be an integer, a string or a boolean, not null"#,
                    r#""parameters[2]" ($3) must be an integer, a string or a boolean, not a number that isn't an integer"#,
                    r#""parameters[3]" ($4) must be an integer, a string or a boolean, not an array"#,
                    r#""parameters[4]" ($5) must be an integer, a string or a boolean, not an object"#,
                ],
            ),
        ];
        for (body, expected) in cases {
            assert_eq!(errors_of(body), expected, "{body}");
        }
    }
}
