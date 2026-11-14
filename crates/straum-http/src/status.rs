//! `/status`: what Straum has done, in plain text for people, as
//! `straum status` prints it.

use std::time::SystemTime;

use jiff::Timestamp;
use straum_app::{Failure, Status, mib};

/// Labels and values line up after this many characters.
const LABEL: usize = 14;

pub(crate) fn why_not(status: &Status) -> Option<String> {
    if status.ready {
        return None;
    }
    // No lag means no stream: the last failure, if any, is why.
    Some(match (status.lag, &status.last_failure) {
        (Some(lag), _) => format!("{} s behind PostgreSQL", lag.as_secs()),
        (None, Some(failure)) => failure.error.clone(),
        (None, None) => "loading".to_owned(),
    })
}

pub(crate) fn write(status: &Status) -> String {
    const UNKNOWN: &str = "unknown until every table is loaded";
    let ready = match why_not(status) {
        None => "yes".to_owned(),
        Some(why) => format!("no, {why}"),
    };
    let applied = match (status.applied, status.committed) {
        (None, _) => "nothing yet".to_owned(),
        (Some(lsn), None) => format!("{lsn}, where the stream started"),
        (Some(lsn), Some(time)) => format!("{lsn}, committed {}", timestamp(time)),
    };
    let as_of = match status.as_of {
        None => UNKNOWN.to_owned(),
        Some(at) => format!("{}, every commit before it applied", timestamp(at)),
    };
    let lag = match status.lag {
        None => UNKNOWN.to_owned(),
        Some(lag) => format!(
            "{:.1} s, at most {} s",
            lag.as_secs_f64(),
            status.limits.max_lag.as_secs()
        ),
    };
    let wal = match status.wal {
        None => UNKNOWN.to_owned(),
        Some(wal) => format!(
            "{}, at most {}; PostgreSQL is at {}",
            mib(wal.retained()),
            mib(status.limits.max_retained_wal),
            wal.written
        ),
    };
    let last_failure = match &status.last_failure {
        None => "none".to_owned(),
        Some(Failure { at, error }) => format!("{}: {error}", timestamp(*at)),
    };
    let sealed = match status.sealed {
        None => "no spill".to_owned(),
        Some(sealed) => format!(
            "{sealed} buckets in the spill, {} unsealed by a change",
            status.unseals
        ),
    };
    let mut out = String::new();
    for (label, value) in [
        ("ready", ready),
        ("applied", applied),
        ("as of", as_of),
        ("lag", lag),
        ("WAL retained", wal),
        ("starts", status.starts.to_string()),
        ("failures", status.failures.to_string()),
        ("last failure", last_failure),
        ("sealed", sealed),
    ] {
        let label = format!("{label}:");
        // An error of several lines keeps them under its value.
        let value = value.replace('\n', &format!("\n{:LABEL$}", ""));
        out.push_str(&format!("{label:LABEL$}{value}\n"));
    }
    out
}

fn timestamp(time: SystemTime) -> String {
    // Past jiff's year 9999, which only a broken clock gives.
    Timestamp::try_from(time).map_or_else(|_| "an unreadable time".to_owned(), |at| at.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use straum_app::{Limits, Lsn, Wal};

    use super::*;

    fn status() -> Status {
        Status {
            ready: true,
            applied: Some(Lsn(0x16_B374_D848)),
            committed: Some(UNIX_EPOCH + Duration::from_micros(1_704_153_600_123_456)),
            as_of: Some(UNIX_EPOCH + Duration::from_secs(1_704_153_660)),
            lag: Some(Duration::from_millis(1_300)),
            wal: Some(Wal {
                written: Lsn(0x16_C000_0000),
                confirmed: Lsn(0x16_B374_D848),
                restart: Lsn(0x16_B000_0000),
            }),
            limits: Limits {
                max_lag: Duration::from_secs(30),
                max_retained_wal: 4 << 30,
            },
            last_failure: Some(Failure {
                at: UNIX_EPOCH + Duration::from_secs(1_704_067_200),
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
    fn says_why_not_ready() {
        let behind = Status {
            ready: false,
            lag: Some(Duration::from_secs(45)),
            ..status()
        };
        assert_eq!(why_not(&behind).as_deref(), Some("45 s behind PostgreSQL"));
        assert!(write(&behind).starts_with("ready:        no, 45 s behind PostgreSQL\n"));

        let failed = Status {
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
        };
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

        let loading = Status {
            last_failure: None,
            ..failed
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
