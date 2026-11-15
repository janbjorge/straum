//! JSON in and out. `Json::parse` hands the bytes of a request body to
//! `serde_json`, which calls `JsonVisitor` for each value it meets; the
//! visitor builds a `Json` tree that keeps every key as sent, so a key
//! given twice is an error instead of a value dropped. `quoted` writes a
//! string the other way, for the answers `answer.rs` and `views.rs` build.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

/// One JSON value as sent, before `ask.rs` checks what it holds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Int(i64),
    /// A number with a fraction or exponent, or an integer past `i64`.
    Float(f64),
    Text(String),
    Array(Vec<Json>),
    /// In the order sent, each key once.
    Object(Vec<Field>),
}

/// One key of a JSON object and its value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Field {
    pub(crate) name: String,
    pub(crate) value: Json,
}

/// Why a body is not JSON, in `serde_json`'s words with the line and column.
#[derive(Debug)]
pub(crate) struct JsonError {
    error: serde_json::Error,
}

impl fmt::Display for JsonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl Json {
    /// Reads `bytes` as one JSON value.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Json, JsonError> {
        match serde_json::from_slice(bytes) {
            Ok(json) => Ok(json),
            Err(error) => Err(JsonError { error }),
        }
    }

    /// What the value is, for errors.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::Bool(_) => "a boolean",
            Json::Int(_) => "an integer",
            Json::Float(_) => "a number that isn't an integer",
            Json::Text(_) => "a string",
            Json::Array(_) => "an array",
            Json::Object(_) => "an object",
        }
    }
}

// The lifetime `'de` is serde's: the text being read lives that long.
impl<'de> Deserialize<'de> for Json {
    // Generic over serde's deserializer, as the trait requires.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Json, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

/// What serde calls back with each value it reads.
struct JsonVisitor;

// The generic error types `E` and `A` below are serde's, fixed by the trait.
impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Json, E> {
        Ok(Json::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Json, E> {
        Ok(Json::Int(value))
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "an integer past i64 is only ever named by its kind, never read as a number"
    )]
    fn visit_u64<E>(self, value: u64) -> Result<Json, E> {
        match i64::try_from(value) {
            Ok(integer) => Ok(Json::Int(integer)),
            Err(_) => Ok(Json::Float(value as f64)),
        }
    }

    fn visit_f64<E>(self, value: f64) -> Result<Json, E> {
        Ok(Json::Float(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Json, E> {
        Ok(Json::Text(value.to_owned()))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Json, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = sequence.next_element()? {
            items.push(item);
        }
        Ok(Json::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut fields: Vec<Field> = Vec::new();
        while let Some(name) = map.next_key::<String>()? {
            if has_field(&fields, &name) {
                return Err(de::Error::custom(format!("key {name:?} given twice")));
            }
            let value = map.next_value()?;
            fields.push(Field { name, value });
        }
        Ok(Json::Object(fields))
    }
}

fn has_field(fields: &[Field], name: &str) -> bool {
    for field in fields {
        if field.name == name {
            return true;
        }
    }
    false
}

/// `text` as a JSON string, with its quotes.
pub(crate) fn quoted(text: &str) -> String {
    serde_json::to_string(text).expect("a str is a JSON string")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, value: Json) -> Field {
        Field {
            name: name.to_owned(),
            value,
        }
    }

    fn parse_error(text: &[u8]) -> String {
        let Err(error) = Json::parse(text) else {
            panic!("{text:?} parsed");
        };
        error.to_string()
    }

    #[test]
    fn reads_json() {
        let json =
            Json::parse(br#"{"a": [null, true, -1, 1.5, "x", {}], "b": 18446744073709551615}"#);
        let array = Json::Array(vec![
            Json::Null,
            Json::Bool(true),
            Json::Int(-1),
            Json::Float(1.5),
            Json::Text("x".into()),
            Json::Object(Vec::new()),
        ]);
        let expected = Json::Object(vec![
            field("a", array),
            field("b", Json::Float(18_446_744_073_709_551_615.0)),
        ]);
        assert_eq!(json.unwrap(), expected);
        assert_eq!(
            Json::parse(b"9223372036854775807").unwrap(),
            Json::Int(i64::MAX)
        );
    }

    #[test]
    fn json_errors() {
        let cases: [(&[u8], &str); 4] = [
            (
                br#"{"a": 1, "a": 2}"#,
                r#"key "a" given twice at line 1 column 12"#,
            ),
            (br#"{"a": {"b": 1, "b": 1}}"#, r#"key "b" given twice"#),
            (b"{", "EOF while parsing an object at line 1 column 1"),
            (b"[1] 2", "trailing characters at line 1 column 5"),
        ];
        for (text, error) in cases {
            let got = parse_error(text);
            assert!(got.starts_with(error), "{got}");
        }
        let deep = "[".repeat(200) + &"]".repeat(200);
        assert!(parse_error(deep.as_bytes()).starts_with("recursion limit"));
    }

    #[test]
    fn kinds() {
        let values = [
            Json::Null,
            Json::Bool(false),
            Json::Int(0),
            Json::Float(0.5),
            Json::Text(String::new()),
            Json::Array(Vec::new()),
            Json::Object(Vec::new()),
        ];
        let kinds: Vec<&str> = values.iter().map(Json::kind).collect();
        assert_eq!(
            kinds,
            [
                "null",
                "a boolean",
                "an integer",
                "a number that isn't an integer",
                "a string",
                "an array",
                "an object"
            ]
        );
    }

    #[test]
    fn writes_strings_and_decimals() {
        assert_eq!(quoted("a \"b\"\n\u{1}ø"), r#""a \"b\"\n\u0001ø""#);

        // The domain tests the digits; here, that they parse as JSON.
        for (number, scale) in [(-1, 2), (i128::MIN, 0), (i128::MAX, 38)] {
            let decimal = straum_domain::decimal(number, scale);
            assert!(
                Json::parse(decimal.as_bytes()).is_ok(),
                "{number} at {scale}"
            );
        }
    }
}
