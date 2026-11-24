//! Retention: a view may keep its narrow widths only for a time, each
//! `Retention` naming a width and how long it is kept. `Spec::new`
//! checks the retentions; `Store::advance` moves each retained width's
//! horizon to `at` less the time kept, snapped down to the next wider
//! width, and drops the buckets before it. Answers at wider widths stay
//! those of every live row; an ask that would read a dropped bucket is
//! refused with `AskError::PastRetention`. The times here sit on no
//! edge, so a horizon not snapped shows.

use crate::common::{Rng, id};
use crate::random::{change_dim, load_dims, random_dims, trip, trips};
use crate::reference::reference;
use crate::views::{spec, typed};
use crate::{AT, DAY, Dims, END, HOUR, START, WEEK, WIDTH, trips_ask};
use straum_domain::{Ask, AskError, AskKey, Datum, Retention, Spec, SpecError, Store, Width};

/// Seconds in a day, as a time.
const DAY_SECONDS: i64 = 86_400;
/// Seconds in a quarter hour, the `date_bin` width, as a time.
const QUARTER_SECONDS: i64 = 900;

/// 2024-03-15 05:47:13 UTC, sixty days after `START`, on no edge of an
/// hour or a quarter hour.
const NOW: i64 = START + 60 * DAY_SECONDS + 5 * 3_600 + 47 * 60 + 13;
/// 05:45 that day, the quarter-hour edge before `NOW`.
const NOW_EDGE: i64 = NOW - 2 * 60 - 13;
/// 2024-03-14 05:00 UTC: where `NOW` puts the horizon of quarter hours
/// kept a day, snapped down to the hour. A day back is 05:47:13.
const QUARTER_KEPT_FROM: i64 = START + 59 * DAY_SECONDS + 5 * 3_600;
/// 2024-03-08 00:00 UTC: where `NOW` puts the horizon of hours kept a
/// week, snapped down to the day. A week back is 05:47:13.
const HOUR_KEPT_FROM: i64 = START + 53 * DAY_SECONDS;

/// The measures the asks here read: a count, a sum, a distinct count and
/// a percentile, so states of every kind are merged across widths.
const MEASURES: [&str; 4] = ["trips", "fare", "riders", "tip_median"];

/// `width` kept for `kept_for`, both in seconds.
pub(crate) fn keep(width: u32, kept_for: u32) -> Retention {
    Retention {
        width: Width { seconds: width },
        kept_for: Width { seconds: kept_for },
    }
}

/// The trips view of [`spec`], its widths retained as `retained` says.
pub(crate) fn retaining(retained: Vec<Retention>) -> Result<Spec, Vec<SpecError>> {
    let plain = spec();
    let mut facts = plain.facts().to_vec();
    facts[0].time.retained = retained;
    Spec::new(plain.dims().to_vec(), facts)
}

/// Quarter hours kept a day, hours a week; days and weeks for good.
fn day_and_week() -> Spec {
    retaining(vec![keep(WIDTH, DAY), keep(HOUR, WEEK)]).unwrap()
}

/// The one error a spec's retentions give.
fn spec_error(retained: Vec<Retention>) -> Vec<SpecError> {
    retaining(retained).unwrap_err()
}

fn width(seconds: u32) -> Width {
    Width { seconds }
}

/// The refusal of an ask reading `seconds` buckets before `kept_from`.
fn past(seconds: u32, kept_from: i64) -> Vec<AskError> {
    vec![AskError::PastRetention {
        width: width(seconds),
        kept_from,
    }]
}

/// The view's time grouped at `seconds`: its own name at the `date_bin`
/// width, `buckets` at a wider one.
fn by_time(seconds: u32) -> AskKey {
    if seconds == WIDTH {
        return AskKey::Key(id("bucket"));
    }
    AskKey::WiderBucket {
        name: id("bucket"),
        width: width(seconds),
    }
}

/// An ask of [`MEASURES`] by `keys` over `from..to`, either end open
/// when `None`.
fn ask_over(keys: Vec<AskKey>, from: Option<i64>, to: Option<i64>) -> Ask {
    Ask {
        from,
        to,
        ..trips_ask(&MEASURES, keys)
    }
}

