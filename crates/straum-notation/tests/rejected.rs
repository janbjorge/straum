//! Every construct outside Straum SQL is an error that names it, on the
//! line it is written on; none is ignored. Most cases are one edit to
//! `BASE`, grouped by the clause they reach.

mod common;

use common::{any_contains, any_message, base, edit, error_lines, rejects, sql_errors};
use straum_notation::parse;

/// Statements and view shape.
#[test]
fn view_shapes_outside_the_subset_are_rejected() {
    rejects(&[
        (
            "CREATE VIEW t",
            "CREATE MATERIALIZED VIEW t",
            1,
            "MATERIALIZED is not in Straum SQL",
        ),
        (
            "CREATE VIEW t",
            "CREATE TEMPORARY VIEW t",
            1,
            "TEMPORARY is not in Straum SQL",
        ),
        (
            "CREATE VIEW t",
            "CREATE VIEW IF NOT EXISTS t",
            1,
            "IF NOT EXISTS is not in Straum SQL",
        ),
        (
            "CREATE VIEW t",
            "CREATE OR ALTER VIEW t",
            1,
            "OR ALTER is not in Straum SQL",
        ),
        (
            "CREATE VIEW t",
            "CREATE OR REPLACE VIEW t",
            1,
            "OR REPLACE is not in Straum SQL",
        ),
        (
            "CREATE VIEW t",
            "CREATE SECURE VIEW t",
            1,
            "SECURE is not in Straum SQL",
        ),
        (
            "AS",
            "COPY GRANTS AS",
            1,
            "COPY GRANTS is not in Straum SQL",
        ),
        (
            "VIEW t AS",
            "VIEW t (a, b) AS",
            1,
            "a view column list is not in Straum SQL",
        ),
        (
            "VIEW t AS",
            "VIEW s.t AS",
            1,
            "view s.t must be a bare name",
        ),
        (
            "VIEW t AS",
            "VIEW \"t\" AS",
            1,
            "quoted identifier `\"t\"` is not in Straum SQL",
        ),
    ]);
}

/// A view takes no options: its widths are in its `buckets` item.
#[test]
fn view_options_are_rejected() {
    rejects(&[
        (
            "VIEW t AS",
            "VIEW t WITH (key = id) AS",
            1,
            "view options `WITH (key = id)` is not in Straum SQL",
        ),
        (
            "VIEW t AS",
            "VIEW t WITH (buckets = '1 hour') AS",
            1,
            "view options `WITH (buckets = '1 hour')` is not in Straum SQL",
        ),
    ]);
}

/// A name is written in lowercase, its one spelling: `KIND` is refused,
/// naming `kind`, and the Kelvin sign before `ind` is no letter at all.
#[test]
fn names_are_written_in_lowercase() {
    rejects(&[
        (
            "count(*) AS n",
            "count(*) AS \u{212A}ind",
            3,
            "bad identifier \u{212A}ind",
        ),
        (
            "count(*) AS n",
            "count(*) AS KIND",
            3,
            "KIND: write names in lowercase, kind",
        ),
    ]);
}

/// Query.
#[test]
fn query_clauses_outside_the_subset_are_rejected() {
    rejects(&[
        (
            "AS\nSELECT",
            "AS\nWITH c AS (SELECT 1) SELECT",
            2,
            "WITH (a CTE) is not in Straum SQL",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 ORDER BY 1",
            2,
            "ORDER BY is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 LIMIT 5",
            2,
            "LIMIT or OFFSET is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 FOR UPDATE",
            2,
            "FOR UPDATE or FOR SHARE",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 UNION SELECT 1",
            2,
            "view body, use one SELECT,",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 FETCH FIRST 1 ROWS ONLY",
            2,
            "FETCH is not",
        ),
    ]);
}

