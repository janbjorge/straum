//! The tests of `complete.rs`: what the shell offers at each point of a
//! line and the hint it shows, over the views a `GET /views` lists.

use super::*;

const VIEWS: &str = r#"{"views":[
    {"name":"orders","time":"bucket",
     "from":"2026-01-01T09:00:00Z","to":"2026-01-01T11:00:00Z",
     "keys":[{"name":"bucket","type":"timestamptz"},{"name":"status","type":"text"},
             {"name":"shipped","type":"boolean"},{"name":"country","type":"text"}],
     "measures":["orders","revenue"]},
    {"name":"logins","time":"bucket",
     "from":null,"to":null,
     "keys":[{"name":"bucket","type":"timestamp"},{"name":"country","type":"text"}],
     "measures":["logins"]}]}"#;

fn held() -> Vec<View> {
    read_views(VIEWS).unwrap()
}

/// The candidates at the end of `line`, and the text they replace.
fn tab(line: &str) -> (String, Vec<String>) {
    let Completion { start, candidates } = complete(&held(), line, line.len());
    (line[start..].to_owned(), candidates)
}

fn offers(line: &str) -> Vec<String> {
    tab(line).1
}

/// The hint with the cursor at the end of `line`.
fn hinted(line: &str) -> Option<String> {
    hint(&held(), line, line.len())
}

fn key(name: &str, column_type: &str) -> Key {
    Key {
        name: name.to_owned(),
        column_type: column_type.to_owned(),
    }
}

#[test]
fn reads_the_views_a_straum_serves() {
    let views = held();
    assert_eq!(views.len(), 2);
    assert_eq!(
        views[1],
        View {
            name: "logins".to_owned(),
            time: "bucket".to_owned(),
            keys: vec![key("bucket", "timestamp"), key("country", "text")],
            measures: vec!["logins".to_owned()],
            from: None,
            to: None,
        }
    );
    assert_eq!(views[0].from.as_deref(), Some("2026-01-01T09:00:00Z"));
    // A numeric says its scale beside its type.
    let scaled = r#"{"views":[{"name":"v","time":"t","measures":[],
        "keys":[{"name":"fare","type":"numeric","scale":2}]}]}"#;
    assert_eq!(
        read_views(scaled).unwrap()[0].keys,
        [key("fare", "numeric scale 2")]
    );
    for not_views in [
        "",
        "[]",
        "{}",
        r#"{"views":{}}"#,
        r#"{"views":[{}]}"#,
        r#"{"views":[{"name":1}]}"#,
        r#"{"views":[{"name":"v"}]}"#,
        r#"{"views":[{"name":"v","keys":[{}]}]}"#,
        r#"{"views":[{"name":"v","keys":[{"name":"k"}]}]}"#,
        r#"{"views":[{"name":"v","keys":[]}]}"#,
        r#"{"views":[{"name":"v","keys":[],"measures":[1]}]}"#,
        r#"{"views":[{"name":"v","keys":[],"measures":[]}]}"#,
    ] {
        assert_eq!(read_views(not_views), None, "{not_views}");
    }
}

/// The widths a view keeps only for a time are for people to read: the
/// shell reads the view as it would without them, whatever their shape.
#[test]
fn a_view_with_kept_widths_still_completes() {
    let to = r#""to":"2026-01-01T11:00:00Z","#;
    let kept = r#""kept":[{"width":"15 minutes","for":"2 days","from":null}],"#;
    let with_kept = VIEWS.replacen(to, &format!("{to}{kept}"), 1);
    assert_eq!(read_views(&with_kept), Some(held()));
    let odd = VIEWS.replacen(to, &format!(r#"{to}"kept":1,"#), 1);
    let views = read_views(&odd).unwrap();
    let line = "SELECT orders FROM orders WHERE ";
    let Completion { candidates, .. } = complete(&views, line, line.len());
    assert_eq!(candidates, offers(line));
    assert_ne!(candidates, Vec::<String>::new());
}

#[test]
fn a_scale_that_is_not_a_number_is_left_out() {
    let scaled = r#"{"views":[{"name":"v","time":"t","measures":[],
        "keys":[{"name":"fare","type":"numeric","scale":"two"}]}]}"#;
    assert_eq!(
        read_views(scaled).unwrap()[0].keys,
        [key("fare", "numeric")]
    );
}

