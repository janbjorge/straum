//! The tests of [`ValueCounts`]: random adds and removes against a map
//! from value to count, through each form a cell takes, and the bytes
//! each form keeps its values in.

use std::collections::BTreeMap;

use super::{Ends, RUN_HEADER, RUN_MAX_BYTES, ValueCount, ValueCounts};

use crate::test_rng::Rng;

fn entry(value: i64, count: u64) -> ValueCount {
    ValueCount { value, count }
}

fn entries_of(held: &BTreeMap<i64, u64>) -> Vec<ValueCount> {
    let mut entries = Vec::with_capacity(held.len());
    for (&value, &count) in held {
        entries.push(entry(value, count));
    }
    entries
}

fn ends_of(held: &BTreeMap<i64, u64>) -> Ends {
    Ends {
        least: held.keys().next().copied().unwrap_or(i64::MAX),
        greatest: held.keys().next_back().copied().unwrap_or(i64::MIN),
    }
}

/// The counts are the reference's, and so are they encoded from its
/// entries at once.
fn check(counts: &ValueCounts, held: &BTreeMap<i64, u64>, at: &str) {
    let entries = entries_of(held);
    assert!(counts.iter().eq(entries.iter().copied()), "{at}");
    assert_eq!(counts.ends(), ends_of(held), "{at}");
    let anew = ValueCounts::from_ascending(&entries);
    assert!(
        anew.iter().eq(entries.iter().copied()),
        "{at}: encoded anew"
    );
    assert!(counts.covers(&anew) && anew.covers(counts), "{at}");
}

fn form(counts: &ValueCounts) -> &'static str {
    match counts {
        ValueCounts::Inline(_) => "cell",
        ValueCounts::Run(_) => "run",
        ValueCounts::Runs(_) => "runs",
    }
}

/// The values a cell draws from, and the most it counts of one at once.
struct Shape {
    name: &'static str,
    least: i64,
    step: u64,
    values: u64,
    most: u64,
    /// The forms the cell passes through, in the order it first takes them.
    forms: &'static [&'static str],
}

impl Shape {
    fn value(&self, index: u64) -> i64 {
        (self.least).wrapping_add(self.step.wrapping_mul(index).cast_signed())
    }
}

const SHAPES: [Shape; 4] = [
    Shape {
        name: "fares",
        least: 250,
        step: 70,
        values: 12_000,
        most: 3,
        forms: &["cell", "run", "runs"],
    },
    Shape {
        name: "cents counted often",
        least: -40,
        step: 1,
        values: 90,
        most: 1 << 20,
        forms: &["cell", "run"],
    },
    Shape {
        name: "every width of value",
        least: i64::MIN,
        step: u64::MAX / 6_000,
        values: 6_000,
        most: 2,
        forms: &["cell", "run", "runs"],
    },
    Shape {
        name: "every width of count",
        least: 0,
        step: 1 << 40,
        values: 3,
        most: 1 << 58,
        forms: &["cell", "run"],
    },
];

/// A cell's counts beside the reference they must match.
struct Followed {
    counts: ValueCounts,
    held: BTreeMap<i64, u64>,
}

/// Takes some of a value near `value`, or more than is held, or a value
/// not held at all: the cell refuses exactly what the reference lacks.
fn take_near(rng: &mut Rng, followed: &mut Followed, value: i64, at: &str) {
    let held = &mut followed.held;
    let near = match held.range(value..).next() {
        Some(near) => Some(near),
        None => held.iter().next(),
    };
    let (target, count) = match (near, rng.below(20)) {
        // More than is held, and a value that isn't.
        (Some((&near_value, &near_count)), 0) => (near_value, near_count + 1),
        (None, _) | (_, 1) => (value.wrapping_sub(1), 1),
        (Some((&near_value, &near_count)), 2..=7) => (near_value, near_count),
        (Some((&near_value, &near_count)), _) => (near_value, 1 + rng.below(near_count)),
    };
    let has = match held.get(&target) {
        Some(&have) => have >= count,
        None => false,
    };
    let counts = &mut followed.counts;
    assert_eq!(
        counts.has(target, count),
        has,
        "{at}: has {count} of {target}"
    );
    assert_eq!(
        counts.sub(target, count),
        has,
        "{at}: takes {count} of {target}"
    );
    if has {
        take_from_reference(held, target, count);
    }
}

fn take_from_reference(held: &mut BTreeMap<i64, u64>, value: i64, count: u64) {
    let have = held.get_mut(&value).expect("the reference holds the value");
    *have -= count;
    if *have == 0 {
        held.remove(&value);
    }
}

