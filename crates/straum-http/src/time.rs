//! Times as RFC 3339 text in UTC, for the answers, views and status the
//! other files write. jiff writes only the years -9999 to 9999, so each
//! function returns `None` past them and its caller says what such a time
//! reads as.

use std::time::SystemTime;

use jiff::Timestamp;

/// A Unix second as RFC 3339, if jiff can write it.
pub(crate) fn rfc3339(seconds: i64) -> Option<String> {
    let Ok(at) = Timestamp::from_second(seconds) else {
        return None;
    };
    Some(at.to_string())
}

/// A clock reading as RFC 3339, if jiff can write it.
pub(crate) fn rfc3339_of(time: SystemTime) -> Option<String> {
    let Ok(at) = Timestamp::try_from(time) else {
        return None;
    };
    Some(at.to_string())
}
