//! The route handlers: `handle` takes one request from the server loop in
//! `lib.rs`, picks the handler by method and path, and returns the reply.
//! An ask goes body, then `ask.rs`, then the `ReadAsk`, then the app on a
//! blocking thread, then `answer.rs`; the other routes read the app and
//! hand what they read to `status.rs` or `views.rs`, or reply with a
//! view's definition as the spec wrote it.

use std::convert::Infallible;
use std::fmt::Display;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{ACCEPT, ALLOW, CACHE_CONTROL, CONTENT_TYPE, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use straum_app::{AnswerError, App, Lsn};
use straum_domain::{Ask, Definition, Start, Window};

use crate::json::quoted;
use crate::{ReadAsk, answer, ask, status, views};

const LSN: HeaderName = HeaderName::from_static("straum-lsn");

/// Where an answer's time went in Straum, so a client can tell it from the
/// network's.
const SERVER_TIMING: HeaderName = HeaderName::from_static("server-timing");

const MAX_ASK_BYTES: usize = 1 << 20;
/// How long an ask's body may take to come whole, from the end of its
/// head: hyper bounds the head alone, and a client that stops mid-body
/// would hold its connection open for ever.
const BODY_TIMEOUT: Duration = Duration::from_secs(10);

/// The rows `GET /views/NAME` writes at most. A view may hold millions,
/// which no one reads.
const MAX_ROWS: usize = 1000;

const ROWS_PREFIX: &str = "/views/";

/// What follows a view's name in `GET /views/NAME/spec`.
const SPEC_SUFFIX: &str = "/spec";

const JSON: &str = "application/json";
const TEXT: &str = "text/plain; charset=utf-8";

/// A reply as hyper sends it: a status, headers and the whole body.
type Reply = Response<Full<Bytes>>;

/// How a reply is written: JSON for programs, or plain text for people.
#[derive(Clone, Copy)]
enum Format {
    Json,
    Text,
}

impl Format {
    /// The first format `Accept` names among its media ranges, whatever
    /// their parameters or case, else the route's own.
    fn of(accept: Option<&HeaderValue>, default: Format) -> Format {
        let Some(Ok(accept)) = accept.map(HeaderValue::to_str) else {
            return default;
        };
        for range in accept.split(',') {
            let media_type = range.split(';').next().unwrap_or_default().trim();
            if media_type.eq_ignore_ascii_case(JSON) {
                return Format::Json;
            }
            if media_type.eq_ignore_ascii_case("text/plain") {
                return Format::Text;
            }
        }
        default
    }

    fn content_type(self) -> &'static str {
        match self {
            Format::Json => JSON,
            Format::Text => TEXT,
        }
    }

    /// The `Server-Timing` name of the time spent writing the body.
    fn phase(self) -> &'static str {
        match self {
            Format::Json => "json",
            Format::Text => "text",
        }
    }

    /// `{"errors": [...]}`, or a line per error.
    // Generic: the body, the query, the range and the app each have their
    // own error type, and a reply prints them all the same way.
    fn errors<E: Display>(self, status: StatusCode, errors: &[E]) -> Reply {
        let mut lines = Vec::with_capacity(errors.len());
        for error in errors {
            lines.push(error.to_string());
        }
        if let Format::Text = self {
            return text(status, lines.join("\n") + "\n");
        }
        let mut quoted_lines = Vec::with_capacity(lines.len());
        for line in &lines {
            quoted_lines.push(quoted(line));
        }
        let body = format!("{{\"errors\":[{}]}}", quoted_lines.join(","));
        respond(status, JSON, body.into_bytes())
    }
}

