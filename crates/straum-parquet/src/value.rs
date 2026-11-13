//! A Parquet value as the value Straum would get from PostgreSQL, had the
//! file been copied into a table of the manifest's types.

use std::collections::HashMap;
use std::fmt::Write;

use jiff::Timestamp;
use jiff::tz::{Offset, TimeZone};

const SECONDS_PER_DAY: i64 = 86_400;
/// The most digits an `i128` always holds.
const MAX_DIGITS: usize = 38;
/// Below this a double holds every integer, with room to spare: the
/// nearest decimal of a scale is then the only one near it.
const EXACT: f64 = 4.0e15;

/// A double as a numeric of `scale`, in units of `10^-scale`. PostgreSQL
/// reads the shortest decimal that is the double and rounds it to the
/// column's scale, half away from zero; so does this. `text` is scratch.
/// `None` for `NaN`, an infinity and a value past an `i64`.
pub(crate) fn numeric(value: f64, scale: u32, text: &mut String) -> Option<i64> {
    if !value.is_finite() {
        return None;
    }
    // A double that is the nearest one to a decimal of this scale reads as
    // that decimal, with no text made.
    let unit = (0..scale).fold(1.0, |unit, _| unit * 10.0);
    let scaled = (value * unit).round();
    if scaled.abs() < EXACT && scaled / unit == value {
        return Some(scaled as i64);
    }
    text.clear();
    write!(text, "{value}").expect("a String takes any text");
    decimal(text, scale)
}

/// An integer as a numeric of `scale`. `None` past an `i64`.
pub(crate) fn whole(value: i64, scale: u32) -> Option<i64> {
    value.checked_mul(10_i64.checked_pow(scale)?)
}

/// `-123.456` at scale 2 is -12346: digits, a point and more digits, as
/// Rust prints a double. `None` for anything else and past an `i64`.
fn decimal(text: &str, scale: u32) -> Option<i64> {
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let digits = |text: &str| text.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !digits(fraction) {
        return None;
    }
    let significant = format!("{whole}{fraction}");
    let significant = significant.trim_start_matches('0');
    if significant.len() > MAX_DIGITS {
        return None;
    }
    let units: i128 = if significant.is_empty() {
        0
    } else {
        significant.parse().ok()?
    };
    // How many places the digits move left to be in units of the scale.
    let places = i64::from(scale) - i64::try_from(fraction.len()).ok()?;
    let scaled = match u32::try_from(places) {
        Ok(places) => units.checked_mul(10_i128.checked_pow(places)?)?,
        Err(_) => match u32::try_from(-places)
            .ok()
            .and_then(|p| 10_i128.checked_pow(p))
        {
            Some(unit) => (units + unit / 2) / unit,
            // Smaller than any digit kept.
            None => 0,
        },
    };
    i64::try_from(if negative { -scaled } else { scaled }).ok()
}

/// A count of `per_second` parts of a second as whole seconds, rounded
/// down as Straum reads a PostgreSQL time.
pub(crate) fn seconds(count: i64, per_second: i64) -> i64 {
    count.div_euclid(per_second)
}

/// A time zone, to read a wall clock as the instant it showed.
pub(crate) struct Zone {
    rules: TimeZone,
    /// Per day of the wall clock: how far the clock was ahead of UTC all
    /// day, or `None` for a day the clock was set on.
    days: HashMap<i64, Option<i64>>,
}

impl Zone {
    /// `None` for a name the IANA database doesn't have.
    pub(crate) fn named(name: &str) -> Option<Zone> {
        Some(Zone {
            rules: TimeZone::get(name).ok()?,
            days: HashMap::new(),
        })
    }

    /// The Unix seconds at which the zone's clock showed `wall`, itself
    /// the seconds its reading would be in UTC. An hour the clock skipped
    /// reads as if it had not been set forward yet, and an hour it showed
    /// twice is the second showing: PostgreSQL's reading of both. `None`
    /// past the years the rules cover.
    pub(crate) fn instant(&mut self, wall: i64) -> Option<i64> {
        let day = wall.div_euclid(SECONDS_PER_DAY);
        let ahead = match self.days.get(&day) {
            Some(ahead) => *ahead,
            None => {
                let ahead = self.ahead_all_day(day);
                self.days.insert(day, ahead);
                ahead
            }
        };
        match ahead {
            Some(ahead) => Some(wall - ahead),
            None => self.resolve(wall),
        }
    }

    /// How far the clock is ahead of UTC through the whole day, when it
    /// is the same at both its ends.
    fn ahead_all_day(&self, day: i64) -> Option<i64> {
        let ahead = |wall: i64| Some(wall - self.resolve(wall)?);
        let start = day.checked_mul(SECONDS_PER_DAY)?;
        let (first, last) = (ahead(start)?, ahead(start.checked_add(SECONDS_PER_DAY)?)?);
        (first == last).then_some(first)
    }

