//! The random workload under retention: a seeded run draws which widths
//! the view keeps for a time, then interleaves trip and dim changes with
//! `advance` calls whose times mostly rise and now and then fall. Every
//! so often random asks, many bounded near a horizon, go to the store.
//! The rule here, written apart from the store's cover, says which asks
//! read a bucket past a horizon: those must be refused, naming exactly
//! the widths read past, and every other must equal the reference over
//! all live rows. A store loaded from the live rows and advanced once to
//! the latest time must hold as many cells, so a late row never makes a
//! narrow cell back.

use std::collections::BTreeSet;
use std::ops::Range;

use crate::common::{Rng, id};
use crate::random::{change_dim, load_dims, meet_a_group, near, random_ask, random_dims, trip};
use crate::reference::reference;
use crate::retention::{keep, retaining};
use crate::views::typed;
use crate::{ASKS_PER_CHECK, AT, CHECK_EVERY, DAY, Dims, HOUR, START, WEEK, WIDTH};
use straum_domain::{Ask, AskError, AskKey, Datum, Retention, Spec, Store, Width};

/// Seeds and steps: fewer than the plain workload's, as each check also
/// rebuilds a store and walks each ask's buckets.
const RETENTION_SEEDS: u64 = 8;
const RETENTION_STEPS: u32 = 1_200;

/// The widths the view keeps, widest first, in seconds.
const WIDEST_FIRST: [u32; 4] = [WEEK, DAY, HOUR, WIDTH];

/// Seconds in a day, as a time.
const DAY_SECONDS: i64 = 86_400;

/// A refusal as the rule finds it: the width read past, in seconds, and
/// its horizon.
type Refusal = (u32, i64);

/// Where each retained width's buckets are kept from, as the rule works
/// it out: the latest `at` less the time kept, down to the next wider
/// width. `None` before any `advance`.
struct Horizons {
    retained: Vec<Retention>,
    kept_from: Vec<Option<i64>>,
}

impl Horizons {
    fn new(retained: Vec<Retention>) -> Horizons {
        let kept_from = vec![None; retained.len()];
        Horizons {
            retained,
            kept_from,
        }
    }

    fn advance(&mut self, at: i64) {
        for (index, retention) in self.retained.iter().enumerate() {
            let wider = next_wider(retention.width.seconds);
            let moved = floor_to(at - i64::from(retention.kept_for.seconds), wider);
            let kept_from = match self.kept_from[index] {
                Some(held) => held.max(moved),
                None => moved,
            };
            self.kept_from[index] = Some(kept_from);
        }
    }

    /// The horizon of `width`; `None` for a width kept for good, or
    /// before any `advance`.
    fn of(&self, width: u32) -> Option<i64> {
        for (index, retention) in self.retained.iter().enumerate() {
            if retention.width.seconds == width {
                return self.kept_from[index];
            }
        }
        None
    }
}

/// The width the view keeps just wider than `width`.
fn next_wider(width: u32) -> u32 {
    let mut wider = WEEK;
    for candidate in WIDEST_FIRST {
        if candidate == width {
            return wider;
        }
        wider = candidate;
    }
    panic!("{width} is no width the view keeps")
}

/// `at` down to a multiple of `width` seconds.
fn floor_to(at: i64, width: u32) -> i64 {
    let width = i64::from(width);
    at.div_euclid(width) * width
}

/// No retention, the quarter hour alone, or the quarter hour, the hour
/// and maybe the day, each kept for a whole number of the next wider
/// width and no less than the width before.
fn random_retention(rng: &mut Rng) -> Vec<Retention> {
    let quarter = keep(WIDTH, HOUR * *rng.pick(&[1, 6, 24, 72]));
    match rng.below(3) {
        0 => Vec::new(),
        1 => vec![quarter],
        _ => {
            let quarter = keep(WIDTH, HOUR * *rng.pick(&[1, 6, 24]));
            let mut several = vec![quarter, keep(HOUR, DAY * *rng.pick(&[1, 3, 7, 14]))];
            if rng.chance(50) {
                several.push(keep(DAY, WEEK * *rng.pick(&[2, 4])));
            }
            several
        }
    }
}

