//! The views the tests build their stores from: `trips` joined to zones,
//! combos and vendors with every kind of measure, variances among them, a smaller one joined to
//! zones alone, and one of percentiles. Also the column types a load sets,
//! so a store here is typed as the source would type it.

use crate::common::{attribute, by_key, condition, dim, id, lit, measure, percentile, table};
use crate::{DAY, DIM_TYPES, DIST, FARE, HOUR, ROW_COLUMNS, TIP, TRIP_SCALES, WEEK, WIDTH};
use straum_domain::{
    Aggregate, ByKey, ColumnType, Fact, Literal, Measure, Ref, RefKind, Spec, Store, Time, Via,
    Width,
};

/// The trips view with every join a left one.
pub(crate) fn spec() -> Spec {
    spec_joining(RefKind::Left)
}

/// As [`spec`], the vendors joined as `vend` says: an inner join drops
/// the trips of a vendor with no row, so their buckets hold no bound
/// cells.
pub(crate) fn spec_joining(vend: RefKind) -> Spec {
    let zones = dim("zones", &["id"], &["borough", "zone"]);
    // Keyed by two fact columns, one of them a by column asks group by.
    let combos = dim("combos", &["pu_id", "vendor_id"], &["label"]);
    let vendors = dim("vendors", &["id"], &["label"]);
    let mut trips = trips_by_pay(
        [
            count_sum_and_rank_measures(),
            average_count_and_bool_measures(),
        ]
        .concat(),
    );
    // Hours, days and weeks, so random ranges read every width.
    trips.time.wider = vec![
        Width { seconds: HOUR },
        Width { seconds: DAY },
        Width { seconds: WEEK },
    ];
    trips.refs = trip_refs(vend);
    trips.by_keys = trip_by_keys();
    Spec::new(vec![zones, combos, vendors], vec![trips]).unwrap()
}

/// Trips by pay at the quarter hour, joined to nothing, with `measures`.
pub(crate) fn trips_by_pay(measures: Vec<Measure>) -> Fact {
    Fact {
        as_text: Vec::new(),
        name: id("trips"),
        table: table("trips"),
        time: Time {
            column: id("at"),
            width: Width { seconds: WIDTH },
            wider: Vec::new(),
            name: id("bucket"),
        },
        refs: Vec::new(),
        by_keys: vec![by_key("pay")],
        filter: vec![],
        measures,
    }
}

/// The trips' joins: pickup and dropoff zones, the combo of pickup and
/// vendor, the vendor as `vend` says, and the home zone.
fn trip_refs(vend: RefKind) -> Vec<Ref> {
    vec![
        Ref {
            alias: id("pickup"),
            dim: id("zones"),
            via: vec![Via::Fact(id("pu"))],
            kind: RefKind::Left,
            reads: vec![
                attribute("pickup_borough", "borough"),
                attribute("pickup_zone", "zone"),
            ],
        },
        Ref {
            alias: id("dropoff"),
            dim: id("zones"),
            via: vec![Via::Fact(id("dro"))],
            kind: RefKind::Left,
            reads: vec![
                attribute("dropoff_borough", "borough"),
                attribute("dropoff_zone", "zone"),
            ],
        },
        Ref {
            alias: id("combo"),
            dim: id("combos"),
            via: vec![Via::Fact(id("pu")), Via::Fact(id("vendor"))],
            kind: RefKind::Left,
            reads: vec![attribute("combo_label", "label")],
        },
        Ref {
            alias: id("vend"),
            dim: id("vendors"),
            via: vec![Via::Fact(id("vendor"))],
            kind: vend,
            reads: vec![attribute("vend_label", "label")],
        },
        // On a column that is no by key: trips from several home
        // zones of one borough share a bound cell, so a zone that
        // moves takes its trips out of a cell the others stay in.
        Ref {
            alias: id("home"),
            dim: id("zones"),
            via: vec![Via::Fact(id("home"))],
            kind: RefKind::Left,
            reads: vec![attribute("home_borough", "borough")],
        },
    ]
}

