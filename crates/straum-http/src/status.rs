//! `GET /status` and `GET /readyz`: what Straum has done, in plain text for
//! people. `routes.rs` takes a `Status` from the app and hands it here;
//! `write` turns it into the lines `straum status` prints, `json` into the
//! same facts for a program that sends `Accept: application/json`, and
//! `why_not` into the one reason `/readyz` gives when Straum is not ready:
//!
//! ```json
//! {"ready": true, "why_not": null, "applied": "16/B374D848",
//!  "committed": "2024-01-02T00:00:00.123456Z",
//!  "as_of": "2024-01-02T00:01:00Z", "lag_ms": 1300, "max_lag_ms": 30000,
//!  "wal_written": "16/C0000000", "wal_retained_bytes": 268435456,
//!  "max_retained_wal_bytes": 4294967296}
//! ```
//!
//! What is unknown until every table is loaded is null, as is a time past
//! the year 9999, which only a broken clock gives.

use std::fmt::Write as _;
use std::time::SystemTime;

use straum_app::{Status, format_mebibytes};

use crate::json::{NULL, quoted, quoted_or_null};
use crate::time::rfc3339_of;

/// Labels and values line up after this many characters.
const LABEL_WIDTH: usize = 14;

const UNKNOWN: &str = "unknown until every table is loaded";

/// Why Straum is not ready, or `None` when it is.
pub(crate) fn why_not(status: &Status) -> Option<String> {
    if status.ready {
        return None;
    }
    // No lag means no stream yet: a failed one ends the process.
    let why = match status.lag {
        Some(lag) => format!("{} s behind PostgreSQL", lag.as_secs()),
        None => "loading".to_owned(),
    };
    Some(why)
}

/// The status as `label: value` lines.
pub(crate) fn write(status: &Status) -> String {
    let mut out = String::new();
    write_line(&mut out, "ready", &ready(status));
    write_line(&mut out, "applied", &applied(status));
    write_line(&mut out, "as of", &as_of(status));
    write_line(&mut out, "lag", &lag(status));
    write_line(&mut out, "WAL retained", &wal(status));
    out
}

/// The status as one JSON object.
pub(crate) fn json(status: &Status) -> Vec<u8> {
    let mut fields = Vec::new();
    fields.push(format!("\"ready\":{}", status.ready));
    fields.push(format!(
        "\"why_not\":{}",
        quoted_or_null(why_not(status).as_deref())
    ));
    let applied = match status.applied {
        Some(lsn) => quoted(&lsn.to_string()),
        None => NULL.to_owned(),
    };
    fields.push(format!("\"applied\":{applied}"));
    fields.push(format!("\"committed\":{}", time_or_null(status.committed)));
    fields.push(format!("\"as_of\":{}", time_or_null(status.as_of)));
    let lag_ms = match status.lag {
        Some(lag) => lag.as_millis().to_string(),
        None => NULL.to_owned(),
    };
    fields.push(format!("\"lag_ms\":{lag_ms}"));
    let max_lag_ms = status.limits.max_lag.as_millis();
    fields.push(format!("\"max_lag_ms\":{max_lag_ms}"));
    let (written, retained) = match status.wal {
        Some(wal) => (quoted(&wal.written.to_string()), wal.retained().to_string()),
        None => (NULL.to_owned(), NULL.to_owned()),
    };
    fields.push(format!("\"wal_written\":{written}"));
    fields.push(format!("\"wal_retained_bytes\":{retained}"));
    let max_retained = status.limits.max_retained_wal;
    fields.push(format!("\"max_retained_wal_bytes\":{max_retained}"));
    format!("{{{}}}\n", fields.join(",")).into_bytes()
}

fn time_or_null(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return NULL.to_owned();
    };
    quoted_or_null(rfc3339_of(time).as_deref())
}

fn write_line(out: &mut String, label: &str, value: &str) {
    let label = format!("{label}:");
    let _ = writeln!(out, "{label:LABEL_WIDTH$}{value}");
}

fn ready(status: &Status) -> String {
    match why_not(status) {
        None => "yes".to_owned(),
        Some(why) => format!("no, {why}"),
    }
}

fn applied(status: &Status) -> String {
    match (status.applied, status.committed) {
        (None, _) => "nothing yet".to_owned(),
        (Some(lsn), None) => format!("{lsn}, where the stream started"),
        (Some(lsn), Some(time)) => format!("{lsn}, committed {}", timestamp(time)),
    }
}

fn as_of(status: &Status) -> String {
    match status.as_of {
        None => UNKNOWN.to_owned(),
        Some(at) => format!("{}, every commit before it applied", timestamp(at)),
    }
}

fn lag(status: &Status) -> String {
    match status.lag {
        None => UNKNOWN.to_owned(),
        Some(lag) => format!(
            "{:.1} s, at most {} s",
            lag.as_secs_f64(),
            status.limits.max_lag.as_secs()
        ),
    }
}