/// A store of `spec` holding the dims and `rows`.
fn loaded(spec: &Spec, dims: &Dims, rows: &[Vec<Datum>]) -> Store {
    let mut store = typed(spec);
    load_dims(&mut store, dims);
    for row in rows {
        store.add_row("trips", row).unwrap();
    }
    store
}

/// Random trips and dims, and a store of `spec` holding them.
fn random_store(spec: &Spec, seed: u64, count: usize) -> (Store, Dims, Vec<Vec<Datum>>) {
    let mut rng = Rng(seed);
    let dims = random_dims(&mut rng);
    let rows = trips(&mut rng, count);
    let store = loaded(spec, &dims, &rows);
    (store, dims, rows)
}

/// One trip at `at`, the same keys whatever the time: one cell a bucket.
fn trip_at(at: Datum) -> Vec<Datum> {
    let mut row = trip(&mut Rng(3));
    row[AT] = at;
    row
}

/// Asserts the store answers `ask` as the reference does over `rows`.
fn assert_exact(store: &Store, spec: &Spec, ask: &Ask, rows: &[Vec<Datum>], dims: &Dims) {
    assert_eq!(
        store.ask(ask),
        Ok(reference(spec, ask, rows, dims)),
        "{ask:?}"
    );
}

/// Days over every bucket the tests fill, and the whole range with no
/// time key: neither reads a retained bucket.
fn wider_asks() -> [Ask; 2] {
    [
        ask_over(vec![by_time(DAY)], Some(START), Some(END)),
        ask_over(vec![AskKey::Key(id("pay"))], Some(START), Some(END)),
    ]
}

#[test]
fn retentions_that_keep_the_rules_are_read() {
    let retained = vec![keep(WIDTH, DAY), keep(HOUR, WEEK)];
    let spec = retaining(retained.clone()).unwrap();
    assert_eq!(spec.fact("trips").unwrap().time.retained, retained);
    // Kept for exactly one of the next wider width, and two widths kept
    // alike, are both whole and in order.
    assert!(retaining(vec![keep(WIDTH, HOUR)]).is_ok());
    assert!(retaining(vec![keep(WIDTH, WEEK), keep(HOUR, WEEK)]).is_ok());
    assert!(
        retaining(vec![
            keep(WIDTH, DAY),
            keep(HOUR, WEEK),
            keep(DAY, 2 * WEEK)
        ])
        .is_ok()
    );
    assert!(retaining(Vec::new()).is_ok());
}

#[test]
fn the_widest_width_is_never_retained() {
    let every = vec![
        keep(WIDTH, DAY),
        keep(HOUR, WEEK),
        keep(DAY, 2 * WEEK),
        keep(WEEK, 4 * WEEK),
    ];
    assert_eq!(
        spec_error(every),
        [SpecError::RetainedWidest {
            fact: id("trips"),
            width: width(WEEK),
        }]
    );
}

#[test]
fn a_retained_width_is_one_the_view_keeps() {
    let two_hours = 2 * HOUR;
    assert_eq!(
        spec_error(vec![keep(WIDTH, DAY), keep(two_hours, 2 * WEEK)]),
        [SpecError::RetainedNotKept {
            fact: id("trips"),
            width: width(two_hours),
        }]
    );
}

/// Ninety minutes is no whole number of hours, and half an hour is less
/// than one.
#[test]
fn a_width_is_kept_for_whole_multiples_of_the_next_wider() {
    for kept_for in [90 * 60, 30 * 60] {
        assert_eq!(
            spec_error(vec![keep(WIDTH, kept_for)]),
            [SpecError::RetentionNotMultiple {
                fact: id("trips"),
                width: width(WIDTH),
                kept_for: width(kept_for),
                wider: width(HOUR),
            }]
        );
    }
    assert_eq!(
        spec_error(vec![keep(WIDTH, DAY), keep(HOUR, 36 * HOUR)]),
        [SpecError::RetentionNotMultiple {
            fact: id("trips"),
            width: width(HOUR),
            kept_for: width(36 * HOUR),
            wider: width(DAY),
        }]
    );
    assert!(retaining(vec![keep(WIDTH, 3 * HOUR)]).is_ok());
}

