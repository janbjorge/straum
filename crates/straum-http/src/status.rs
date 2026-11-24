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
//!  "caught_up": "16/B374D848",
//!  "commits_per_second": 12, "changes_per_second": 480,
//!  "wal_written": "16/C0000000", "wal_retained_bytes": 268435456,
//!  "max_retained_wal_bytes": 4294967296,
//!  "resident_bytes": 536870912, "max_memory_bytes": 2147483648,
//!  "version": "0.12.0"}
//! ```
//!
//! What is unknown until every table is loaded is null, as is a time past
//! the year 9999, which only a broken clock gives. Memory is null where
//! the system gives no figure; the limit is null when
//! `--max-memory` is not set.

use std::fmt::Write as _;
use std::time::SystemTime;

use straum_app::{Memory, Rate, Status, format_mebibytes};

use crate::VERSION;
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
    let why = match (status.lag, &status.loading) {
        (Some(lag), _) => format!("{} s behind PostgreSQL", lag.as_secs()),
        (None, Some(loading)) => loading.to_string(),
        (None, None) => "loading".to_owned(),
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
    write_line(&mut out, "applying", &applying(status));
    write_line(&mut out, "WAL retained", &wal(status));
    write_line(&mut out, "memory", &memory(status.memory));
    write_line(&mut out, "version", VERSION);
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
    let caught_up = match status.wal {
        Some(wal) => quoted(&wal.applied.to_string()),
        None => NULL.to_owned(),
    };
    fields.push(format!("\"caught_up\":{caught_up}"));
    let (commits, changes) = match status.applying {
        Some(rate) => (
            rate.commits_per_second.to_string(),
            rate.changes_per_second.to_string(),
        ),
        None => (NULL.to_owned(), NULL.to_owned()),
    };
    fields.push(format!("\"commits_per_second\":{commits}"));
    fields.push(format!("\"changes_per_second\":{changes}"));
    let (written, retained) = match status.wal {
        Some(wal) => (quoted(&wal.written.to_string()), wal.retained().to_string()),
        None => (NULL.to_owned(), NULL.to_owned()),
    };
    fields.push(format!("\"wal_written\":{written}"));
    fields.push(format!("\"wal_retained_bytes\":{retained}"));
    let max_retained = status.limits.max_retained_wal;
    fields.push(format!("\"max_retained_wal_bytes\":{max_retained}"));
    let (resident, max_memory) = match status.memory {
        Some(memory) => (
            memory.resident_bytes.to_string(),
            number_or_null(memory.limit_bytes),
        ),
        None => (NULL.to_owned(), NULL.to_owned()),
    };
    fields.push(format!("\"resident_bytes\":{resident}"));
    fields.push(format!("\"max_memory_bytes\":{max_memory}"));
    fields.push(format!("\"version\":{}", quoted(VERSION)));
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

/// The lag, and the position it counts to: every commit up to there is
/// applied, WAL that holds nothing for Straum included, so on a quiet
/// server it runs ahead of `applied`, which only a commit moves.
fn lag(status: &Status) -> String {
    let Some(lag) = status.lag else {
        return UNKNOWN.to_owned();
    };
    let mut text = format!(
        "{:.1} s, at most {} s",
        lag.as_secs_f64(),
        status.limits.max_lag.as_secs()
    );
    if let Some(wal) = status.wal {
        let _ = write!(text, "; caught up to {}", wal.applied);
    }
    text
}

fn applying(status: &Status) -> String {
    match (status.applying, status.lag) {
        (None, None) => UNKNOWN.to_owned(),
        (None, Some(_)) => "unknown for the stream's first second".to_owned(),
        (
            Some(Rate {
                commits_per_second,
                changes_per_second,
            }),
            _,
        ) => format!("{commits_per_second} commits and {changes_per_second} changes a second"),
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

fn number_or_null(number: Option<u64>) -> String {
    match number {
        Some(number) => number.to_string(),
        None => NULL.to_owned(),
    }
}

fn memory(memory: Option<Memory>) -> String {
    let Some(memory) = memory else {
        return "unknown; this system has neither /proc/self/status nor ps".to_owned();
    };
    let resident = format_mebibytes(memory.resident_bytes);
    match memory.limit_bytes {
        None => format!("{resident} resident, no --max-memory set"),
        Some(limit) => format!("{resident} resident, at most {}", format_mebibytes(limit)),
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

    use straum_app::{Estimate, Limits, Loading, Lsn, Memory, Rate, Wal};

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
            applying: Some(Rate {
                commits_per_second: 12,
                changes_per_second: 480,
            }),
            loading: None,
            memory: Some(Memory {
                resident_bytes: 512 << 20,
                limit_bytes: Some(2 << 30),
            }),
            limits: Limits {
                max_lag: Duration::from_secs(30),
                max_retained_wal: FOUR_GIBIBYTES,
                max_load_memory: 1 << 30,
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
lag:          1.3 s, at most 30 s; caught up to 16/B374D848
applying:     12 commits and 480 changes a second
WAL retained: 256.0 MiB, at most 4096.0 MiB; PostgreSQL is at 16/C0000000
memory:       512.0 MiB resident, at most 2048.0 MiB
"
            .to_owned()
                + &version_line()
        );
    }

    /// The last line of the status, which names the version built.
    fn version_line() -> String {
        format!("version:      {VERSION}\n")
    }

    #[test]
    fn memory_without_a_limit_says_so() {
        let unlimited = Status {
            memory: Some(Memory {
                resident_bytes: 512 << 20,
                limit_bytes: None,
            }),
            ..status()
        };
        assert!(
            write(&unlimited).contains("memory:       512.0 MiB resident, no --max-memory set\n")
        );
        let json = json_of(&unlimited);
        assert_eq!(json["resident_bytes"], 512 << 20);
        assert!(json["max_memory_bytes"].is_null());
    }

    #[test]
    fn names_the_version_built() {
        assert!(write(&status()).ends_with(&version_line()));
        assert_eq!(json_of(&status())["version"], VERSION);
    }

    fn json_of(status: &Status) -> serde_json::Value {
        serde_json::from_slice(&json(status)).unwrap()
    }

    #[test]
    fn writes_a_streaming_straum_as_json() {
        let expected = concat!(
            r#"{"ready":true,"why_not":null,"applied":"16/B374D848","#,
            r#""committed":"2024-01-02T00:00:00.123456Z","as_of":"2024-01-02T00:01:00Z","#,
            r#""lag_ms":1300,"max_lag_ms":30000,"caught_up":"16/B374D848","#,
            r#""commits_per_second":12,"changes_per_second":480,"wal_written":"16/C0000000","#,
            r#""wal_retained_bytes":268435456,"max_retained_wal_bytes":4294967296,"#,
            r#""resident_bytes":536870912,"max_memory_bytes":2147483648,"#,
        );
        let expected = format!("{expected}\"version\":\"{VERSION}\"}}\n");
        assert_eq!(String::from_utf8(json(&status())).unwrap(), expected);
    }

    #[test]
    fn json_says_null_for_what_is_unknown() {
        let loading = json_of(&loading());
        assert_eq!(loading["ready"], false);
        assert_eq!(loading["why_not"], "loading");
        for unknown in [
            "applied",
            "committed",
            "as_of",
            "lag_ms",
            "commits_per_second",
            "changes_per_second",
            "caught_up",
        ] {
            assert!(loading[unknown].is_null(), "{unknown}");
        }
        assert!(loading["wal_written"].is_null());
        assert!(loading["wal_retained_bytes"].is_null());
        assert!(loading["resident_bytes"].is_null());
        assert!(loading["max_memory_bytes"].is_null());
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

    /// No stream yet: every table is loading, and no `--max-memory`.
    fn loading() -> Status {
        Status {
            ready: false,
            applied: None,
            committed: None,
            as_of: None,
            lag: None,
            wal: None,
            applying: None,
            memory: None,
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
applying:     unknown until every table is loaded
WAL retained: unknown until every table is loaded
memory:       unknown; this system has neither /proc/self/status nor ps
"
            .to_owned()
                + &version_line()
        );
    }

    /// While a table loads, the reason says which and how far.
    #[test]
    fn says_how_far_the_load_is() {
        let table = Status {
            loading: Some(Loading {
                table: "public.trips".to_owned(),
                position: 2,
                tables: 4,
                estimate: Estimate {
                    rows: Some(1000),
                    bytes: 0,
                },
                rows_read: 410,
                held_bytes: 0,
                max_load_memory: 1 << 30,
            }),
            ..loading()
        };
        let why = "loading public.trips, table 2 of 4, 41% of rows; \
                   commits held 0.0 MiB of 1024.0 MiB";
        assert_eq!(why_not(&table).as_deref(), Some(why));
        assert_eq!(json_of(&table)["why_not"], why);
    }

    #[test]
    fn says_the_rate_is_unknown_in_the_stream_s_first_second() {
        let first_second = Status {
            applying: None,
            ..status()
        };
        let text = write(&first_second);
        assert!(
            text.contains("applying:     unknown for the stream's first second\n"),
            "{text}"
        );
        assert!(json_of(&first_second)["commits_per_second"].is_null());
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
