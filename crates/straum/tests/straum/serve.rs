//! A Straum serving a database, through every command in turn: the plan
//! sets PostgreSQL up, `straum run` loads and follows it, and `status`,
//! `views`, `ask`, `shell` and HTTP answer what PostgreSQL answers, as a
//! person and as a script read them. Then a change is followed and a
//! SIGTERM stops it.

use std::io::Write;
use std::process::Stdio;

use crate::{
    ASK, PostgresContainer, Running, SPEC, Scratch, TABLES, Until, no_slot_left, output, postgres,
    run, set_up, straum,
};

#[test]
fn serves_asks_from_postgres() {
    let pg = postgres();
    pg.psql(TABLES);
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    assert_eq!(set_up(&pg, &spec, None), Vec::<String>::new());
    // From a file, as a mounted secret would give it.
    let dsn = scratch.file("dsn", &format!("{}\n", pg.dsn("straum")));
    let mut straum = run(
        &[
            "run",
            "--wal",
            "logical",
            &spec,
            "--listen",
            "127.0.0.1:0",
            "--dsn-file",
            &dsn,
        ],
        "not a DSN",
    );
    straum.wait_for("ready", &Until::Ready);

    let url = format!("http://{}", straum.address);
    status_says_ready(&url);
    let views = views_lists_the_view_and_the_dim(&url);
    ask_prints_a_table_or_says_why_not(&url);
    scripts_read_json_from_straum_url(&url);
    shell_answers_a_line_at_a_time(&url, &views);
    http_answers_in_json(&pg, &straum);
    follows_changes(&pg, &mut straum);
    stops_on_sigterm(&pg, straum);
}

// What `straum status` says, as from inside the pod.
fn status_says_ready(url: &str) {
    let (code, status, _) = output(&mut straum(&["status", "--url", url]));
    assert_eq!(code, Some(0), "{status}");
    assert!(
        status.starts_with("ready:        yes\napplied:      "),
        "{status}"
    );
    assert!(status.contains("\nWAL retained: "), "{status}");
    assert!(!status.contains("failure"), "{status}");
}

fn views_lists_the_view_and_the_dim(url: &str) -> String {
    let (code, views, _) = output(&mut straum(&["views", "--url", url]));
    assert_eq!(code, Some(0), "{views}");
    assert_eq!(
        views,
        "\
view trips
  buckets:  1 hour
  holds:    4 cells, 2024-01-01T00:00:00Z to 2024-01-01T04:00:00Z
  keys:     bucket timestamptz, kind text, zone_id integer, borough text
  measures: trips, fare

dim shop.zones
  holds:    2 rows
  asked as: trips.borough
"
    );

    views
}

fn ask_prints_a_table_or_says_why_not(url: &str) {
    // The range `straum views` gave, as `straum ask` takes it.
    let query = "SELECT kind, borough, trips, fare FROM trips \
                 WHERE bucket >= $1 AND bucket < $2 GROUP BY kind, borough";
    let range = ["2024-01-01T00:00:00Z", "2024-01-01T04:00:00Z"];
    let (code, table, _) = output(&mut straum(&[
        "ask", "--url", url, query, range[0], range[1],
    ]));
    assert_eq!(code, Some(0), "{table}");
    let rows = "\
kind | borough | trips | fare
-----+---------+-------+------
null | Bronx   |     1 | -2.25
cab  | Bronx   |     1 | 12.50
cab  | Queens  |     1 |  0.05
van  | null    |     1 | null
4 rows, as of 20";
    assert!(table.starts_with(rows), "{table}");
    let wrong = "SELECT trips FROM trips WHERE kind = $1 AND bucket >= $2 AND bucket < $3";
    let (code, _, stderr) = output(&mut straum(&["ask", "--url", url, wrong, "7", "0", "3600"]));
    assert_eq!(code, Some(1), "{stderr}");
    assert_eq!(
        stderr,
        format!(
            "straum: {url} answered \"HTTP/1.1 400 Bad Request\"\n\
             straum: kind is text, so it can't be the integer 7\n"
        )
    );
}

/// `straum <args>` finding Straum through `STRAUM_URL` alone.
fn with_url_variable(url: &str, args: &[&str]) -> (Option<i32>, String, String) {
    output(straum(args).env("STRAUM_URL", url))
}

fn json(text: &str) -> serde_json::Value {
    match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => panic!("{error}: {text}"),
    }
}

