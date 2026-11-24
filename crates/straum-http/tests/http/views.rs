//! `GET /views`, `GET /views/NAME` and `GET /views/NAME/spec` end to
//! end: the views as text and as JSON, a view's rows, its spec, and each
//! while the tables still load.

use std::net::SocketAddr;

use crate::fake::{Fake, LIMITS, SPEC, fake, int, trip, writer, writer_with};
use crate::{Reply, call, request, serve, started};

fn views_as_json(address: SocketAddr) -> Reply {
    request(address, "GET", "/views", Some("application/json"), "")
}

#[test]
fn views_wait_for_every_table() {
    let mut writer = writer();
    let server = serve(writer.app().clone());
    let reply = call(server.address, "GET", "/views", "");
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (503, "not ready: loading\n")
    );

    writer.step().unwrap();
    let reply = call(server.address, "GET", "/views", "");
    assert_eq!(reply.status, 200);
    // A failed writer leaves no views to read.
    writer.step().unwrap();
    writer.step().unwrap_err();
    let reply = call(server.address, "GET", "/views", "");
    assert_eq!(reply.status, 503);
    let reply = views_as_json(server.address);
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (503, r#"{"errors":["not ready: loading"]}"#)
    );
    server.stop();
}

#[test]
fn views_say_what_straum_holds() {
    let (_writer, server) = started(writer());
    let reply = call(server.address, "GET", "/views", "");
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.body,
        "\
view trips
  buckets:  1 hour, 1 day
  holds:    4 cells, 2024-01-01T00:00:00Z to 2024-01-02T00:00:00Z
  keys:     bucket timestamptz, kind integer, zone_id integer, borough text
  measures: trips, fare, fare_avg, fare_max

dim public.zones
  holds:    2 rows
  asked as: trips.borough
"
    );
    server.stop();
}

#[test]
fn views_as_json_name_each_views_keys_and_measures() {
    let (_writer, server) = started(writer());
    let reply = views_as_json(server.address);
    assert!(
        reply.head.contains("content-type: application/json"),
        "{}",
        reply.head
    );
    assert_eq!(
        reply.body,
        concat!(
            r#"{"views":[{"name":"trips","time":"bucket","#,
            r#""from":"2024-01-01T00:00:00Z","to":"2024-01-02T00:00:00Z","kept":[],"#,
            r#""keys":[{"name":"bucket","type":"timestamptz"},{"name":"kind","type":"integer"},"#,
            r#"{"name":"zone_id","type":"integer"},{"name":"borough","type":"text"}],"#,
            r#""measures":["trips","fare","fare_avg","fare_max"]}]}"#,
            "\n"
        )
    );
    server.stop();
}

#[test]
fn a_view_shows_its_rows() {
    let mut writer = writer();
    let server = serve(writer.app().clone());
    let reply = call(server.address, "GET", "/views/trips", "");
    assert_eq!((reply.status, reply.body.as_str()), (503, "not ready\n"));
    let reply = call(server.address, "GET", "/views/zones", "");
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (404, "no view zones; GET /views lists them\n")
    );
    let reply = call(server.address, "POST", "/views/trips", "");
    assert_eq!((reply.status, reply.body.as_str()), (405, "use GET\n"));

    writer.step().unwrap();
    let reply = call(server.address, "GET", "/views/trips", "");
    assert_eq!(reply.status, 200, "{}", reply.body);
    let (table, as_of) = reply.body.split_at(reply.body.find("4 rows").unwrap());
    assert_eq!(
        table,
        "\
bucket               | kind | zone_id | borough    | trips | fare  | fare_avg | fare_max
---------------------+------+---------+------------+-------+-------+----------+---------
2024-01-01T00:00:00Z |    1 |       1 | Bronx      |     1 | 12.50 |     12.5 |    12.50
2024-01-01T01:00:00Z |    1 |       1 | Bronx      |     1 |  2.50 |      2.5 |     2.50
2024-01-01T02:00:00Z |    2 |       3 | null       |     1 | null  | null     | null
2024-01-01T23:00:00Z |    2 |       2 | Queens \"Q\" |     1 | -0.05 |    -0.05 |    -0.05
"
    );
    assert!(as_of.starts_with("4 rows, as of 2"), "{as_of}");
    server.stop();
}

/// The rows `GET /views/NAME` shows at most.
const SHOWN_ROWS: i64 = 1_000;

/// One trip more than a view shows, each of its own kind, so each is a
/// cell of its own.
fn more_trips_than_shown() -> Fake {
    let mut trips = Vec::new();
    for kind in 0..=SHOWN_ROWS {
        trips.push(trip(0, kind, 1, int(100)));
    }
    Fake { trips, ..fake() }
}

#[test]
fn a_view_shows_its_first_thousand_rows() {
    let (_writer, server) = started(writer_with(LIMITS, more_trips_than_shown()));
    let reply = call(server.address, "GET", "/views/trips", "");
    // The names, the rule, the rows, then the count.
    let lines = usize::try_from(SHOWN_ROWS + 3).unwrap();
    assert_eq!(reply.body.lines().count(), lines, "{}", reply.body);
    let count = reply.body.lines().last().unwrap();
    assert!(count.starts_with("1000 of 1001 rows, as of "), "{count}");
    server.stop();
}

/// The lines of a reply after the names and the rule.
fn rows_and_count(body: &str) -> Vec<&str> {
    body.lines().skip(2).collect()
}

#[test]
fn a_view_shows_the_rows_its_query_string_picks() {
    let (_writer, server) = started(writer_with(LIMITS, more_trips_than_shown()));
    let reply = call(server.address, "GET", "/views/trips?start=-2&count=1", "");
    assert_eq!(reply.status, 200, "{}", reply.body);
    let lines = rows_and_count(&reply.body);
    assert_eq!(lines.len(), 2, "{}", reply.body);
    assert!(lines[0].contains("|  999 |"), "{}", reply.body);
    assert!(
        lines[1].starts_with("1 of 1001 rows from row 999, as of "),
        "{}",
        reply.body
    );
    let reply = call(server.address, "GET", "/views/trips?start=1000", "");
    let lines = rows_and_count(&reply.body);
    assert!(lines[0].contains("| 1000 |"), "{}", reply.body);
    assert!(lines[1].starts_with("1 of 1001 rows from row 1000, as of "));

    let reply = call(server.address, "GET", "/views/trips?count=1001", "");
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (400, "count is 0 to 1000, not \"1001\"\n")
    );
    let reply = call(server.address, "GET", "/views/zones?start=x", "");
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (404, "no view zones; GET /views lists them\n")
    );
    server.stop();
}

#[test]
fn a_view_shows_its_spec_while_the_tables_load() {
    let writer = writer();
    let server = serve(writer.app().clone());
    let reply = call(server.address, "GET", "/views/trips/spec", "");
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.body, format!("{}\n", SPEC.trim()));
    let reply = call(server.address, "GET", "/views/zones/spec", "");
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (404, "no view zones; GET /views lists them\n")
    );
    let reply = call(server.address, "POST", "/views/trips/spec", "");
    assert_eq!((reply.status, reply.body.as_str()), (405, "use GET\n"));
    server.stop();
}
