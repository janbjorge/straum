//! A bucket width and its one spelling: a count of the largest unit that
//! divides it, singular for one, as `15 minutes`, `1 hour` or `7 days`.
//! Specs and asks are read in this spelling, and errors write widths in
//! it, so a width reads the same everywhere.
//!
//! Every width is a grid that starts at the epoch, 1970-01-01 00:00 UTC.
//! A width that divides a day starts a bucket on each UTC midnight; a
//! wider one starts where the count from the epoch lands, so `7 days`
//! buckets start on Thursdays.

use std::fmt;

/// A bucket width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Width {
    pub seconds: u32,
}

/// A unit a width is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unit {
    /// Singular, as `hour`; a count other than one adds an `s`.
    pub name: &'static str,
    pub seconds: u32,
}

/// The units a width is written in, largest first. A month or a year is
/// no fixed number of seconds, so neither is one.
pub const UNITS: [Unit; 4] = [
    Unit {
        name: "day",
        seconds: 86_400,
    },
    Unit {
        name: "hour",
        seconds: 3_600,
    },
    Unit {
        name: "minute",
        seconds: 60,
    },
    Unit {
        name: "second",
        seconds: 1,
    },
];

impl Width {
    /// The start of the bucket `seconds` falls in. Buckets are UTC.
    pub fn bucket_start(self, seconds: i64) -> i64 {
        seconds.div_euclid(i64::from(self.seconds)) * i64::from(self.seconds)
    }

    /// Whether `seconds` is the start of a bucket.
    pub fn is_edge(self, seconds: i64) -> bool {
        seconds.rem_euclid(i64::from(self.seconds)) == 0
    }
}

/// The largest unit that divides `seconds`; a zero width counts in
/// seconds.
fn unit_of(seconds: u32) -> Unit {
    for unit in UNITS {
        if seconds > 0 && seconds.is_multiple_of(unit.seconds) {
            return unit;
        }
    }
    UNITS[UNITS.len() - 1]
}

impl fmt::Display for Width {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let unit = unit_of(self.seconds);
        let count = self.seconds / unit.seconds;
        let plural = if count == 1 { "" } else { "s" };
        write!(f, "{count} {}{plural}", unit.name)
    }
}
