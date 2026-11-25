//! The route handlers: `handle` takes one request from the server loop in
//! `lib.rs`, refuses it if its `Host` is not one `host.rs` allows, picks
//! the handler by method and path, and returns the reply.
//! An ask sent as JSON goes body, then `ask.rs`, then the `AskSyntax`, then
//! the app on a blocking thread, then `answer.rs`; the other routes read the app and
//! hand what they read to `status.rs` or `views.rs`, or reply with a
//! view's definition as the spec wrote it, or with `openapi.json`.

use std::convert::Infallible;
use std::fmt::{self, Display};
use std::panic;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{ACCEPT, ALLOW, CACHE_CONTROL, CONTENT_TYPE, HOST, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use straum_app::{AnswerError, App, Lsn};
use straum_domain::{Ask, Cut, Definition, Start, Window};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::json::quoted;
use crate::{AskSyntax, DescribeAskError, Served, answer, ask, status, views};

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

/// The OpenAPI document of these routes, built in so the binary serves
/// the contract it keeps.
const OPENAPI: &str = include_str!("../openapi.json");

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
            let media_type = media_type(range);
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
        let lines = lines(errors);
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
    served: Arc<Served>,
) -> Result<Reply, Infallible> {
    let Served {
        app,
        syntax,
        definitions,
        hosts,
        max_answer_rows,
        turns,
    } = &*served;
    let (parts, body) = request.into_parts();
    // 421, as for a name this server does not serve: the request was not
    // meant for it. 403 would say the client lacks a right it could have.
    let host = parts.headers.get(HOST).map(HeaderValue::as_bytes);
    if let Some(refusal) = hosts.refusal(host) {
        return Ok(text(StatusCode::MISDIRECTED_REQUEST, refusal));
    }
    let accept = parts.headers.get(ACCEPT);
    let reply = match (&parts.method, parts.uri.path()) {
        (&Method::POST, "/ask") => {
            let format = Format::of(accept, Format::Json);
            if is_json(parts.headers.get(CONTENT_TYPE)) {
                let rows = Rows::Every {
                    max_rows: *max_answer_rows,
                };
                let writing = Writing { format, rows };
                post_ask(body, app, turns, *syntax, writing).await
            } else {
                let error = format!("an ask is JSON, sent with Content-Type: {JSON}");
                format.errors(StatusCode::UNSUPPORTED_MEDIA_TYPE, &[error])
            }
        }
        (&Method::GET, "/livez") => text(StatusCode::OK, "live\n".into()),
        (&Method::GET, "/openapi.json") => respond(StatusCode::OK, JSON, OPENAPI.into()),
        (&Method::GET, "/readyz") => get_readyz(app),
        (&Method::GET, "/status") => get_status(app, Format::of(accept, Format::Text)),
        (&Method::GET, "/views") => {
            let format = Format::of(accept, Format::Text);
            views_off_async(app, turns, format).await
        }
        (_, "/ask") => not_allowed("POST"),
        (_, "/livez" | "/openapi.json" | "/readyz" | "/status" | "/views") => not_allowed("GET"),
        (&Method::GET, path) if path.starts_with(ROWS_PREFIX) => {
            let named = &path[ROWS_PREFIX.len()..];
            match named.strip_suffix(SPEC_SUFFIX) {
                Some(view) => get_spec(definitions, view),
                None => get_rows(app, turns, syntax.describe, named, parts.uri.query()).await,
            }
        }
        (_, path) if path.starts_with(ROWS_PREFIX) => not_allowed("GET"),
        (_, path) => text(StatusCode::NOT_FOUND, format!("no {path} here\n")),
    };
    Ok(reply)
}

/// A turn to read the store, if one is free. Waiting for one would let
/// asks pile up without end; a client refused one can ask again.
fn take_turn(turns: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    Arc::clone(turns).try_acquire_owned().ok()
}

/// The reply when every turn to read the store is taken.
fn busy(format: Format) -> Reply {
    let error = "busy: every core is answering an ask; ask again shortly";
    format.errors(StatusCode::SERVICE_UNAVAILABLE, &[error])
}

/// `GET /views` on a blocking thread, holding a turn till it is done.
async fn views_off_async(app: &App, turns: &Arc<Semaphore>, format: Format) -> Reply {
    let Some(turn) = take_turn(turns) else {
        return busy(format);
    };
    let app = app.clone();
    // The views are read under the store's lock, which a long ask or a
    // writer can hold: the health routes don't queue behind.
    let read = tokio::task::spawn_blocking(move || {
        let _turn = turn;
        get_views(&app, format)
    });
    match read.await {
        Ok(reply) => reply,
        // A panic is not the ask's error: pass it on, and send no reply.
        Err(error) => panic::resume_unwind(error.into_panic()),
    }
}