/// The trips' by keys: five columns, and two null-ness keys.
fn trip_by_keys() -> Vec<ByKey> {
    vec![
        by_key("pay"),
        by_key("vendor"),
        by_key("shared"),
        by_key("pu"),
        by_key("dro"),
        // Over by columns, which the row carries once: one key for
        // each way round.
        ByKey::IsNull {
            column: id("shared"),
            name: id("known"),
            negated: true,
        },
        ByKey::IsNull {
            column: id("pay"),
            name: id("unpaid"),
            negated: false,
        },
    ]
}

/// Counts, sums, mins, maxes and percentiles, one filtered.
fn count_sum_and_rank_measures() -> Vec<Measure> {
    vec![
        measure("trips", Aggregate::Count),
        measure("fare", Aggregate::Sum(id("fare"))),
        measure("tip_min", Aggregate::Min(id("tip"))),
        measure("tip_max", Aggregate::Max(id("tip"))),
        measure("tip_median", percentile(5, 1, "tip")),
        measure("tip_lowest", percentile(0, 0, "tip")),
        measure("tip_highest", percentile(1, 0, "tip")),
        // A rank PostgreSQL's float8 puts one past exact math's at
        // every hundred values.
        measure("fare_p07", percentile(7, 2, "fare")),
        Measure {
            filter: vec![condition("pay", vec![Literal::Int(1)])],
            ..measure("card_tip_p90", percentile(90, 2, "tip"))
        },
        // Beside the fare's sum and the distance's average, so they share
        // one state; the tip's on its own.
        // Beside the tip's min, max and percentiles, so it reads their
        // counts; the distance's on its own.
        measure("tip_mode", Aggregate::Mode(id("tip"))),
        measure("distance_mode", Aggregate::Mode(id("dist"))),
        // Distinct values of numbers: the fare's beside its p07, the
        // tip's beside its min, max and percentiles, so both read the
        // counts those keep; the pickups' below keep their own. Each
        // count reads the tally of the sum or average under the same
        // FILTER, and the card tips' count comes first, so the tally
        // must hold a sum.
        measure("distinct_fares", Aggregate::SumDistinct(id("fare"))),
        measure("distinct_fare_count", Aggregate::CountDistinct(id("fare"))),
        Measure {
            filter: vec![condition("pay", vec![Literal::Int(1)])],
            ..measure("card_distinct_tips", Aggregate::CountDistinct(id("tip")))
        },
        Measure {
            filter: vec![condition("pay", vec![Literal::Int(1)])],
            ..measure("card_distinct_tip", Aggregate::AvgDistinct(id("tip")))
        },
        measure("fare_var", Aggregate::VarSamp(id("fare"))),
        measure("tip_var_pop", Aggregate::VarPop(id("tip"))),
        measure("distance_sd", Aggregate::StddevSamp(id("dist"))),
        Measure {
            filter: vec![condition("pay", vec![Literal::Int(1)])],
            ..measure("card_tip_sd_pop", Aggregate::StddevPop(id("tip")))
        },
    ]
}

