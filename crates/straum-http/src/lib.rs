//! Straum over HTTP: asks and health checks, on tokio and hyper. Asks
//! run on tokio's blocking threads, since a big one reads every cell of a
//! fact.
//!
//! - `POST /ask`: a query and its parameters as JSON (see `ask.rs`), the
//!   query read by the `ReadAsk` that `serve` is given; the answer as JSON
//!   (see `answer.rs`), with the LSN it reflects in `Straum-LSN` and where
//!   the time went in `Server-Timing`. 400 for an ask Straum can't answer,
//!   503 while a table it reads isn't ready. With `Accept: text/plain` the
//!   answer is a table and the errors are lines, for people (ADR-0039).
//! - `GET /livez`: 200 while the process serves.
//! - `GET /readyz`: 200 when every table is loaded and Straum is within its
//!   lag bound, else 503 and why. Asks still answer while Straum lags.
//! - `GET /status`: what Straum has done, in plain text for people; `straum
//!   status` prints it.
//! - `GET /views`: every view and how much of it Straum holds, in plain text
//!   for people; `straum views` prints it. 503 until every table is loaded.
//!   With `Accept: application/json` the same as JSON (see `views.rs`).
//! - `GET /views/NAME`: the rows PostgreSQL would give for the view, the
//!   first `MAX_ROWS` of them, as the table an ask gets; the `\rows` of
//!   `straum shell`. 404 for a name no view has.

#![forbid(unsafe_code)]

mod answer;
mod ask;
mod json;
mod status;
mod views;

use std::convert::Infallible;
use std::future::{Future, poll_fn};
use std::io;
use std::net;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{ACCEPT, ALLOW, CACHE_CONTROL, CONTENT_TYPE, HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use straum_app::{AnswerError, App, Lsn};
use straum_domain::{Ask, Literal};
use tokio::net::TcpListener;

const LSN: HeaderName = HeaderName::from_static("straum-lsn");

/// Where an answer's time went in Straum, so a client can tell it from the
/// network's.
const SERVER_TIMING: HeaderName = HeaderName::from_static("server-timing");

const MAX_ASK: usize = 1 << 20;

/// The rows `GET /views/NAME` writes. A view may hold millions, which no
/// one reads.
const MAX_ROWS: usize = 1000;

const ROWS: &str = "/views/";

const JSON: &str = "application/json";
const TEXT: &str = "text/plain; charset=utf-8";

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
}

/// How long to wait before accepting again after an accept fails, as when
/// the process is out of file descriptors.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Reads an ask's query with its parameters, or says everything wrong with
/// it. The binary picks the syntax, so HTTP stays apart from how asks are
/// written.
pub type ReadAsk = fn(&str, &[Literal]) -> Result<Ask, Vec<String>>;

