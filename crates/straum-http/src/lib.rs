//! straum over HTTP: JSON asks and health checks, on tokio and hyper. Asks
//! run on tokio's blocking threads, since a big one reads every cell of a
//! fact.
//!
//! - `POST /ask`: an ask as JSON (see `ask.rs`); the answer as JSON (see
//!   `answer.rs`). 400 for an ask straum can't answer, 503 while a table it
//!   reads isn't ready.
//! - `GET /livez`: 200 while the process serves.
//! - `GET /readyz`: 200 when every table is loaded and straum is within its
//!   lag bound, else 503 and why. Asks still answer while straum lags.

#![forbid(unsafe_code)]

mod answer;
mod ask;
mod json;

use std::convert::Infallible;
use std::future::{Future, poll_fn};
use std::io;
use std::net;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::header::{ALLOW, CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use straum_app::{AnswerError, App};
use tokio::net::TcpListener;

/// The largest ask body straum reads.
const MAX_ASK: usize = 1 << 20;

/// How long to wait before accepting again after an accept fails, as when
/// the process is out of file descriptors.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Serves `app` on `listener` until `shutdown` completes, then finishes the
/// requests in flight and returns. Runs on a tokio runtime with I/O and
/// time enabled.
pub async fn serve(
    listener: net::TcpListener,
    app: App,
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
        let app = Arc::clone(&app);
        let service = service_fn(move |request| handle(request, Arc::clone(&app)));
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
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = match (method, path.as_str()) {
        (Method::POST, "/ask") => ask(request, &app).await,
        (Method::GET, "/livez") => text(StatusCode::OK, "live\n".into()),
        (Method::GET, "/readyz") => readyz(&app),
        (_, "/ask") => not_allowed("POST"),
        (_, "/livez" | "/readyz") => not_allowed("GET"),
        (_, path) => text(StatusCode::NOT_FOUND, format!("no {path} here\n")),
    };
    Ok(response)
}

async fn ask(request: Request<Incoming>, app: &App) -> Response<Full<Bytes>> {
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
    let request = match ask::read(&body) {
        Ok(ask) => ask,
        Err(e) => return errors(StatusCode::BAD_REQUEST, &e),
    };
    let app = app.clone();
    let answered = tokio::task::spawn_blocking(move || {
        let answered = app.ask(&request)?;
        Ok(answer::write(&request, &answered))
    })
    .await;
    match answered {
        Ok(Ok(json)) => respond(StatusCode::OK, "application/json", json),
        Ok(Err(AnswerError::NotReady)) => {
            errors(StatusCode::SERVICE_UNAVAILABLE, &["not ready".to_owned()])
        }
        Ok(Err(AnswerError::Invalid(invalid))) => {
            let invalid: Vec<String> = invalid.iter().map(ToString::to_string).collect();
            errors(StatusCode::BAD_REQUEST, &invalid)
        }
        Err(_) => {
            let error = "straum failed while answering; its log says why".to_owned();
            errors(StatusCode::INTERNAL_SERVER_ERROR, &[error])
        }
    }
}

fn readyz(app: &App) -> Response<Full<Bytes>> {
    if app.ready() {
        return text(StatusCode::OK, "ready\n".into());
    }
    let why = match (app.error(), app.lag()) {
        (Some(error), _) => error,
        (None, Some(lag)) => format!("{} s behind PostgreSQL", lag.as_secs()),
        (None, None) => "loading".to_owned(),
    };
    text(
        StatusCode::SERVICE_UNAVAILABLE,
        format!("not ready: {why}\n"),
    )
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
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}