/// Whether `Content-Type` names JSON, whatever its parameters or case.
/// A page on another site may have a browser send a form or plain text
/// to Straum without asking first, but not JSON, so this keeps such a
/// page from running asks.
fn is_json(content_type: Option<&HeaderValue>) -> bool {
    let Some(Ok(content_type)) = content_type.map(HeaderValue::to_str) else {
        return false;
    };
    media_type(content_type).eq_ignore_ascii_case(JSON)
}

/// A media type or range without its parameters or the spaces around it.
fn media_type(value: &str) -> &str {
    value.split(';').next().unwrap_or_default().trim()
}

async fn post_ask(
    body: Incoming,
    app: &App,
    turns: &Arc<Semaphore>,
    syntax: AskSyntax,
    writing: Writing,
) -> Reply {
    let format = writing.format;
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
    let Some(turn) = take_turn(turns) else {
        return busy(format);
    };
    let parsing = Instant::now();
    // Reading a 1 MiB ask takes time an async thread can't spare: the
    // health routes queue behind it. The turn goes along, so a client
    // that hangs up frees it only once the read is done.
    let read = tokio::task::spawn_blocking(move || (read_body(&body, syntax), turn)).await;
    let (asked, turn) = match read {
        Ok((Ok(asked), turn)) => (asked, turn),
        Ok((Err(errors), _)) => return format.errors(StatusCode::BAD_REQUEST, &errors),
        // A panic is not the ask's error: pass it on, and send no reply.
        Err(error) => panic::resume_unwind(error.into_panic()),
    };
    let parse_time = parsing.elapsed();
    answer(
        app,
        asked,
        writing,
        syntax.describe,
        parse_time,
        start,
        turn,
    )
    .await
}

