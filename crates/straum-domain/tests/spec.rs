//! `Spec::new` on hand-built declarations, including shapes straum SQL can't
//! produce (a ref to a missing dim, a dim attribute listed twice).

use straum_domain::{
    Agg, By, Cond, Dim, Fact, Grain, Ident, Key, KeyPath, Literal, Measure, Origin, Ref, Spec,
    SpecError, Table, Time,
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
        key: vec![id("id")],
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
            origin: Origin::Timestamptz,
        },
        refs: vec![Ref {
            name: id("r"),
            dim: id("z"),
            via: vec![id("z_id")],
            attrs: vec![id("name")],
        }],
        by: vec![By::Column(id("a")), By::Column(id("z_id"))],
        measures: vec![Measure {
            name: id("n"),
            agg: Agg::Count,
            filter: vec![],
        }],
    }
}

fn path(s: &str) -> KeyPath {
    match s.split_once('.') {
        Some((head, attr)) => KeyPath {
            head: id(head),
            attr: Some(id(attr)),
        },
        None => KeyPath {
            head: id(s),
            attr: None,
        },
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

/// A bare name is a by key; `ref.attr` is an attribute the ref reads. A
/// ref has no value of its own, and reads only what it declares.
#[test]
fn key_paths_resolve_to_by_keys_and_attributes() {
    let spec = Spec::new(vec![dim()], vec![fact()]).unwrap();
    let t = spec.fact("t").unwrap();
    assert_eq!(t.resolve(&path("a")), Some(Key::By(&t.by[0])));
    assert_eq!(t.resolve(&path("z_id")), Some(Key::By(&t.by[1])));
    assert_eq!(
        t.resolve(&path("r.name")),
        Some(Key::Attr(&t.refs[0], &t.refs[0].attrs[0]))
    );
    for missing in ["r", "r.id", "r.nope", "q.name", "b"] {
        assert_eq!(t.resolve(&path(missing)), None, "{missing}");
    }
}

/// A row is read from the time column, the by columns, the via columns and
/// the measure inputs, each once, and then holds the ref attributes.
#[test]
fn columns_and_rollup_key() {
    let mut t = fact();
    t.by = vec![By::Column(id("a"))];
    t.measures.push(Measure {
        name: id("s"),
        agg: Agg::Sum(id("z_id")),
        filter: vec![],
    });
    assert_eq!(t.columns(), [id("at"), id("a"), id("z_id")]);
    let attrs: Vec<String> = t.attrs().map(|(r, a)| format!("{}.{a}", r.name)).collect();
    assert_eq!(attrs, ["r.name"]);
    assert_eq!(t.rollup_key().count(), 3);
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
    let mut w = dim();
    w.name = id("w");
    w.key = vec![];
    let mut t = fact();
    t.name = id("z");
    t.time.grain = Grain(420);
    t.by = vec![
        By::Column(id("id")),
        By::Column(id("at")),
        By::Column(id("r")),
        By::Column(id("id")),
        By::IsNull {
            column: id("at"),
            name: id("no_time"),
            negated: false,
        },
    ];
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
    // A ref on two columns to a key of one, one joining on a column twice,
    // one reading nothing, and one reading an attribute the dim lacks and
    // another twice.
    let mut u = fact();
    u.name = id("u");
    u.by = vec![
        By::Column(id("a")),
        By::Column(id("z_id")),
        By::Column(id("b")),
    ];
    u.refs = vec![
        Ref {
            name: id("r"),
            dim: id("z"),
            via: vec![id("z_id"), id("b")],
            attrs: vec![id("name")],
        },
        Ref {
            name: id("s"),
            dim: id("z"),
            via: vec![id("a"), id("a")],
            attrs: vec![id("name")],
        },
        Ref {
            name: id("e"),
            dim: id("z"),
            via: vec![id("a")],
            attrs: vec![],
        },
        Ref {
            name: id("q"),
            dim: id("z"),
            via: vec![id("b")],
            attrs: vec![id("nope"), id("name"), id("name")],
        },
    ];
    u.measures = vec![];

    let errors = Spec::new(vec![z, w], vec![t, u]).unwrap_err();
    let shown: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert_eq!(
        shown,
        [
            "z declared twice",
            "z: name declared twice",
            "dim w: no key columns",
            "fact z: grain of 420s does not divide a day",
            "fact z: at is the time column, so it can't be a by column too",
            "z: id declared twice",
            "fact z: no_time asks whether the time column at is null, which no ask could see: \
             a row with no time is never answered",
            "z: r declared twice",
            "fact z: ref r points at unknown dim nope",
            "fact z: measure m filters on c, which is not a by column or ref attribute",
            "z: m declared twice",
            "fact u: ref r joins on 2 columns, but the key of dim z has 1",
            "fact u: ref s joins on a twice",
            "fact u: ref s joins on 2 columns, but the key of dim z has 1",
            "fact u: ref e reads no column of its dim",
            "fact u: ref q reads nope, which dim z has no column of",
            "q: name declared twice",
            "fact u: no measures",
        ]
    );
    assert!(matches!(errors[8], SpecError::UnknownDim { .. }));
}