/// The reply to one request.
pub(crate) async fn handle(
    request: Request<Incoming>,
    app: Arc<App>,
    read_ask: ReadAsk,
    definitions: Arc<[Definition]>,
) -> Result<Reply, Infallible> {
    let (parts, body) = request.into_parts();
    let accept = parts.headers.get(ACCEPT);
    let reply = match (&parts.method, parts.uri.path()) {
        (&Method::POST, "/ask") => {
            post_ask(body, &app, read_ask, Format::of(accept, Format::Json)).await
        }
        (&Method::GET, "/livez") => text(StatusCode::OK, "live\n".into()),
        (&Method::GET, "/readyz") => get_readyz(&app),
        (&Method::GET, "/status") => get_status(&app, Format::of(accept, Format::Text)),
        (&Method::GET, "/views") => {
            let format = Format::of(accept, Format::Text);
            let app = Arc::clone(&app);
            // The views are read under the store's lock, which a long ask
            // or a writer can hold: the health routes don't queue behind.
            match tokio::task::spawn_blocking(move || get_views(&app, format)).await {
                Ok(reply) => reply,
                Err(_) => failed(format),
            }
        }
        (_, "/ask") => not_allowed("POST"),
        (_, "/livez" | "/readyz" | "/status" | "/views") => not_allowed("GET"),
        (&Method::GET, path) if path.starts_with(ROWS_PREFIX) => {
            let named = &path[ROWS_PREFIX.len()..];
            match named.strip_suffix(SPEC_SUFFIX) {
                Some(view) => get_spec(&definitions, view),
                None => get_rows(&app, named, parts.uri.query()).await,
            }
        }
        (_, path) if path.starts_with(ROWS_PREFIX) => not_allowed("GET"),
        (_, path) => text(StatusCode::NOT_FOUND, format!("no {path} here\n")),
    };
    Ok(reply)
}

async fn post_ask(body: Incoming, app: &App, read_ask: ReadAsk, format: Format) -> Reply {
    let start = Instant::now();
    let collected = Limited::new(body, MAX_ASK_BYTES).collect();
    let Ok(collected) = tokio::time::timeout(BODY_TIMEOUT, collected).await else {
        let error = format!(
            "the ask's body did not come whole within {} s",
            BODY_TIMEOUT.as_secs()
        );
        return format.errors(StatusCode::REQUEST_TIMEOUT, &[error]);
    };
    let body = match collected {
        Ok(body) => body.to_bytes(),
        Err(error) if error.is::<LengthLimitError>() => {
            let error = format!("an ask is at most {MAX_ASK_BYTES} bytes");
            return format.errors(StatusCode::PAYLOAD_TOO_LARGE, &[error]);
        }
        Err(error) => {
            let error = format!("reading the ask: {error}");
            return format.errors(StatusCode::BAD_REQUEST, &[error]);
        }
    };
    let parsing = Instant::now();
    // Reading a 1 MiB ask takes time an async thread can't spare: the
    // health routes queue behind it.
    let read = tokio::task::spawn_blocking(move || read_body(&body, read_ask)).await;
    let asked = match read {
        Ok(Ok(asked)) => asked,
        Ok(Err(errors)) => return format.errors(StatusCode::BAD_REQUEST, &errors),
        Err(_) => return failed(format),
    };
    let writing = Writing {
        format,
        window: None,
    };
    answer(app, asked, writing, parsing.elapsed(), start).await
}

/// The ask a body holds, or every error in it.
fn read_body(body: &Bytes, read_ask: ReadAsk) -> Result<Ask, Vec<String>> {
    let sent = match ask::read(body) {
        Ok(sent) => sent,
        Err(errors) => return Err(lines(&errors)),
    };
    match read_ask(&sent.query, &sent.parameters) {
        Ok(asked) => Ok(asked),
        Err(errors) => Err(lines(&errors)),
    }
}

/// Each error as the reply says it.
// Any error a body or a query can have.
fn lines<E: Display>(errors: &[E]) -> Vec<String> {
    let mut lines = Vec::with_capacity(errors.len());
    for error in errors {
        lines.push(error.to_string());
    }
    lines
}

/// The reply when a task off the async threads panicked.
fn failed(format: Format) -> Reply {
    let error = "Straum failed while answering; its log says why";
    format.errors(StatusCode::INTERNAL_SERVER_ERROR, &[error])
}

/// The view's `CREATE VIEW` statement. The spec is fixed at start, so it
/// answers while the tables still load.
fn get_spec(definitions: &[Definition], view: &str) -> Reply {
    for definition in definitions {
        if definition.view.as_str() == view {
            return text(StatusCode::OK, format!("{}\n", definition.text));
        }
    }
    text(
        StatusCode::NOT_FOUND,
        format!("no view {view}; GET /views lists them\n"),
    )
}

async fn get_rows(app: &App, view: &str, query: Option<&str>) -> Reply {
    let start = Instant::now();
    let Some(asked) = app.rows_ask(view) else {
        return text(
            StatusCode::NOT_FOUND,
            format!("no view {view}; GET /views lists them\n"),
        );
    };
    let window = match rows_window(query.unwrap_or("")) {
        Ok(window) => window,
        Err(error) => return text(StatusCode::BAD_REQUEST, format!("{error}\n")),
    };
    let writing = Writing {
        format: Format::Text,
        window: Some(window),
    };
    answer(app, asked, writing, Duration::ZERO, start).await
}

