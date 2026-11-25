//! `Store` against what it must answer: a reference that aggregates the
//! live trips directly and joins them to the dims as they are, over random
//! inserts, updates and deletes of trips and dim rows, and hand-made cases
//! for joins, ranges and errors. Rows hold the fact's columns only;
//! a dim's values come through its projection when asked. This file holds
//! the row layout, the ask of the trips the modules start from, and the
//! modules, one per subject.

#[path = "../common/mod.rs"]
mod common;

mod asks;
mod bound;
mod dims;
mod having;
mod measures;
mod random;
mod ranges;
mod reference;
mod retention;
mod retention_random;
mod rows;
mod views;
mod workload;

use std::collections::BTreeMap;
use straum_domain::{Ask, AskKey, ColumnType, Datum, Store, Value};

use common::{id, names};

/// The `date_bin` width of every view here, a quarter hour, in seconds.
const WIDTH: u32 = 900;
/// 2024-01-15 and 2024-04-15, 00:00 UTC: four months, one leap day.
const START: i64 = 1_705_276_800;
const END: i64 = 1_713_139_200;

// The wider bucket widths, in seconds.
const HOUR: u32 = 3_600;
const DAY: u32 = 86_400;
const WEEK: u32 = 604_800;

// Seeds of the random workload, and its size: the steps, how often it
// asks, and how many asks then.
const SEEDS: u64 = 16;
const STEPS: u32 = 2_000;
const CHECK_EVERY: u32 = 100;
const ASKS_PER_CHECK: usize = 10;

// Row positions; `columns` and `rollup_key` check them.
const AT: usize = 0;
const PAY: usize = 1;
const VENDOR: usize = 2;
const SHARED: usize = 3;
const PU: usize = 4;
const DRO: usize = 5;
const HOME: usize = 6;
const FARE: usize = 7;
const TIP: usize = 8;
const DIST: usize = 9;
const RIDER: usize = 10;
const TOLLED: usize = 11;
/// Columns in a trip row.
const ROW_COLUMNS: usize = 12;

/// The dims as the projections key them: zones by `id`, combos by
/// `(pu_id, vendor_id)`, vendors by `id`; each key to its attributes.
type Dims = BTreeMap<&'static str, DimRows>;

/// One dim's rows: each key to its attributes.
type DimRows = BTreeMap<Vec<Datum>, Vec<Datum>>;

/// The types a load sets for a dim: the key, then the attributes.
struct DimTypes {
    dim: &'static str,
    types: &'static [ColumnType],
}

const DIM_TYPES: [DimTypes; 3] = [
    DimTypes {
        dim: "zones",
        types: &[ColumnType::Int, ColumnType::Text, ColumnType::Text],
    },
    DimTypes {
        dim: "combos",
        types: &[ColumnType::Int, ColumnType::Int, ColumnType::Text],
    },
    DimTypes {
        dim: "vendors",
        types: &[ColumnType::Int, ColumnType::Text],
    },
];

/// The scales of the numeric columns a load sets, from this up. Each
/// differs, so an answer that takes a scale from the wrong column shows.
const TRIP_SCALES: u32 = 20;

/// Every key of the trips view an ask may group by, the bucket aside.
const KEYS: [&str; 14] = [
    "pay",
    "vendor",
    "shared",
    "pu",
    "dro",
    "pickup_borough",
    "pickup_zone",
    "dropoff_borough",
    "dropoff_zone",
    "combo_label",
    "known",
    "unpaid",
    "vend_label",
    "home_borough",
];

/// An ask of the trips view for `measures` by `keys` over every bucket
/// the tests fill, with no filter or HAVING.
fn trips_ask(measures: &[&str], keys: Vec<AskKey>) -> Ask {
    Ask {
        view: id("trips"),
        measures: names(measures),
        keys,
        filter: Vec::new(),
        having: Vec::new(),
        from: Some(START),
        to: Some(END),
        bounded: None,
        bound_zone: straum_domain::BoundZone::Utc,
    }
}

/// The measures of the first row of the answer to `ask`.
fn first_values(store: &Store, ask: &Ask) -> Vec<Value> {
    store.ask(ask).unwrap().rows[0].values.clone()
}
