//! straum over HTTP: asks and health checks, on tokio and hyper. Asks
//! run on tokio's blocking threads, since a big one reads every cell of a
//! fact.
//!
//! - `POST /ask`: a query and its parameters as JSON (see `ask.rs`), the
//!   query read by the `ReadAsk` that `serve` is given; the answer as JSON
//!   (see `answer.rs`), with the LSN it reflects in `Straum-LSN` and where
//!   the time went in `Server-Timing`. 400 for an ask straum can't answer,
//!   503 while a table it reads isn't ready.
//! - `GET /livez`: 200 while the process serves.
//! - `GET /readyz`: 200 when every table is loaded and straum is within its
//!   lag bound, else 503 and why. Asks still answer while straum lags.
//! - `GET /status`: what straum has done, in plain text for people; `straum
//!   status` prints it.

#![forbid(unsafe_code)]

mod answer;
mod ask;
mod json;
mod status;

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
use hyper::header::{ALLOW, CACHE_CONTROL, CONTENT_TYPE, HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use straum_app::{AnswerError, App};
use straum_domain::{Ask, Literal};
use tokio::net::TcpListener;

/// The LSN an answer reflects.
const LSN: HeaderName = HeaderName::from_static("straum-lsn");

/// Where an answer's time went in straum, so a client can tell it from the
/// network's.
const SERVER_TIMING: HeaderName = HeaderName::from_static("server-timing");

/// The largest ask body straum reads.
const MAX_ASK: usize = 1 << 20;

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
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = match (method, path.as_str()) {
        (Method::POST, "/ask") => ask(request, &app, read_ask).await,
        (Method::GET, "/livez") => text(StatusCode::OK, "live\n".into()),
        (Method::GET, "/readyz") => readyz(&app),
        (Method::GET, "/status") => text(StatusCode::OK, status::write(&app.status())),
        (_, "/ask") => not_allowed("POST"),
        (_, "/livez" | "/readyz" | "/status") => not_allowed("GET"),
        (_, path) => text(StatusCode::NOT_FOUND, format!("no {path} here\n")),
    };
    Ok(response)
}

async fn ask(request: Request<Incoming>, app: &App, read_ask: ReadAsk) -> Response<Full<Bytes>> {
    let start = Instant::now();
    let body = match Limited::new(request.into_body(), MAX_ASK).collect().await {
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
    let range = answer::range_errors(&request);
    if !range.is_empty() {
        return errors(StatusCode::BAD_REQUEST, &range);
    }
    let parse = parsing.elapsed();
    let app = app.clone();
    let answered = tokio::task::spawn_blocking(move || {
        let answering = Instant::now();
        let answered = app.ask(&request)?;
        let writing = Instant::now();
        let json = answer::write(&answered);
        Ok((answered.lsn, json, writing - answering, writing.elapsed()))
    })
    .await;
    match answered {
        Ok(Ok((lsn, json, answer, write))) => {
            let mut response = respond(StatusCode::OK, "application/json", json);
            let lsn = HeaderValue::try_from(lsn.to_string()).expect("an LSN is ASCII");
            response.headers_mut().insert(LSN, lsn);
            // total also holds reading the body and the hops to and from the
            // blocking thread, which no phase covers.
            let phases = [
                ("parse", parse),
                ("answer", answer),
                ("json", write),
                ("total", start.elapsed()),
            ];
            (response.headers_mut()).insert(SERVER_TIMING, server_timing(&phases));
            response
        }
        Ok(Err(AnswerError::NotReady)) => {
            errors(StatusCode::SERVICE_UNAVAILABLE, &["not ready".to_owned()])
        }
        Ok(Err(AnswerError::Invalid(invalid))) => {
            let invalid: Vec<String> = invalid.iter().map(ToString::to_string).collect();
            errors(StatusCode::BAD_REQUEST, &invalid)
        }
        Ok(Err(e @ AnswerError::Spill(_))) => {
            errors(StatusCode::INTERNAL_SERVER_ERROR, &[e.to_string()])
        }
        Err(_) => {
            let error = "straum failed while answering; its log says why".to_owned();
            errors(StatusCode::INTERNAL_SERVER_ERROR, &[error])
        }
    }
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

/// `{"errors": [...]}`.
fn errors(status: StatusCode, errors: &[String]) -> Response<Full<Bytes>> {
    let mut body = b"{\"errors\":[".to_vec();
    for (i, error) in errors.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        json::string(&mut body, error);
    }
    body.extend_from_slice(b"]}");
    respond(status, "application/json", body)
}

fn not_allowed(allow: &'static str) -> Response<Full<Bytes>> {
    let mut response = text(StatusCode::METHOD_NOT_ALLOWED, format!("use {allow}\n"));
    response
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static(allow));
    response
}

fn text(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    respond(status, "text/plain; charset=utf-8", body.into_bytes())
}

fn respond(status: StatusCode, content_type: &'static str, body: Vec<u8>) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    // An answer is right as of its as_of and no later, so nothing between
    // straum and a client may keep one.
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