/// The rows `GET /views/NAME` writes, from `start` and `count` in its
/// query string: `start` is the first, a negative one counted back from
/// the end, 0 when not given; `count` is at most `MAX_ROWS`, which it is
/// when not given.
fn rows_window(query: &str) -> Result<Window, String> {
    let mut start = None;
    let mut count = None;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let slot = match name {
            "start" => &mut start,
            "count" => &mut count,
            _ => {
                return Err(format!(
                    "GET /views/NAME takes start and count, not {name:?}"
                ));
            }
        };
        if slot.replace(value).is_some() {
            return Err(format!("{name} is given twice"));
        }
    }
    Ok(Window {
        start: window_start(start.unwrap_or("0"))?,
        count: window_count(count)?,
    })
}

fn window_start(value: &str) -> Result<Start, String> {
    let Ok(row) = value.parse::<i64>() else {
        return Err(format!("start is a row number, not {value:?}"));
    };
    let from = usize::try_from(row.unsigned_abs()).unwrap_or(usize::MAX);
    if row < 0 {
        return Ok(Start::FromLast(from));
    }
    Ok(Start::FromFirst(from))
}

fn window_count(value: Option<&str>) -> Result<usize, String> {
    let Some(value) = value else {
        return Ok(MAX_ROWS);
    };
    match value.parse::<usize>() {
        Ok(count) if count <= MAX_ROWS => Ok(count),
        _ => Err(format!("count is 0 to {MAX_ROWS}, not {value:?}")),
    }
}

/// How an answer is to be written: its format, and which rows a table
/// shows.
#[derive(Clone, Copy)]
struct Writing {
    format: Format,
    window: Option<Window>,
}

async fn answer(
    app: &App,
    asked: Ask,
    writing: Writing,
    parse_time: Duration,
    start: Instant,
) -> Reply {
    let format = writing.format;
    let range_errors = answer::range_errors(&asked);
    if !range_errors.is_empty() {
        return format.errors(StatusCode::BAD_REQUEST, &range_errors);
    }
    let app = app.clone();
    // A big ask reads every cell of a fact, so it runs off the async
    // threads.
    let answered = tokio::task::spawn_blocking(move || answer_timed(&app, &asked, writing)).await;
    match answered {
        Ok(Ok(timed)) => answered_reply(timed, format, parse_time, start),
        Ok(Err(error)) => answer_error(&error, format),
        Err(_) => failed(format),
    }
}

/// A written answer, and how long the app and the writing took.
struct Timed {
    applied: Lsn,
    body: Vec<u8>,
    answer_time: Duration,
    write_time: Duration,
}

fn answer_timed(app: &App, asked: &Ask, writing: Writing) -> Result<Timed, AnswerError> {
    let answering = Instant::now();
    let answered = match writing.window {
        Some(window) => app.ask_window(asked, window)?,
        None => app.ask(asked)?,
    };
    let written_from = Instant::now();
    let body = match writing.format {
        Format::Json => answer::write(&answered),
        Format::Text => answer::table(&answered).into_bytes(),
    };
    Ok(Timed {
        applied: answered.applied,
        body,
        answer_time: written_from - answering,
        write_time: written_from.elapsed(),
    })
}

/// A part of answering an ask, and how long it took.
struct Phase {
    name: &'static str,
    took: Duration,
}

fn answered_reply(timed: Timed, format: Format, parse_time: Duration, start: Instant) -> Reply {
    let mut reply = respond(StatusCode::OK, format.content_type(), timed.body);
    let lsn = HeaderValue::try_from(timed.applied.to_string()).expect("an LSN is ASCII");
    reply.headers_mut().insert(LSN, lsn);
    // total also holds reading the body and the hops to and from the
    // blocking thread, which no phase covers.
    let phases = [
        Phase {
            name: "parse",
            took: parse_time,
        },
        Phase {
            name: "answer",
            took: timed.answer_time,
        },
        Phase {
            name: format.phase(),
            took: timed.write_time,
        },
        Phase {
            name: "total",
            took: start.elapsed(),
        },
    ];
    reply
        .headers_mut()
        .insert(SERVER_TIMING, server_timing(&phases));
    reply
}

fn answer_error(error: &AnswerError, format: Format) -> Reply {
    match error {
        AnswerError::Invalid(invalid) => format.errors(StatusCode::BAD_REQUEST, invalid),
        AnswerError::NotReady => format.errors(StatusCode::SERVICE_UNAVAILABLE, &[error]),
    }
}

