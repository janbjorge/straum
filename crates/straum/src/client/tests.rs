//! The tests of `client.rs`: requests written and replies read against
//! a fake Straum that answers on a local socket.

use super::*;
use crate::testing::{
    GET_READYZ, GET_ROWS, GET_STATUS, GET_VIEWS, POST_ASK, POST_ASK_JSON, answering, args, server,
};

/// What the user reads: a reply's body, or an error's words.
#[derive(Debug, PartialEq)]
enum Said {
    Body(String),
    Error(String),
}

fn said(result: Result<String, ClientError>) -> Said {
    match result {
        Ok(text) => Said::Body(text),
        Err(error) => Said::Error(error.to_string()),
    }
}

/// The body of a reply that was timed, without its timing.
fn text(result: Result<Timed, ClientError>) -> Result<String, ClientError> {
    Ok(result?.text)
}

fn phase_of(name: &str, took_us: u64) -> Phase {
    Phase {
        name: name.to_owned(),
        took: Duration::from_micros(took_us),
    }
}

#[test]
fn asks_for_the_status() {
    let reply = "HTTP/1.1 200 OK\r\ncontent-length: 11\r\n\r\nready: yes\n";
    let url = answering(GET_STATUS, reply);
    assert_eq!(
        said(status(&server(&url), Output::Text)),
        Said::Body("ready: yes\n".to_owned())
    );
    let url = answering(GET_STATUS, "HTTP/1.1 200 OK\r\n\r\n");
    assert_eq!(
        said(status(&server(&format!("{url}/")), Output::Text)),
        Said::Body(String::new())
    );
    // HTTP/1.0 is HTTP/1.1's elder, and its 200 an answer too.
    let url = answering(GET_STATUS, "HTTP/1.0 200 OK\r\n\r\nready: yes\n");
    assert_eq!(
        said(status(&server(&url), Output::Text)),
        Said::Body("ready: yes\n".to_owned())
    );
    // Straum never sends chunks: a proxy stands between.
    let chunked =
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\nb\r\nready: yes\n\r\n0\r\n\r\n";
    let url = answering(GET_STATUS, chunked);
    assert_eq!(
        said(status(&server(&url), Output::Text)),
        Said::Error(format!(
            "{url} sent a chunked reply, which Straum never sends; give the URL Straum \
             listens on, not a proxy's"
        ))
    );
    // A reply cut short is no answer, whatever its status said.
    let short = "HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\nready: yes\n";
    let url = answering(GET_STATUS, short);
    assert_eq!(
        said(status(&server(&url), Output::Text)),
        Said::Error(format!(
            "{url} sent 11 of the 20 bytes its reply said it would"
        ))
    );
}

/// A server that takes the connection and never answers is given the
/// timeout, and the error says no reply came within it, with no hint
/// about the URL, which was right.
#[test]
fn a_silent_server_is_given_the_timeout() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let timeout = Duration::from_secs(1);
    let holder = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        std::thread::sleep(timeout * 3);
        drop(stream);
    });
    let clock = Instant::now();
    let silent = Server {
        url: url.clone(),
        timeout,
    };
    let said = status(&silent, Output::Text).unwrap_err().to_string();
    assert!(clock.elapsed() >= timeout, "{:?}", clock.elapsed());
    assert!(clock.elapsed() < timeout * 2, "{:?}", clock.elapsed());
    assert_eq!(
        said,
        format!("asking {url} for its status: no reply came within 1 s")
    );
    holder.join().unwrap();
}

#[test]
fn asks_for_the_views() {
    let url = answering(GET_VIEWS, "HTTP/1.1 200 OK\r\n\r\nview trips\n");
    assert_eq!(
        said(views(&server(&url), Output::Text)),
        Said::Body("view trips\n".to_owned())
    );
    let url = answering(
        GET_STATUS,
        "HTTP/1.1 503 Service Unavailable\r\n\r\nnot ready: loading\n",
    );
    let answered =
        format!("{url} answered \"HTTP/1.1 503 Service Unavailable\"\nnot ready: loading");
    assert_eq!(
        said(status(&server(&url), Output::Text)),
        Said::Error(answered)
    );
    // Not ready in either HTTP/1, and nothing else is.
    for (status_line, not_ready) in [
        ("HTTP/1.0 503 Service Unavailable", true),
        ("HTTP/1.1 503 Service Unavailable", true),
        ("HTTP/1.1 500 Internal Server Error", false),
    ] {
        let refused = ClientError::Refused {
            url: url.clone(),
            status_line: status_line.to_owned(),
            body: String::new(),
        };
        assert_eq!(refused.is_not_ready(), not_ready, "{status_line}");
    }
}

