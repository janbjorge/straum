//! Dates as PostgreSQL keeps them: days since 2000-01-01 in an `i32`, its
//! greatest and least values standing for `infinity` and `-infinity`.
//! Text comes in and goes out as PostgreSQL prints a date under
//! `DateStyle = ISO`: `2024-01-15`, `0044-03-15 BC`, `infinity`. The
//! stream reads a date's text with [`parse_date`], an ask's parameter is
//! read the same way, and an answer prints a date with [`format_date`].
//! [`days_from_civil`] also turns a timestamp's calendar day into days.

/// `DATEVAL_NOEND` and `DATEVAL_NOBEGIN` in PostgreSQL's `date.h`.
const DATE_INFINITY: i32 = i32::MAX;
const DATE_MINUS_INFINITY: i32 = i32::MIN;
/// The first day a date holds, 4714-11-24 BC (Julian day 0), and the day
/// past the last, 5874898-01-01, both in days since 2000-01-01
/// (`IS_VALID_DATE`).
const FIRST_DAY: i64 = -2_451_545;
const END_DAY: i64 = 2_145_031_949;
/// Days from 1970-01-01 to 2000-01-01, PostgreSQL's epoch.
const PG_EPOCH_DAYS: i64 = 10_957;
/// Days from 0000-03-01 to 1970-01-01 in the proleptic Gregorian calendar.
const UNIX_EPOCH_DAYS: i64 = 719_468;
const DAYS_PER_ERA: i64 = 146_097;
const YEARS_PER_ERA: i64 = 400;
/// PostgreSQL pads a year to four digits, and the last year a date holds
/// has seven.
const LEAST_YEAR_DIGITS: usize = 4;
const MOST_YEAR_DIGITS: usize = 7;
const MONTH_DIGITS: usize = 2;
const DAY_DIGITS: usize = 2;
/// Past this many years from year 0 the day count could overflow; no
/// date or timestamp PostgreSQL keeps comes near.
const FARTHEST_YEAR: i64 = 1_000_000_000;
const BEFORE_CHRIST: &str = " BC";

/// A day of the proleptic Gregorian calendar; year 0 is 1 BC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Civil {
    year: i64,
    month: i64,
    day: i64,
}

/// A date's text as PostgreSQL prints it, in days since 2000-01-01.
/// `None` for any other spelling, such as `2024-1-5`, `20240115` or a
/// day the month lacks: one spelling per date, so a filter's value finds
/// the dates an answer prints so.
pub fn parse_date(text: &str) -> Option<i32> {
    match text {
        "infinity" => return Some(DATE_INFINITY),
        "-infinity" => return Some(DATE_MINUS_INFINITY),
        _ => {}
    }
    let (text, before_christ) = match text.strip_suffix(BEFORE_CHRIST) {
        Some(text) => (text, true),
        None => (text, false),
    };
    let (year_month, day) = text.rsplit_once('-')?;
    let (year, month) = year_month.rsplit_once('-')?;
    let year_digits = LEAST_YEAR_DIGITS..=MOST_YEAR_DIGITS;
    // Past four digits a year has no leading zero, as PostgreSQL prints it.
    let padded = year.len() > LEAST_YEAR_DIGITS && year.starts_with('0');
    if !year_digits.contains(&year.len()) || padded {
        return None;
    }
    if month.len() != MONTH_DIGITS || day.len() != DAY_DIGITS {
        return None;
    }
    let mut year = digits(year)?;
    // 1 BC is year 0, 2 BC year -1; there is no year 0 either way.
    if year == 0 {
        return None;
    }
    if before_christ {
        year = 1 - year;
    }
    let days = days_from_civil(year, digits(month)?, digits(day)?)? - PG_EPOCH_DAYS;
    if !(FIRST_DAY..END_DAY).contains(&days) {
        return None;
    }
    i32::try_from(days).ok()
}

