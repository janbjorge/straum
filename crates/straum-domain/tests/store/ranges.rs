//! Ranges and buckets: an ask reads the buckets from its start to its end,
//! an open end reaching to the buckets held, a time floors to its bucket even before 1970, and a row without a time
//! is kept but never answered.

use crate::common::{Rng, id};
use crate::random::trip;
use crate::views::{Widths, spec, typed, zones_spec};
use crate::{AT, END, HOUR, START, WEEK, WIDTH};
use straum_domain::{AnswerRow, Ask, AskKey, Datum, RowError, Store, Value, Width};

/// A row of the zones view without its join: a time and pay 1.
fn paid_trip_at(time: Datum) -> [Datum; 2] {
    [time, Datum::Int(1)]
}

/// The range runs from the first `date_bin` bucket's start to the last
/// one's end, and a row without a time is in neither.
#[test]
fn the_range_spans_the_buckets_held() {
    let mut store = Store::new(zones_spec(None, Widths::QuarterAndHour));
    assert_eq!(store.range("trips"), None);
    assert_eq!(store.range("zones"), None);
    let width = i64::from(WIDTH);
    store.add_row("trips", &paid_trip_at(Datum::Null)).unwrap();
    assert_eq!(store.range("trips"), None);
    store
        .add_row("trips", &paid_trip_at(Datum::Int(START + 1)))
        .unwrap();
    assert_eq!(store.range("trips"), Some(START..START + width));
    store
        .add_row("trips", &paid_trip_at(Datum::Int(START + 3 * width)))
        .unwrap();
    assert_eq!(store.range("trips"), Some(START..START + 4 * width));
    // A bucket whose last row leaves is gone, and the range with it.
    store
        .remove_row("trips", &paid_trip_at(Datum::Int(START + 3 * width)))
        .unwrap();
    assert_eq!(store.range("trips"), Some(START..START + width));
    store
        .remove_row("trips", &paid_trip_at(Datum::Int(START + 1)))
        .unwrap();
    assert_eq!(store.range("trips"), None);
}

/// Before 1970 a range rounds to the wider buckets inside it toward the
/// range, not toward zero: a quarter-aligned range over a quarter and an
/// hour reads the quarters it holds, one row each, and no more.
#[test]
fn a_range_before_1970_reads_the_wider_buckets_inside_it() {
    let mut store = Store::new(zones_spec(None, Widths::QuarterAndHour));
    let width = i64::from(WIDTH);
    let hour = i64::from(HOUR);
    let mut at = -4 * hour;
    while at < 0 {
        store
            .add_row("trips", &paid_trip_at(Datum::Int(at)))
            .unwrap();
        at += width;
    }
    for (from, to) in [
        (-2 * hour - width, -width),
        (-3 * hour - 3 * width, -hour + width),
        (-4 * hour, -hour - width),
        (-hour - width, 0),
    ] {
        let quarters = (to - from) / width;
        assert_eq!(
            trips_counted(&store, from, to),
            Value::Int(quarters.into()),
            "{from}..{to}"
        );
    }
}

/// A bound cuts even past everything held, and an empty store answers a
/// zero count.
#[test]
fn a_range_cuts_at_both_ends() {
    let spec = spec();
    let mut store = typed(&spec);
    let mut rng = Rng(5);
    let mut rows = Vec::new();
    for bucket in 0..4 {
        let mut row = trip(&mut rng);
        row[AT] = Datum::Int(START + i64::from(WIDTH) * bucket);
        store.add_row("trips", &row).unwrap();
        rows.push(row);
    }
    let width = i64::from(WIDTH);
    assert_eq!(
        trips_counted(&store, START - width, END + width),
        Value::Int(4)
    );
    assert_eq!(trips_counted(&store, START + width, END), Value::Int(3));
    assert_eq!(trips_counted(&store, START, START + width), Value::Int(1));
    assert_eq!(trips_counted(&store, START - width, START), Value::Int(0));
    assert_eq!(trips_counted(&store, END, END + width), Value::Int(0));
    let empty = typed(&spec);
    assert_eq!(trips_counted(&empty, START, END), Value::Int(0));
}

/// A range at either end of `i64` reads what it holds, no more: the
/// wider buckets inside it are found without overflow.
#[test]
fn a_range_at_the_ends_of_time_reads_what_it_holds() {
    let mut store = Store::new(zones_spec(None, Widths::QuarterAndHour));
    for at in [START, START + i64::from(WIDTH)] {
        store
            .add_row("trips", &paid_trip_at(Datum::Int(at)))
            .unwrap();
    }
    let width = i64::from(WIDTH);
    // The last and first bounds of the narrowest width: no hour bucket
    // ends after the one, or starts before the other, within `i64`.
    let last = i64::MAX - i64::MAX.rem_euclid(width);
    let first = -last;
    assert_eq!(trips_counted(&store, first, last), Value::Int(2));
    assert_eq!(trips_counted(&store, last - width, last), Value::Int(0));
    assert_eq!(trips_counted(&store, first, first + width), Value::Int(0));
    assert_eq!(trips_counted(&store, START, last), Value::Int(2));
    assert_eq!(trips_counted(&store, first, START + width), Value::Int(1));
}