#[test]
fn asks_for_a_views_rows() {
    let url = answering(GET_ROWS, "HTTP/1.1 200 OK\r\n\r\ntrips\n");
    let all = RowWindow::default();
    assert_eq!(
        said(text(rows(&server(&url), "trips", all))),
        Said::Body("trips\n".to_owned())
    );
    for name in ["trips HTTP/1.1", "a/b", "tri\rps", "trips;", "", "Trips"] {
        let error = format!("{name:?} is not a view's name");
        assert_eq!(
            said(text(rows(&server("http://127.0.0.1:1"), name, all))),
            Said::Error(error)
        );
    }
    // Nothing listens on port 1.
    let error = rows(&server("http://127.0.0.1:1"), "trips", all).unwrap_err();
    let asking = "asking http://127.0.0.1:1 for its rows: ";
    let said = error.to_string();
    assert!(said.starts_with(asking), "{said}");
    assert!(
        said.ends_with("\ntry: --url URL, or set STRAUM_URL"),
        "{said}"
    );
}

#[test]
fn asks_for_the_rows_a_window_picks() {
    let asked = [
        (Some(-20), None, "?start=-20"),
        (Some(5), Some(10), "?start=5&count=10"),
        (None, Some(0), "?count=0"),
    ];
    for (start, count, query) in asked {
        let request = GET_ROWS.replacen("trips", &format!("trips{query}"), 1);
        let url = answering(&request, "HTTP/1.1 200 OK\r\n\r\nsome\n");
        let window = RowWindow { start, count };
        assert_eq!(
            said(text(rows(&server(&url), "trips", window))),
            Said::Body("some\n".to_owned()),
            "{query}"
        );
    }
}

#[test]
fn loads_the_views_as_json() {
    let url = answering(
        "GET /views HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\naccept: application/json\r\n\r\n",
        "HTTP/1.1 200 OK\r\n\r\n{\"views\":[],\"dims\":[]}",
    );
    assert_eq!(load_views(&server(&url)), Some(Vec::new()));
    assert_eq!(load_views(&server("http://127.0.0.1:1")), None);
}

#[test]
fn says_whether_straum_is_ready() {
    let url = answering(GET_READYZ, "HTTP/1.1 200 OK\r\n\r\nready\n");
    assert!(is_ready(&server(&url)).unwrap());
    let loading = "HTTP/1.1 503 Service Unavailable\r\n\r\nnot ready: loading\n";
    let url = answering(GET_READYZ, loading);
    assert!(!is_ready(&server(&url)).unwrap());
    let url = answering(GET_READYZ, "HTTP/1.1 404 Not Found\r\n\r\n");
    let error = is_ready(&server(&url)).unwrap_err();
    assert!(!error.is_not_ready(), "{error}");
    let error = is_ready(&server("http://127.0.0.1:1")).unwrap_err();
    assert!(
        error.to_string().contains("for whether it is ready: "),
        "{error}"
    );
    // Only a 503 says not ready; an ask refused for itself is a 400.
    let url = answering(GET_VIEWS, loading);
    assert!(
        views(&server(&url), Output::Text)
            .unwrap_err()
            .is_not_ready()
    );
    assert!(!ClientError::NotAViewName("x".into()).is_not_ready());
}

#[test]
fn json_asks_straum_for_json_and_passes_it_through() {
    let json_status = "GET /status HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\naccept: application/json\r\n\r\n";
    let url = answering(json_status, "HTTP/1.1 200 OK\r\n\r\n{\"ready\":true}\n");
    let body = Said::Body("{\"ready\":true}\n".to_owned());
    assert_eq!(said(status(&server(&url), Output::Json)), body);
    let json_views = "GET /views HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\naccept: application/json\r\n\r\n";
    let url = answering(json_views, "HTTP/1.1 200 OK\r\n\r\n{\"views\":[]}");
    let body = Said::Body("{\"views\":[]}".to_owned());
    assert_eq!(said(views(&server(&url), Output::Json)), body);
    let url = answering(POST_ASK_JSON, "HTTP/1.1 200 OK\r\n\r\n{\"rows\":[]}");
    let parameters = args("7 paid true");
    let body = Said::Body("{\"rows\":[]}".to_owned());
    let answered = ask(&server(&url), "SELECT \"x\"", &parameters, Output::Json);
    assert_eq!(said(text(answered)), body);
}