// What a script reads: `--json`, with Straum found through STRAUM_URL.
fn scripts_read_json_from_straum_url(url: &str) {
    let (code, status, _) = with_url_variable(url, &["status", "--json"]);
    assert_eq!(code, Some(0), "{status}");
    let status = json(&status);
    assert_eq!(status["ready"], true, "{status}");
    assert!(status["why_not"].is_null(), "{status}");
    assert!(status["lag_ms"].is_u64(), "{status}");

    let (code, views, _) = with_url_variable(url, &["views", "--json"]);
    assert_eq!(code, Some(0), "{views}");
    assert_eq!(json(&views)["views"][0]["name"], "trips", "{views}");

    let query = "SELECT kind, borough, trips FROM trips \
                 WHERE bucket >= $1 AND bucket < $2 GROUP BY kind, borough";
    let range = ["2024-01-01T00:00:00Z", "2024-01-01T04:00:00Z"];
    let (code, answer, _) = with_url_variable(url, &["ask", "--json", query, range[0], range[1]]);
    assert_eq!(code, Some(0), "{answer}");
    let answer = json(&answer);
    assert!(answer["as_of"].is_string(), "{answer}");
    assert_eq!(answer["rows"].as_array().unwrap().len(), 4, "{answer}");

    // Refused: nothing on stdout, the errors as JSON on stderr, exit 1.
    let (code, stdout, stderr) =
        with_url_variable(url, &["ask", "--json", "SELECT nope FROM trips"]);
    assert_eq!((code, stdout.as_str()), (Some(1), ""), "{stderr}");
    assert!(stderr.contains(r#"{"errors":["#), "{stderr}");
}

// The same from `straum shell`, a line each, on a pipe.
fn shell_answers_a_line_at_a_time(url: &str, views: &str) {
    let mut shell = straum(&["shell", "--url", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let lines = "\\d\n\\rows trips\n\
        SELECT trips FROM trips WHERE bucket >= $1 AND bucket < $2; 0 2024-01-01T04:00:00Z\n\
        \\rows nope\n\\q\n\\d\n";
    (shell.stdin.take().unwrap())
        .write_all(lines.as_bytes())
        .unwrap();
    let shell = shell.wait_with_output().unwrap();
    let shelled = String::from_utf8(shell.stdout).unwrap();
    // A command that fails goes to stderr and the others still answer; the
    // exit says one failed.
    assert_eq!(shell.status.code(), Some(1), "{shelled}");
    assert_eq!(
        String::from_utf8(shell.stderr).unwrap(),
        format!(
            "straum: {url} answered \"HTTP/1.1 404 Not Found\"\n\
             no view nope; GET /views lists them\n\
             straum: commands that failed: 1\n"
        )
    );
    let rows = "\
bucket               | kind | zone_id | borough | trips | fare
---------------------+------+---------+---------+-------+------
2024-01-01T00:00:00Z | cab  |       1 | Bronx   |     1 | 12.50
2024-01-01T01:00:00Z | cab  |       2 | Queens  |     1 |  0.05
2024-01-01T02:00:00Z | van  |       3 | null    |     1 | null
2024-01-01T03:00:00Z | null |       1 | Bronx   |     1 | -2.25
4 rows, as of 20";
    let (described, rest) = shelled.split_at(views.len());
    assert_eq!(described, views);
    assert!(rest.starts_with(rows), "{rest}");
    let (_, rest) = rest.split_once("Z\n").unwrap();
    assert!(
        rest.starts_with("trips\n-----\n    4\n1 row, as of 20"),
        "{rest}"
    );
    assert_eq!(rest.split_once("Z\n").unwrap().1, "");
}

fn http_answers_in_json(pg: &PostgresContainer, straum: &Running) {
    let (status, body) = straum.call("POST", "/ask", ASK);
    assert_eq!(status, 200, "{body}");
    assert!(body.starts_with(r#"{"as_of":"20"#), "{body}");
    assert!(!body.contains("lsn"), "{body}");
    let rows = pg.expected_rows();
    assert!(body.ends_with(&rows), "{body}\nis not\n{rows}");
    let cab = r#"{"kind":"cab","borough":"Bronx","trips":1,"fare":12.50}"#;
    assert!(body.contains(cab), "{body}");
}

fn follows_changes(pg: &PostgresContainer, straum: &mut Running) {
    pg.psql("INSERT INTO shop.trips VALUES (5, '2024-01-02 00:00Z', 'cab', 2, 1.25)");
    pg.psql("DELETE FROM shop.trips WHERE id = 3");
    pg.psql("UPDATE shop.trips SET fare = 10 WHERE id = 1");
    straum.wait_for_rows(pg, "the changes");

    // A zone renamed shows in the next answer: the dim is held and
    // followed, so nothing loads again.
    pg.psql("UPDATE shop.zones SET borough = 'Brooklyn' WHERE id = 2");
    straum.wait_for_rows(pg, "the dim change");
    pg.psql("UPDATE shop.trips SET fare = 20 WHERE id = 2");
    straum.wait_for_rows(pg, "the trip change");
    assert!(!straum.log().contains("no longer fits"), "{}", straum.log());
}

fn stops_on_sigterm(pg: &PostgresContainer, straum: Running) {
    let (code, log) = straum.stop("-TERM");
    assert_eq!(code, Some(0), "{log}");
    for line in [
        "straum: opened a stream at ",
        "straum: loaded view trips in ",
        "straum: loaded every table; streaming from ",
    ] {
        assert!(log.contains(line), "{log}");
    }
    assert!(
        log.ends_with("straum: stopping: finishing the asks in flight\nstraum: stopped\n"),
        "{log}"
    );
    no_slot_left(pg);
}
