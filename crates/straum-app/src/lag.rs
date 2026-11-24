//! How far Straum is behind the server. The writer reads the server's WAL
//! position a second or so apart and hands each reading to `Lag`, which
//! keeps the ones Straum hasn't applied yet; `App::status` asks it how
//! long ago Straum was last caught up.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::source::{Lsn, Wal};

/// Readings kept while Straum hasn't applied them: an hour's worth.
/// Past this the newest two merge, which can only make the lag longer.
const READINGS: usize = 3_600;

/// The WAL readings that tell how far behind the server Straum is.
#[derive(Debug)]
pub(crate) struct Lag {
    /// The server's position at each reading Straum hasn't applied
    /// yet, oldest first.
    unapplied: VecDeque<Reading>,
    /// The last reading Straum had applied all of. WAL it hasn't
    /// applied was written after it.
    caught_up: Instant,
}

#[derive(Debug)]
struct Reading {
    at: Instant,
    written: Lsn,
}

impl Lag {
    /// Nothing unapplied as of `at`.
    pub(crate) fn new(at: Instant) -> Lag {
        Lag {
            unapplied: VecDeque::new(),
            caught_up: at,
        }
    }

    /// Takes a reading of the server's WAL, made at `at`.
    pub(crate) fn read(&mut self, wal: &Wal, at: Instant) {
        let full = self.unapplied.len() == READINGS;
        match self.unapplied.back_mut() {
            // The later of two readings at one position is the closer
            // bound.
            Some(last) if last.written >= wal.written => last.at = at,
            Some(last) if full => last.written = wal.written,
            _ => self.unapplied.push_back(Reading {
                at,
                written: wal.written,
            }),
        }
        while let Some(oldest) = self.unapplied.front()
            && oldest.written <= wal.applied
        {
            self.caught_up = oldest.at;
            self.unapplied.pop_front();
        }
    }

    /// The time since Straum was last known to have applied all the
    /// WAL there was, at the reading that said so. Readings a second apart
    /// make it up to a second longer than it is, never shorter; when they
    /// stop, as on a silent server, it grows, since nothing is known since.
    pub(crate) fn behind(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.caught_up)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wal(server: u64, applied: u64) -> Wal {
        Wal {
            written: Lsn(server),
            applied: Lsn(applied),
            restart: Lsn(applied),
        }
    }

    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    #[test]
    fn behind_since_last_caught_up() {
        let start = Instant::now();
        let mut lag = Lag::new(start);
        assert_eq!(lag.behind(start + secs(5)), secs(5), "nothing read yet");

        // Caught up at the reading, a second in: nothing is known since.
        lag.read(&wal(10, 10), start + secs(1));
        assert_eq!(lag.behind(start + secs(9)), secs(8));
        lag.read(&wal(20, 10), start + secs(2));
        assert_eq!(lag.behind(start + secs(9)), secs(8));
        // Confirming part of the WAL moves the bound to the reading it
        // covers.
        lag.read(&wal(30, 20), start + secs(3));
        assert_eq!(lag.behind(start + secs(9)), secs(7));
        lag.read(&wal(30, 25), start + secs(4));
        assert_eq!(lag.behind(start + secs(9)), secs(7));
        lag.read(&wal(30, 30), start + secs(5));
        assert_eq!(lag.behind(start + secs(9)), secs(4));
        assert_eq!(lag.behind(start + secs(5)), secs(0));
        // A clock read before the bound reads as no time.
        lag.read(&wal(40, 30), start + secs(6));
        assert_eq!(lag.behind(start), secs(0));
    }

    #[test]
    fn the_first_reading_counts_from_the_start() {
        // WAL written while the tables loaded is as old as the load.
        let start = Instant::now();
        let mut lag = Lag::new(start);
        lag.read(&wal(50, 10), start + secs(40));
        assert_eq!(lag.behind(start + secs(41)), secs(41));
    }

    /// Every reading kept. The WAL Straum hasn't applied was written
    /// after the last reading Straum had applied all of.
    struct Naive {
        start: Instant,
        readings: Vec<NaiveReading>,
    }

    struct NaiveReading {
        at: Instant,
        wal: Wal,
    }

    impl Naive {
        fn read(&mut self, wal: Wal, at: Instant) {
            self.readings.push(NaiveReading { at, wal });
        }

        fn behind(&self, now: Instant) -> Duration {
            let Some(last) = self.readings.last() else {
                return now.saturating_duration_since(self.start);
            };
            let applied = last.wal.applied;
            let mut since = self.start;
            for reading in self.readings.iter().rev() {
                if reading.wal.written <= applied {
                    since = reading.at;
                    break;
                }
            }
            now.saturating_duration_since(since)
        }
    }

    /// splitmix64. `tests/app` has its own: a unit test can't reach an
    /// integration test's helpers, and the crate exports no test code.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, bound: u64) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut bits = self.0;
            bits = (bits ^ (bits >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            bits = (bits ^ (bits >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (bits ^ (bits >> 31)) % bound
        }
    }

    #[test]
    fn matches_every_reading_kept() {
        let start = Instant::now();
        for seed in 0..200 {
            let mut random = Rng(seed);
            let mut lag = Lag::new(start);
            let mut naive = Naive {
                start,
                readings: Vec::new(),
            };
            let mut at = start;
            let mut server = 100;
            let mut applied = 100;
            for _ in 0..200 {
                at += Duration::from_millis(random.below(2_000));
                // The server stands still now and then; Straum applies
                // some, all or none of what it has.
                server += random.below(3) * random.below(50);
                applied += random.below(server - applied + 1) * random.below(2);
                let reading = wal(server, applied);
                lag.read(&reading, at);
                naive.read(reading, at);
                let now = at + Duration::from_millis(random.below(3_000));
                assert_eq!(lag.behind(now), naive.behind(now), "seed {seed}");
            }
        }
    }

    #[test]
    fn a_stuck_writer_keeps_a_bounded_number_of_readings() {
        let start = Instant::now();
        let mut lag = Lag::new(start);
        let mut naive = Naive {
            start,
            readings: Vec::new(),
        };
        let total = READINGS as u64 + 500;
        for second in 1..=total {
            let reading = wal(100 + second, 100);
            let at = start + secs(second);
            lag.read(&reading, at);
            naive.read(reading, at);
        }
        assert_eq!(lag.unapplied.len(), READINGS);
        let end = start + secs(total);
        assert_eq!(lag.behind(end), naive.behind(end));
        // Confirming up to a merged reading: never shorter than the truth.
        for applied in [100 + READINGS as u64, 100 + total - 1] {
            let reading = wal(100 + total, applied);
            lag.read(&reading, end);
            naive.read(reading, end);
            assert!(lag.behind(end) >= naive.behind(end), "{applied}");
        }
        assert!(lag.behind(end) > naive.behind(end));
    }
}
