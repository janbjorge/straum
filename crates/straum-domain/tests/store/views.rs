//! The views the tests build their stores from: `trips` joined to zones,
//! combos and vendors with every kind of measure, variances among them, a smaller one joined to
//! zones alone, and one of percentiles. Also the column types a load sets,
//! so a store here is typed as the source would type it.

use crate::common::{attribute, by_key, condition, id, lit, percentile, table};
use crate::{DAY, DIM_TYPES, DIST, FARE, HOUR, ROW_COLUMNS, TIP, TRIP_SCALES, WEEK, WIDTH};
use straum_domain::{
    Aggregate, ByKey, ColumnType, Condition, Dim, Fact, Literal, Measure, Origin, Ref, RefKind,
    Spec, Store, Time, Via, Width,
};

/// The trips view with every join a left one.
pub(crate) fn spec() -> Spec {
    spec_joining(RefKind::Left)
}

/// As [`spec`], the vendors joined as `vend` says: an inner join drops
/// the trips of a vendor with no row, so their buckets hold no bound
/// cells.
pub(crate) fn spec_joining(vend: RefKind) -> Spec {
    let zones = Dim {
        as_text: Vec::new(),
        name: id("zones"),
        table: table("zones"),
        key: vec![id("id")],
        attributes: vec![id("borough"), id("zone")],
    };
    // Keyed by two fact columns, one of them a by column asks group by.
    let combos = Dim {
        as_text: Vec::new(),
        name: id("combos"),
        table: table("combos"),
        key: vec![id("pu_id"), id("vendor_id")],
        attributes: vec![id("label")],
    };
    let vendors = Dim {
        as_text: Vec::new(),
        name: id("vendors"),
        table: table("vendors"),
        key: vec![id("id")],
        attributes: vec![id("label")],
    };
    let trips = Fact {
        as_text: Vec::new(),
        name: id("trips"),
        table: table("trips"),
        time: Time {
            column: id("at"),
            width: Width { seconds: WIDTH },
            // Hours, days and weeks, so random ranges read every width.
            wider: vec![
                Width { seconds: HOUR },
                Width { seconds: DAY },
                Width { seconds: WEEK },
            ],
            origin: Origin::Timestamptz,
        },
        refs: trip_refs(vend),
        by_keys: trip_by_keys(),
        filter: vec![],
        measures: [
            count_sum_and_rank_measures(),
            average_count_and_bool_measures(),
        ]
        .concat(),
    };
    Spec::new(vec![zones, combos, vendors], vec![trips]).unwrap()
}

/// A measure of the trips view.
pub(crate) fn new_measure(name: &str, aggregate: Aggregate, filter: Vec<Condition>) -> Measure {
    Measure {
        name: id(name),
        aggregate,
        filter,
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
        new_measure("trips", Aggregate::Count, vec![]),
        new_measure("fare", Aggregate::Sum(id("fare")), vec![]),
        new_measure("tip_min", Aggregate::Min(id("tip")), vec![]),
        new_measure("tip_max", Aggregate::Max(id("tip")), vec![]),
        new_measure("tip_median", percentile(5, 1, "tip"), vec![]),
        new_measure("tip_lowest", percentile(0, 0, "tip"), vec![]),
        new_measure("tip_highest", percentile(1, 0, "tip"), vec![]),
        // A rank PostgreSQL's float8 puts one past exact math's at
        // every hundred values.
        new_measure("fare_p07", percentile(7, 2, "fare"), vec![]),
        new_measure(
            "card_tip_p90",
            percentile(90, 2, "tip"),
            vec![condition("pay", vec![Literal::Int(1)])],
        ),
        // Beside the fare's sum and the distance's average, so they share
        // one state; the tip's on its own.
        new_measure("fare_var", Aggregate::VarSamp(id("fare")), vec![]),
        new_measure("tip_var_pop", Aggregate::VarPop(id("tip")), vec![]),
        new_measure("distance_sd", Aggregate::StddevSamp(id("dist")), vec![]),
        new_measure(
            "card_tip_sd_pop",
            Aggregate::StddevPop(id("tip")),
            vec![condition("pay", vec![Literal::Int(1)])],
        ),
    ]
}