#[test]
fn other_dialects_select_clauses_are_rejected() {
    rejects(&[
        (
            "SELECT buckets",
            "SELECT /*+ x */ buckets",
            2,
            "an optimizer hint is not",
        ),
        (
            "SELECT buckets",
            "SELECT ALL buckets",
            2,
            "SELECT ALL is not",
        ),
        ("SELECT buckets", "SELECT TOP 1 buckets", 2, "TOP is not"),
        (
            "count(*) AS n",
            "count(*) AS n INTO x",
            2,
            "SELECT INTO is not",
        ),
        (
            "z ON z.id = f.z_id",
            "z ON z.id = f.z_id LATERAL VIEW explode(x) e AS y",
            2,
            "LATERAL VIEW is not",
        ),
        (
            "z ON z.id = f.z_id",
            "z ON z.id = f.z_id CONNECT BY a = b",
            2,
            "CONNECT BY is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 CLUSTER BY 1",
            2,
            "CLUSTER BY is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 DISTRIBUTE BY 1",
            2,
            "DISTRIBUTE BY is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 SORT BY 1",
            2,
            "SORT BY is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 QUALIFY n > 1",
            2,
            "QUALIFY is not",
        ),
    ]);
}

#[test]
fn table_and_call_options_are_rejected() {
    rejects(&[
        (
            "FROM public.t AS f",
            "FROM public.t(1) AS f",
            4,
            "a table function is not",
        ),
        (
            "FROM public.t AS f",
            "FROM public.t(1) WITH ORDINALITY AS f",
            4,
            "WITH ORDINALITY is not",
        ),
        (
            "FROM public.t AS f",
            "FROM public.t AS f WITH (NOLOCK)",
            4,
            "a table hint is not",
        ),
        (
            "LEFT OUTER JOIN",
            "GLOBAL LEFT OUTER JOIN",
            5,
            "GLOBAL is not",
        ),
        (
            "count(*) AS n",
            "count(*) RESPECT NULLS AS n",
            3,
            "IGNORE or RESPECT NULLS is not",
        ),
        (
            "count(*) AS n",
            "{fn count(*)} AS n",
            3,
            "ODBC call syntax is not",
        ),
    ]);
}

/// Select clauses.
#[test]
fn select_clauses_outside_the_subset_are_rejected() {
    rejects(&[
        (
            "SELECT buckets",
            "SELECT DISTINCT buckets",
            2,
            "SELECT DISTINCT is not",
        ),
        (
            "\nGROUP BY",
            "\nWHERE f.b = 1\nGROUP BY",
            6,
            "WHERE on f.b needs f.b AS <name> in SELECT",
        ),
        (
            "\nGROUP BY",
            "\nWHERE f.b IS NULL\nGROUP BY",
            6,
            "WHERE f.b IS NULL needs the key (f.b IS NULL) AS <name> in SELECT",
        ),
        (
            "\nGROUP BY",
            "\nWHERE a = 1\nGROUP BY",
            6,
            "column, write alias.column, `a`",
        ),
        (
            "\nGROUP BY",
            "\nWHERE f.a = 1 OR f.a = 2\nGROUP BY",
            6,
            "filter condition, write = or IN joined by AND,",
        ),
        (
            "\nGROUP BY",
            "\nWHERE f.a IN (1, 1)\nGROUP BY",
            6,
            "1 given twice",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 HAVING count(*) > 1",
            2,
            "HAVING is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 WINDOW w AS ()",
            2,
            "WINDOW is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY ROLLUP (1, 2, 3, 4)",
            6,
            "GROUP BY item, write the key's position in SELECT, `ROLLUP (1, 2, 3, 4)`",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY ALL",
            2,
            "GROUP BY ALL is not",
        ),
    ]);
}

/// FROM: one table, written whole.
#[test]
fn from_forms_outside_the_subset_are_rejected() {
    rejects(&[
        (
            "FROM public.t AS f",
            "FROM public.t AS f, public.u AS u",
            2,
            "exactly one FROM table",
        ),
        (
            "FROM public.t AS f",
            "FROM (SELECT 1) AS f",
            4,
            "table `(SELECT 1) AS f` is not",
        ),
        (
            "FROM public.t AS f",
            "FROM public.t AS f(a)",
            4,
            "an alias column list is not",
        ),
        (
            "FROM public.t AS f",
            "FROM public.t AS f TABLESAMPLE SYSTEM (10)",
            4,
            "TABLESAMPLE is not",
        ),
        (
            "FROM public.t AS f",
            "FROM ONLY public.t AS f",
            4,
            "ONLY or * is not",
        ),
        (
            "FROM public.t AS f",
            "FROM public.t * AS f",
            4,
            "ONLY or * is not",
        ),
    ]);
}