/// The error names the retained width past the gap.
#[test]
fn retained_widths_run_on_from_the_narrowest() {
    assert_eq!(
        spec_error(vec![keep(HOUR, WEEK)]),
        [SpecError::RetentionGap {
            fact: id("trips"),
            width: width(HOUR),
        }]
    );
    assert_eq!(
        spec_error(vec![keep(WIDTH, DAY), keep(DAY, 2 * WEEK)]),
        [SpecError::RetentionGap {
            fact: id("trips"),
            width: width(DAY),
        }]
    );
    assert!(retaining(vec![keep(WIDTH, DAY)]).is_ok());
}

#[test]
fn a_wider_width_is_kept_no_shorter_than_a_narrower() {
    assert_eq!(
        spec_error(vec![keep(WIDTH, 2 * DAY), keep(HOUR, DAY)]),
        [SpecError::RetentionShorter {
            fact: id("trips"),
            width: width(HOUR),
            kept_for: width(DAY),
            narrower_kept_for: width(2 * DAY),
        }]
    );
    assert!(retaining(vec![keep(WIDTH, 2 * DAY), keep(HOUR, 2 * DAY)]).is_ok());
}

/// Most quarter hours and many hours go, keyed and bound alike; days,
/// weeks and the whole range still count every row.
#[test]
fn advance_drops_old_narrow_buckets_and_wider_answers_stay_exact() {
    let spec = day_and_week();
    let (mut store, dims, rows) = random_store(&spec, 4, 3_000);
    let keyed = store.cells("trips").unwrap();
    let bound = store.bound_cells("trips").unwrap();
    store.advance(NOW);
    let kept = store.cells("trips").unwrap();
    // The rows run on to `END`, a month past `NOW`: the quarter hours
    // from a day back hold about a third of them, never half.
    assert!(kept * 2 < keyed, "{kept} of {keyed} cells kept");
    let bound_kept = store.bound_cells("trips").unwrap();
    assert!(
        bound_kept * 2 < bound,
        "{bound_kept} of {bound} bound cells kept"
    );
    for ask in wider_asks() {
        assert_exact(&store, &spec, &ask, &rows, &dims);
    }
    let recent = ask_over(vec![by_time(WIDTH)], Some(QUARTER_KEPT_FROM), Some(END));
    assert_exact(&store, &spec, &recent, &rows, &dims);
    let hours = ask_over(vec![by_time(HOUR)], Some(HOUR_KEPT_FROM), Some(END));
    assert_exact(&store, &spec, &hours, &rows, &dims);
}

/// Without retention `advance` drops nothing and refuses nothing.
#[test]
fn advance_without_retention_keeps_every_bucket() {
    let spec = spec();
    let (mut store, dims, rows) = random_store(&spec, 5, 1_000);
    let keyed = store.cells("trips");
    store.advance(NOW);
    assert_eq!(store.cells("trips"), keyed);
    let every = ask_over(vec![by_time(WIDTH)], Some(START), Some(END));
    assert_exact(&store, &spec, &every, &rows, &dims);
}

/// Before any `advance` a retained width has no horizon, so nothing is
/// dropped or refused.
#[test]
fn before_any_advance_every_bucket_answers() {
    let spec = day_and_week();
    let (store, dims, rows) = random_store(&spec, 6, 1_000);
    let every = ask_over(vec![by_time(WIDTH)], Some(START), Some(END));
    assert_exact(&store, &spec, &every, &rows, &dims);
}

/// A horizon a day or a week back from `NOW` lies inside an hour or a
/// day: it snaps down to that edge, so the bucket starting there answers
/// and the one before is refused.
#[test]
fn the_horizon_snaps_down_to_the_next_wider_width() {
    let spec = day_and_week();
    let (mut store, dims, rows) = random_store(&spec, 7, 2_000);
    store.advance(NOW);
    let quarter = i64::from(WIDTH);
    let at_edge = ask_over(
        vec![by_time(WIDTH)],
        Some(QUARTER_KEPT_FROM),
        Some(NOW_EDGE),
    );
    assert_exact(&store, &spec, &at_edge, &rows, &dims);
    let before_edge = ask_over(
        vec![by_time(WIDTH)],
        Some(QUARTER_KEPT_FROM - quarter),
        Some(NOW_EDGE),
    );
    assert_eq!(store.ask(&before_edge), Err(past(WIDTH, QUARTER_KEPT_FROM)));
    let hour = i64::from(HOUR);
    let hours = ask_over(vec![by_time(HOUR)], Some(HOUR_KEPT_FROM), Some(END));
    assert_exact(&store, &spec, &hours, &rows, &dims);
    let hour_before = ask_over(vec![by_time(HOUR)], Some(HOUR_KEPT_FROM - hour), Some(END));
    assert_eq!(store.ask(&hour_before), Err(past(HOUR, HOUR_KEPT_FROM)));
}