#[test]
fn sends_an_ask_and_prints_its_table() {
    let url = answering(POST_ASK, "HTTP/1.1 200 OK\r\n\r\ntrips\n-----\n    1\n");
    let parameters = args("7 paid true");
    let answered = ask(&server(&url), "SELECT \"x\"", &parameters, Output::Text);
    assert_eq!(
        said(text(answered)),
        Said::Body("trips\n-----\n    1\n".to_owned())
    );
}

#[test]
fn an_answer_comes_with_how_long_it_took() {
    let reply = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\
        Server-Timing: parse;dur=0.031, answer;dur=2.480, text;dur=0.401, total;dur=2.950\r\n\
        \r\n    1\n";
    let url = answering(POST_ASK, reply);
    let sent = Instant::now();
    let answered = ask(
        &server(&url),
        "SELECT \"x\"",
        &args("7 paid true"),
        Output::Text,
    )
    .unwrap();
    let waited = sent.elapsed();
    assert_eq!(answered.text, "    1\n");
    let phases = [
        phase_of("parse", 31),
        phase_of("answer", 2480),
        phase_of("text", 401),
        phase_of("total", 2950),
    ];
    assert_eq!(answered.timing.phases, phases);
    let round_trip = answered.timing.round_trip;
    assert!(round_trip > Duration::ZERO && round_trip <= waited);
    // A view's rows are an ask the app builds, timed the same.
    let reply = "HTTP/1.1 200 OK\r\nserver-timing: total;dur=1.000\r\n\r\ntrips\n";
    let url = answering(GET_ROWS, reply);
    let answered = rows(&server(&url), "trips", RowWindow::default()).unwrap();
    assert_eq!(answered.timing.phases, [phase_of("total", 1000)]);
}

#[test]
fn a_reply_without_phases_is_timed_at_this_end_alone() {
    let url = answering(POST_ASK, "HTTP/1.1 200 OK\r\n\r\n    1\n");
    let answered = ask(
        &server(&url),
        "SELECT \"x\"",
        &args("7 paid true"),
        Output::Text,
    )
    .unwrap();
    assert_eq!(answered.timing.phases, []);
    assert!(answered.timing.round_trip > Duration::ZERO);
    // Only Server-Timing is read, and only entries that say a time.
    let head = "HTTP/1.1 200 OK\r\nx-timing: parse;dur=1\r\n\
        server-timing: total, cache;desc=hit, parse;dur=x, answer;dur=-1, \
        text;dur=inf, json;dur=NaN, db;dur=1e400, miss;dur=, total;dur=2";
    assert_eq!(phases(head), [phase_of("total", 2000)]);
}

#[test]
fn a_timing_is_written_as_a_line() {
    let timing = Timing {
        round_trip: Duration::from_micros(3412),
        phases: vec![
            phase_of("parse", 31),
            phase_of("answer", 2480),
            phase_of("total", 2950),
        ],
    };
    assert_eq!(
        timing.to_string(),
        "Time: 3.412 ms (in Straum: parse 0.031, answer 2.480, total 2.950)"
    );
    let timing = Timing {
        round_trip: Duration::from_millis(1500),
        phases: Vec::new(),
    };
    assert_eq!(timing.to_string(), "Time: 1500.000 ms");
}

#[test]
fn an_ask_refused_says_why() {
    let url = answering(
        POST_ASK,
        "HTTP/1.1 400 Bad Request\r\n\r\nkind asked twice\ntrips asked twice\n",
    );
    let refused =
        format!("{url} answered \"HTTP/1.1 400 Bad Request\"\nkind asked twice\ntrips asked twice");
    let answered = ask(
        &server(&url),
        "SELECT \"x\"",
        &args("7 paid true"),
        Output::Text,
    );
    assert_eq!(said(text(answered)), Said::Error(refused));
    // Nothing listens on port 1.
    let error = ask(
        &server("http://127.0.0.1:1"),
        "SELECT",
        &args(""),
        Output::Text,
    )
    .unwrap_err();
    let asking = "asking http://127.0.0.1:1 for an answer: ";
    let said = error.to_string();
    assert!(said.starts_with(asking), "{said}");
    assert!(
        said.ends_with("\ntry: --url URL, or set STRAUM_URL"),
        "{said}"
    );
}