/// Averages, counts of distinct and of not-null values, and the boolean
/// aggregates, several filtered on keys and dim columns.
fn average_count_and_bool_measures() -> Vec<Measure> {
    vec![
        // A sum over a column the load groups by.
        measure("tip_avg", Aggregate::Avg(id("tip"))),
        measure("distance", Aggregate::Avg(id("dist"))),
        measure("riders", Aggregate::CountDistinct(id("rider"))),
        measure("pickups", Aggregate::CountDistinct(id("pu"))),
        Measure {
            filter: vec![condition("pay", vec![Literal::Int(1)])],
            ..measure("card", Aggregate::Count)
        },
        Measure {
            filter: vec![condition("shared", vec![Literal::Bool(true)])],
            ..measure("shared_fare", Aggregate::Sum(id("fare")))
        },
        Measure {
            filter: vec![
                condition("pickup_borough", vec![lit("m"), lit("q")]),
                condition("vendor", vec![Literal::Int(2)]),
            ],
            ..measure("mq_fare", Aggregate::Sum(id("fare")))
        },
        Measure {
            filter: vec![condition("dropoff_borough", vec![lit("m")])],
            ..measure("m_riders", Aggregate::CountDistinct(id("rider")))
        },
        Measure {
            filter: vec![condition("dro", vec![Literal::Int(3), Literal::Int(6)])],
            ..measure("far_tip", Aggregate::Max(id("tip")))
        },
        Measure {
            filter: vec![condition("pu", vec![Literal::Int(4)])],
            ..measure("zone_four_distance", Aggregate::Avg(id("dist")))
        },
        measure("tipped", Aggregate::CountNotNull(id("tip"))),
        // Over text: a count reads no value, only that there is one.
        Measure {
            filter: vec![condition("dro", vec![Literal::Int(3), Literal::Int(6)])],
            ..measure("far_riders", Aggregate::CountNotNull(id("rider")))
        },
        measure("all_tolled", Aggregate::BoolAnd(id("tolled"))),
        measure("any_tolled", Aggregate::BoolOr(id("tolled"))),
        Measure {
            filter: vec![condition("pay", vec![Literal::Int(1)])],
            ..measure("card_tolled", Aggregate::BoolOr(id("tolled")))
        },
    ]
}

/// The column types a load sets for the trips.
pub(crate) fn trip_types() -> Vec<ColumnType> {
    let types = vec![
        ColumnType::Time { zoned: true },
        ColumnType::Int,
        ColumnType::Int,
        ColumnType::Bool,
        ColumnType::Int,
        ColumnType::Int,
        ColumnType::Int,
        trip_numeric(FARE),
        trip_numeric(TIP),
        trip_numeric(DIST),
        ColumnType::Text,
        ColumnType::Bool,
    ];
    assert_eq!(types.len(), ROW_COLUMNS);
    types
}

/// A numeric of a scale its own, so a scale taken from the wrong column
/// shows.
fn trip_numeric(column: usize) -> ColumnType {
    ColumnType::Numeric {
        scale: TRIP_SCALES + u32::try_from(column).expect("a row position fits a scale"),
    }
}

/// A store with the types a load would set, for the fact and the dims.
pub(crate) fn typed(spec: &Spec) -> Store {
    let mut store = Store::new(spec.clone());
    store.set_types("trips", &trip_types()).unwrap();
    for dim in DIM_TYPES {
        store.set_types(dim.dim, dim.types).unwrap();
    }
    store
}

/// The bucket widths of the zones view: a quarter hour, and an hour too.
#[derive(Clone, Copy)]
pub(crate) enum Widths {
    Quarter,
    QuarterAndHour,
}

/// Trips by pay, joined to zones for the borough as `pickup` says, or
/// with no join at all.
pub(crate) fn zones_spec(pickup: Option<RefKind>, widths: Widths) -> Spec {
    let zones = dim("zones", &["id"], &["borough"]);
    let mut trips = trips_by_pay(vec![measure("trips", Aggregate::Count)]);
    if let Widths::QuarterAndHour = widths {
        trips.time.wider = vec![Width { seconds: HOUR }];
    }
    if let Some(kind) = pickup {
        trips.refs.push(Ref {
            alias: id("pickup"),
            dim: id("zones"),
            via: vec![Via::Fact(id("pu"))],
            kind,
            reads: vec![attribute("borough", "borough")],
        });
    }
    Spec::new(vec![zones], vec![trips]).unwrap()
}

/// Trips by pay with a max and percentiles of the fare, joined to nothing.
pub(crate) fn percentile_spec() -> Spec {
    let trips = trips_by_pay(vec![
        measure("top", Aggregate::Max(id("fare"))),
        measure("median", percentile(5, 1, "fare")),
        measure("p99", percentile(99, 2, "fare")),
        measure("lowest", percentile(0, 0, "fare")),
        measure("highest", percentile(1, 0, "fare")),
        measure("p07", percentile(7, 2, "fare")),
    ]);
    Spec::new(vec![], vec![trips]).unwrap()
}