/// A row in the quarter hour that starts on the horizon keeps its cell,
/// and leaves and comes back to it after; one a second before it loses
/// it.
#[test]
fn the_bucket_on_the_horizon_stays() {
    let mut store = typed(&day_and_week());
    let on_edge = trip_at(Datum::Int(QUARTER_KEPT_FROM));
    store.add_row("trips", &on_edge).unwrap();
    store
        .add_row("trips", &trip_at(Datum::Int(QUARTER_KEPT_FROM - 1)))
        .unwrap();
    store.advance(NOW);
    assert_eq!(store.cells("trips"), Some(1));
    store.remove_row("trips", &on_edge).unwrap();
    assert_eq!(store.cells("trips"), Some(0));
    store.add_row("trips", &on_edge).unwrap();
    assert_eq!(store.cells("trips"), Some(1));
}

/// An earlier `at` leaves every horizon where it was: the old range stays
/// refused and a row in it makes no narrow cell. A later one moves them on.
#[test]
fn the_horizon_never_moves_back() {
    let spec = day_and_week();
    let mut store = typed(&spec);
    store.advance(NOW);
    store.advance(NOW - 30 * DAY_SECONDS);
    let old = ask_over(
        vec![by_time(WIDTH)],
        Some(QUARTER_KEPT_FROM - 3_600),
        Some(NOW_EDGE),
    );
    assert_eq!(store.ask(&old), Err(past(WIDTH, QUARTER_KEPT_FROM)));
    store
        .add_row("trips", &trip_at(Datum::Int(QUARTER_KEPT_FROM - 3_600)))
        .unwrap();
    assert_eq!(store.cells("trips"), Some(0));
    store.advance(NOW + DAY_SECONDS);
    let moved = ask_over(
        vec![by_time(WIDTH)],
        Some(QUARTER_KEPT_FROM),
        Some(NOW_EDGE),
    );
    assert_eq!(
        store.ask(&moved),
        Err(past(WIDTH, QUARTER_KEPT_FROM + DAY_SECONDS))
    );
}

/// An insert, an update and a delete of rows past the horizon raise no
/// error, make no quarter-hour cell, and count at the wider widths.
#[test]
fn a_late_row_changes_only_the_wider_levels() {
    let spec = day_and_week();
    let dims = random_dims(&mut Rng(15));
    let mut store = loaded(&spec, &dims, &[]);
    let old = trip_at(Datum::Int(START + 10 * DAY_SECONDS + 100));
    let recent = trip_at(Datum::Int(NOW - 600));
    store.add_row("trips", &old).unwrap();
    store.add_row("trips", &recent).unwrap();
    assert_eq!(store.cells("trips"), Some(2));
    store.advance(NOW);
    assert_eq!(store.cells("trips"), Some(1));
    let late = trip_at(Datum::Int(START + 20 * DAY_SECONDS + 5));
    store.add_row("trips", &late).unwrap();
    assert_eq!(store.cells("trips"), Some(1), "a late insert made a cell");
    assert_answers(&store, &spec, &[&old, &recent, &late], &dims);
    store.remove_row("trips", &old).unwrap();
    assert_eq!(store.cells("trips"), Some(1));
    assert_answers(&store, &spec, &[&recent, &late], &dims);
    let moved = trip_at(Datum::Int(START + 21 * DAY_SECONDS));
    store.update_row("trips", &late, &moved).unwrap();
    assert_eq!(store.cells("trips"), Some(1), "a late update made a cell");
    assert_answers(&store, &spec, &[&recent, &moved], &dims);
    // Out of the old range into the kept one: a cell of its own.
    let now_moved = trip_at(Datum::Int(NOW - 1_200));
    store.update_row("trips", &moved, &now_moved).unwrap();
    assert_eq!(store.cells("trips"), Some(2));
    assert_answers(&store, &spec, &[&recent, &now_moved], &dims);
    // And back: its cell goes.
    let back = trip_at(Datum::Int(START + 30 * DAY_SECONDS));
    store.update_row("trips", &now_moved, &back).unwrap();
    assert_eq!(store.cells("trips"), Some(1));
    assert_answers(&store, &spec, &[&recent, &back], &dims);
    store.remove_row("trips", &back).unwrap();
    store.remove_row("trips", &recent).unwrap();
    assert_eq!(store.cells("trips"), Some(0));
    assert_answers(&store, &spec, &[], &dims);
}