/// Averages, counts of distinct and of not-null values, and the boolean
/// aggregates, several filtered on keys and dim columns.
fn average_count_and_bool_measures() -> Vec<Measure> {
    vec![
        // A sum over a column the load groups by.
        new_measure("tip_avg", Aggregate::Avg(id("tip")), vec![]),
        new_measure("distance", Aggregate::Avg(id("dist")), vec![]),
        new_measure("riders", Aggregate::CountDistinct(id("rider")), vec![]),
        new_measure("pickups", Aggregate::CountDistinct(id("pu")), vec![]),
        new_measure(
            "card",
            Aggregate::Count,
            vec![condition("pay", vec![Literal::Int(1)])],
        ),
        new_measure(
            "shared_fare",
            Aggregate::Sum(id("fare")),
            vec![condition("shared", vec![Literal::Bool(true)])],
        ),
        new_measure(
            "mq_fare",
            Aggregate::Sum(id("fare")),
            vec![
                condition("pickup_borough", vec![lit("m"), lit("q")]),
                condition("vendor", vec![Literal::Int(2)]),
            ],
        ),
        new_measure(
            "m_riders",
            Aggregate::CountDistinct(id("rider")),
            vec![condition("dropoff_borough", vec![lit("m")])],
        ),
        new_measure(
            "far_tip",
            Aggregate::Max(id("tip")),
            vec![condition("dro", vec![Literal::Int(3), Literal::Int(6)])],
        ),
        new_measure(
            "zone_four_distance",
            Aggregate::Avg(id("dist")),
            vec![condition("pu", vec![Literal::Int(4)])],
        ),
        new_measure("tipped", Aggregate::CountNotNull(id("tip")), vec![]),
        // Over text: a count reads no value, only that there is one.
        new_measure(
            "far_riders",
            Aggregate::CountNotNull(id("rider")),
            vec![condition("dro", vec![Literal::Int(3), Literal::Int(6)])],
        ),
        new_measure("all_tolled", Aggregate::BoolAnd(id("tolled")), vec![]),
        new_measure("any_tolled", Aggregate::BoolOr(id("tolled")), vec![]),
        new_measure(
            "card_tolled",
            Aggregate::BoolOr(id("tolled")),
            vec![condition("pay", vec![Literal::Int(1)])],
        ),
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
    let zones = Dim {
        as_text: Vec::new(),
        name: id("zones"),
        table: table("zones"),
        key: vec![id("id")],
        attributes: vec![id("borough")],
    };
    let mut refs = Vec::new();
    if let Some(kind) = pickup {
        refs.push(Ref {
            alias: id("pickup"),
            dim: id("zones"),
            via: vec![Via::Fact(id("pu"))],
            kind,
            reads: vec![attribute("borough", "borough")],
        });
    }
    let trips = Fact {
        as_text: Vec::new(),
        name: id("trips"),
        table: table("trips"),
        time: Time {
            column: id("at"),
            width: Width { seconds: WIDTH },
            wider: match widths {
                Widths::Quarter => Vec::new(),
                Widths::QuarterAndHour => vec![Width { seconds: HOUR }],
            },
            origin: Origin::Timestamptz,
        },
        refs,
        by_keys: vec![by_key("pay")],
        filter: vec![],
        measures: vec![Measure {
            name: id("trips"),
            aggregate: Aggregate::Count,
            filter: vec![],
        }],
    };
    Spec::new(vec![zones], vec![trips]).unwrap()
}

/// Trips by pay with a max and percentiles of the fare, joined to nothing.
pub(crate) fn percentile_spec() -> Spec {
    let trips = Fact {
        as_text: Vec::new(),
        name: id("trips"),
        table: table("trips"),
        time: Time {
            column: id("at"),
            width: Width { seconds: WIDTH },
            wider: Vec::new(),
            origin: Origin::Timestamptz,
        },
        refs: Vec::new(),
        by_keys: vec![by_key("pay")],
        filter: vec![],
        measures: vec![
            new_measure("top", Aggregate::Max(id("fare")), vec![]),
            new_measure("median", percentile(5, 1, "fare"), vec![]),
            new_measure("p99", percentile(99, 2, "fare"), vec![]),
            new_measure("lowest", percentile(0, 0, "fare"), vec![]),
            new_measure("highest", percentile(1, 0, "fare"), vec![]),
            new_measure("p07", percentile(7, 2, "fare"), vec![]),
        ],
    };
    Spec::new(vec![], vec![trips]).unwrap()
}