/// `name;dur=ms, ...`: the spec counts in milliseconds, and asks take
/// microseconds.
fn server_timing(phases: &[Phase]) -> HeaderValue {
    let entries: Vec<String> = phases.iter().map(timing_entry).collect();
    HeaderValue::try_from(entries.join(", ")).expect("names and numbers are ASCII")
}

fn timing_entry(phase: &Phase) -> String {
    const MILLISECONDS_PER_SECOND: f64 = 1e3;
    let took_ms = phase.took.as_secs_f64() * MILLISECONDS_PER_SECOND;
    format!("{};dur={took_ms:.3}", phase.name)
}

fn get_status(app: &App, format: Format) -> Reply {
    let status = app.status();
    match format {
        Format::Text => text(StatusCode::OK, status::write(&status)),
        Format::Json => respond(StatusCode::OK, JSON, status::json(&status)),
    }
}

fn get_readyz(app: &App) -> Reply {
    match status::why_not(&app.status()) {
        None => text(StatusCode::OK, "ready\n".into()),
        Some(why) => text(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("not ready: {why}\n"),
        ),
    }
}

fn get_views(app: &App, format: Format) -> Reply {
    let Some(held) = app.held() else {
        // A load can finish between the two reads, so there may be no
        // reason left by the time the status is read.
        let why = match status::why_not(&app.status()) {
            Some(why) => why,
            None => "loading".to_owned(),
        };
        let why = format!("not ready: {why}");
        return format.errors(StatusCode::SERVICE_UNAVAILABLE, &[why]);
    };
    match format {
        Format::Text => text(StatusCode::OK, views::write(&held)),
        Format::Json => respond(StatusCode::OK, JSON, views::json(&held)),
    }
}

fn not_allowed(allow: &'static str) -> Reply {
    let mut reply = text(StatusCode::METHOD_NOT_ALLOWED, format!("use {allow}\n"));
    reply
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static(allow));
    reply
}

fn text(status: StatusCode, body: String) -> Reply {
    respond(status, TEXT, body.into_bytes())
}

fn respond(status: StatusCode, content_type: &'static str, body: Vec<u8>) -> Reply {
    let mut reply = Response::new(Full::new(Bytes::from(body)));
    *reply.status_mut() = status;
    let headers = reply.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    // An answer is right as of its as_of and no later, so nothing between
    // Straum and a client may keep one.
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(start: Start, count: usize) -> Window {
        Window { start, count }
    }

    #[test]
    fn a_query_string_picks_the_rows() {
        let first = Start::FromFirst(0);
        assert_eq!(rows_window(""), Ok(window(first, MAX_ROWS)));
        assert_eq!(rows_window("&"), Ok(window(first, MAX_ROWS)));
        assert_eq!(
            rows_window("start=40"),
            Ok(window(Start::FromFirst(40), MAX_ROWS))
        );
        assert_eq!(
            rows_window("start=-20"),
            Ok(window(Start::FromLast(20), MAX_ROWS))
        );
        assert_eq!(rows_window("start=-0"), Ok(window(first, MAX_ROWS)));
        assert_eq!(
            rows_window("count=5&start=3"),
            Ok(window(Start::FromFirst(3), 5))
        );
        assert_eq!(rows_window("count=0"), Ok(window(first, 0)));
        assert_eq!(rows_window("count=1000"), Ok(window(first, 1000)));
        let far = i64::MIN.unsigned_abs();
        let far = usize::try_from(far).unwrap_or(usize::MAX);
        assert_eq!(
            rows_window("start=-9223372036854775808"),
            Ok(window(Start::FromLast(far), MAX_ROWS))
        );
    }

    #[test]
    fn a_query_string_says_what_is_wrong_with_it() {
        let wrong = [
            (
                "limit=5",
                "GET /views/NAME takes start and count, not \"limit\"",
            ),
            ("start", "start is a row number, not \"\""),
            ("start=x", "start is a row number, not \"x\""),
            ("start=1.5", "start is a row number, not \"1.5\""),
            ("start=1&start=2", "start is given twice"),
            ("count=1001", "count is 0 to 1000, not \"1001\""),
            ("count=-1", "count is 0 to 1000, not \"-1\""),
            ("count=", "count is 0 to 1000, not \"\""),
        ];
        for (query, error) in wrong {
            assert_eq!(rows_window(query), Err(error.to_owned()), "{query}");
        }
    }
}
