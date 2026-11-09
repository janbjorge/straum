//! A request against one fact: named measures, grouped by keys, over a
//! time range.

use std::fmt;
use std::iter;

use crate::ident::Ident;
use crate::spec::{Cond, Fact, Grain, KeyPath, Spec};

const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;
const WEEK: i64 = 7 * DAY;
/// 1970-01-05, the first Monday after the epoch.
const MONDAY: i64 = 4 * DAY;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub fact: Ident,
    pub measures: Vec<Ident>,
    pub by: Vec<AskKey>,
    /// Every condition must hold.
    pub filter: Vec<Cond>,
    /// Where hours, days, weeks and months start.
    pub zone: Zone,
    /// Start of the half-open range `[from, to)`, Unix seconds.
    pub from: i64,
    pub to: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskKey {
    Key(KeyPath),
    Time(TimeKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeKey {
    Bucket,
    Hour,
    Day,
    Week,
    Month,
}

/// A time zone's UTC offsets over an ask's range. Adapters fill it from the
/// tz database, so the domain carries no zone data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    /// Seconds east of UTC before the first change.
    initial: i64,
    changes: Vec<Change>,
}

/// From `at` (Unix seconds) on, the zone is `offset` seconds east of UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub at: i64,
    pub offset: i64,
}

impl Zone {
    pub fn utc() -> Zone {
        Zone {
            initial: 0,
            changes: Vec::new(),
        }
    }

    /// `None` unless the changes are in strictly ascending order of `at`.
    pub fn new(initial: i64, changes: Vec<Change>) -> Option<Zone> {
        changes
            .is_sorted_by(|a, b| a.at < b.at)
            .then_some(Zone { initial, changes })
    }

    /// The initial offset, then the offset of each change.
    fn offsets(&self) -> impl Iterator<Item = i64> {
        iter::once(self.initial).chain(self.changes.iter().map(|c| c.offset))
    }

    fn offset_at(&self, t: i64) -> i64 {
        match self.changes.partition_point(|c| c.at <= t) {
            0 => self.initial,
            i => self.changes[i - 1].offset,
        }
    }

    /// Every offset and change falls on a bucket edge, so no hour, day, week
    /// or month starts inside a bucket.
    fn fits(&self, grain: Grain) -> bool {
        let grain = i64::from(grain.0);
        iter::once(self.initial)
            .chain(self.changes.iter().flat_map(|c| [c.at, c.offset]))
            .all(|s| s.rem_euclid(grain) == 0)
    }

    /// The instant a wall-clock time names. When the clock went back and the
    /// time happened twice, the first. When the clock jumped over it, the
    /// time read with the offset from before the jump, as RFC 5545 does.
    fn instant(&self, wall: i64) -> i64 {
        let first = self
            .offsets()
            .map(|offset| wall - offset)
            .filter(|&t| t + self.offset_at(t) == wall)
            .min();
        first.unwrap_or_else(|| {
            // Each change paired with the offset in effect before it.
            self.changes
                .iter()
                .zip(self.offsets())
                .find(|(c, before)| c.at + before <= wall && wall < c.at + c.offset)
                .map(|(_, before)| wall - before)
                .expect("a wall time no offset reaches lies in a jump forward")
        })
    }
}

impl TimeKey {
    /// Span the grain must divide. Weeks and months are whole days.
    fn span(self) -> Option<u32> {
        match self {
            TimeKey::Bucket => None,
            TimeKey::Hour => Some(3_600),
            TimeKey::Day | TimeKey::Week | TimeKey::Month => Some(86_400),
        }
    }

    /// Start of the period holding `t`, in Unix seconds. Buckets are UTC;
    /// the rest follow `zone`, and weeks start on Monday.
    pub fn start(self, t: i64, grain: Grain, zone: &Zone) -> i64 {
        let floor = |t: i64, span: i64| t.div_euclid(span) * span;
        let wall = t + zone.offset_at(t);
        match self {
            TimeKey::Bucket => floor(t, i64::from(grain.0)),
            // From the instant, so an hour the clock repeats stays two hours.
            TimeKey::Hour => t - wall.rem_euclid(HOUR),
            TimeKey::Day => zone.instant(floor(wall, DAY)),
            TimeKey::Week => zone.instant(floor(wall - MONDAY, WEEK) + MONDAY),
            TimeKey::Month => {
                let day = wall.div_euclid(DAY);
                zone.instant((day - day_of_month(day) + 1) * DAY)
            }
        }
    }
}

