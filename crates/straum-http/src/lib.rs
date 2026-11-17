//! Straum over HTTP, on tokio and hyper. This file holds the server loop:
//! `serve` accepts connections until told to stop, hands each request to
//! `routes.rs`, and on shutdown finishes the requests in flight. The other
//! files each write one kind of reply: `answer.rs` an answer, `views.rs`
//! the views, `status.rs` the status; `ask.rs` and `json.rs` read an ask.
//!
//! - `POST /ask`: a query and its parameters as JSON (see `ask.rs`), the
//!   query read by the `ReadAsk` that `serve` is given; the answer as JSON
//!   (see `answer.rs`), with the LSN it reflects in `Straum-LSN` and where
//!   the time went in `Server-Timing`. 400 for an ask Straum can't answer,
//!   503 while a table it reads isn't ready. With `Accept: text/plain` the
//!   answer is a table and the errors are lines, for people (ADR-0011).
//! - `GET /livez`: 200 while the process serves.
//! - `GET /readyz`: 200 when every table is loaded and Straum is within its
//!   lag bound, else 503 and why. Asks still answer while Straum lags.
//! - `GET /status`: what Straum has done, in plain text for people; `straum
//!   status` prints it. As JSON to those who accept it.
//! - `GET /views`: every view and how much of it Straum holds, in plain text
//!   for people; `straum views` prints it. 503 until every table is loaded.
//!   With `Accept: application/json` each view's names and range as JSON,
//!   which `straum shell` completes from (see `views.rs`).
//! - `GET /views/NAME`: the rows PostgreSQL would give for the view, as
//!   the table an ask gets: `count` of them, at most `MAX_ROWS`, from row
//!   `start`, a negative one counted back from the end; the `\rows` of
//!   `straum shell`. 400 for a query string it can't read, 404 for a name
//!   no view has.
//! - `GET /views/NAME/spec`: the view's `CREATE VIEW` statement, as the
//!   spec wrote it; the `\d NAME` of `straum shell`. 404 for a name no
//!   view has. It answers while the tables still load.

#![forbid(unsafe_code)]

mod answer;
mod ask;
mod json;
mod routes;
mod status;
mod time;
mod views;

use std::fmt;
use std::future::{Future, poll_fn};
use std::io;
use std::net;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use straum_app::App;
use straum_domain::{Ask, Definition, Literal};
use tokio::net::{TcpListener, TcpStream};
use tokio::signal::unix::{Signal, SignalKind, signal};

/// How long to wait before accepting again after an accept fails, as when
/// the process is out of file descriptors.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// One thing wrong with an ask's query, as the `ReadAsk` that read it
/// says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    /// The line of the query it is on, counted from 1, if known.
    pub line: Option<u64>,
    pub message: String,
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(formatter, "line {line}: {}", self.message),
            None => formatter.write_str(&self.message),
        }
    }
}

/// Reads an ask's query with its parameters, or says everything wrong with
/// it. The binary picks the syntax, so HTTP stays apart from how asks are
/// written.
pub type ReadAsk = fn(&str, &[Literal]) -> Result<Ask, Vec<QueryError>>;

/// Serves `app` on `listener`, and each view's `definitions`, until
/// `shutdown` completes, then finishes the
/// requests in flight and returns. Runs on a tokio runtime with I/O and
/// time enabled.
// `impl Future`: each caller passes an `async` block, whose type has no name.
pub async fn serve(
    listener: net::TcpListener,
    app: App,
    read_ask: ReadAsk,
    definitions: Vec<Definition>,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(listener)?;
    let app = Arc::new(app);
    let definitions: Arc<[Definition]> = Arc::from(definitions);
    let graceful = GracefulShutdown::new();
    let mut shutdown = pin!(shutdown);
    loop {
        // Wait for whichever comes first: the shutdown or a connection.
        let accepted = poll_fn(|context| match shutdown.as_mut().poll(context) {
            Poll::Ready(()) => Poll::Ready(None),
            Poll::Pending => listener.poll_accept(context).map(Some),
        })
        .await;
        match accepted {
            None => break,
            Some(Ok((stream, _))) => {
                spawn_connection(stream, &app, read_ask, &definitions, &graceful);
            }
            Some(Err(_)) => tokio::time::sleep(ACCEPT_BACKOFF).await,
        }
    }
    drop(listener);
    graceful.shutdown().await;
    Ok(())
}

/// Serves `app` on `listener` on a tokio runtime of its own until SIGTERM
/// or SIGINT, then finishes the requests in flight and returns. The
/// caller stays synchronous, since only this crate runs tokio (ADR-0002).
/// `stopping` runs when the signal arrives.
pub fn serve_until_signal(
    listener: net::TcpListener,
    app: App,
    read_ask: ReadAsk,
    definitions: Vec<Definition>,
    stopping: fn(),
) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    // Signals register with the runtime, so they need it entered.
    let (mut terminate, mut interrupt) = {
        let _entered = runtime.enter();
        (
            signal(SignalKind::terminate())?,
            signal(SignalKind::interrupt())?,
        )
    };
    let shutdown = async move {
        either_signal(&mut terminate, &mut interrupt).await;
        stopping();
    };
    runtime.block_on(serve(listener, app, read_ask, definitions, shutdown))
}

async fn either_signal(terminate: &mut Signal, interrupt: &mut Signal) {
    poll_fn(|context| {
        // Poll both, so each wakes this task when its signal comes.
        let terminated = terminate.poll_recv(context).is_ready();
        let interrupted = interrupt.poll_recv(context).is_ready();
        if terminated || interrupted {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

fn spawn_connection(
    stream: TcpStream,
    app: &Arc<App>,
    read_ask: ReadAsk,
    definitions: &Arc<[Definition]>,
    graceful: &GracefulShutdown,
) {
    // Nagle would hold an answer's second write until the client acks
    // the first, which a client that delays acks makes 40 ms.
    stream.set_nodelay(true).ok();
    let app = Arc::clone(app);
    let definitions = Arc::clone(definitions);
    // hyper takes the handler as a closure; each request gets its own
    // handle on the app and the definitions.
    let service = service_fn(move |request| {
        routes::handle(
            request,
            Arc::clone(&app),
            read_ask,
            Arc::clone(&definitions),
        )
    });
    let connection = http1::Builder::new()
        .timer(TokioTimer::new())
        .serve_connection(TokioIo::new(stream), service);
    let connection = graceful.watch(connection);
    // A client that hangs up mid-request is its own problem.
    tokio::spawn(async move { connection.await.ok() });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_error_names_its_line_when_known() {
        let at_line = QueryError {
            line: Some(2),
            message: "nope".to_owned(),
        };
        assert_eq!(at_line.to_string(), "line 2: nope");
        let anywhere = QueryError {
            line: None,
            message: "nope at column 3".to_owned(),
        };
        assert_eq!(anywhere.to_string(), "nope at column 3");
    }
}