/// Serves `app` on `listener` until `shutdown` completes, then finishes the
/// requests in flight and returns. Runs on a tokio runtime with I/O and
/// time enabled.
pub async fn serve(
    listener: net::TcpListener,
    app: App,
    read_ask: ReadAsk,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(listener)?;
    let app = Arc::new(app);
    let graceful = GracefulShutdown::new();
    let mut shutdown = pin!(shutdown);
    loop {
        let accepted = poll_fn(|cx| match shutdown.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(None),
            Poll::Pending => listener.poll_accept(cx).map(Some),
        })
        .await;
        let stream = match accepted {
            None => break,
            Some(Ok((stream, _))) => stream,
            Some(Err(_)) => {
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        // Nagle would hold an answer's second write until the client acks
        // the first, which a client that delays acks makes 40 ms.
        stream.set_nodelay(true).ok();
        let app = Arc::clone(&app);
        let service = service_fn(move |request| handle(request, Arc::clone(&app), read_ask));
        let connection = http1::Builder::new()
            .timer(TokioTimer::new())
            .serve_connection(TokioIo::new(stream), service);
        let connection = graceful.watch(connection);
        // A client that hangs up mid-request is its own problem.
        tokio::spawn(async move { connection.await.ok() });
    }
    drop(listener);
    graceful.shutdown().await;
    Ok(())
}

async fn handle(
    request: Request<Incoming>,
    app: Arc<App>,
    read_ask: ReadAsk,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let (parts, body) = request.into_parts();
    let accept = parts.headers.get(ACCEPT);
    let response = match (&parts.method, parts.uri.path()) {
        (&Method::POST, "/ask") => {
            ask(body, &app, read_ask, Format::of(accept, Format::Json)).await
        }
        (&Method::GET, "/livez") => text(StatusCode::OK, "live\n".into()),
        (&Method::GET, "/readyz") => readyz(&app),
        (&Method::GET, "/status") => text(StatusCode::OK, status::write(&app.status())),
        (&Method::GET, "/views") => views(&app, Format::of(accept, Format::Text)),
        (_, "/ask") => not_allowed("POST"),
        (_, "/livez" | "/readyz" | "/status" | "/views") => not_allowed("GET"),
        (&Method::GET, path) if path.starts_with(ROWS) => rows(&app, &path[ROWS.len()..]).await,
        (_, path) if path.starts_with(ROWS) => not_allowed("GET"),
        (_, path) => text(StatusCode::NOT_FOUND, format!("no {path} here\n")),
    };
    Ok(response)
}

async fn ask(
    body: Incoming,
    app: &App,
    read_ask: ReadAsk,
    format: Format,
) -> Response<Full<Bytes>> {
    let start = Instant::now();
    let errors = |status, errors: &[String]| errors_response(status, format, errors);
    let body = match Limited::new(body, MAX_ASK).collect().await {
        Ok(body) => body.to_bytes(),
        Err(e) if e.is::<LengthLimitError>() => {
            let error = format!("an ask is at most {MAX_ASK} bytes");
            return errors(StatusCode::PAYLOAD_TOO_LARGE, &[error]);
        }
        Err(e) => {
            return errors(StatusCode::BAD_REQUEST, &[format!("reading the ask: {e}")]);
        }
    };
    let parsing = Instant::now();
    let request =
        match ask::read(&body).and_then(|(query, parameters)| read_ask(&query, &parameters)) {
            Ok(ask) => ask,
            Err(e) => return errors(StatusCode::BAD_REQUEST, &e),
        };
    answer(app, request, format, None, parsing.elapsed(), start).await
}

/// Every row of a view, as an ask for them is answered.
async fn rows(app: &App, view: &str) -> Response<Full<Bytes>> {
    let start = Instant::now();
    match app.rows_ask(view) {
        Some(ask) => {
            answer(
                app,
                ask,
                Format::Text,
                Some(MAX_ROWS),
                Duration::ZERO,
                start,
            )
            .await
        }
        None => text(
            StatusCode::NOT_FOUND,
            format!("no view {view}; GET /views lists them\n"),
        ),
    }
}

async fn answer(
    app: &App,
    request: Ask,
    format: Format,
    limit: Option<usize>,
    parse: Duration,
    start: Instant,
) -> Response<Full<Bytes>> {
    let errors = |status, errors: &[String]| errors_response(status, format, errors);
    let range = answer::range_errors(&request);
    if !range.is_empty() {
        return errors(StatusCode::BAD_REQUEST, &range);
    }
    let app = app.clone();
    let answered = tokio::task::spawn_blocking(move || {
        let answering = Instant::now();
        let answered = app.ask(&request)?;
        let writing = Instant::now();
        let body = match format {
            Format::Json => answer::write(&answered),
            Format::Text => answer::table(&answered, limit).into_bytes(),
        };
        Ok(Timed {
            applied: answered.applied,
            body,
            answer: writing - answering,
            write: writing.elapsed(),
        })
    })
    .await;
    match answered {
        Ok(Ok(timed)) => answered_response(timed, format, parse, start),
        Ok(Err(e)) => answer_error(&e, format),
        Err(_) => {
            let error = "Straum failed while answering; its log says why".to_owned();
            errors(StatusCode::INTERNAL_SERVER_ERROR, &[error])
        }
    }
}

struct Timed {
    applied: Lsn,
    body: Vec<u8>,
    answer: Duration,
    write: Duration,
}

fn answered_response(
    timed: Timed,
    format: Format,
    parse: Duration,
    start: Instant,
) -> Response<Full<Bytes>> {
    let mut response = respond(StatusCode::OK, format.content_type(), timed.body);
    let lsn = HeaderValue::try_from(timed.applied.to_string()).expect("an LSN is ASCII");
    response.headers_mut().insert(LSN, lsn);
    // total also holds reading the body and the hops to and from the
    // blocking thread, which no phase covers.
    let phases = [
        ("parse", parse),
        ("answer", timed.answer),
        (format.phase(), timed.write),
        ("total", start.elapsed()),
    ];
    (response.headers_mut()).insert(SERVER_TIMING, server_timing(&phases));
    response
}

fn answer_error(e: &AnswerError, format: Format) -> Response<Full<Bytes>> {
    let (status, errors) = match e {
        AnswerError::Invalid(invalid) => (
            StatusCode::BAD_REQUEST,
            invalid.iter().map(ToString::to_string).collect(),
        ),
        AnswerError::NotReady => (StatusCode::SERVICE_UNAVAILABLE, vec![e.to_string()]),
        AnswerError::Spill(_) => (StatusCode::INTERNAL_SERVER_ERROR, vec![e.to_string()]),
    };
    errors_response(status, format, &errors)
}

/// `name;dur=ms, ...`: the spec counts in milliseconds, and asks take
/// microseconds.
fn server_timing(phases: &[(&str, Duration)]) -> HeaderValue {
    let phases: Vec<String> = phases
        .iter()
        .map(|(name, took)| format!("{name};dur={:.3}", took.as_secs_f64() * 1e3))
        .collect();
    HeaderValue::try_from(phases.join(", ")).expect("names and numbers are ASCII")
}

fn readyz(app: &App) -> Response<Full<Bytes>> {
    match status::why_not(&app.status()) {
        None => text(StatusCode::OK, "ready\n".into()),
        Some(why) => text(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("not ready: {why}\n"),
        ),
    }
}

fn views(app: &App, format: Format) -> Response<Full<Bytes>> {
    match (app.held(), format) {
        (Some(held), Format::Text) => text(StatusCode::OK, views::write(&held)),
        (Some(held), Format::Json) => respond(StatusCode::OK, JSON, views::json(&held)),
        (None, _) => {
            // A load can finish between the two reads.
            let why = status::why_not(&app.status()).unwrap_or_else(|| "loading".to_owned());
            let why = format!("not ready: {why}");
            errors_response(StatusCode::SERVICE_UNAVAILABLE, format, &[why])
        }
    }
}

/// `{"errors": [...]}`, or a line per error.
fn errors_response(status: StatusCode, format: Format, errors: &[String]) -> Response<Full<Bytes>> {
    if let Format::Text = format {
        return text(status, errors.join("\n") + "\n");
    }
    let errors: Vec<String> = errors.iter().map(|error| json::quoted(error)).collect();
    let body = format!("{{\"errors\":[{}]}}", errors.join(","));
    respond(status, JSON, body.into_bytes())
}

fn not_allowed(allow: &'static str) -> Response<Full<Bytes>> {
    let mut response = text(StatusCode::METHOD_NOT_ALLOWED, format!("use {allow}\n"));
    response
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static(allow));
    response
}

fn text(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    respond(status, TEXT, body.into_bytes())
}

fn respond(status: StatusCode, content_type: &'static str, body: Vec<u8>) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    // An answer is right as of its as_of and no later, so nothing between
    // Straum and a client may keep one.
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
