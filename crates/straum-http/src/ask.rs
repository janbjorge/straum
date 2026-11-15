//! The JSON around an ask: its query and the values for its `$n`
//! parameters. Fields Straum doesn't know are errors, never ignored.
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

use straum_domain::Literal;

use crate::json::Json;

/// An ask as sent, before the `ReadAsk` reads its query.
#[derive(Debug, PartialEq)]
pub(crate) struct AskBody {
    pub(crate) query: String,
    pub(crate) parameters: Vec<Literal>,
}

/// Every error found, each naming its field.
pub(crate) fn read(body: &[u8]) -> Result<AskBody, Vec<String>> {
    let json = Json::parse(body).map_err(|error| vec![error])?;
    let Json::Object(fields) = json else {
        return Err(vec![format!("an ask is an object, not {}", json.kind())]);
    };
    let mut errors = Vec::new();
    let mut query = None;
    let mut parameters = Vec::new();
    let mut saw_query = false;
    for (field, value) in fields {
        saw_query |= field == "query";
        match (field.as_str(), value) {
            ("query", Json::Text(text)) => query = Some(text),
            ("query", other) => {
                errors.push(format!("\"query\" must be a string, not {}", other.kind()));
            }
            ("parameters", Json::Array(items)) => {
                for (index, item) in items.iter().enumerate() {
                    match literal(index, item) {
                        Ok(literal) => parameters.push(literal),
                        Err(error) => errors.push(error),
                    }
                }
            }
            ("parameters", other) => errors.push(format!(
                "\"parameters\" must be an array, not {}",
                other.kind()
            )),
            (other, _) => errors.push(format!("unknown field {other:?}")),
        }
    }
    if !saw_query {
        errors.push("\"query\" is missing".into());
    }
    match query {
        Some(query) if errors.is_empty() => Ok(AskBody { query, parameters }),
        _ => Err(errors),
    }
}

fn literal(index: usize, value: &Json) -> Result<Literal, String> {
    match value {
        Json::Int(integer) => Ok(Literal::Int(*integer)),
        Json::Text(text) => Ok(Literal::Text(text.clone())),
        Json::Bool(boolean) => Ok(Literal::Bool(*boolean)),
        // `= NULL` matches nothing in SQL, so it can only be a mistake.
        other => Err(format!(
            "\"parameters[{index}]\" (${}) must be an integer, a string or a boolean, not {}",
            index + 1,
            other.kind()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_str(body: &str) -> Result<AskBody, Vec<String>> {
        read(body.as_bytes())
    }

    #[test]
    fn reads_the_query_and_its_parameters() {
        assert_eq!(
            read_str(r#"{"parameters": [2, "x", false], "query": "SELECT"}"#).unwrap(),
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
            read_str(r#"{"query": "SELECT"}"#).unwrap(),
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
            let errors = read_str(body).unwrap_err();
            assert_eq!(errors, expected, "{body}");
        }
    }
}
