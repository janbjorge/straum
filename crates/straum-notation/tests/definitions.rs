//! Each view's `CREATE VIEW` statement, cut from the spec's text as
//! written: its own comments kept, a file's header and stray semicolons
//! left out, and a semicolon in a string or comment no end.

use straum_domain::Definition;
use straum_notation::{definitions, parse};

const TAXI: &str = include_str!("taxi.sql");

/// Each view's name and text, from a spec that parses.
fn cut(text: &str) -> Vec<(String, String)> {
    let spec = parse(text).unwrap();
    let mut named = Vec::new();
    for Definition { view, text } in definitions(text, &spec) {
        named.push((view.as_str().to_owned(), text));
    }
    named
}

fn view(name: &str, body: &str) -> String {
    format!(
        "CREATE VIEW {name} WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket, {body}
FROM public.t AS f
GROUP BY 1"
    )
}

/// A view whose string and comment each hold a semicolon, and which ends
/// the text without one.
const LABELLED: &str = "CREATE VIEW b WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket,
       f.label AS label,
       count(*) FILTER (WHERE f.label = 'x;y') AS n -- no end;
FROM public.t AS f
GROUP BY 1, 2";

#[test]
fn every_view_of_the_taxi_spec_reads_back_as_the_same_spec() {
    let spec = parse(TAXI).unwrap();
    let cut = definitions(TAXI, &spec);
    assert_eq!(cut.len(), spec.facts().len());
    let mut joined = String::new();
    for (definition, fact) in cut.iter().zip(spec.facts()) {
        assert_eq!(definition.view, fact.name);
        assert!(
            definition
                .text
                .contains(&format!("CREATE VIEW {}", fact.name))
        );
        assert!(definition.text.ends_with(';'), "{}", definition.text);
        assert!(TAXI.contains(&definition.text));
        joined.push_str(&definition.text);
        joined.push('\n');
    }
    assert_eq!(parse(&joined).unwrap(), spec);
}

#[test]
fn a_view_keeps_the_comments_right_above_it_and_not_a_header() {
    let text = format!(
        "-- The file's header.\n\n-- Counts t.\n/* by hour */\n{};\n",
        view("a", "count(*) AS n")
    );
    let cut = cut(&text);
    assert_eq!(cut.len(), 1);
    assert!(
        cut[0]
            .1
            .starts_with("-- Counts t.\n/* by hour */\nCREATE VIEW a"),
        "{:?}",
        cut[0].1
    );
    assert!(!cut[0].1.contains("header"));
}

#[test]
fn views_are_cut_at_their_semicolons_and_named_in_order() {
    let text = format!(
        ";\n{};;\n-- only a comment\n;\n{}",
        view("a", "count(*) AS n"),
        LABELLED
    );
    let cut = cut(&text);
    assert_eq!(
        cut[0],
        ("a".to_owned(), format!("{};", view("a", "count(*) AS n")))
    );
    assert_eq!(cut[1].0, "b");
    assert!(cut[1].1.starts_with("CREATE VIEW b"));
    assert!(cut[1].1.contains("'x;y'") && cut[1].1.contains("-- no end;"));
    assert!(cut[1].1.ends_with("GROUP BY 1, 2"));
    assert_eq!(cut.len(), 2);
}

#[test]
fn a_view_after_wide_characters_is_cut_where_it_starts() {
    let text = format!("-- Ærøskøbing, 東京\n\n{};", view("a", "count(*) AS n"));
    let cut = cut(&text);
    assert_eq!(cut[0].1, format!("{};", view("a", "count(*) AS n")));
}

#[test]
fn a_text_the_tokenizer_refuses_has_no_definitions() {
    let spec = parse(&format!("{};", view("a", "count(*) AS n"))).unwrap();
    assert_eq!(definitions("SELECT 'unclosed", &spec), []);
}