    fn resolve(&self, wall: i64) -> Option<i64> {
        let clock = Offset::UTC.to_datetime(Timestamp::from_second(wall).ok()?);
        let instant = self.rules.to_ambiguous_timestamp(clock).later().ok()?;
        Some(instant.as_second())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(value: f64, scale: u32) -> Option<i64> {
        numeric(value, scale, &mut String::new())
    }

    /// The long way: the double's shortest decimal, rounded.
    fn through_text(value: f64, scale: u32) -> Option<i64> {
        value
            .is_finite()
            .then(|| decimal(&value.to_string(), scale))
            .flatten()
    }

    /// splitmix64; the tests need no crate for randomness.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn int(&mut self, lo: i64, hi: i64) -> i64 {
            lo + (self.next() % (hi - lo) as u64) as i64
        }
    }

    #[test]
    fn doubles_are_integers_in_their_scale() {
        let cases = [
            (12.5, 2, 1_250),
            (17.7, 2, 1_770),
            (3.0, 2, 300),
            (-0.05, 2, -5),
            (-0.0, 2, 0),
            (0.0, 0, 0),
            (70.3, 2, 7_030),
            (0.1 + 0.2, 2, 30),
            (1e-5, 5, 1),
            (312_722.3, 2, 31_272_230),
            (4.0e15, 0, 4_000_000_000_000_000),
            (9.2e18, 0, 9_200_000_000_000_000_000),
            (1.5, 18, 1_500_000_000_000_000_000),
        ];
        for (value, scale, units) in cases {
            assert_eq!(read(value, scale), Some(units), "{value} at {scale}");
        }
    }

    /// PostgreSQL rounds a numeric to its column's scale on the way in,
    /// half away from zero, from the decimal the double prints as.
    #[test]
    fn doubles_round_to_their_scale_as_postgresql_does() {
        let cases = [
            (3.454, 2, 345),
            (3.455, 2, 346),
            // In binary 1.005 is below itself; its decimal is not.
            (1.005, 2, 101),
            (-1.005, 2, -101),
            (-3.455, 2, -346),
            (0.5, 0, 1),
            (-0.5, 0, -1),
            (2.5, 0, 3),
            (0.49, 0, 0),
            (1e-5, 2, 0),
            (5e-3, 2, 1),
            (1e-300, 2, 0),
            (5e-324, 18, 0),
        ];
        for (value, scale, units) in cases {
            assert_eq!(read(value, scale), Some(units), "{value} at {scale}");
        }
    }

    #[test]
    fn refuses_what_is_no_numeric() {
        for value in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1e19,
            -1e19,
            1e300,
        ] {
            assert_eq!(read(value, 0), None, "{value}");
        }
        assert_eq!(read(1e17, 2), None);
        assert_eq!(read(92_233_720_368_547_760.0, 2), None);
    }

    /// The reference is the long way, which every double may go. The
    /// short way must agree wherever it is taken: on cents, on doubles
    /// next to cents, and on doubles with digits to spare.
    #[test]
    fn the_short_way_reads_as_the_text_does() {
        let mut rng = Rng(9);
        for _ in 0..200_000 {
            let scale = (rng.next() % 5) as u32;
            let unit = 10_f64.powi(scale as i32);
            let cents = rng.int(-2_000_000, 2_000_000) as f64 / unit;
            let near = f64::from_bits(cents.to_bits().wrapping_add(rng.next() % 5).wrapping_sub(2));
            let any = rng.int(-1_000_000_000, 1_000_000_000) as f64 / 1e5;
            let huge = rng.int(-9_000_000_000_000_000, 9_000_000_000_000_000) as f64;
            for value in [cents, near, any, huge, huge / unit] {
                assert_eq!(
                    read(value, scale),
                    through_text(value, scale),
                    "{value:?} at {scale}"
                );
            }
        }
    }

    #[test]
    fn decimals_read_as_rust_prints_them() {
        let cases = [
            ("0", 2, Some(0)),
            ("-0", 2, Some(0)),
            ("007", 0, Some(7)),
            ("12.50", 2, Some(1_250)),
            ("-123.456", 2, Some(-12_346)),
            ("92233720368547758.07", 2, Some(i64::MAX)),
            ("-92233720368547758.08", 2, Some(i64::MIN)),
            ("92233720368547758.08", 2, None),
            ("0.0000000000000000000000000000000000000000001", 2, Some(0)),
            ("123456789012345678901234567890123456789", 0, None),
            ("", 2, None),
            ("-", 2, None),
            (".5", 2, None),
            ("1.2.3", 2, None),
            ("1e5", 2, None),
            ("NaN", 2, None),
            ("inf", 2, None),
            ("+1", 2, None),
            ("--1", 2, None),
            (" 1", 2, None),
        ];
        for (text, scale, units) in cases {
            assert_eq!(decimal(text, scale), units, "{text:?} at {scale}");
        }
    }

    #[test]
    fn integers_scale_up() {
        assert_eq!(whole(7, 0), Some(7));
        assert_eq!(whole(-7, 2), Some(-700));
        assert_eq!(
            whole(92_233_720_368_547_758, 2),
            Some(9_223_372_036_854_775_800)
        );
        assert_eq!(whole(92_233_720_368_547_759, 2), None);
        assert_eq!(whole(1, 19), None);
        assert_eq!(whole(0, 19), None);
    }

    #[test]
    fn seconds_round_down() {
        assert_eq!(seconds(1_705_314_600_999_999, 1_000_000), 1_705_314_600);
        assert_eq!(seconds(1_500, 1_000), 1);
        assert_eq!(seconds(-1, 1_000_000_000), -1);
        assert_eq!(seconds(-1_000, 1_000), -1);
        assert_eq!(seconds(0, 1_000), 0);
    }

    /// Unix seconds of a UTC date and clock, for walls and instants alike.
    fn at(date: (i16, i8, i8), clock: (i8, i8)) -> i64 {
        let date = jiff::civil::date(date.0, date.1, date.2);
        let utc = Offset::UTC.to_timestamp(date.at(clock.0, clock.1, 0, 0));
        utc.unwrap().as_second()
    }

    #[test]
    fn a_wall_clock_is_the_instant_it_showed() {
        let mut new_york = Zone::named("America/New_York").unwrap();
        let cases = [
            // Winter, five hours behind UTC; summer, four.
            (((2024, 1, 15), (10, 30)), ((2024, 1, 15), (15, 30))),
            (((2024, 7, 15), (10, 30)), ((2024, 7, 15), (14, 30))),
            // 2024-03-10: the clock went from 02:00 to 03:00. Before it,
            // the hour it skipped as if not set forward yet, and after.
            (((2024, 3, 10), (1, 59)), ((2024, 3, 10), (6, 59))),
            (((2024, 3, 10), (2, 30)), ((2024, 3, 10), (7, 30))),
            (((2024, 3, 10), (3, 0)), ((2024, 3, 10), (7, 0))),
            (((2024, 3, 10), (23, 0)), ((2024, 3, 11), (3, 0))),
            // 2024-11-03: the clock went from 02:00 back to 01:00. The
            // hour shown twice is its second showing.
            (((2024, 11, 3), (0, 59)), ((2024, 11, 3), (4, 59))),
            (((2024, 11, 3), (1, 30)), ((2024, 11, 3), (6, 30))),
            (((2024, 11, 3), (2, 0)), ((2024, 11, 3), (7, 0))),
        ];
        for (wall, instant) in cases {
            let got = new_york.instant(at(wall.0, wall.1));
            assert_eq!(got, Some(at(instant.0, instant.1)), "{wall:?}");
        }
        let mut utc = Zone::named("UTC").unwrap();
        assert_eq!(utc.instant(1_705_314_600), Some(1_705_314_600));
        assert_eq!(utc.instant(-1), Some(-1));
    }

    #[test]
    fn refuses_a_zone_or_a_time_nobody_has() {
        assert!(Zone::named("America/Gotham").is_none());
        assert!(Zone::named("").is_none());
        let mut new_york = Zone::named("America/New_York").unwrap();
        for wall in [i64::MAX, i64::MIN, 400_000_000_000, -400_000_000_000] {
            assert_eq!(new_york.instant(wall), None, "{wall}");
        }
    }

    /// The reference asks the rules for every time. The days kept must
    /// give the same, on the days the clock is set and all others.
    #[test]
    fn days_kept_read_as_the_rules_do() {
        let mut rng = Rng(21);
        for name in [
            "America/New_York",
            "Europe/Oslo",
            "Australia/Lord_Howe",
            "Asia/Kolkata",
        ] {
            let mut zone = Zone::named(name).unwrap();
            let (from, to) = (at((2023, 1, 1), (0, 0)), at((2025, 1, 1), (0, 0)));
            for _ in 0..50_000 {
                let wall = rng.int(from, to);
                assert_eq!(zone.instant(wall), zone.resolve(wall), "{name} at {wall}");
            }
            // Around each setting of the clock, second by second.
            for (month, day) in [(3, 10), (3, 31), (4, 7), (10, 6), (10, 27), (11, 3)] {
                let midnight = at((2024, month, day), (0, 0));
                for wall in (midnight - 3_600..midnight + 90_000).step_by(61) {
                    assert_eq!(zone.instant(wall), zone.resolve(wall), "{name} at {wall}");
                }
            }
            assert!(zone.days.values().any(Option::is_none) || name == "Asia/Kolkata");
        }
    }
}
