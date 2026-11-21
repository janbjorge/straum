//! Times as RFC 3339 text in UTC, for the answers, views and status the
//! other files write. jiff writes only the years -9999 to 9999, so
//! `rfc3339` and `rfc3339_of` return `None` past them and their caller
//! says what such a time reads as, or `rfc3339_or_unix_second` writes it
//! as its Unix second.

use std::time::SystemTime;

use jiff::Timestamp;

/// A Unix second as RFC 3339, if jiff can write it.
pub(crate) fn rfc3339(seconds: i64) -> Option<String> {
    let Ok(at) = Timestamp::from_second(seconds) else {
        return None;
    };
    Some(at.to_string())
}

/// A Unix second as RFC 3339, or as `Unix second N` past jiff's years.
pub(crate) fn rfc3339_or_unix_second(seconds: i64) -> String {
    match rfc3339(seconds) {
        Some(text) => text,
        // PostgreSQL keeps times past jiff's year 9999, and an open end
        // reads them.
        None => format!("Unix second {seconds}"),
    }
}

/// A clock reading as RFC 3339, if jiff can write it.
pub(crate) fn rfc3339_of(time: SystemTime) -> Option<String> {
    let Ok(at) = Timestamp::try_from(time) else {
        return None;
    };
    Some(at.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_past_jiff_is_written_as_a_unix_second() {
        assert_eq!(rfc3339_or_unix_second(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339_or_unix_second(i64::MAX),
            "Unix second 9223372036854775807"
        );
    }
}
