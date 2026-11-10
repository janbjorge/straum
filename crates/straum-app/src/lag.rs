//! How far straum is behind the server, from readings of the server's WAL
//! position a second or so apart.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::source::{Lsn, Wal};

/// Readings kept while the slot hasn't confirmed them: an hour's worth.
/// Past this the newest two merge, which can only make the lag longer.
const READINGS: usize = 3_600;

#[derive(Debug)]
pub(crate) struct Lag {
    /// The server's position at each reading the slot hasn't confirmed
    /// yet, oldest first.
    unconfirmed: VecDeque<(Instant, Lsn)>,
    /// The last reading the slot had confirmed all of. WAL it hasn't
    /// confirmed was written after it.
    caught_up: Instant,
}

impl Lag {
    /// Nothing unconfirmed as of `at`.
    pub(crate) fn new(at: Instant) -> Lag {
        Lag {
            unconfirmed: VecDeque::new(),
            caught_up: at,
        }
    }

    pub(crate) fn read(&mut self, wal: &Wal, at: Instant) {
        let full = self.unconfirmed.len() == READINGS;
        match self.unconfirmed.back_mut() {
            // The later of two readings at one position is the closer
            // bound.
            Some((when, lsn)) if *lsn >= wal.server => *when = at,
            Some((_, lsn)) if full => *lsn = wal.server,
            _ => self.unconfirmed.push_back((at, wal.server)),
        }
        while let Some(&(when, lsn)) = self.unconfirmed.front()
            && lsn <= wal.confirmed
        {
            self.caught_up = when;
            self.unconfirmed.pop_front();
        }
    }

    /// When the slot was last known to have confirmed all the WAL there
    /// was.
    pub(crate) fn caught_up(&self) -> Instant {
        self.caught_up
    }

    /// The time since the slot was last known to have confirmed all the
    /// WAL there was: zero when it had at the last reading. Readings a
    /// second apart make it up to a second longer than it is, never
    /// shorter.
    pub(crate) fn behind(&self, now: Instant) -> Duration {
        if self.unconfirmed.is_empty() {
            return Duration::ZERO;
        }
        now.saturating_duration_since(self.caught_up)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wal(server: u64, confirmed: u64) -> Wal {
        Wal {
            server: Lsn(server),
            confirmed: Lsn(confirmed),
            restart: Lsn(confirmed),
        }
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn behind_since_last_caught_up() {
        let t = Instant::now();
        let mut lag = Lag::new(t);
        assert_eq!(lag.behind(t + secs(5)), secs(0), "nothing read yet");

        lag.read(&wal(10, 10), t + secs(1));
        assert_eq!(lag.behind(t + secs(9)), secs(0));
        lag.read(&wal(20, 10), t + secs(2));
        assert_eq!(lag.behind(t + secs(9)), secs(8));
        // Confirming part of the WAL moves the bound to the reading it
        // covers.
        lag.read(&wal(30, 20), t + secs(3));
        assert_eq!(lag.behind(t + secs(9)), secs(7));
        lag.read(&wal(30, 25), t + secs(4));
        assert_eq!(lag.behind(t + secs(9)), secs(7));
        lag.read(&wal(30, 30), t + secs(5));
        assert_eq!(lag.behind(t + secs(9)), secs(0));
        // A clock read before the bound reads as no time.
        lag.read(&wal(40, 30), t + secs(6));
        assert_eq!(lag.behind(t), secs(0));
    }

    #[test]
    fn the_first_reading_counts_from_the_start() {
        // WAL written while the tables loaded is as old as the load.
        let t = Instant::now();
        let mut lag = Lag::new(t);
        lag.read(&wal(50, 10), t + secs(40));
        assert_eq!(lag.behind(t + secs(41)), secs(41));
    }

    /// Every reading kept. The WAL the slot hasn't confirmed was written
    /// after the last reading the slot had confirmed all of.
    struct Naive {
        start: Instant,
        readings: Vec<(Instant, Wal)>,
    }

    impl Naive {
        fn behind(&self, now: Instant) -> Duration {
            let Some((_, last)) = self.readings.last() else {
                return Duration::ZERO;
            };
            let confirmed = last.confirmed;
            if last.server <= confirmed {
                return Duration::ZERO;
            }
            let since = (self.readings.iter().rev())
                .find(|(_, w)| w.server <= confirmed)
                .map_or(self.start, |(at, _)| *at);
            now.saturating_duration_since(since)
        }
    }

    /// splitmix64.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (z ^ (z >> 31)) % n
        }
    }

    #[test]
    fn matches_every_reading_kept() {
        let start = Instant::now();
        for seed in 0..200 {
            let mut rng = Rng(seed);
            let mut lag = Lag::new(start);
            let mut naive = Naive {
                start,
                readings: Vec::new(),
            };
            let (mut at, mut server, mut confirmed) = (start, 100, 100);
            for _ in 0..200 {
                at += Duration::from_millis(rng.below(2_000));
                // The server stands still now and then; the slot confirms
                // some, all or none of what it has.
                server += rng.below(3) * rng.below(50);
                confirmed += rng.below(server - confirmed + 1) * rng.below(2);
                let reading = wal(server, confirmed);
                lag.read(&reading, at);
                naive.readings.push((at, reading));
                let now = at + Duration::from_millis(rng.below(3_000));
                assert_eq!(lag.behind(now), naive.behind(now), "seed {seed}");
            }
        }
    }

    #[test]
    fn a_stuck_slot_keeps_a_bounded_number_of_readings() {
        let start = Instant::now();
        let mut lag = Lag::new(start);
        let mut naive = Naive {
            start,
            readings: Vec::new(),
        };
        let total = READINGS as u64 + 500;
        for i in 1..=total {
            let reading = wal(100 + i, 100);
            let at = start + secs(i);
            lag.read(&reading, at);
            naive.readings.push((at, reading));
        }
        assert_eq!(lag.unconfirmed.len(), READINGS);
        let end = start + secs(total);
        assert_eq!(lag.behind(end), naive.behind(end));
        // Confirming up to a merged reading: never shorter than the truth.
        for confirmed in [100 + READINGS as u64, 100 + total - 1] {
            let reading = wal(100 + total, confirmed);
            lag.read(&reading, end);
            naive.readings.push((end, reading));
            assert!(lag.behind(end) >= naive.behind(end), "{confirmed}");
        }
        assert!(lag.behind(end) > naive.behind(end));
    }
}