#[test]
fn a_line_starts_with_select_or_a_command() {
    assert_eq!(offers(""), ["SELECT ", "\\d ", "\\rows ", "\\? ", "\\q "]);
    assert_eq!(tab("se"), ("se".to_owned(), vec!["SELECT ".to_owned()]));
    assert_eq!(offers("\\"), ["\\d ", "\\rows ", "\\? ", "\\q "]);
    assert_eq!(offers("\\r"), ["\\rows "]);
    assert_eq!(offers("\\rows "), ["orders ", "logins "]);
    assert_eq!(offers("\\rows lo"), ["logins "]);
    assert_eq!(offers("\\rows logins "), Vec::<String>::new());
    assert_eq!(offers("\\d "), ["orders ", "logins "]);
    assert_eq!(offers("\\d lo"), ["logins "]);
    assert_eq!(offers("\\d logins "), Vec::<String>::new());
}

#[test]
fn select_offers_the_names_not_yet_listed() {
    // Any view's names, each once, until FROM says which view.
    assert_eq!(
        offers("SELECT "),
        [
            "bucket ", "status ", "shipped ", "country ", "orders ", "revenue ", "logins "
        ]
    );
    assert_eq!(offers("select s"), ["status ", "shipped "]);
    assert_eq!(
        offers("SELECT status, "),
        [
            "bucket ", "shipped ", "country ", "orders ", "revenue ", "logins ", "FROM "
        ]
    );
    assert_eq!(offers("SELECT status,o"), ["orders "]);
    assert_eq!(offers("SELECT status, orders f"), ["FROM "]);
    // With the view named further on, only its names.
    let (typed, candidates) = tab("SELECT ");
    assert_eq!(typed, "");
    assert!(candidates.contains(&"logins ".to_owned()));
    let line = "SELECT  FROM logins";
    let Completion { candidates, .. } = complete(&held(), line, 7);
    assert_eq!(candidates, ["bucket ", "country ", "logins "]);
}

#[test]
fn from_offers_the_views_then_where_or_the_end() {
    assert_eq!(offers("SELECT orders FROM "), ["orders ", "logins "]);
    assert_eq!(offers("SELECT orders FROM lo"), ["logins "]);
    // WHERE is not needed: without bounds an ask reads every bucket.
    assert_eq!(
        offers("SELECT orders FROM orders "),
        ["WHERE ", "; ", "HAVING "]
    );
    assert_eq!(
        offers("SELECT status, orders FROM orders "),
        ["WHERE ", "GROUP BY status; ", "GROUP BY status HAVING "]
    );
    assert_eq!(offers("SELECT orders FROM orders w"), ["WHERE "]);
    assert_eq!(
        offers("SELECT orders FROM orders WHERE x"),
        Vec::<String>::new()
    );
}

#[test]
fn where_offers_whole_conditions_with_the_next_parameter() {
    let line = "SELECT status, orders FROM orders WHERE ";
    assert_eq!(
        offers(line),
        [
            "bucket >= $1 AND bucket < $2 ",
            "status = $1 ",
            "shipped = $1 ",
            "country = $1 "
        ]
    );
    assert_eq!(
        tab(&format!("{line}bu")),
        (
            "bu".to_owned(),
            vec!["bucket >= $1 AND bucket < $2 ".to_owned()]
        )
    );
    // A key is conditioned once, and a bound written is not offered.
    let line = format!("{line}status = $1 AND ");
    assert_eq!(
        offers(&line),
        [
            "bucket >= $2 AND bucket < $3 ",
            "shipped = $2 ",
            "country = $2 "
        ]
    );
    let line = format!("{line}bucket >= $2 AND ");
    assert_eq!(
        offers(&line),
        ["bucket < $3 ", "shipped = $3 ", "country = $3 "]
    );
    assert_eq!(
        offers("SELECT orders FROM orders WHERE bucket<$1 AND "),
        [
            "bucket >= $2 ",
            "status = $2 ",
            "shipped = $2 ",
            "country = $2 "
        ]
    );
}

