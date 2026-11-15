//! Helpers the notation test binaries share: the months asks read, names,
//! conditions and keys as the domain spells them, `BASE`, the one valid
//! view most cases edit, and the errors a spec gives, as printed.

// Each test binary compiles this module whole and uses only part of it.
#![allow(dead_code)]

use std::fmt;

use straum_domain::{
    Ask, AskKey, Attribute, ByKey, Condition, Identifier, KeyPart, Literal, Spec, SpecError,
};
use straum_notation::{Error, SqlError, parse};

/// 2024-01-01, 00:00 UTC.
pub const JAN: i64 = 1_704_067_200;
/// 2024-02-01, 00:00 UTC.
pub const FEB: i64 = 1_706_745_600;

/// One fact `t` with a ref `z`, valid as written. Tests swap one piece.
pub const BASE: &str = "CREATE VIEW t WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket,
       f.a AS a, f.z_id AS z_id, z.name AS name, count(*) AS n
FROM public.t AS f
LEFT OUTER JOIN public.z AS z ON z.id = f.z_id
GROUP BY 1, 2, 3, 4";

/// `BASE` with its first `from` replaced by `to`.
pub fn base(from: &str, to: &str) -> String {
    edit(BASE, from, to)
}

/// A name the tests know is valid.
pub fn id(name: &str) -> Identifier {
    Identifier::new(name).unwrap()
}

/// `key IN (values)`, as the domain holds it.
pub fn condition(key: &str, values: Vec<Literal>) -> Condition {
    Condition {
        key: id(key),
        values,
    }
}

/// A key over the fact column of the same name.
pub fn by_key(name: &str) -> ByKey {
    ByKey::Column {
        column: id(name),
        name: id(name),
    }
}

/// A dim column read under the name `name`.
pub fn attribute(name: &str, column: &str) -> Attribute {
    Attribute {
        name: id(name),
        column: id(column),
    }
}

/// `text` with its first `from` replaced by `to`; a `from` it lacks is a
/// broken case, not a pass.
pub fn edit(text: &str, from: &str, to: &str) -> String {
    assert!(text.contains(from), "{from}");
    text.replacen(from, to, 1)
}

/// Each error as printed.
// Any printable errors: the walker's or the spec's.
pub fn strings(items: &[impl fmt::Display]) -> Vec<String> {
    let mut lines = Vec::new();
    for item in items {
        lines.push(item.to_string());
    }
    lines
}

/// Every error `text` gives, Straum SQL or spec, as printed.
pub fn error_lines(text: &str) -> Vec<String> {
    match parse(text) {
        Err(Error::Sql(errors)) => strings(&errors),
        Err(Error::Spec(errors)) => strings(&errors),
        Ok(_) => panic!("parsed:\n{text}"),
    }
}

/// The spec's own errors; any other outcome fails the test.
pub fn spec_errors(text: &str) -> Vec<SpecError> {
    match parse(text) {
        Err(Error::Spec(errors)) => errors,
        other => panic!("expected spec errors, got {other:?}"),
    }
}

/// The walker's errors; any other outcome fails the test.
pub fn sql_errors(text: &str) -> Vec<SqlError> {
    match parse(text) {
        Err(Error::Sql(errors)) => errors,
        other => panic!("expected Straum SQL errors, got {other:?}"),
    }
}

/// Whether any error contains `fragment`.
pub fn any_contains(errors: &[String], fragment: &str) -> bool {
    for error in errors {
        if error.contains(fragment) {
            return true;
        }
    }
    false
}

/// Whether any error's message contains `fragment`, on `line` if given.
pub fn any_message(errors: &[SqlError], line: Option<u64>, fragment: &str) -> bool {
    for error in errors {
        let on_line = match line {
            Some(line) => error.line == line,
            None => true,
        };
        if on_line && error.message.contains(fragment) {
            return true;
        }
    }
    false
}

/// An ask of view `t`'s count by these keys, over January.
pub fn ask_view_t(by: &[&str]) -> Ask {
    let mut keys = Vec::new();
    for key in by {
        keys.push(AskKey::Key(id(key)));
    }
    Ask {
        view: id("t"),
        measures: vec![id("n")],
        keys,
        filter: vec![],
        from: JAN,
        to: FEB,
    }
}

/// The rollup key as text: column names, null-ness tests and `bucket`.
pub fn key_text(spec: &Spec, fact: &str) -> Vec<String> {
    let mut texts = Vec::new();
    for part in spec.fact(fact).unwrap().rollup_key() {
        texts.push(key_part_text(part));
    }
    texts
}

fn key_part_text(part: KeyPart) -> String {
    match part {
        KeyPart::Column(column) => column.to_string(),
        KeyPart::IsNull { column, negated } => {
            format!("{column} is {}null", if negated { "not " } else { "" })
        }
        KeyPart::Bucket => "bucket".into(),
    }
}
