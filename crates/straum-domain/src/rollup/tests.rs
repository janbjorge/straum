//! Code reclaiming when the `date_bin` level no longer holds every key: a
//! test drops a `date_bin` bucket by hand, as dropping old buckets there
//! while wider levels keep them would, then reclaims the codes and reads
//! the keys back from the widest level. Then the inputs that measures share.

use super::*;
use crate::spec::{ByKey, Measure, Table, Time};

const DATE_BIN_SECONDS: u32 = 900;
const HOUR_SECONDS: u32 = 3_600;
/// More keys than a part holds before a recode is worth it.
const DEAD_KEYS: i64 = 5_000;
const PAY: PartIndex = PartIndex(0);
/// The key only the hour level keeps once its `date_bin` bucket goes.
const OLD_PAY: i64 = -1;
/// The key both levels keep.
const NEW_PAY: i64 = -2;
const NEW_AT: i64 = 7_200;

fn id(name: &str) -> Identifier {
    Identifier::new(name).expect("a valid identifier")
}

/// Trips counted by pay with `measures`, every quarter hour and every hour.
fn rollup(measures: Vec<Measure>) -> Rollup {
    let fact = Fact {
        as_text: Vec::new(),
        name: id("trips"),
        table: Table {
            schema: id("public"),
            name: id("trips"),
        },
        time: Time {
            column: id("at"),
            width: Width {
                seconds: DATE_BIN_SECONDS,
            },
            wider: vec![Width {
                seconds: HOUR_SECONDS,
            }],
            retained: Vec::new(),
            name: id("bucket"),
        },
        refs: Vec::new(),
        by_keys: vec![ByKey::Column {
            column: id("pay"),
            name: id("pay"),
        }],
        filter: Vec::new(),
        measures,
    };
    Rollup::new(&fact)
}

/// A rollup whose pay part holds many dead codes, then one key at the
/// start of time and one later.
fn rollup_with_dead_codes() -> Rollup {
    let mut rollup = rollup(Vec::new());
    for pay in 0..DEAD_KEYS {
        rollup.add(&[Datum::Int(0), Datum::Int(pay)]).unwrap();
    }
    for pay in 0..DEAD_KEYS {
        rollup.remove(&[Datum::Int(0), Datum::Int(pay)]).unwrap();
    }
    rollup.add(&[Datum::Int(0), Datum::Int(OLD_PAY)]).unwrap();
    rollup
        .add(&[Datum::Int(NEW_AT), Datum::Int(NEW_PAY)])
        .unwrap();
    rollup
}

/// Takes the `date_bin` bucket at `start` out, its cells with it.
fn drop_date_bin_bucket(rollup: &mut Rollup, start: i64) {
    let cells = rollup.levels[DATE_BIN.0].buckets.remove(&Some(start));
    let cells = cells.expect("the bucket holds a cell");
    rollup.level_cells[DATE_BIN.0] -= cells.len();
}

/// The pay of every cell at the widest level, in bucket order.
fn widest_pays(rollup: &Rollup) -> Vec<Datum> {
    let mut pays = Vec::new();
    for cells in rollup.buckets(rollup.widest()).values() {
        for cell in 0..cells.len() {
            pays.push(rollup.value(PAY, cells.code(cell, PAY)).clone());
        }
    }
    pays
}

#[test]
fn a_key_only_a_wider_level_holds_keeps_its_code() {
    let mut rollup = rollup_with_dead_codes();
    drop_date_bin_bucket(&mut rollup, 0);
    rollup.reclaim_codes();
    assert_eq!(rollup.codes(PAY), 2);
    assert_eq!(
        widest_pays(&rollup),
        vec![Datum::Int(OLD_PAY), Datum::Int(NEW_PAY)]
    );
    assert!(rollup.holds_cells());
    assert_eq!(rollup.cells(), 1);
}

#[test]
fn a_key_no_level_holds_loses_its_code() {
    let mut rollup = rollup_with_dead_codes();
    rollup.reclaim_codes();
    assert_eq!(rollup.codes(PAY), 2);
    assert_eq!(rollup.code_of(PAY, &Datum::Int(0)), None);
    rollup
        .remove(&[Datum::Int(0), Datum::Int(OLD_PAY)])
        .unwrap();
    rollup
        .remove(&[Datum::Int(NEW_AT), Datum::Int(NEW_PAY)])
        .unwrap();
    assert!(!rollup.holds_cells());
}

/// A count of a column beside a sum of it reads the nulls that the sum
/// keeps, so the cells keep one state for both. A count alone keeps its own.
#[test]
fn a_count_beside_a_sum_shares_its_state() {
    let count = Aggregate::CountNotNull(id("fare"));
    let sum = Aggregate::Sum(id("fare"));
    let alone = rollup(vec![measure("fares", &count)]);
    assert_eq!(alone.inputs.len(), 1);
    let both = rollup(vec![measure("fares", &count), measure("fare", &sum)]);
    assert_eq!(both.inputs.len(), 1);
    assert_eq!(both.state_of(&count), both.state_of(&sum));
}

fn measure(name: &str, aggregate: &Aggregate) -> Measure {
    Measure {
        name: id(name),
        aggregate: aggregate.clone(),
        filter: Vec::new(),
    }
}