/// A join ON compares a dim key to a fact column, never through a dim
/// that keeps no row or a column the near dim is not read for.
#[test]
fn join_on_names_a_dim_key_and_a_fact_column() {
    rejects(&[
        (
            "LEFT OUTER JOIN public.z AS z ON z.id = f.z_id",
            "LEFT OUTER JOIN public.z AS z ON z.id = f.z_id \
             LEFT OUTER JOIN public.w AS w ON w.id = q.x",
            5,
            "join ON w.<key> = f.<column>",
        ),
        (
            "ON z.id = f.z_id",
            "ON z.id = 1",
            5,
            "join ON z.<key> = f.<column>, the joined table's column on the left, one or \
             more joined by AND; found z.id = 1",
        ),
        (
            "ON z.id = f.z_id",
            "ON z.id = f.z_id OR z.id = f.a",
            5,
            "join ON z.<key> = f.<column>",
        ),
        (
            "ON z.id = f.z_id",
            "ON z.id > f.z_id",
            5,
            "join ON z.<key> = f.<column>",
        ),
        (
            "ON z.id = f.z_id",
            "ON z.id = z.z_id",
            5,
            "join ON z.<key> = f.<column>",
        ),
        (
            "public.z AS z ON z.id",
            "public.z AS f ON f.id",
            5,
            "alias f used twice",
        ),
    ]);
}

/// SELECT items and columns: every item named, every column owned.
#[test]
fn select_items_are_named_and_columns_owned() {
    rejects(&[
        (
            "f.a AS a, f.z_id AS z_id",
            "*, f.z_id AS z_id",
            3,
            "SELECT item `*` is not",
        ),
        (
            "f.a AS a, f.z_id AS z_id",
            "f.*, f.z_id AS z_id",
            3,
            "SELECT item `f.*` is not",
        ),
        (
            "f.a AS a, f.z_id AS z_id",
            "public.f.a AS a, f.z_id AS z_id",
            3,
            "column, write alias.column, `public.f.a`",
        ),
        (
            "f.a AS a, f.z_id AS z_id",
            "q.a AS a, f.z_id AS z_id",
            3,
            "unknown table alias q",
        ),
        (
            "f.a AS a, f.z_id AS z_id",
            "f.a + 1 AS a, f.z_id AS z_id",
            3,
            "column, write alias.column, `f.a + 1`",
        ),
        (
            "f.a AS a, f.z_id AS z_id",
            "f.a AS \"A\", f.z_id AS z_id",
            3,
            "quoted identifier `\"A\"` is not",
        ),
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE z.code = 'x') AS n",
            3,
            "FILTER on z.code needs z.code AS <name> in SELECT",
        ),
    ]);
}

/// Aggregates.
#[test]
fn aggregates_outside_the_subset_are_rejected() {
    rejects(&[
        (
            "count(*) AS n",
            "median(f.x) AS n",
            3,
            "aggregate `median(f.x)` is not",
        ),
        (
            "count(*) AS n",
            "min(DISTINCT f.x) AS n",
            3,
            "DISTINCT outside count, sum and avg",
        ),
        (
            "count(*) AS n",
            "sum(ALL f.x) AS n",
            3,
            "ALL inside a call is not",
        ),
        (
            "count(*) AS n",
            "sum(z.name) AS n",
            3,
            "an aggregated column must be a f column",
        ),
        (
            "count(*) AS n",
            "sum(*) AS n",
            3,
            "arguments `sum(*)` is not",
        ),
        (
            "count(*) AS n",
            "sum(f.x, f.y) AS n",
            3,
            "arguments `sum(f.x, f.y)` is not",
        ),
        (
            "count(*) AS n",
            "count(*) OVER () AS n",
            3,
            "a window function (OVER) is not",
        ),
        (
            "count(*) AS n",
            "sum(f.x ORDER BY f.x) AS n",
            3,
            "ORDER BY or LIMIT inside a call",
        ),
        (
            "count(*) AS n",
            "sum(f.x) WITHIN GROUP (ORDER BY f.x) AS n",
            3,
            "WITHIN GROUP is not",
        ),
        (
            "count(*) AS n",
            "\"count\"(*) AS n",
            3,
            "aggregate `\"count\"(*)` is not",
        ),
        (
            "count(*) AS n",
            "current_timestamp AS n",
            3,
            "function call `current_timestamp` is not",
        ),
    ]);
}