#[test]
fn where_offers_nothing_mid_condition_or_for_an_unknown_view() {
    // In the middle of a condition there is nothing to offer.
    assert_eq!(
        offers("SELECT orders FROM orders WHERE status "),
        Vec::<String>::new()
    );
    // A view Straum doesn't serve names no time, so nothing is offered.
    assert_eq!(offers("SELECT x FROM nope WHERE "), Vec::<String>::new());
}

/// The bounds go by the name the view gives its time.
#[test]
fn where_bounds_the_time_by_its_name() {
    let views = r#"{"views":[{"name":"trips","time":"pickup",
        "from":"2026-01-01T09:00:00Z","to":"2026-01-01T11:00:00Z",
        "keys":[{"name":"pickup","type":"timestamptz"},{"name":"kind","type":"text"}],
        "measures":["trips"]}]}"#;
    let held = read_views(views).unwrap();
    let line = "SELECT trips FROM trips WHERE ";
    let Completion { candidates, .. } = complete(&held, line, line.len());
    assert_eq!(candidates, ["pickup >= $1 AND pickup < $2 ", "kind = $1 "]);
    // With both bounds written, only the keys are left.
    let both = "SELECT trips FROM trips WHERE pickup >= $1 AND pickup < $2 AND ";
    let Completion { candidates, .. } = complete(&held, both, both.len());
    assert_eq!(candidates, ["kind = $3 "]);
    let bounded = "SELECT trips FROM trips WHERE pickup >= $1; ";
    assert_eq!(
        hint(&held, bounded, bounded.len()).unwrap(),
        "(the timestamptz for pickup >= $1, Tab gives 2026-01-01T09:00:00Z)"
    );
}

#[test]
fn a_finished_condition_offers_what_follows() {
    let bounds = "WHERE bucket >= $1 AND bucket < $2 ";
    // A bound is not needed to end the query.
    assert_eq!(
        offers("SELECT orders FROM orders WHERE status = $1 "),
        ["AND ", "; ", "HAVING "]
    );
    assert_eq!(
        offers(&format!("SELECT orders FROM orders {bounds}")),
        ["AND ", "; ", "HAVING "]
    );
    assert_eq!(
        offers(&format!(
            "SELECT status, country, orders FROM orders {bounds}"
        )),
        [
            "AND ",
            "GROUP BY status, country; ",
            "GROUP BY status, country HAVING "
        ]
    );
    assert_eq!(
        offers(&format!("SELECT bucket, orders FROM orders {bounds}g")),
        ["GROUP BY bucket; ", "GROUP BY bucket HAVING "]
    );
}

#[test]
fn group_by_offers_the_selected_keys() {
    let line = "SELECT status, country, orders FROM orders \
                WHERE bucket >= $1 AND bucket < $2 GROUP ";
    assert_eq!(offers(line), ["BY "]);
    assert_eq!(offers(&format!("{line}BY ")), ["status, country "]);
    assert_eq!(offers(&format!("{line}BY country, ")), ["status "]);
    assert_eq!(
        offers(&format!("{line}BY country, status ")),
        ["; ", "HAVING "]
    );
}