/// A date, days since 2000-01-01, as PostgreSQL prints it.
pub fn format_date(days: i32) -> String {
    match days {
        DATE_INFINITY => return "infinity".to_owned(),
        DATE_MINUS_INFINITY => return "-infinity".to_owned(),
        _ => {}
    }
    let Civil { year, month, day } = civil_from_days(i64::from(days) + PG_EPOCH_DAYS);
    if year <= 0 {
        let year = 1 - year;
        return format!("{year:04}-{month:02}-{day:02}{BEFORE_CHRIST}");
    }
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since 1970-01-01, after Howard Hinnant's `days_from_civil`.
/// `None` for a month or day the calendar lacks, such as 2023-02-29, and
/// for a year too far out to count in days.
pub fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if !(-FARTHEST_YEAR..=FARTHEST_YEAR).contains(&year) {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(YEARS_PER_ERA);
    let year_of_era = year - era * YEARS_PER_ERA;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * DAYS_PER_ERA + day_of_era - UNIX_EPOCH_DAYS;
    // A day past the month's end lands in the next month.
    let year = if month <= 2 { year + 1 } else { year };
    let civil = Civil { year, month, day };
    (civil_from_days(days) == civil).then_some(days)
}

/// The calendar day of `days` since 1970-01-01, after Howard Hinnant's
/// `civil_from_days`.
fn civil_from_days(days: i64) -> Civil {
    let shifted = days + UNIX_EPOCH_DAYS;
    let era = shifted.div_euclid(DAYS_PER_ERA);
    let day_of_era = shifted - era * DAYS_PER_ERA;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * YEARS_PER_ERA;
    let year = if month <= 2 { year + 1 } else { year };
    Civil { year, month, day }
}

/// Digits only, no sign.
fn digits(text: &str) -> Option<i64> {
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
    }
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_dates_as_postgresql_prints_them() {
        // Expected days from `SELECT d - date '2000-01-01'` in PostgreSQL.
        let cases = [
            ("2000-01-01", 0),
            ("2024-01-15", 8_780),
            ("1999-12-31", -1),
            ("1970-01-01", -10_957),
            ("2024-02-29", 8_825),
            ("0001-01-01", -730_119),
            ("0001-12-31 BC", -730_120),
            ("0044-03-15 BC", -746_117),
            ("4714-11-24 BC", -2_451_545),
            ("12345-06-07", 3_778_591),
            ("5874897-12-31", 2_145_031_948),
            ("infinity", DATE_INFINITY),
            ("-infinity", DATE_MINUS_INFINITY),
        ];
        for (text, days) in cases {
            assert_eq!(parse_date(text), Some(days), "{text}");
            assert_eq!(format_date(days), text, "{days}");
        }
    }

    #[test]
    fn refuses_other_spellings_and_days_no_date_holds() {
        let cases = [
            "",
            "2024-1-15",
            "2024-01-5",
            "24-01-15",
            "02024-01-15",
            "20240115",
            "2024/01/15",
            "2024-01-15 00:00:00",
            "2024-01-15 AD",
            "2024-01-15 bc",
            "2023-02-29",
            "2024-04-31",
            "2024-13-01",
            "2024-00-01",
            "2024-01-00",
            "2024-01-32",
            "0000-01-01",
            "0000-01-01 BC",
            "4714-11-23 BC",
            "5874898-01-01",
            "99999999-01-01",
            "+2024-01-15",
            "-2024-01-15",
            "2024-+1-15",
            "Infinity",
            "+infinity",
        ];
        for text in cases {
            assert_eq!(parse_date(text), None, "{text}");
        }
    }

    /// Every day from 600 BC to 2600 AD goes to text and back, one day
    /// after the one before.
    #[test]
    fn every_day_reads_back_as_written() {
        let first = parse_date("0600-01-01 BC").unwrap();
        let last = parse_date("2600-12-31").unwrap();
        let mut previous = format_date(first - 1);
        for days in first..=last {
            let text = format_date(days);
            assert_eq!(parse_date(&text), Some(days), "{text}");
            assert_ne!(text, previous);
            previous = text;
        }
    }

    #[test]
    fn days_from_civil_refuses_what_no_calendar_holds() {
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        assert_eq!(days_from_civil(2000, 2, 29), Some(11_016));
        assert_eq!(days_from_civil(1900, 2, 29), None);
        assert_eq!(days_from_civil(i64::MIN, 1, 1), None);
        assert_eq!(days_from_civil(i64::MAX, 3, 1), None);
    }
}