#[test]
fn percentile_disc_orders_by_one_column() {
    rejects(&[
        (
            "count(*) AS n",
            "percentile_cont(0.5) WITHIN GROUP (ORDER BY f.x) AS n",
            3,
            "percentile_cont, use percentile_disc, `percentile_cont(0.5) WITHIN GROUP",
        ),
        (
            "count(*) AS n",
            "percentile_disc(0.5) AS n",
            3,
            "percentile_disc without WITHIN GROUP (ORDER BY alias.column) `percentile_disc(0.5)` is not",
        ),
        (
            "count(*) AS n",
            "percentile_disc(0.5) WITHIN GROUP (ORDER BY f.x, f.a) AS n",
            3,
            "WITHIN GROUP of more than one column",
        ),
        (
            "count(*) AS n",
            "percentile_disc(0.5) WITHIN GROUP (ORDER BY f.x DESC) AS n",
            3,
            "ASC, DESC or USING in WITHIN GROUP is not",
        ),
        (
            "count(*) AS n",
            "percentile_disc(0.5) WITHIN GROUP (ORDER BY f.x ASC) AS n",
            3,
            "ASC, DESC or USING in WITHIN GROUP is not",
        ),
        (
            "count(*) AS n",
            "percentile_disc(0.5) WITHIN GROUP (ORDER BY f.x NULLS LAST) AS n",
            3,
            "NULLS FIRST or LAST is not",
        ),
        (
            "count(*) AS n",
            "percentile_disc(DISTINCT 0.5) WITHIN GROUP (ORDER BY f.x) AS n",
            3,
            "DISTINCT outside count, sum and avg is not",
        ),
        (
            "count(*) AS n",
            "percentile_disc(0.5, 0.9) WITHIN GROUP (ORDER BY f.x) AS n",
            3,
            "arguments, write one fraction,",
        ),
        (
            "count(*) AS n",
            "percentile_disc(ARRAY[0.5, 0.9]) WITHIN GROUP (ORDER BY f.x) AS n",
            3,
            "fraction, write a number from 0 to 1, `ARRAY[0.5, 0.9]` is not",
        ),
        (
            "count(*) AS n",
            "percentile_disc(0.5) WITHIN GROUP (ORDER BY z.name) AS n",
            3,
            "an aggregated column must be a f column",
        ),
        (
            "count(*) AS n",
            "percentile_disc(0.5) WITHIN GROUP (ORDER BY f.x) FILTER (WHERE f.a > 1) AS n",
            3,
            "filter condition",
        ),
    ]);
}

#[test]
fn a_fraction_is_written_from_0_to_1() {
    rejects(&[
        (
            "count(*) AS n",
            "percentile_disc(-0.5) WITHIN GROUP (ORDER BY f.x) AS n",
            3,
            "fraction, write a number from 0 to 1, `-0.5` is not",
        ),
        (
            "count(*) AS n",
            "percentile_disc(.5) WITHIN GROUP (ORDER BY f.x) AS n",
            3,
            "fraction, write a number from 0 to 1, `.5` is not",
        ),
        (
            "count(*) AS n",
            "percentile_disc(5e-1) WITHIN GROUP (ORDER BY f.x) AS n",
            3,
            "fraction, write a number from 0 to 1,",
        ),
        (
            "count(*) AS n",
            "percentile_disc(f.a) WITHIN GROUP (ORDER BY f.x) AS n",
            3,
            "fraction, write a number from 0 to 1, `f.a` is not",
        ),
        (
            "count(*) AS n",
            "percentile_disc(1.5) WITHIN GROUP (ORDER BY f.x) AS n",
            3,
            "fraction 1.5: write a number from 0 to 1 in at most 19 places",
        ),
        (
            "count(*) AS n",
            "percentile_disc(0.00000000000000000001) WITHIN GROUP (ORDER BY f.x) AS n",
            3,
            "in at most 19 places",
        ),
    ]);
}

