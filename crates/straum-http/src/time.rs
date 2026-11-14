//! Times as RFC 3339 in UTC. jiff writes only the years -9999 to 9999, so
//! each caller says what a time past them reads as.

use std::time::SystemTime;

use jiff::Timestamp;

pub(crate) fn rfc3339(seconds: i64) -> Option<String> {
    Timestamp::from_second(seconds)
        .ok()
        .map(|at| at.to_string())
}

pub(crate) fn rfc3339_of(time: SystemTime) -> Option<String> {
    Timestamp::try_from(time).ok().map(|at| at.to_string())
}