/// The ask a body holds, or every error in it.
fn read_body(body: &Bytes, syntax: AskSyntax) -> Result<Ask, Vec<String>> {
    let sent = match ask::read(body) {
        Ok(sent) => sent,
        Err(errors) => return Err(lines(&errors)),
    };
    match (syntax.read)(&sent.query, &sent.parameters) {
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

/// The view's `CREATE VIEW` statement. The spec is fixed at start, so it
/// answers while the tables still load.
fn get_spec(definitions: &[Definition], view: &str) -> Reply {
    for definition in definitions {
        if definition.view.as_str() == view {
            return text(StatusCode::OK, format!("{}\n", definition.text));
        }
    }
    no_view(view)
}

fn no_view(view: &str) -> Reply {
    text(
        StatusCode::NOT_FOUND,
        format!("no view {view}; GET /views lists them\n"),
    )
}

async fn get_rows(
    app: &App,
    turns: &Arc<Semaphore>,
    describe: DescribeAskError,
    view: &str,
    query: Option<&str>,
) -> Reply {
    let start = Instant::now();
    let Some(asked) = app.rows_ask(view) else {
        return no_view(view);
    };
    let window = match rows_window(query.unwrap_or("")) {
        Ok(window) => window,
        Err(error) => return Format::Text.errors(StatusCode::BAD_REQUEST, &[error]),
    };
    let writing = Writing {
        format: Format::Text,
        rows: Rows::Window(window),
    };
    let Some(turn) = take_turn(turns) else {
        return busy(Format::Text);
    };
    answer(app, asked, writing, describe, Duration::ZERO, start, turn).await
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

/// How an answer is to be written: its format, and which of its rows.
#[derive(Clone, Copy)]
struct Writing {
    format: Format,
    rows: Rows,
}

/// Which of an answer's rows a reply writes.
#[derive(Clone, Copy)]
enum Rows {
    /// Every one, or none when there are more than `max_rows`.
    Every { max_rows: usize },
    /// Those the window shows.
    Window(Window),
}

/// Why an answer was not written.
#[derive(Debug, PartialEq, Eq)]
enum Unwritten {
    App(AnswerError),
    /// The answer has `total` rows, past the `max_rows` an ask may get.
    TooManyRows {
        total: usize,
        max_rows: usize,
    },
}

impl From<AnswerError> for Unwritten {
    fn from(error: AnswerError) -> Unwritten {
        Unwritten::App(error)
    }
}

impl Display for Unwritten {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unwritten::App(error) => error.fmt(formatter),
            Unwritten::TooManyRows { total, max_rows } => write!(
                formatter,
                "the answer has {total} rows, past the {max_rows} an ask may get; \
                 narrow the ask: a shorter range, fewer keys or a filter"
            ),
        }
    }
}

/// The rows an ask builds: up to the most it may answer, so that a
/// refused one builds no more than an answered one.
fn answer_window(max_rows: usize) -> Window {
    Window {
        start: Start::FromFirst(0),
        count: max_rows,
    }
}

/// Refuses an answer of every row when it has more than the `max_rows` an
/// ask may get.
fn within_answer_rows(cut: Cut, max_rows: usize) -> Result<(), Unwritten> {
    if cut.total > max_rows {
        let total = cut.total;
        return Err(Unwritten::TooManyRows { total, max_rows });
    }
    Ok(())
}

/// The answer to `asked` on a blocking thread, holding `turn` till it is
/// written: a client that hangs up stops neither, so the turn is not
/// freed while the store is still being read.
async fn answer(
    app: &App,
    asked: Ask,
    writing: Writing,
    describe: DescribeAskError,
    parse_time: Duration,
    start: Instant,
    turn: OwnedSemaphorePermit,
) -> Reply {
    let format = writing.format;
    let range_errors = answer::range_errors(&asked);
    if !range_errors.is_empty() {
        return format.errors(StatusCode::BAD_REQUEST, &range_errors);
    }
    let app = app.clone();
    // A big ask reads every cell of a fact, so it runs off the async
    // threads.
    let answered = tokio::task::spawn_blocking(move || {
        let _turn = turn;
        answer_timed(&app, &asked, writing)
    })
    .await;
    match answered {
        Ok(Ok(timed)) => answered_reply(timed, format, parse_time, start),
        Ok(Err(error)) => answer_error(&error, describe, format),
        // A panic is not the ask's error: pass it on, and send no reply.
        Err(error) => panic::resume_unwind(error.into_panic()),
    }
}

/// A written answer, and how long the app and the writing took.
struct Timed {
    applied: Lsn,
    body: Vec<u8>,
    answer_time: Duration,
    write_time: Duration,
}

fn answer_timed(app: &App, asked: &Ask, writing: Writing) -> Result<Timed, Unwritten> {
    let answering = Instant::now();
    let answered = match writing.rows {
        Rows::Window(window) => app.ask_window(asked, window)?,
        Rows::Every { max_rows } => {
            let answered = app.ask_window(asked, answer_window(max_rows))?;
            within_answer_rows(answered.cut, max_rows)?;
            answered
        }
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

fn answer_error(error: &Unwritten, describe: DescribeAskError, format: Format) -> Reply {
    match error {
        Unwritten::App(AnswerError::Invalid(invalid)) => {
            let mut lines = Vec::with_capacity(invalid.len());
            for ask_error in invalid {
                lines.push(describe(ask_error));
            }
            format.errors(StatusCode::BAD_REQUEST, &lines)
        }
        Unwritten::App(AnswerError::NotReady) => {
            format.errors(StatusCode::SERVICE_UNAVAILABLE, &[error])
        }
        Unwritten::TooManyRows { .. } => format.errors(StatusCode::BAD_REQUEST, &[error]),
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
        Some(why) => Format::Text.errors(StatusCode::SERVICE_UNAVAILABLE, &[not_ready(&why)]),
    }
}

/// Why `GET /readyz` and `GET /views` refuse, as they say it.
fn not_ready(why: &str) -> String {
    format!("not ready: {why}")
}

fn get_views(app: &App, format: Format) -> Reply {
    let Some(held) = app.held() else {
        // A load can finish between the two reads, so there may be no
        // reason left by the time the status is read.
        let why = match status::why_not(&app.status()) {
            Some(why) => why,
            None => "loading".to_owned(),
        };
        return format.errors(StatusCode::SERVICE_UNAVAILABLE, &[not_ready(&why)]);
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

    fn content_type(value: &'static str) -> HeaderValue {
        HeaderValue::from_static(value)
    }

    #[test]
    fn an_ask_is_json_whatever_its_parameters_or_case() {
        for value in [
            "application/json",
            "application/json; charset=utf-8",
            "Application/JSON",
            " application/json ;charset=UTF-8",
        ] {
            assert!(is_json(Some(&content_type(value))), "{value}");
        }
    }

    #[test]
    fn an_ask_in_any_other_media_type_is_not_json() {
        assert!(!is_json(None));
        for value in [
            "",
            "text/plain",
            "application/x-www-form-urlencoded",
            "multipart/form-data; boundary=x",
            "application/jsonx",
            "application/json-seq",
            "text/plain; application/json",
        ] {
            assert!(!is_json(Some(&content_type(value))), "{value}");
        }
        let not_text = HeaderValue::from_bytes(b"application/json\xff").unwrap();
        assert!(!is_json(Some(&not_text)));
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

    #[test]
    fn an_answer_of_at_most_the_most_rows_is_written() {
        let max_rows = 3;
        for total in [0, 1, max_rows] {
            let cut = Cut::of(Some(answer_window(max_rows)), total);
            assert_eq!(within_answer_rows(cut, max_rows), Ok(()), "{total}");
            assert_eq!(cut.count, total);
        }
    }

    #[test]
    fn an_answer_of_more_rows_is_refused_naming_them() {
        let max_rows = 100_000;
        let total = max_rows + 1;
        let refused = within_answer_rows(Cut::of(Some(answer_window(max_rows)), total), max_rows);
        assert_eq!(refused, Err(Unwritten::TooManyRows { total, max_rows }));
        assert_eq!(
            refused.unwrap_err().to_string(),
            "the answer has 100001 rows, past the 100000 an ask may get; \
             narrow the ask: a shorter range, fewer keys or a filter"
        );
    }
}
