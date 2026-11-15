//! `GET /status` and `GET /readyz`: what Straum has done, in plain text for
//! people. `routes.rs` takes a `Status` from the app and hands it here;
//! `write` turns it into the lines `straum status` prints, and `why_not`
//! into the one reason `/readyz` gives when Straum is not ready.

use std::fmt::Write as _;
use std::time::SystemTime;

use straum_app::{Failure, Status, format_mebibytes};

use crate::time::rfc3339_of;

/// Labels and values line up after this many characters.
const LABEL_WIDTH: usize = 14;

const UNKNOWN: &str = "unknown until every table is loaded";

/// Why Straum is not ready, or `None` when it is.
pub(crate) fn why_not(status: &Status) -> Option<String> {
    if status.ready {
        return None;
    }
    // No lag means no stream: the last failure, if any, is why.
    let why = match (status.lag, &status.last_failure) {
        (Some(lag), _) => format!("{} s behind PostgreSQL", lag.as_secs()),
        (None, Some(failure)) => failure.error.clone(),
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
    write_line(&mut out, "WAL retained", &wal(status));
    write_line(&mut out, "starts", &status.starts.to_string());
    write_line(&mut out, "failures", &status.failures.to_string());
    write_line(&mut out, "last failure", &last_failure(status));
    write_line(&mut out, "sealed", &sealed(status));
    out
}

fn write_line(out: &mut String, label: &str, value: &str) {
    let label = format!("{label}:");
    // An error of several lines keeps them under its value.
    let value = value.replace('\n', &format!("\n{:LABEL_WIDTH$}", ""));
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

fn last_failure(status: &Status) -> String {
    match &status.last_failure {
        None => "none".to_owned(),
        Some(Failure { at, error }) => format!("{}: {error}", timestamp(*at)),
    }
}

fn sealed(status: &Status) -> String {
    match status.sealed {
        None => "no spill".to_owned(),
        Some(sealed) => format!(
            "{sealed} buckets in the spill, {} unsealed by a change",
            status.unseals
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
                confirmed: Lsn(0x16_B374_D848),
                restart: Lsn(0x16_B000_0000),
            }),
            limits: Limits {
                max_lag: Duration::from_secs(30),
                max_retained_wal: FOUR_GIBIBYTES,
            },
            last_failure: Some(Failure {
                at: UNIX_EPOCH + Duration::from_secs(JAN_1),
                error: "a change no longer fits trips".to_owned(),
            }),
            starts: 2,
            failures: 1,
            sealed: Some(12),
            unseals: 3,
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
starts:       2
failures:     1
last failure: 2024-01-01T00:00:00Z: a change no longer fits trips
sealed:       12 buckets in the spill, 3 unsealed by a change
"
        );
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

    /// No stream, after a failure of two lines.
    fn failed() -> Status {
        Status {
            ready: false,
            applied: None,
            committed: None,
            as_of: None,
            lag: None,
            wal: None,
            last_failure: Some(Failure {
                at: UNIX_EPOCH,
                error: "trips: one\ntrips: two".to_owned(),
            }),
            ..status()
        }
    }

    #[test]
    fn says_the_last_failure_without_a_stream() {
        let failed = failed();
        assert_eq!(why_not(&failed).as_deref(), Some("trips: one\ntrips: two"));
        assert_eq!(
            write(&failed),
            "\
ready:        no, trips: one
              trips: two
applied:      nothing yet
as of:        unknown until every table is loaded
lag:          unknown until every table is loaded
WAL retained: unknown until every table is loaded
starts:       2
failures:     1
last failure: 1970-01-01T00:00:00Z: trips: one
              trips: two
sealed:       12 buckets in the spill, 3 unsealed by a change
"
        );
    }

    #[test]
    fn says_loading_before_a_stream_or_a_failure() {
        let loading = Status {
            last_failure: None,
            ..failed()
        };
        assert_eq!(why_not(&loading).as_deref(), Some("loading"));
        assert_eq!(why_not(&status()), None);
    }

    #[test]
    fn where_the_stream_started_and_broken_clocks() {
        let started = Status {
            committed: None,
            last_failure: Some(Failure {
                at: UNIX_EPOCH + Duration::from_secs(400_000_000_000),
                error: "x".to_owned(),
            }),
            ..status()
        };
        let text = write(&started);
        assert!(
            text.contains("applied:      16/B374D848, where the stream started\n"),
            "{text}"
        );
        assert!(
            text.contains("last failure: an unreadable time: x\n"),
            "{text}"
        );
    }
}