/// Adds and removes at random against a map from value to count: the
/// entries, the ends and every refusal are the map's, through each form
/// a cell takes on its way up and back to empty.
#[test]
fn counts_follow_a_reference_through_every_form() {
    for shape in &SHAPES {
        let mut rng = Rng::new(11);
        let mut followed = Followed {
            counts: ValueCounts::EMPTY,
            held: BTreeMap::new(),
        };
        let mut forms = vec![form(&followed.counts)];
        let grow = shape.values * 3;
        let mut step = 0;
        while step < grow || !followed.held.is_empty() {
            let at = format!("{}, step {step}", shape.name);
            // Mostly adds on the way up, then removes until none is left.
            let adds = if step < grow { 80 } else { 10 };
            let value = shape.value(rng.below(shape.values));
            if rng.below(100) < adds {
                let count = 1 + rng.below(shape.most);
                followed.counts.add(value, count);
                *followed.held.entry(value).or_default() += count;
            } else {
                take_near(&mut rng, &mut followed, value, &at);
            }
            let Followed { counts, held } = &followed;
            assert_eq!(counts.ends(), ends_of(held), "{at}");
            if step % 199 == 0 || step + 1 == grow || held.is_empty() {
                check(counts, held, &at);
            }
            if step < grow && forms.last() != Some(&form(counts)) {
                forms.push(form(counts));
            }
            step += 1;
        }
        forms.dedup();
        assert!(forms.ends_with(shape.forms), "{}: {forms:?}", shape.name);
        assert!(
            matches!(followed.counts, ValueCounts::Inline(cell) if cell[0] == 0),
            "{}: {} when empty",
            shape.name,
            form(&followed.counts)
        );
    }
}

/// Values that arrive in order: each new least value goes to the first
/// run, below its key, and each greatest to the end of the last.
#[test]
fn values_in_order_find_their_runs() {
    for descending in [false, true] {
        let mut counts = ValueCounts::EMPTY;
        let mut held = BTreeMap::new();
        for index in 0..20_000_i64 {
            let value = if descending {
                -index * 1_000
            } else {
                index * 1_000
            };
            counts.add(value, 1);
            held.insert(value, 1);
            assert_eq!(counts.ends(), ends_of(&held), "{value}");
        }
        assert_eq!(form(&counts), "runs");
        check(&counts, &held, "in order");
        // Out again from the least: every run's key leaves ahead of its run.
        for ValueCount { value, count } in entries_of(&held) {
            assert!(counts.sub(value, count), "{value}");
            held.remove(&value);
            assert_eq!(counts.ends(), ends_of(&held), "{value}");
        }
        assert_eq!(form(&counts), "cell");
    }
}

fn draw(rng: &mut Rng, values: u64) -> BTreeMap<i64, u64> {
    let mut held = BTreeMap::new();
    for _ in 0..values {
        *held
            .entry(rng.below(20_000).cast_signed() * 70)
            .or_default() += 1 + rng.below(3);
    }
    held
}

/// A cell's counts fold into another's and come back out, as when a
/// dim row moves its cells; a cell covers only what it holds.
#[test]
fn counts_fold_in_and_come_out_again() {
    for (ours, theirs) in [(3, 2), (300, 40), (9_000, 9_000), (0, 5)] {
        let mut rng = Rng::new(5);
        let (our_held, their_held) = (draw(&mut rng, ours), draw(&mut rng, theirs));
        let mut counts = ValueCounts::from_ascending(&entries_of(&our_held));
        let other = ValueCounts::from_ascending(&entries_of(&their_held));
        counts.add_counts(&other);
        let mut both = our_held.clone();
        for (&value, &count) in &their_held {
            *both.entry(value).or_default() += count;
        }
        check(&counts, &both, "folded in");
        assert!(counts.covers(&other));
        counts.sub_counts(&other);
        check(&counts, &our_held, "taken out");
        // One more of a value than is left, and a value never held.
        let (&value, &count) = their_held.iter().next().unwrap();
        let more = our_held.get(&value).copied().unwrap_or(0) + count + 1;
        assert!(!counts.covers(&ValueCounts::from_ascending(&[entry(value, more)])));
        assert!(!counts.covers(&ValueCounts::from_ascending(&[entry(-7, 1)])));
    }
}

/// Nothing leaves a cell that holds less, whichever form it has, and
/// the cell is as it was.
#[test]
fn a_cell_gives_only_what_it_holds() {
    for values in [0, 3, 400, 20_000] {
        let mut entries = Vec::new();
        for index in 0..values {
            entries.push(entry(index * 70, 2));
        }
        let mut counts = ValueCounts::from_ascending(&entries);
        for (value, count) in [(-70, 1), (35, 1), (values * 70, 1), (0, 3), (70, u64::MAX)] {
            assert!(
                !counts.has(value, count),
                "{values} values: has {count} of {value}"
            );
            assert!(
                !counts.sub(value, count),
                "{values} values: gave {count} of {value}"
            );
        }
        assert!(counts.iter().eq(entries.iter().copied()), "{values} values");
    }
}

