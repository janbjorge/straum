//! The route handlers: `handle` takes one request from the server loop in
//! `lib.rs`, picks the handler by method and path, and returns the reply.
//! An ask goes body, then `ask.rs`, then the `ReadAsk`, then the app on a
//! blocking thread, then `answer.rs`; the other routes read the app and
//! hand what they read to `status.rs` or `views.rs`.

use std::convert::Infallible;
use std::fmt::Display;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{ACCEPT, ALLOW, CACHE_CONTROL, CONTENT_TYPE, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use straum_app::{AnswerError, App, Lsn};
use straum_domain::Ask;

use crate::json::quoted;
use crate::{ReadAsk, answer, ask, status, views};

const LSN: HeaderName = HeaderName::from_static("straum-lsn");

/// Where an answer's time went in Straum, so a client can tell it from the
/// network's.
const SERVER_TIMING: HeaderName = HeaderName::from_static("server-timing");

const MAX_ASK_BYTES: usize = 1 << 20;

/// The rows `GET /views/NAME` writes. A view may hold millions, which no
/// one reads.
const MAX_ROWS: usize = 1000;

const ROWS_PREFIX: &str = "/views/";

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
    /// The format `Accept` names, else the route's own.
    fn of(accept: Option<&HeaderValue>, default: Format) -> Format {
        match accept {
            Some(accept) if accept == JSON => Format::Json,
            Some(accept) if accept == "text/plain" => Format::Text,
            _ => default,
        }
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
        (&Method::GET, "/views") => get_views(&app, Format::of(accept, Format::Text)),
        (_, "/ask") => not_allowed("POST"),
        (_, "/livez" | "/readyz" | "/status" | "/views") => not_allowed("GET"),
        (&Method::GET, path) if path.starts_with(ROWS_PREFIX) => {
            get_rows(&app, &path[ROWS_PREFIX.len()..]).await
        }
        (_, path) if path.starts_with(ROWS_PREFIX) => not_allowed("GET"),
        (_, path) => text(StatusCode::NOT_FOUND, format!("no {path} here\n")),
    };
    Ok(reply)
}

async fn post_ask(body: Incoming, app: &App, read_ask: ReadAsk, format: Format) -> Reply {
    let start = Instant::now();
    let body = match Limited::new(body, MAX_ASK_BYTES).collect().await {
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
    let sent = match ask::read(&body) {
        Ok(sent) => sent,
        Err(errors) => return format.errors(StatusCode::BAD_REQUEST, &errors),
    };
    let asked = match read_ask(&sent.query, &sent.parameters) {
        Ok(asked) => asked,
        Err(errors) => return format.errors(StatusCode::BAD_REQUEST, &errors),
    };
    let writing = Writing {
        format,
        max_rows: None,
    };
    answer(app, asked, writing, parsing.elapsed(), start).await
}

/// Every row of a view, as an ask for them is answered.
async fn get_rows(app: &App, view: &str) -> Reply {
    let start = Instant::now();
    let Some(asked) = app.rows_ask(view) else {
        return text(
            StatusCode::NOT_FOUND,
            format!("no view {view}; GET /views lists them\n"),
        );
    };
    let writing = Writing {
        format: Format::Text,
        max_rows: Some(MAX_ROWS),
    };
    answer(app, asked, writing, Duration::ZERO, start).await
}

/// How an answer is to be written: its format, and how many rows a table
/// shows.
#[derive(Clone, Copy)]
struct Writing {
    format: Format,
    max_rows: Option<usize>,
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
        Err(_) => {
            let error = "Straum failed while answering; its log says why";
            format.errors(StatusCode::INTERNAL_SERVER_ERROR, &[error])
        }
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
    let answered = app.ask(asked)?;
    let written_from = Instant::now();
    let body = match writing.format {
        Format::Json => answer::write(&answered),
        Format::Text => answer::table(&answered, writing.max_rows).into_bytes(),
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