/// The store under test, the live rows and dims, and the horizons the
/// rule keeps beside it.
struct Workload {
    seed: u64,
    spec: Spec,
    store: Store,
    rng: Rng,
    rows: Vec<Vec<Datum>>,
    dims: Dims,
    horizons: Horizons,
    /// The time the next rising `advance` starts from.
    clock: i64,
    /// The latest time advanced to.
    latest: Option<i64>,
}

impl Workload {
    fn new(seed: u64) -> Workload {
        let mut rng = Rng(seed);
        let retained = random_retention(&mut rng);
        let spec = retaining(retained.clone()).unwrap();
        let mut store = typed(&spec);
        let dims = random_dims(&mut rng);
        load_dims(&mut store, &dims);
        let clock = START + rng.int(0, 30 * DAY_SECONDS);
        Workload {
            seed,
            spec,
            store,
            rng,
            rows: Vec::new(),
            dims,
            horizons: Horizons::new(retained),
            clock,
            latest: None,
        }
    }

    fn change_one(&mut self) {
        match self.rng.below(100) {
            0..=7 => self.advance(),
            8..=11 => change_dim(&mut self.rng, &mut self.store, &mut self.dims),
            12..=21 if !self.rows.is_empty() => self.remove_trip(),
            22..=34 if !self.rows.is_empty() => self.update_trip(),
            _ => self.add_trip(),
        }
    }

    /// Mostly on from the clock by up to two days; now and then back by
    /// up to three weeks, which must move no horizon.
    fn advance(&mut self) {
        let at = if self.rng.chance(80) {
            self.clock += self.rng.int(0, 2 * DAY_SECONDS);
            self.clock
        } else {
            self.clock - self.rng.int(0, 21 * DAY_SECONDS)
        };
        self.store.advance(at);
        self.horizons.advance(at);
        let latest = match self.latest {
            Some(latest) => latest.max(at),
            None => at,
        };
        self.latest = Some(latest);
    }

    fn remove_trip(&mut self) {
        let row = self.rows.swap_remove(self.rng.below(self.rows.len()));
        self.store.remove_row("trips", &row).unwrap();
    }

    fn update_trip(&mut self) {
        let index = self.rng.below(self.rows.len());
        let mut new = trip(&mut self.rng);
        if self.rng.chance(50) {
            near(&mut self.rng, &mut new, &self.rows[index]);
        }
        let old = &self.rows[index];
        self.store.update_row("trips", old, &new).unwrap();
        self.rows[index] = new;
    }

    fn add_trip(&mut self) {
        let mut row = trip(&mut self.rng);
        if !self.rows.is_empty() && self.rng.chance(40) {
            let other = self.rng.below(self.rows.len());
            near(&mut self.rng, &mut row, &self.rows[other]);
        }
        self.store.add_row("trips", &row).unwrap();
        self.rows.push(row);
    }

    /// The cells equal a store's loaded now and advanced once, and random
    /// asks are refused or answered as the rule and the reference say.
    fn check(&mut self, step: u32) {
        let seed = self.seed;
        let mut fresh = typed(&self.spec);
        load_dims(&mut fresh, &self.dims);
        for row in &self.rows {
            fresh.add_row("trips", row).unwrap();
        }
        if let Some(latest) = self.latest {
            fresh.advance(latest);
        }
        let context = format!("seed {seed}, step {step}");
        assert_eq!(self.store.cells("trips"), fresh.cells("trips"), "{context}");
        let bound = self.store.bound_cells("trips");
        assert_eq!(bound, fresh.bound_cells("trips"), "{context}");
        for _ in 0..ASKS_PER_CHECK {
            self.check_ask(&context);
        }
    }

    fn check_ask(&mut self, context: &str) {
        let mut ask = random_ask(&mut self.rng, &self.spec);
        if self.rng.chance(60) {
            near_a_horizon(&mut self.rng, &mut ask, &self.horizons);
        }
        let refusals = refusals(&ask, &self.rows, &self.horizons);
        if !refusals.is_empty() {
            let errors = self.store.ask(&ask).err();
            assert_eq!(refused_widths(errors), refusals, "{context}, {ask:?}");
            let counted = self.store.cells_read(&ask).err();
            assert_eq!(refused_widths(counted), refusals, "{context}, {ask:?}");
            return;
        }
        let mut every_group = ask.clone();
        every_group.having.clear();
        let groups = reference(&self.spec, &every_group, &self.rows, &self.dims);
        meet_a_group(&mut self.rng, &mut ask, &groups);
        let expected = reference(&self.spec, &ask, &self.rows, &self.dims);
        assert_eq!(self.store.ask(&ask), Ok(expected), "{context}, {ask:?}");
    }