/// Day of the month, from 1, of a day count since 1970-01-01. The day part
/// of Howard Hinnant's `civil_from_days`: it counts in 400-year eras that
/// start on 0000-03-01, so each year ends with its leap day.
fn day_of_month(day: i64) -> i64 {
    let day_of_era = (day + 719_468).rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    // 0 is March, 11 is February.
    let month = (5 * day_of_year + 2) / 153;
    day_of_year - (153 * month + 2) / 5 + 1
}

impl Spec {
    /// Checks an ask against the spec and returns the fact it reads.
    pub fn check(&self, ask: &Ask) -> Result<&Fact, Vec<AskError>> {
        let Some(fact) = self.fact(ask.fact.as_str()) else {
            return Err(vec![AskError::UnknownFact(ask.fact.clone())]);
        };
        let grain = fact.time.grain;
        let mut errors = Vec::new();

        if ask.measures.is_empty() {
            errors.push(AskError::NoMeasures);
        }
        for m in &ask.measures {
            if fact.measure(m.as_str()).is_none() {
                errors.push(AskError::UnknownMeasure(m.clone()));
            }
        }

        let mut time = None;
        for key in &ask.by {
            match key {
                AskKey::Key(path) => {
                    if self.resolve(fact, path).is_none() {
                        errors.push(AskError::UnknownKey(path.clone()));
                    }
                }
                AskKey::Time(t) => {
                    if time.replace(*t).is_some() {
                        errors.push(AskError::TimeKeyTwice);
                    }
                    if let Some(span) = t.span()
                        && !span.is_multiple_of(grain.0)
                    {
                        errors.push(AskError::TimeKeyUnaligned { key: *t, grain });
                    }
                }
            }
        }
        if time.is_some_and(|t| t != TimeKey::Bucket) && !ask.zone.fits(grain) {
            errors.push(AskError::ZoneUnaligned { grain });
        }
        for cond in &ask.filter {
            if self.resolve(fact, &cond.path).is_none() {
                errors.push(AskError::UnknownKey(cond.path.clone()));
            }
        }

        if ask.from >= ask.to {
            errors.push(AskError::RangeEmpty {
                from: ask.from,
                to: ask.to,
            });
        }
        for at in [ask.from, ask.to] {
            if at.rem_euclid(i64::from(grain.0)) != 0 {
                errors.push(AskError::RangeUnaligned { at, grain });
            }
        }

        if errors.is_empty() {
            Ok(fact)
        } else {
            Err(errors)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskError {
    UnknownFact(Ident),
    UnknownMeasure(Ident),
    UnknownKey(KeyPath),
    NoMeasures,
    TimeKeyTwice,
    /// The time key does not split into whole buckets.
    TimeKeyUnaligned {
        key: TimeKey,
        grain: Grain,
    },
    /// The zone's offsets or changes fall inside a bucket.
    ZoneUnaligned {
        grain: Grain,
    },
    RangeEmpty {
        from: i64,
        to: i64,
    },
    /// A range end falls inside a bucket. Asks never read raw rows.
    RangeUnaligned {
        at: i64,
        grain: Grain,
    },
}

impl fmt::Display for AskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AskError::UnknownFact(name) => write!(f, "unknown fact {name}"),
            AskError::UnknownMeasure(name) => write!(f, "unknown measure {name}"),
            AskError::UnknownKey(path) => {
                write!(f, "{path} is not a by column, ref or ref attribute")
            }
            AskError::NoMeasures => write!(f, "no measures asked"),
            AskError::TimeKeyTwice => write!(f, "more than one time key"),
            AskError::TimeKeyUnaligned { key, grain } => {
                write!(f, "{key} does not split into {grain} buckets")
            }
            AskError::ZoneUnaligned { grain } => {
                write!(f, "zone offsets do not fall on {grain} bucket edges")
            }
            AskError::RangeEmpty { from, to } => write!(f, "empty range [{from}, {to})"),
            AskError::RangeUnaligned { at, grain } => {
                write!(f, "range end {at} is not on a {grain} bucket edge")
            }
        }
    }
}

impl std::error::Error for AskError {}

impl fmt::Display for TimeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TimeKey::Bucket => "bucket",
            TimeKey::Hour => "hour",
            TimeKey::Day => "day",
            TimeKey::Week => "week",
            TimeKey::Month => "month",
        })
    }
}
