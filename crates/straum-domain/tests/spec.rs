//! `Spec::new` on hand-built declarations, including shapes straum SQL can't
//! produce (a ref to a missing dim, a dim attribute listed twice).

use straum_domain::{
    Agg, Cond, Dim, Fact, Grain, Ident, KeyPath, Literal, Measure, Ref, Spec, SpecError, Table,
    Time,
};

fn id(s: &str) -> Ident {
    Ident::new(s).unwrap()
}

fn table(name: &str) -> Table {
    Table {
        schema: id("public"),
        name: id(name),
    }
}

fn dim() -> Dim {
    Dim {
        name: id("z"),
        table: table("z"),
        key: id("id"),
        attrs: vec![id("name")],
    }
}

fn fact() -> Fact {
    Fact {
        name: id("t"),
        table: table("t"),
        time: Time {
            column: id("at"),
            grain: Grain(3_600),
            buckets: vec![],
        },
        refs: vec![Ref {
            name: id("r"),
            dim: id("z"),
            via: id("z_id"),
        }],
        by: vec![id("a")],
        measures: vec![Measure {
            name: id("n"),
            agg: Agg::Count,
            filter: vec![],
        }],
    }
}

#[test]
fn which_measures_read_numbers() {
    let x = || id("x");
    let cases = [
        (Agg::Sum(x()), true),
        (Agg::Min(x()), true),
        (Agg::Max(x()), true),
        (Agg::Avg(x()), true),
        (Agg::CountDistinct(x()), false),
        (Agg::Count, false),
    ];
    for (agg, numbers) in cases {
        assert_eq!(agg.reads_numbers(), numbers, "{agg:?}");
    }
}

#[test]
fn valid() {
    assert!(Spec::new(vec![dim()], vec![fact()]).is_ok());
}

#[test]
fn tables_once_each_dims_first() {
    let mut zones = dim();
    zones.name = id("zones");
    // A fact over the dim's own table, as a log of the dim's rows.
    let mut log = fact();
    log.name = id("log");
    log.table = table("z");
    let mut other = dim();
    other.name = id("other");
    other.table.schema = id("other");
    let spec = Spec::new(vec![dim(), other, zones], vec![fact(), log]).unwrap();
    let tables: Vec<String> = (spec.tables().iter())
        .map(|t| format!("{}.{}", t.schema, t.name))
        .collect();
    assert_eq!(tables, ["public.z", "other.z", "public.t"]);
}

#[test]
fn buckets_widen_by_whole_multiples() {
    let mut t = fact();
    t.time.buckets = vec![Grain(7_200), Grain(86_400), Grain(604_800)];
    assert!(Spec::new(vec![dim()], vec![t.clone()]).is_ok());

    // 1.5 hours over an hour, then a day that doesn't widen the day.
    t.time.buckets = vec![Grain(5_400), Grain(86_400), Grain(86_400)];
    let errors = Spec::new(vec![dim()], vec![t]).unwrap_err();
    let shown: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert_eq!(
        shown,
        [
            "fact t: bucket of 5400s is not a wider multiple of 3600s",
            "fact t: bucket of 86400s is not a wider multiple of 86400s",
        ]
    );
}

#[test]
fn every_error_is_reported_and_displayed() {
    let mut z = dim();
    z.attrs = vec![id("name"), id("name")];
    let mut t = fact();
    t.name = id("z");
    t.time.grain = Grain(420);
    t.by = vec![id("id"), id("at"), id("r")];
    t.refs[0].dim = id("nope");
    t.measures = vec![
        Measure {
            name: id("m"),
            agg: Agg::Count,
            filter: vec![Cond {
                path: KeyPath {
                    head: id("c"),
                    attr: None,
                },
                values: vec![Literal::Int(1)],
            }],
        },
        Measure {
            name: id("m"),
            agg: Agg::Count,
            filter: vec![],
        },
    ];
    let mut u = fact();
    u.name = id("u");
    u.measures = vec![];

    let errors = Spec::new(vec![z], vec![t, u]).unwrap_err();
    let shown: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert_eq!(
        shown,
        [
            "z declared twice",
            "z: name declared twice",
            "fact z: grain of 420s does not divide a day",
            "fact z: column at used twice among time, by and ref",
            "z: r declared twice",
            "fact z: ref r points at unknown dim nope",
            "fact z: measure m filters on c, which is not a by column, ref or ref attribute",
            "z: m declared twice",
            "fact u: no measures",
        ]
    );
    assert!(matches!(errors[5], SpecError::UnknownDim { .. }));
}
