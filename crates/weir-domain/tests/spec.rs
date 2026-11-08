//! `Spec::new` on hand-built declarations, including shapes weir SQL can't
//! produce (a ref to a missing dim, a dim attribute listed twice).

use weir_domain::{
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
        key: id("id"),
        time: Time {
            column: id("at"),
            grain: Grain(3_600),
        },
        refs: vec![Ref {
            name: id("r"),
            dim: id("z"),
            via: id("z_id"),
        }],
        by: vec![id("a")],
        append_only: false,
        measures: vec![Measure {
            name: id("n"),
            agg: Agg::Count,
            filter: vec![],
        }],
    }
}

#[test]
fn valid() {
    assert!(Spec::new(vec![dim()], vec![fact()]).is_ok());
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
            "fact z: key column id can't be part of the rollup key",
            "fact z: column at used twice among time, by and ref",
            "z: r declared twice",
            "fact z: ref r points at unknown dim nope",
            "fact z: measure m filters on c, which is not a by column, ref or ref attribute",
            "z: m declared twice",
            "fact u: no measures",
        ]
    );
    assert!(matches!(errors[6], SpecError::UnknownDim { .. }));
}