/// Measure filters: one condition form.
#[test]
fn measure_filters_take_one_condition_form() {
    rejects(&[
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a > 1) AS n",
            3,
            "filter condition",
        ),
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a = f.b) AS n",
            3,
            "literal, use an integer, 'text', true or false, `f.b`",
        ),
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a = NULL) AS n",
            3,
            "literal",
        ),
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a = 1.5) AS n",
            3,
            "literal",
        ),
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a = -'x') AS n",
            3,
            "literal",
        ),
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a = 99999999999999999999) AS n",
            3,
            "literal",
        ),
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a IN (1, x)) AS n",
            3,
            "literal, use an integer, 'text', true or false, `x`",
        ),
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a IN (1, 2, 1)) AS n",
            3,
            "1 given twice",
        ),
    ]);
    // Each value once is read.
    let distinct = base(
        "count(*) AS n",
        "count(*) FILTER (WHERE f.a IN (1, 2)) AS n",
    );
    assert!(parse(&distinct).is_ok());
}

/// The view's time: one `buckets` item, a fact column and its widths.
#[test]
fn the_time_is_one_buckets_item() {
    rejects(&[
        (
            "buckets(f.at",
            "buckets(z.at",
            2,
            "the buckets column must be a f column",
        ),
        (
            "'1 hour') AS bucket",
            "'1 hour') FILTER (WHERE f.a = 1) AS bucket",
            2,
            "FILTER on buckets",
        ),
        (
            "buckets(f.at",
            "buckets(DISTINCT f.at",
            2,
            "DISTINCT on buckets",
        ),
        ("buckets(", "date_trunc(", 2, "aggregate `date_trunc"),
        (
            "buckets(f.at, '1 hour')",
            "date_bin('1 hour', f.at, timestamptz 'epoch')",
            2,
            "date_bin in a view: write buckets(<alias.column>, '<width>', ...) AS <name>",
        ),
        (
            "buckets(f.at, '1 hour')",
            "buckets(f.at)",
            2,
            "buckets needs a width after the column",
        ),
        (
            "SELECT buckets(f.at, '1 hour') AS bucket,",
            "SELECT",
            2,
            "SELECT needs its buckets",
        ),
        (
            "AS bucket,",
            "AS bucket, buckets(f.at, '1 day') AS day,",
            2,
            "more than one buckets item",
        ),
        ("AS bucket,", "AS user,", 2, "user is a word SQL reserves"),
    ]);
}

#[test]
fn bucket_widths_are_spelled_one_way() {
    rejects(&[
        (
            "'1 hour')",
            "'15x')",
            2,
            "bucket width '15x': use '<n> seconds|minutes|hours|days'",
        ),
        ("'1 hour')", "'1 week')", 2, "bucket width '1 week'"),
        ("'1 hour')", "'99999999 days')", 2, "bucket width"),
        ("'1 hour')", "interval '1 hour')", 2, "bucket width"),
        (
            "'1 hour')",
            "'1 hour', '2 weeks')",
            2,
            "bucket width '2 weeks'",
        ),
        ("'1 hour')", "'')", 2, "bucket width '': use"),
        ("'1 hour')", "1)", 2, "bucket width 1: use"),
        (
            "'1 hour')",
            "'60 minutes')",
            2,
            "bucket width '60 minutes': write '1 hour'",
        ),
        ("'1 hour')", "'1 hour', f.a)", 2, "bucket width f.a: use"),
    ]);
}

