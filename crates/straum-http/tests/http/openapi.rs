//! The OpenAPI document against the replies it describes. Each JSON reply
//! is walked with the schema the document gives its route and status:
//! every required field there, no field the schema lacks, each value of
//! its type. The walk knows only the parts of JSON Schema the document
//! uses, so a schema keyword it doesn't know fails the test.

use serde_json::{Map, Value};

use crate::fake::writer;
use crate::{JSON, call, first_hour, request, serve, started};

const DOCUMENT: &str = include_str!("../../openapi.json");

/// The keywords `departures` checks or may pass by.
const KNOWN_KEYWORDS: [&str; 12] = [
    "$ref",
    "type",
    "required",
    "properties",
    "additionalProperties",
    "items",
    "enum",
    "description",
    "format",
    "minimum",
    "maximum",
    "default",
];

fn document() -> Value {
    serde_json::from_str(DOCUMENT).unwrap()
}

/// The schema of a JSON reply to `method path` with `status`.
fn reply_schema<'a>(document: &'a Value, method: &str, path: &str, status: u16) -> &'a Value {
    let response = &document["paths"][path][method]["responses"][status.to_string()];
    let schema = &resolve(document, response)["content"][JSON]["schema"];
    assert!(
        !schema.is_null(),
        "no JSON schema for {method} {path} {status}"
    );
    schema
}

/// The node a `$ref` points at, or the node itself.
fn resolve<'a>(document: &'a Value, node: &'a Value) -> &'a Value {
    let Some(reference) = node.get("$ref").and_then(Value::as_str) else {
        return node;
    };
    let mut found = document;
    for part in reference.trim_start_matches("#/").split('/') {
        found = &found[part];
    }
    assert!(!found.is_null(), "{reference} points at nothing");
    found
}

/// Where `value` departs from `schema`, a line each, `at` naming where
/// in the reply it is.
fn departures(document: &Value, schema: &Value, value: &Value, at: &str) -> Vec<String> {
    let schema = resolve(document, schema);
    let Some(keywords) = schema.as_object() else {
        return vec![format!("{at}: the schema is not an object")];
    };
    for keyword in keywords.keys() {
        assert!(
            KNOWN_KEYWORDS.contains(&keyword.as_str()),
            "the test doesn't check {keyword}"
        );
    }
    if !of_type(schema.get("type"), value) {
        return vec![format!("{at}: {value} is not of type {}", schema["type"])];
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return vec![format!("{at}: {value} is not one of {allowed:?}")];
    }
    if let Some(object) = value.as_object() {
        return object_departures(document, schema, object, at);
    }
    let mut found = Vec::new();
    if let (Some(items), Some(array)) = (schema.get("items"), value.as_array()) {
        for (index, item) in array.iter().enumerate() {
            found.extend(departures(document, items, item, &format!("{at}[{index}]")));
        }
    }
    found
}

fn object_departures(
    document: &Value,
    schema: &Value,
    object: &Map<String, Value>,
    at: &str,
) -> Vec<String> {
    let mut found = Vec::new();
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for name in required {
            let name = name.as_str().unwrap();
            if !object.contains_key(name) {
                found.push(format!("{at}: {name} is missing"));
            }
        }
    }
    let properties = schema.get("properties");
    let additional = schema.get("additionalProperties");
    for (name, field) in object {
        let place = format!("{at}.{name}");
        let known = match properties {
            Some(properties) => properties.get(name),
            None => None,
        };
        let field_schema = match (known, additional) {
            (Some(field_schema), _) => field_schema,
            (None, Some(Value::Bool(false))) => {
                found.push(format!("{place} is not in the document"));
                continue;
            }
            (None, Some(other)) => other,
            (None, None) => continue,
        };
        found.extend(departures(document, field_schema, field, &place));
    }
    found
}

/// Whether `value` is of the type, or one of the types, the schema
/// names; any type when it names none.
fn of_type(named: Option<&Value>, value: &Value) -> bool {
    let Some(named) = named else {
        return true;
    };
    if let Some(name) = named.as_str() {
        return is_type(name, value);
    }
    for name in named.as_array().unwrap() {
        if is_type(name.as_str().unwrap(), value) {
            return true;
        }
    }
    false
}

fn is_type(name: &str, value: &Value) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_u64() || value.is_i64(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        other => panic!("no JSON type {other}"),
    }
}

/// The departures of a JSON reply body from its route's schema.
fn reply_departures(method: &str, path: &str, status: u16, body: &str) -> Vec<String> {
    let document = document();
    let schema = reply_schema(&document, method, path, status);
    let value: Value = serde_json::from_str(body).unwrap();
    departures(&document, schema, &value, "reply")
}

fn assert_matches(method: &str, path: &str, status: u16, body: &str) {
    let found = reply_departures(method, path, status, body);
    assert!(found.is_empty(), "{method} {path} {status}: {found:?}");
}

#[test]
fn serves_its_openapi_document() {
    let server = serve(writer().app().clone());
    let reply = call(server.address, "GET", "/openapi.json", "");
    assert_eq!(reply.status, 200);
    assert!(reply.head.contains("content-type: application/json"));
    assert_eq!(reply.body, DOCUMENT);
    assert_eq!(document()["openapi"], "3.1.0");
    server.stop();
}

#[test]
fn every_json_reply_matches_the_document() {
    let loading = serve(writer().app().clone());
    let status = request(loading.address, "GET", "/status", Some(JSON), "");
    assert_matches("get", "/status", 200, &status.body);
    let refused = request(loading.address, "GET", "/views", Some(JSON), "");
    assert_eq!(refused.status, 503);
    assert_matches("get", "/views", 503, &refused.body);
    loading.stop();

    let (_writer, server) = started(writer());
    let status = request(server.address, "GET", "/status", Some(JSON), "");
    assert_matches("get", "/status", 200, &status.body);
    let views = request(server.address, "GET", "/views", Some(JSON), "");
    assert_matches("get", "/views", 200, &views.body);
    let answer = call(server.address, "POST", "/ask", &first_hour());
    assert_eq!(answer.status, 200);
    assert_matches("post", "/ask", 200, &answer.body);
    let error = call(server.address, "POST", "/ask", "{");
    assert_eq!(error.status, 400);
    assert_matches("post", "/ask", 400, &error.body);
    server.stop();
}

#[test]
fn a_reply_the_document_does_not_describe_departs() {
    let (_writer, server) = started(writer());
    let body = request(server.address, "GET", "/status", Some(JSON), "").body;
    server.stop();
    let honest: Value = serde_json::from_str(&body).unwrap();
    let mut missing = honest.clone();
    missing.as_object_mut().unwrap().remove("ready");
    let mut extra = honest.clone();
    extra["uptime_ms"] = Value::from(1);
    let mut wrong_type = honest;
    wrong_type["lag_ms"] = Value::from("soon");
    for (broken, departure) in [
        (missing, "reply: ready is missing"),
        (extra, "reply.uptime_ms is not in the document"),
        (wrong_type, "reply.lag_ms: \"soon\" is not of type"),
    ] {
        let found = reply_departures("get", "/status", 200, &broken.to_string());
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with(departure), "{found:?}");
    }
}