/// The form entries are encoded in, how many records or runs or bytes
/// it holds, and its value and count widths.
fn layout(entries: &[ValueCount]) -> (&'static str, usize, u8, u8) {
    match ValueCounts::from_ascending(entries) {
        ValueCounts::Inline(cell) => ("cell", usize::from(cell[0]), cell[1], cell[2]),
        ValueCounts::Run(run) => ("run", run.len(), run[0], run[1]),
        ValueCounts::Runs(runs) => ("runs", runs.len(), 0, 0),
    }
}

fn once(values: i64, least: i64, step: i64) -> Vec<ValueCount> {
    let mut entries = Vec::new();
    for index in 0..values {
        entries.push(entry(least + index * step, 1));
    }
    entries
}

/// Where a cell keeps its values, and in how many bytes.
#[test]
fn a_cell_keeps_its_values_in_the_fewest_bytes() {
    // Three fares seen once: two bytes each, and none for the counts.
    assert_eq!(
        layout(&[entry(1280, 1), entry(1350, 1), entry(1420, 1)]),
        ("cell", 3, 2, 0)
    );
    // A fare seen twice gives every count a byte.
    assert_eq!(
        layout(&[entry(1280, 2), entry(1350, 1), entry(1420, 1)]),
        ("cell", 3, 2, 1)
    );
    assert_eq!(layout(&[]), ("cell", 0, 1, 0));
    // What a cell holds itself, and one value more.
    assert_eq!(layout(&once(28, 0, 1)), ("cell", 28, 1, 0));
    assert_eq!(layout(&once(29, 0, 1)), ("run", RUN_HEADER + 29, 1, 0));
    // A run's values are kept from its middle one, so far from zero
    // they are as narrow as near it.
    assert_eq!(layout(&once(40, 0, 70)), ("run", RUN_HEADER + 80, 2, 0));
    assert_eq!(
        layout(&once(40, 1 << 50, 70)),
        ("run", RUN_HEADER + 80, 2, 0)
    );
    // A run takes RUN_MAX_BYTES bytes, and one record more makes two.
    let full = i64::try_from((RUN_MAX_BYTES - RUN_HEADER) / 2).expect("a run's records");
    assert_eq!(layout(&once(full, 0, 5)), ("run", RUN_MAX_BYTES, 2, 0));
    assert_eq!(layout(&once(full + 1, 0, 5)), ("runs", 2, 0, 0));
}

/// Each width a value or a count takes, at its ends, read back as
/// written whether encoded at once or added one at a time.
#[test]
fn values_and_counts_read_back_at_every_width() {
    for width in 1..=8_u32 {
        let greatest = i64::MAX >> (64 - 8 * width);
        let most = u64::MAX >> (64 - 8 * width);
        for entries in [
            vec![entry(greatest, 1)],
            vec![entry(-greatest - 1, 1)],
            vec![
                entry(-greatest - 1, most),
                entry(0, 1),
                entry(greatest, most),
            ],
            vec![
                entry(-greatest - 1, 1),
                entry(-1, most),
                entry(greatest - 1, 2),
                entry(greatest, 1),
            ],
        ] {
            let at = format!("{width} bytes: {entries:?}");
            let counts = ValueCounts::from_ascending(&entries);
            assert!(counts.iter().eq(entries.iter().copied()), "{at}");
            let mut added = ValueCounts::EMPTY;
            for added_entry in entries.iter().rev() {
                added.add(added_entry.value, added_entry.count);
            }
            assert!(added.iter().eq(entries.iter().copied()), "{at}: added");
            let ends = Ends {
                least: entries[0].value,
                greatest: entries[entries.len() - 1].value,
            };
            assert_eq!(added.ends(), ends, "{at}");
        }
    }
}

/// A run widens when a value or a count outgrows it, whatever it holds.
#[test]
fn a_run_widens_for_a_value_or_a_count_it_cannot_hold() {
    let narrow = once(100, 0, 1);
    for (value, count) in [
        (1 << 20, 1),
        (-(1 << 40), 1),
        (i64::MAX, 1),
        (i64::MIN, 1),
        (50, 1),
        (50, 1 << 30),
        (200, u64::MAX),
    ] {
        let mut counts = ValueCounts::from_ascending(&narrow);
        let mut held: BTreeMap<i64, u64> = BTreeMap::new();
        for narrow_entry in &narrow {
            held.insert(narrow_entry.value, narrow_entry.count);
        }
        counts.add(value, count);
        *held.entry(value).or_default() += count;
        check(&counts, &held, &format!("{count} of {value}"));
        assert!(counts.sub(value, count));
        take_from_reference(&mut held, value, count);
        check(&counts, &held, &format!("{count} of {value} taken"));
    }
}