#[test]
fn having_offers_selected_measures_then_comparisons() {
    let grouped = "SELECT status, orders, revenue FROM orders \
                   WHERE bucket >= $1 AND bucket < $2 GROUP BY status ";
    assert_eq!(offers(grouped), ["; ", "HAVING "]);
    let having = format!("{grouped}HAVING ");
    assert_eq!(offers(&having), ["orders ", "revenue "]);
    assert_eq!(
        offers(&format!("{having}orders ")),
        ["= $3 ", "<> $3 ", "< $3 ", "<= $3 ", "> $3 ", ">= $3 "]
    );
    assert_eq!(offers(&format!("{having}orders > $3 ")), ["AND ", "; "]);
    assert_eq!(
        offers(&format!("{having}orders > $3 AND ")),
        ["orders ", "revenue "]
    );
    // Nothing mid-condition, for a key, or for an unknown view.
    assert_eq!(offers(&format!("{having}orders > ")), Vec::<String>::new());
    assert_eq!(offers(&format!("{having}status ")), Vec::<String>::new());
    assert_eq!(
        offers("SELECT x FROM nope WHERE bucket >= $1 AND bucket < $2 HAVING "),
        Vec::<String>::new()
    );
    assert_eq!(
        hinted(&format!("{having}revenue >= $3; a b ")).as_deref(),
        Some("(the value for revenue >= $3)")
    );
}

#[test]
fn a_parameter_that_is_a_bound_offers_the_buckets_held() {
    let query = "SELECT orders FROM orders WHERE shipped = $3 \
                 AND bucket >= $1 AND bucket < $2";
    assert_eq!(offers(&format!("{query}; ")), ["2026-01-01T09:00:00Z "]);
    assert_eq!(offers(&format!("{query};")), ["2026-01-01T09:00:00Z "]);
    assert_eq!(
        tab(&format!("{query}; 2026-01-01T09:00:00Z 20")),
        ("20".to_owned(), vec!["2026-01-01T11:00:00Z ".to_owned()])
    );
    assert_eq!(offers(&format!("{query}; a b ")), ["true ", "false "]);
    assert_eq!(offers(&format!("{query}; a b f")), ["false "]);
}

#[test]
fn a_quoted_value_is_one_parameter() {
    let query = "SELECT orders FROM orders WHERE shipped = $3 \
                 AND bucket >= $1 AND bucket < $2";
    assert_eq!(
        offers(&format!("{query}; 'a b' 'c d' ")),
        ["true ", "false "]
    );
    assert_eq!(
        hinted(&format!("{query}; 'a b' 'c d' ")).as_deref(),
        Some("(the boolean for shipped = $3)")
    );
}

#[test]
fn inside_an_open_quote_nothing_is_offered_or_hinted() {
    let query = "SELECT orders FROM orders WHERE shipped = $3 \
                 AND bucket >= $1 AND bucket < $2";
    for open in ["; 'a b ", "; a 'b c ", "; a 'b c f"] {
        let line = format!("{query}{open}");
        assert_eq!(offers(&line), Vec::<String>::new(), "{open}");
        assert_eq!(hinted(&line), None, "{open}");
    }
}

#[test]
fn a_parameter_offers_nothing_where_nothing_is_known() {
    let query = "SELECT orders FROM orders WHERE shipped = $3 \
                 AND bucket >= $1 AND bucket < $2";
    // Past the last parameter, and where nothing is known.
    assert_eq!(offers(&format!("{query}; a b c ")), Vec::<String>::new());
    assert_eq!(
        offers("SELECT orders FROM orders WHERE status = $1; "),
        Vec::<String>::new()
    );
    let logins = "SELECT logins FROM logins WHERE bucket >= $1 AND bucket < $2; ";
    assert_eq!(offers(logins), Vec::<String>::new());
    assert_eq!(
        offers("SELECT x FROM nope WHERE bucket >= $1; "),
        Vec::<String>::new()
    );
    assert_eq!(
        offers("SELECT x WHERE bucket >= $1; "),
        Vec::<String>::new()
    );
    assert_eq!(
        offers("SELECT orders FROM orders WHERE $1; "),
        Vec::<String>::new()
    );
}

