// Each test binary compiles this module whole and uses only part of it.
#![allow(dead_code)]

use std::fmt;

use straum_domain::{Condition, Identifier, Literal};
use straum_notation::{Error, parse};

// 2024-01-01 and 2024-02-01, 00:00 UTC.
pub const JAN: i64 = 1_704_067_200;
pub const FEB: i64 = 1_706_745_600;

pub fn id(s: &str) -> Identifier {
    Identifier::new(s).unwrap()
}

pub fn cond(key: &str, values: Vec<Literal>) -> Condition {
    Condition {
        key: id(key),
        values,
    }
}

/// `text` with its first `from` replaced by `to`; a `from` it lacks is a
/// broken case, not a pass.
pub fn edit(text: &str, from: &str, to: &str) -> String {
    assert!(text.contains(from), "{from}");
    text.replacen(from, to, 1)
}

pub fn strings(items: &[impl fmt::Display]) -> Vec<String> {
    items.iter().map(ToString::to_string).collect()
}

/// Every error `src` gives, Straum SQL or spec, as printed.
pub fn error_lines(src: &str) -> Vec<String> {
    match parse(src) {
        Err(Error::Sql(errors)) => strings(&errors),
        Err(Error::Spec(errors)) => strings(&errors),
        Ok(_) => panic!("parsed:\n{src}"),
    }
}