/// An open end reads to the edge of the buckets held, on either side, and
/// a bound still cuts the other side.
#[test]
fn an_open_end_reads_to_the_buckets_held() {
    let spec = spec();
    let mut store = typed(&spec);
    let mut rng = Rng(5);
    let width = i64::from(WIDTH);
    for bucket in 0..4 {
        let mut row = trip(&mut rng);
        row[AT] = Datum::Int(START + width * bucket);
        store.add_row("trips", &row).unwrap();
    }
    assert_eq!(trips_reaching(&store, None, None), Value::Int(4));
    assert_eq!(
        trips_reaching(&store, Some(START + width), None),
        Value::Int(3)
    );
    assert_eq!(
        trips_reaching(&store, None, Some(START + width)),
        Value::Int(1)
    );
    assert_eq!(trips_reaching(&store, Some(END), None), Value::Int(0));
    assert_eq!(trips_reaching(&store, None, Some(START)), Value::Int(0));
    let by_bucket = Ask {
        from: None,
        to: None,
        ..trips_over(START, END, vec![AskKey::Bucket])
    };
    assert_eq!(store.ask(&by_bucket).unwrap().rows.len(), 4);
}

/// With no bucket held, an open end reads nothing, whatever the other end.
#[test]
fn an_open_end_over_an_empty_store_reads_nothing() {
    let empty = typed(&spec());
    assert_eq!(trips_reaching(&empty, None, None), Value::Int(0));
    assert_eq!(trips_reaching(&empty, Some(START), None), Value::Int(0));
    assert_eq!(trips_reaching(&empty, None, Some(END)), Value::Int(0));
}

/// The trips over a range whose ends may be open.
fn trips_reaching(store: &Store, from: Option<i64>, to: Option<i64>) -> Value {
    let ask = Ask {
        from,
        to,
        ..trips_over(START, END, Vec::new())
    };
    first_value(store, &ask)
}

/// The first measure of the first row of the answer.
pub(crate) fn first_value(store: &Store, ask: &Ask) -> Value {
    store.ask(ask).unwrap().rows[0].values[0].clone()
}

/// The trips over `from..to`.
fn trips_counted(store: &Store, from: i64, to: i64) -> Value {
    first_value(store, &trips_over(from, to, Vec::new()))
}

/// The count of trips, by bucket or not, over a range.
fn trips_over(from: i64, to: i64, keys: Vec<AskKey>) -> Ask {
    Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys,
        filter: Vec::new(),
        having: Vec::new(),
        from: Some(from),
        to: Some(to),
    }
}

/// A time before 1970 falls in the bucket that starts below it, at every
/// width: buckets floor, they don't round toward the epoch.
#[test]
fn a_time_before_the_epoch_floors_to_its_bucket() {
    let mut store = typed(&spec());
    let width = i64::from(WIDTH);
    let mut row = trip(&mut Rng(2));
    row[AT] = Datum::Int(-1);
    store.add_row("trips", &row).unwrap();
    let by_bucket = vec![AskKey::Bucket];
    let below = trips_over(-width, 0, by_bucket.clone());
    assert_eq!(
        store.ask(&below).unwrap().rows,
        [AnswerRow {
            keys: vec![Datum::Int(-width)],
            values: vec![Value::Int(1)]
        }]
    );
    assert_eq!(
        store.ask(&trips_over(0, width, by_bucket)).unwrap().rows,
        []
    );
    // The week before 1970 holds it, the one after does not.
    let week = i64::from(WEEK);
    assert_eq!(store.cells_read(&trips_over(-week, 0, Vec::new())), Ok(1));
    assert_eq!(store.cells_read(&trips_over(0, week, Vec::new())), Ok(0));
    store.remove_row("trips", &row).unwrap();
    assert_eq!(store.cells("trips"), Some(0));
}

/// A row without a time is kept, and no ask reads it, however wide its
/// range; it comes out as it went in.
#[test]
fn a_row_without_a_time_is_kept_and_never_answered() {
    let mut store = typed(&spec());
    let width = i64::from(WIDTH);
    let mut timed = trip(&mut Rng(2));
    timed[AT] = Datum::Int(START);
    let mut untimed = timed.clone();
    untimed[AT] = Datum::Null;
    store.add_row("trips", &timed).unwrap();
    store.add_row("trips", &untimed).unwrap();
    assert_eq!(store.cells("trips"), Some(2));
    assert_eq!(store.bound_cells("trips"), Some(2));

    // The widest range on bucket edges.
    let from = i64::MIN / width * width;
    let to = i64::MAX / width * width;
    let all = store.ask(&trips_over(from, to, Vec::new())).unwrap();
    assert_eq!(all.rows[0].values, [Value::Int(1)]);
    let by_bucket = trips_over(from, to, vec![AskKey::Bucket]);
    assert_eq!(
        store.ask(&by_bucket).unwrap().rows,
        [AnswerRow {
            keys: vec![Datum::Int(START)],
            values: vec![Value::Int(1)]
        }]
    );

    // Held once, so it comes out once.
    store.remove_row("trips", &untimed).unwrap();
    assert_eq!(store.cells("trips"), Some(1));
    assert_eq!(store.remove_row("trips", &untimed), Err(RowError::Missing));
}

#[test]
fn time_key_start() {
    let width = Width { seconds: WIDTH };
    let cases = [
        // 2024-02-29 13:47:05 UTC.
        (1_709_214_425, 1_709_214_300),
        // 1969-12-31 23:59:59, before the epoch.
        (-1, -900),
    ];
    for (time, start) in cases {
        assert_eq!(width.bucket_start(time), start, "{time}");
    }
}