#[test]
fn a_hint_says_what_the_next_parameter_is() {
    let query = "SELECT orders FROM orders WHERE status IN ($3, $4) \
                 AND bucket >= $1 AND bucket < $2";
    assert_eq!(
        hinted(&format!("{query};")).as_deref(),
        Some(" (the timestamptz for bucket >= $1, Tab gives 2026-01-01T09:00:00Z)")
    );
    assert_eq!(
        hinted(&format!("{query}; x ")).as_deref(),
        Some("(the timestamptz for bucket < $2, Tab gives 2026-01-01T11:00:00Z)")
    );
    assert_eq!(
        hinted(&format!("{query}; x y ")).as_deref(),
        Some("(the text for status IN $3)")
    );
    assert_eq!(
        hinted(&format!("{query}; x y z ")).as_deref(),
        Some("(the text for status IN $4)")
    );
    let logins = "SELECT logins FROM logins WHERE bucket >= $1 AND bucket < $2; ";
    assert_eq!(
        hinted(logins).as_deref(),
        Some("(the timestamp for bucket >= $1)")
    );
    // None while a parameter is being typed, past the last one, in
    // the query, or with the cursor back in the line.
    assert_eq!(hinted(&format!("{query}; x")), None);
    assert_eq!(hinted(&format!("{query}; a b c d ")), None);
    assert_eq!(hinted(query), None);
    let line = format!("{query}; ");
    assert_eq!(hint(&held(), &line, 3), None);
}

#[test]
fn words_part_at_commas_parentheses_and_operators() {
    assert_eq!(
        words("a,b (c)  d>=$1 AND e<$2 x = y"),
        [
            "a", "b", "c", "d", ">=", "$1", "AND", "e", "<", "$2", "x", "=", "y"
        ]
    );
    assert_eq!(words(" ,( "), Vec::<String>::new());
    // The cursor's word starts after the last part, whatever its width.
    assert_eq!(tab("SELECT blå"), ("blå".to_owned(), Vec::new()));
}

#[test]
fn a_candidate_fits_what_is_typed_in_any_case() {
    assert!(starts_with_ignoring_case("SELECT ", "sel"));
    assert!(starts_with_ignoring_case("SELECT ", ""));
    // Typed past the candidate's end, or into the middle of a letter.
    assert!(!starts_with_ignoring_case("SEL", "SELECT"));
    assert!(!starts_with_ignoring_case("blåbær", "bla"));
    assert!(!starts_with_ignoring_case("FROM ", "SEL"));
}

/// The first word of each line of `help` that names a shell command:
/// two spaces, then a backslash.
fn help_commands(help: &str) -> Vec<&str> {
    let mut commands = Vec::new();
    for line in help.lines() {
        let Some(entry) = line.strip_prefix("  ") else {
            continue;
        };
        let Some(command) = entry.split_whitespace().next() else {
            continue;
        };
        if command.starts_with('\\') && !commands.contains(&command) {
            commands.push(command);
        }
    }
    commands
}

/// Each command Tab offers that `help` doesn't list, then each command
/// `help` lists that Tab doesn't offer.
fn command_drift(offered: &[&str], help: &str) -> Vec<String> {
    let listed = help_commands(help);
    let mut trimmed = Vec::new();
    for command in offered {
        trimmed.push(command.trim());
    }
    let mut drift = Vec::new();
    for command in &trimmed {
        if !listed.contains(command) {
            drift.push((*command).to_owned());
        }
    }
    for command in &listed {
        if !trimmed.contains(command) {
            drift.push((*command).to_owned());
        }
    }
    drift
}

#[test]
fn tab_offers_each_command_the_shell_help_lists() {
    let help = crate::help::SHELL_HELP;
    assert_eq!(help_commands(help), ["\\d", "\\rows", "\\?", "\\q"]);
    assert_eq!(command_drift(&COMMANDS, help), Vec::<String>::new());
}

#[test]
fn a_command_on_one_side_alone_drifts() {
    let help = "Reads commands until \\q.\n\n  \\d   views\n  \\y   why\n  --url URL\n";
    assert_eq!(help_commands(help), ["\\d", "\\y"]);
    assert_eq!(command_drift(&["\\d", "\\x "], help), ["\\x", "\\y"]);
}