#[test]
fn a_parameter_says_its_type() {
    let cases = [
        ("7", "7"),
        ("-7", "-7"),
        ("+7", "7"),
        ("true", "true"),
        ("false", "false"),
        ("paid", "\"paid\""),
        ("2026-01-01T00:00:00Z", "\"2026-01-01T00:00:00Z\""),
        ("1.5", "1.5"),
        ("-0.05", "-0.05"),
        // A decimal as JSON writes one, else a string.
        ("1.", "\"1.\""),
        (".5", "\".5\""),
        ("00.5", "\"00.5\""),
        ("+1.5", "\"+1.5\""),
        ("1.2.3", "\"1.2.3\""),
        ("1e5", "\"1e5\""),
        ("True", "\"True\""),
        ("", "\"\""),
        // Quotes make a string of an integer or a boolean.
        ("'7'", "\"7\""),
        ("'true'", "\"true\""),
        ("'1.5'", "\"1.5\""),
        ("''", "\"\""),
        ("'", "\"'\""),
        ("it's", "\"it's\""),
        ("a\"b\\c", "\"a\\\"b\\\\c\""),
        ("a\nb\u{1}", "\"a\\nb\\u0001\""),
        ("blåbær", "\"blåbær\""),
    ];
    for (text, json) in cases {
        assert_eq!(parameter(text), json, "{text}");
    }
}

#[test]
fn a_quoted_parameter_word_holds_its_spaces() {
    let cases: [(&str, &[&str]); 7] = [
        (" 7  paid\ttrue ", &["7", "paid", "true"]),
        ("'Staten Island' 7", &["'Staten Island'", "7"]),
        ("'O'Brien  St' x", &["'O'Brien  St'", "x"]),
        ("'' 'blå bær'", &["''", "'blå bær'"]),
        ("'7' '", &["'7'", "'"]),
        ("it's 'a", &["it's", "'a"]),
        ("", &[]),
    ];
    for (text, words) in cases {
        assert_eq!(parameter_words(text), words, "{text}");
    }
    // The quoted word is one string, its quotes gone.
    assert_eq!(parameter("'Staten Island'"), "\"Staten Island\"");
}

#[test]
fn a_quote_that_never_closes_is_a_word_like_any_other() {
    assert_eq!(parameter_words("'Staten Island"), ["'Staten", "Island"]);
    assert_eq!(parameter_words("'a'b c"), ["'a'b", "c"]);
    for open in ["'", "x 'Staten", "'Staten Is", "'a'b"] {
        assert!(has_open_quote(open), "{open}");
    }
    for closed in ["", "x", "it's", "'Staten Island'", "'' y", "'a' 'b'"] {
        assert!(!has_open_quote(closed), "{closed}");
    }
}

#[test]
fn status_errors() {
    let url = answering(
        GET_STATUS,
        "HTTP/1.1 404 Not Found\r\n\r\nno /status here\n",
    );
    let answered = format!("{url} answered \"HTTP/1.1 404 Not Found\"\nno /status here");
    assert_eq!(
        said(status(&server(&url), Output::Text)),
        Said::Error(answered)
    );
    let url = answering(GET_STATUS, "HTTP/1.1 404 Not Found\r\n\r\n");
    let answered = format!("{url} answered \"HTTP/1.1 404 Not Found\"");
    assert_eq!(
        said(status(&server(&url), Output::Text)),
        Said::Error(answered)
    );
    let url = answering(GET_STATUS, "garbage");
    assert_eq!(
        said(status(&server(&url), Output::Text)),
        Said::Error(format!("{url} sent no HTTP reply"))
    );
    for bad in [
        "https://x:1",
        "x:1",
        "http://",
        "http:///",
        "http://x:1/status",
    ] {
        let error = format!("--url or STRAUM_URL takes http://HOST:PORT, not {bad:?}");
        assert_eq!(
            said(status(&server(bad), Output::Text)),
            Said::Error(error),
            "{bad}"
        );
    }
    // Nothing listens on port 1.
    let error = status(&server("http://127.0.0.1:1"), Output::Text).unwrap_err();
    let asking = "asking http://127.0.0.1:1 for its status: ";
    let said = error.to_string();
    assert!(said.starts_with(asking), "{said}");
    assert!(
        said.ends_with("\ntry: --url URL, or set STRAUM_URL"),
        "{said}"
    );
    // A server that answers, even with a refusal, needs no such hint.
    let url = answering(GET_STATUS, "HTTP/1.1 404 Not Found\r\n\r\n");
    assert!(
        !status(&server(&url), Output::Text)
            .unwrap_err()
            .to_string()
            .contains("STRAUM_URL")
    );
}