fn wal(status: &Status) -> String {
    match status.wal {
        None => UNKNOWN.to_owned(),
        Some(wal) => format!(
            "{}, at most {}; PostgreSQL is at {}",
            format_mebibytes(wal.retained()),
            format_mebibytes(status.limits.max_retained_wal),
            wal.written
        ),
    }
}

fn timestamp(time: SystemTime) -> String {
    match rfc3339_of(time) {
        Some(text) => text,
        // Past jiff's year 9999, which only a broken clock gives.
        None => "an unreadable time".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use straum_app::{Limits, Lsn, Wal};

    use super::*;

    /// 2024-01-01 00:00 UTC, in Unix seconds.
    const JAN_1: u64 = 1_704_067_200;
    const JAN_2: u64 = JAN_1 + 86_400;
    const FOUR_GIBIBYTES: u64 = 4 << 30;

    fn status() -> Status {
        Status {
            ready: true,
            applied: Some(Lsn(0x16_B374_D848)),
            committed: Some(UNIX_EPOCH + Duration::from_micros(1_704_153_600_123_456)),
            as_of: Some(UNIX_EPOCH + Duration::from_secs(JAN_2 + 60)),
            lag: Some(Duration::from_millis(1_300)),
            wal: Some(Wal {
                written: Lsn(0x16_C000_0000),
                applied: Lsn(0x16_B374_D848),
                restart: Lsn(0x16_B000_0000),
            }),
            limits: Limits {
                max_lag: Duration::from_secs(30),
                max_retained_wal: FOUR_GIBIBYTES,
            },
        }
    }

    #[test]
    fn writes_a_streaming_straum() {
        assert_eq!(
            write(&status()),
            "\
ready:        yes
applied:      16/B374D848, committed 2024-01-02T00:00:00.123456Z
as of:        2024-01-02T00:01:00Z, every commit before it applied
lag:          1.3 s, at most 30 s
WAL retained: 256.0 MiB, at most 4096.0 MiB; PostgreSQL is at 16/C0000000
"
        );
    }

    fn json_of(status: &Status) -> serde_json::Value {
        serde_json::from_slice(&json(status)).unwrap()
    }

    #[test]
    fn writes_a_streaming_straum_as_json() {
        let expected = concat!(
            r#"{"ready":true,"why_not":null,"applied":"16/B374D848","#,
            r#""committed":"2024-01-02T00:00:00.123456Z","as_of":"2024-01-02T00:01:00Z","#,
            r#""lag_ms":1300,"max_lag_ms":30000,"wal_written":"16/C0000000","#,
            r#""wal_retained_bytes":268435456,"max_retained_wal_bytes":4294967296}"#,
            "\n",
        );
        assert_eq!(String::from_utf8(json(&status())).unwrap(), expected);
    }

    #[test]
    fn json_says_null_for_what_is_unknown() {
        let loading = json_of(&loading());
        assert_eq!(loading["ready"], false);
        assert_eq!(loading["why_not"], "loading");
        for unknown in ["applied", "committed", "as_of", "lag_ms"] {
            assert!(loading[unknown].is_null(), "{unknown}");
        }
        assert!(loading["wal_written"].is_null());
        assert!(loading["wal_retained_bytes"].is_null());
        let streaming = json_of(&status());
        assert!(streaming["why_not"].is_null());
        assert_eq!(streaming["wal_written"], "16/C0000000");
        let broken_clock = Status {
            as_of: Some(UNIX_EPOCH + Duration::from_secs(400_000_000_000)),
            ..status()
        };
        assert!(json_of(&broken_clock)["as_of"].is_null());
    }

    #[test]
    fn says_how_far_behind() {
        let behind = Status {
            ready: false,
            lag: Some(Duration::from_secs(45)),
            ..status()
        };
        assert_eq!(why_not(&behind).as_deref(), Some("45 s behind PostgreSQL"));
        assert!(write(&behind).starts_with("ready:        no, 45 s behind PostgreSQL\n"));
        assert_eq!(why_not(&status()), None);
    }

    /// No stream yet: every table is loading.
    fn loading() -> Status {
        Status {
            ready: false,
            applied: None,
            committed: None,
            as_of: None,
            lag: None,
            wal: None,
            ..status()
        }
    }

    #[test]
    fn says_loading_before_a_stream() {
        assert_eq!(why_not(&loading()).as_deref(), Some("loading"));
        assert_eq!(why_not(&status()), None);
        assert_eq!(
            write(&loading()),
            "\
ready:        no, loading
applied:      nothing yet
as of:        unknown until every table is loaded
lag:          unknown until every table is loaded
WAL retained: unknown until every table is loaded
"
        );
    }

    #[test]
    fn where_the_stream_started_and_broken_clocks() {
        let started = Status {
            committed: None,
            as_of: Some(UNIX_EPOCH + Duration::from_secs(400_000_000_000)),
            ..status()
        };
        let text = write(&started);
        assert!(
            text.contains("applied:      16/B374D848, where the stream started\n"),
            "{text}"
        );
        assert!(
            text.contains("as of:        an unreadable time, every commit before it applied\n"),
            "{text}"
        );
    }
}