/// The day and ungrouped asks, and quarter hours of the kept range, each
/// as the reference answers over `rows`.
fn assert_answers(store: &Store, spec: &Spec, rows: &[&Vec<Datum>], dims: &Dims) {
    let mut live = Vec::with_capacity(rows.len());
    for row in rows {
        live.push((*row).clone());
    }
    let recent = ask_over(vec![by_time(WIDTH)], Some(QUARTER_KEPT_FROM), Some(END));
    for ask in wider_asks().iter().chain([&recent]) {
        assert_exact(store, spec, ask, &live, dims);
    }
}

/// A range that starts or ends in a quarter hour past the horizon reads
/// that quarter hour, so it is refused; the same range out to the hour
/// reads an hour bucket, which the view still holds.
#[test]
fn a_range_end_in_the_old_range_is_refused() {
    let spec = day_and_week();
    let (mut store, dims, rows) = random_store(&spec, 8, 2_000);
    store.advance(NOW);
    let pay = || vec![AskKey::Key(id("pay"))];
    let quarter_off = QUARTER_KEPT_FROM - 3_600 + QUARTER_SECONDS * 3;
    let starts_late = ask_over(pay(), Some(quarter_off), Some(END));
    assert_eq!(store.ask(&starts_late), Err(past(WIDTH, QUARTER_KEPT_FROM)));
    let on_the_hour = ask_over(pay(), Some(QUARTER_KEPT_FROM - 3_600), Some(END));
    assert_exact(&store, &spec, &on_the_hour, &rows, &dims);
    let day_40 = START + 40 * DAY_SECONDS;
    let ends_late = ask_over(pay(), Some(START), Some(day_40 + QUARTER_SECONDS));
    assert_eq!(store.ask(&ends_late), Err(past(WIDTH, QUARTER_KEPT_FROM)));
    let on_the_day = ask_over(pay(), Some(START), Some(day_40));
    assert_exact(&store, &spec, &on_the_day, &rows, &dims);
}

/// Hours before the hours' horizon are refused even where no row was
/// ever held; days over the same range answer.
#[test]
fn an_ask_by_hour_over_the_old_range_is_refused() {
    let spec = day_and_week();
    let (mut store, dims, rows) = random_store(&spec, 9, 2_000);
    store.advance(NOW);
    let from = START + 40 * DAY_SECONDS;
    let to = START + 42 * DAY_SECONDS;
    let by_hour = ask_over(vec![by_time(HOUR)], Some(from), Some(to));
    assert_eq!(store.ask(&by_hour), Err(past(HOUR, HOUR_KEPT_FROM)));
    let by_day = ask_over(vec![by_time(DAY)], Some(from), Some(to));
    assert_exact(&store, &spec, &by_day, &rows, &dims);
    let mut empty = typed(&spec);
    empty.advance(NOW);
    assert_eq!(empty.ask(&by_hour), Err(past(HOUR, HOUR_KEPT_FROM)));
}

/// An open start reads from the first week held, so an open ask by hour
/// or quarter hour reaches the old range and is refused; one with no
/// time key reads weeks there, and answers.
#[test]
fn an_open_ask_by_a_retained_width_is_refused() {
    let spec = day_and_week();
    let (mut store, dims, rows) = random_store(&spec, 10, 2_000);
    store.advance(NOW);
    let by_hour = ask_over(vec![by_time(HOUR)], None, None);
    assert_eq!(store.ask(&by_hour), Err(past(HOUR, HOUR_KEPT_FROM)));
    let by_quarter = ask_over(vec![by_time(WIDTH)], None, Some(END));
    assert_eq!(store.ask(&by_quarter), Err(past(WIDTH, QUARTER_KEPT_FROM)));
    let open = ask_over(vec![AskKey::Key(id("pay"))], None, None);
    assert_exact(&store, &spec, &open, &rows, &dims);
    let by_day = ask_over(vec![by_time(DAY)], None, None);
    assert_exact(&store, &spec, &by_day, &rows, &dims);
    let open_end = ask_over(vec![by_time(WIDTH)], Some(QUARTER_KEPT_FROM), None);
    assert_exact(&store, &spec, &open_end, &rows, &dims);
}