    /// Every row leaves without an error, and every cell with them.
    fn finish(mut self) {
        let seed = self.seed;
        for row in &self.rows {
            self.store.remove_row("trips", row).unwrap();
        }
        assert_eq!(self.store.cells("trips"), Some(0), "seed {seed}");
        assert_eq!(self.store.bound_cells("trips"), Some(0), "seed {seed}");
    }
}

/// The widths and horizons the errors name; an error of any other kind,
/// or an answer, fails.
fn refused_widths(errors: Option<Vec<AskError>>) -> BTreeSet<Refusal> {
    let Some(errors) = errors else {
        panic!("answered an ask past a horizon");
    };
    let mut named = BTreeSet::new();
    for error in errors {
        let AskError::PastRetention { width, kept_from } = error else {
            panic!("refused with {error:?}");
        };
        assert!(named.insert((width.seconds, kept_from)), "named twice");
    }
    named
}

/// The width each answer row covers, and so each bound is on an edge of:
/// the time key's, else the `date_bin` width.
fn row_width(ask: &Ask) -> u32 {
    for key in &ask.keys {
        match key {
            AskKey::Key(name) if name.as_str() == "bucket" => return WIDTH,
            AskKey::WiderBucket { width, .. } => return width.seconds,
            AskKey::Key(_) => {}
        }
    }
    WIDTH
}

/// The widest width that may read a bucket: the time key's, else the
/// widest the view keeps.
fn widest_read(ask: &Ask) -> u32 {
    for key in &ask.keys {
        match key {
            AskKey::Key(name) if name.as_str() == "bucket" => return WIDTH,
            AskKey::WiderBucket { width, .. } => return width.seconds,
            AskKey::Key(_) => {}
        }
    }
    WEEK
}

/// Bounds `ask` a few row widths either side of a horizon drawn from
/// those set: its start, or its end with the start well before it.
fn near_a_horizon(rng: &mut Rng, ask: &mut Ask, horizons: &Horizons) {
    let mut set = Vec::new();
    for kept_from in horizons.kept_from.iter().flatten() {
        set.push(*kept_from);
    }
    if set.is_empty() {
        return;
    }
    let step = row_width(ask);
    let edge = floor_to(*rng.pick(&set), step);
    let step = i64::from(step);
    let open = rng.chance(20);
    if rng.chance(50) {
        let from = edge + step * rng.int(-3, 4);
        ask.from = Some(from);
        ask.to = Some(from + step * rng.int(1, 12));
        if open {
            ask.to = None;
        }
    } else {
        let to = edge + step * rng.int(-2, 4);
        ask.to = Some(to);
        ask.from = Some(to - step * rng.int(1, 60));
        if open {
            ask.from = None;
        }
    }
}

/// The range an ask reads once its open ends are set: from the first
/// week held, to the end of the last quarter hour held, or of the last
/// week when the quarter hours hold no bucket. Both stand at 0 with no
/// row held.
fn read_range(ask: &Ask, rows: &[Vec<Datum>], horizons: &Horizons) -> Range<i64> {
    let quarter_from = horizons.of(WIDTH).unwrap_or(i64::MIN);
    // No row's time is either extreme, so they stand for none.
    let mut first = i64::MAX;
    let mut last = i64::MIN;
    let mut last_recent = i64::MIN;
    for row in rows {
        let Datum::Int(at) = row[AT] else { continue };
        first = first.min(at);
        last = last.max(at);
        if at >= quarter_from {
            last_recent = last_recent.max(at);
        }
    }
    let (open_start, open_end) = if first == i64::MAX {
        (0, 0)
    } else if last_recent != i64::MIN {
        (
            floor_to(first, WEEK),
            floor_to(last_recent, WIDTH) + i64::from(WIDTH),
        )
    } else {
        (
            floor_to(first, WEEK),
            floor_to(last, WEEK) + i64::from(WEEK),
        )
    };
    ask.from.unwrap_or(open_start)..ask.to.unwrap_or(open_end)
}

