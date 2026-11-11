//! JSON in and out. `serde_json` reads the text; [`Json`] keeps every key
//! as sent, so a key given twice is an error instead of a value dropped.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

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
    Object(Vec<(String, Json)>),
}

impl Json {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Json, String> {
        serde_json::from_slice(bytes).map_err(|e| e.to_string())
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

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Json, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_bool<E>(self, v: bool) -> Result<Json, E> {
        Ok(Json::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Json, E> {
        Ok(Json::Int(v))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Json, E> {
        Ok(i64::try_from(v).map_or(Json::Float(v as f64), Json::Int))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Json, E> {
        Ok(Json::Float(v))
    }

    fn visit_str<E>(self, v: &str) -> Result<Json, E> {
        Ok(Json::Text(v.to_owned()))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Json::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut fields: Vec<(String, Json)> = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            if fields.iter().any(|(k, _)| *k == key) {
                return Err(de::Error::custom(format!("key {key:?} given twice")));
            }
            let value = map.next_value()?;
            fields.push((key, value));
        }
        Ok(Json::Object(fields))
    }
}

pub(crate) fn string(out: &mut Vec<u8>, s: &str) {
    serde_json::to_writer(out, s).expect("a Vec takes every write");
}

/// Appends `n` in units of `10^-scale` as a JSON number, exactly: `1250`
/// at scale 2 is `12.50`.
pub(crate) fn decimal(out: &mut Vec<u8>, n: i128, scale: u32) {
    out.extend_from_slice(straum_domain::decimal(n, scale).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_json() {
        let json =
            Json::parse(br#"{"a": [null, true, -1, 1.5, "x", {}], "b": 18446744073709551615}"#);
        assert_eq!(
            json,
            Ok(Json::Object(vec![
                (
                    "a".into(),
                    Json::Array(vec![
                        Json::Null,
                        Json::Bool(true),
                        Json::Int(-1),
                        Json::Float(1.5),
                        Json::Text("x".into()),
                        Json::Object(Vec::new()),
                    ])
                ),
                ("b".into(), Json::Float(18_446_744_073_709_551_615.0)),
            ]))
        );
        assert_eq!(
            Json::parse(br#"9223372036854775807"#),
            Ok(Json::Int(i64::MAX))
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
            let got = Json::parse(text).unwrap_err();
            assert!(got.starts_with(error), "{got}");
        }
        let deep = "[".repeat(200) + &"]".repeat(200);
        assert!(
            Json::parse(deep.as_bytes())
                .unwrap_err()
                .starts_with("recursion limit")
        );
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
        let mut out = Vec::new();
        string(&mut out, "a \"b\"\n\u{1}ø");
        assert_eq!(out, r#""a \"b\"\n\u0001ø""#.as_bytes());

        // The domain tests the digits; here, that they parse as JSON.
        for (n, scale) in [(-1, 2), (i128::MIN, 0), (i128::MAX, 38)] {
            let mut out = Vec::new();
            decimal(&mut out, n, scale);
            assert!(Json::parse(&out).is_ok(), "{n} at {scale}");
        }
    }
}