/// The rows without a time keep their bucket through any `advance`, and
/// leave it without an error.
#[test]
fn a_row_without_a_time_survives_advance() {
    let mut store = typed(&day_and_week());
    let untimed = trip_at(Datum::Null);
    store.add_row("trips", &untimed).unwrap();
    store.add_row("trips", &trip_at(Datum::Int(START))).unwrap();
    store.advance(NOW);
    store.advance(i64::MAX / 2);
    assert_eq!(store.cells("trips"), Some(1), "the untimed bucket went");
    store.remove_row("trips", &untimed).unwrap();
    assert_eq!(store.cells("trips"), Some(0));
}

/// Dim rows relabelled, removed and added after a drop move the bound
/// cells of every width still held, as they would in a store loaded
/// after the change and advanced alike.
#[test]
fn a_dim_change_after_a_drop_moves_the_wider_bound_cells() {
    let spec = day_and_week();
    let (mut store, mut dims, rows) = random_store(&spec, 11, 2_000);
    store.advance(NOW);
    let mut rng = Rng(12);
    for _ in 0..40 {
        change_dim(&mut rng, &mut store, &mut dims);
    }
    let mut fresh = loaded(&spec, &dims, &rows);
    fresh.advance(NOW);
    assert_eq!(store.bound_cells("trips"), fresh.bound_cells("trips"));
    assert_eq!(store.cells("trips"), fresh.cells("trips"));
    let home = || AskKey::Key(id("home_borough"));
    let asks = [
        ask_over(vec![home(), by_time(DAY)], Some(START), Some(END)),
        ask_over(vec![home(), by_time(WEEK)], None, None),
        ask_over(vec![home()], Some(START), Some(END)),
        ask_over(vec![by_time(WIDTH), home()], Some(QUARTER_KEPT_FROM), None),
        ask_over(vec![by_time(HOUR), home()], Some(HOUR_KEPT_FROM), Some(END)),
    ];
    for ask in &asks {
        assert_exact(&store, &spec, ask, &rows, &dims);
    }
}

/// Counting the cells an ask reads and building a window of its rows
/// refuse what an ask refuses, and read what it reads.
#[test]
fn cells_read_and_ask_window_refuse_too() {
    let spec = day_and_week();
    let (mut store, dims, rows) = random_store(&spec, 13, 1_000);
    store.advance(NOW);
    let old = ask_over(vec![by_time(HOUR)], Some(START), Some(END));
    assert_eq!(store.cells_read(&old), Err(past(HOUR, HOUR_KEPT_FROM)));
    let window = store.ask_window(&old, None).err();
    assert_eq!(window, Some(past(HOUR, HOUR_KEPT_FROM)));
    let kept = ask_over(vec![by_time(HOUR)], Some(HOUR_KEPT_FROM), Some(END));
    assert!(store.cells_read(&kept).unwrap() > 0);
    let answer = store.ask_window(&kept, None).unwrap().answer;
    assert_eq!(answer, reference(&spec, &kept, &rows, &dims));
}

/// Two retained widths both read before their horizons: one refusal for
/// each, narrowest first.
#[test]
fn each_width_read_past_its_horizon_is_named_once() {
    let spec = day_and_week();
    let (mut store, _, _) = random_store(&spec, 14, 500);
    store.advance(NOW);
    let from = START + 40 * DAY_SECONDS + 3 * QUARTER_SECONDS;
    let ask = ask_over(vec![by_time(WIDTH)], Some(from), Some(END));
    assert_eq!(store.ask(&ask), Err(past(WIDTH, QUARTER_KEPT_FROM)));
    let mixed = ask_over(vec![AskKey::Key(id("pay"))], Some(from), Some(END));
    let mut expected = past(WIDTH, QUARTER_KEPT_FROM);
    expected.extend(past(HOUR, HOUR_KEPT_FROM));
    assert_eq!(store.ask(&mixed), Err(expected));
}
