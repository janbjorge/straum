//! How fast the writer applies the stream. The writer counts each commit
//! it applies, and its changes, into `Applying`; each reading of the
//! server's WAL, about once a second, closes the interval and turns what
//! it counted into a `Rate`, which the status shows.

use std::time::Instant;

const MICROSECONDS_PER_SECOND: u128 = 1_000_000;

/// Commits and changes applied each second, over the last interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub commits_per_second: u64,
    pub changes_per_second: u64,
}

/// What the writer has applied since the interval began, and the rate
/// of the last interval closed.
#[derive(Debug)]
pub(crate) struct Applying {
    commits: u64,
    changes: u64,
    since: Instant,
    /// `None` until the first interval closes.
    pub(crate) rate: Option<Rate>,
}

impl Applying {
    /// An interval that begins at `now`, with no rate yet.
    pub(crate) fn starting(now: Instant) -> Applying {
        Applying {
            commits: 0,
            changes: 0,
            since: now,
            rate: None,
        }
    }

    /// Counts one commit applied, of `changes` changes.
    pub(crate) fn count(&mut self, changes: usize) {
        let changes = u64::try_from(changes).unwrap_or(u64::MAX);
        self.commits = self.commits.saturating_add(1);
        self.changes = self.changes.saturating_add(changes);
    }

    /// Closes the interval at `now`, its counts becoming the rate, and
    /// begins the next. An interval of no time stays open, as no rate
    /// can be taken over it.
    pub(crate) fn close(&mut self, now: Instant) {
        let micros = now.saturating_duration_since(self.since).as_micros();
        if micros == 0 {
            return;
        }
        let rate = Rate {
            commits_per_second: per_second(self.commits, micros),
            changes_per_second: per_second(self.changes, micros),
        };
        *self = Applying {
            rate: Some(rate),
            ..Applying::starting(now)
        };
    }
}

/// `count` over `micros` microseconds, as a count each second, rounded
/// to the nearest: rounded down, one commit in 1.1 s would read as none.
fn per_second(count: u64, micros: u128) -> u64 {
    let rate = (u128::from(count) * MICROSECONDS_PER_SECOND + micros / 2) / micros;
    u64::try_from(rate).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_closed_interval_gives_the_rate() {
        let start = Instant::now();
        let mut applying = Applying::starting(start);
        for changes in [10, 20, 30, 40] {
            applying.count(changes);
        }
        applying.close(start + Duration::from_secs(2));
        let rate = Rate {
            commits_per_second: 2,
            changes_per_second: 50,
        };
        assert_eq!(applying.rate, Some(rate));
        // The next interval counts from zero: nothing applied is a rate
        // of zero, not the last one again.
        applying.close(start + Duration::from_secs(3));
        let idle = Rate {
            commits_per_second: 0,
            changes_per_second: 0,
        };
        assert_eq!(applying.rate, Some(idle));
    }

    #[test]
    fn a_rate_rounds_to_the_nearest() {
        let start = Instant::now();
        let mut applying = Applying::starting(start);
        applying.count(3);
        applying.close(start + Duration::from_millis(1_100));
        let rate = Rate {
            commits_per_second: 1,
            changes_per_second: 3,
        };
        assert_eq!(applying.rate, Some(rate));
    }

    #[test]
    fn no_rate_before_an_interval_closes_or_over_no_time() {
        let start = Instant::now();
        let mut applying = Applying::starting(start);
        applying.count(5);
        assert_eq!(applying.rate, None);
        applying.close(start);
        assert_eq!(applying.rate, None);
        // What an interval of no time counted goes into the next rate.
        applying.close(start + Duration::from_millis(500));
        let rate = Rate {
            commits_per_second: 2,
            changes_per_second: 10,
        };
        assert_eq!(applying.rate, Some(rate));
    }
}