/// GROUP BY: the keys' positions, in order.
#[test]
fn group_by_takes_the_key_positions() {
    rejects(&[
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4, 9",
            2,
            "GROUP BY 1, 2, 3, 4, 9 must list the keys' positions in SELECT",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 0, 1, 2, 3, 4",
            2,
            "GROUP BY 0, 1, 2, 3, 4 must list the keys' positions in SELECT",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, (2), 3, 4",
            6,
            "GROUP BY item, write the key's position in SELECT, `(2)`",
        ),
    ]);
}

#[test]
fn joins_of_every_kind_rejected() {
    let base = "CREATE VIEW t AS
SELECT buckets(f.at, '1 hour') AS bucket,
       f.a AS a, z.name AS name, count(*) AS n
FROM public.t AS f
INNER JOIN public.z AS z ON z.id = f.z_id
WHERE EXISTS (SELECT 1 FROM public.p AS p WHERE p.k = f.a)
GROUP BY 1, 2, 3";
    // Each case: the edit, the GROUP BY its keys need, and the error.
    for (from, to, group_by, fragment) in [
        // A semi join has no columns to select.
        (
            "z.name AS name,",
            "z.name AS name, p.x AS x,",
            "GROUP BY 1, 2, 3, 4",
            "p.x is read outside its EXISTS; p is a semi join, which keeps no row to read",
        ),
        // Nor to join through.
        (
            "GROUP BY",
            "AND EXISTS (SELECT 1 FROM public.q AS q WHERE q.k = p.x)\nGROUP BY",
            "GROUP BY 1, 2, 3",
            "p.x is read outside its EXISTS; p is a semi join, which keeps no row to read",
        ),
        // An inner join that selects nothing.
        (
            "z.name AS name,",
            "",
            "GROUP BY 1, 2",
            "join z reads no column of its dim",
        ),
    ] {
        let text = edit(base, from, to).replace("GROUP BY 1, 2, 3", group_by);
        let errors = error_lines(&text);
        assert!(
            any_contains(&errors, fragment),
            "{to}: {errors:?} has no {fragment:?}"
        );
    }
}

/// Clauses few would write, each rejected by name on its own line.
#[test]
fn rare_clauses_are_rejected_by_name() {
    rejects(&[
        (
            "CREATE VIEW t",
            "CREATE ALGORITHM = MERGE VIEW t",
            1,
            "ALGORITHM, DEFINER or SQL SECURITY is not in Straum SQL",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 FOR XML RAW",
            2,
            "FOR is not in Straum SQL",
        ),
        (
            "f.a AS a",
            "CAST(f.a AS TEXT FORMAT 'x') AS a",
            3,
            "CAST ... FORMAT is not in Straum SQL",
        ),
        (
            "f.a AS a",
            "TRY_CAST(f.a AS TEXT) AS a",
            3,
            "cast `TRY_CAST(f.a AS TEXT)` is not in Straum SQL",
        ),
        (
            "buckets(f.at, '1 hour')",
            "buckets(f.at, '1 hour') WITHIN GROUP (ORDER BY f.a)",
            2,
            "WITHIN GROUP is not in Straum SQL",
        ),
    ]);
}

/// A key or measure an ask could not name is refused where the view
/// names it: these words read as SQL's own in an ask. Names that only
/// look like SQL's stay.
#[test]
fn a_name_no_ask_can_say_is_refused() {
    let reserved = [
        "top",
        "all",
        "distinct",
        "user",
        "not",
        "null",
        "true",
        "false",
        "interval",
        "current_date",
        "current_time",
        "from",
        "exists",
        "trim",
    ];
    for word in reserved {
        for (from, to) in [
            ("f.a AS a,", format!("f.a AS {word},")),
            ("count(*) AS n", format!("count(*) AS {word}")),
        ] {
            let view = base(from, &to);
            let errors = sql_errors(&view);
            let message = format!("{word} is a word SQL reserves, so no ask could name it");
            assert!(any_message(&errors, None, &message), "{view}: {errors:?}");
        }
    }
    // `name` is BASE's own.
    for word in [
        "count", "sum", "type", "kind", "key", "value", "user_id", "date",
    ] {
        let view = base("f.a AS a,", &format!("f.a AS {word},"));
        assert!(parse(&view).is_ok(), "{view}: {:?}", parse(&view));
    }
}