/// The width serving the `date_bin` bucket at `at`: the widest no wider
/// than `widest` whose bucket there lies wholly inside `range`.
fn serving(at: i64, range: &Range<i64>, widest: u32) -> u32 {
    for width in WIDEST_FIRST {
        if width > widest {
            continue;
        }
        let start = floor_to(at, width);
        if start >= range.start && start + i64::from(width) <= range.end {
            return width;
        }
    }
    WIDTH
}

/// What the rule refuses `ask` for: each width that would serve a bucket
/// starting before its horizon, with that horizon. A width's bucket
/// serves each `date_bin` bucket inside it alike, so the walk steps a
/// whole serving bucket at a time.
fn refusals(ask: &Ask, rows: &[Vec<Datum>], horizons: &Horizons) -> BTreeSet<Refusal> {
    let mut refused = BTreeSet::new();
    if horizons.retained.is_empty() {
        return refused;
    }
    let range = read_range(ask, rows, horizons);
    let widest = widest_read(ask);
    let mut at = range.start;
    while at < range.end {
        let width = serving(at, &range, widest);
        let start = floor_to(at, width);
        if let Some(kept_from) = horizons.of(width)
            && start < kept_from
        {
            refused.insert((width, kept_from));
        }
        at = start + i64::from(width);
    }
    refused
}

#[test]
fn store_under_retention_matches_naive_reference() {
    for seed in 0..RETENTION_SEEDS {
        let mut workload = Workload::new(seed);
        for step in 0..RETENTION_STEPS {
            workload.change_one();
            if step % CHECK_EVERY == CHECK_EVERY - 1 {
                workload.check(step);
            }
        }
        workload.finish();
    }
}

/// The rule against hand-worked cases, so a slip in it can't pass for
/// the store's: hours kept a week from 2024-03-08, quarter hours a day
/// from 2024-03-14 05:00.
#[test]
fn the_rule_refuses_what_reads_past_a_horizon() {
    let mut horizons = Horizons::new(vec![keep(WIDTH, DAY), keep(HOUR, WEEK)]);
    let now = START + 60 * DAY_SECONDS + 5 * 3_600 + 47 * 60 + 13;
    horizons.advance(now);
    horizons.advance(now - 10 * DAY_SECONDS);
    let quarter_from = START + 59 * DAY_SECONDS + 5 * 3_600;
    let hour_from = START + 53 * DAY_SECONDS;
    assert_eq!(horizons.of(WIDTH), Some(quarter_from));
    assert_eq!(horizons.of(HOUR), Some(hour_from));
    assert_eq!(horizons.of(DAY), None);
    let mut ask = random_ask(&mut Rng(1), &retaining(Vec::new()).unwrap());
    ask.keys = vec![AskKey::Key(id("pay"))];
    let cases = [
        (hour_from, quarter_from, vec![]),
        (hour_from - 900, hour_from, vec![(WIDTH, quarter_from)]),
        (hour_from - 3_600, hour_from, vec![(HOUR, hour_from)]),
        (hour_from - DAY_SECONDS, hour_from, vec![]),
        (
            quarter_from - 900,
            quarter_from + 3_600,
            vec![(WIDTH, quarter_from)],
        ),
        (
            START + 900,
            START + DAY_SECONDS,
            vec![(WIDTH, quarter_from), (HOUR, hour_from)],
        ),
    ];
    for (from, to, expected) in cases {
        ask.from = Some(from);
        ask.to = Some(to);
        let expected: BTreeSet<Refusal> = expected.into_iter().collect();
        assert_eq!(refusals(&ask, &[], &horizons), expected, "{from}..{to}");
    }
    ask.keys = vec![AskKey::WiderBucket {
        name: id("bucket"),
        width: Width { seconds: DAY },
    }];
    ask.from = Some(START);
    ask.to = Some(START + DAY_SECONDS);
    assert!(refusals(&ask, &[], &horizons).is_empty());
}
