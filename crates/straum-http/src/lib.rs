//! Straum over HTTP, on tokio and hyper. This file holds the server loop:
//! `serve` accepts connections until told to stop, hands each request to
//! `routes.rs`, and on shutdown finishes the requests in flight. The other
//! files each write one kind of reply: `answer.rs` an answer, `views.rs`
//! the views, `status.rs` the status. `ask.rs` reads an ask; `json.rs`
//! parses JSON for it and writes the strings and nulls of every reply;
//! `time.rs` writes times as RFC 3339.
//!
//! On loopback a request whose `Host` names another machine or port gets
//! 421 (see `host.rs`).
//!
//! - `POST /ask`: a query and its parameters as JSON (see `ask.rs`), the
//!   query read by the `AskSyntax` that `serve` is given; the answer as JSON
//!   (see `answer.rs`), with the LSN it reflects in `Straum-LSN` and where
//!   the time went in `Server-Timing`. 400 for an ask Straum can't answer
//!   or whose answer has more rows than `serve` is told it may, 503 while a
//!   table it reads isn't ready or while every core answers another ask. With `Accept: text/plain` the
//!   answer is a table and the errors are lines, for people (ADR-0011).
//! - `GET /livez`: 200 while the process serves.
//! - `GET /openapi.json`: the OpenAPI document of these routes, the
//!   crate's `openapi.json`; its JSON bodies are the contract.
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
mod host;
mod json;
mod routes;
mod status;
mod time;
mod views;

use std::fmt;
use std::future::{Future, poll_fn};
use std::io;
use std::net;
use std::num::NonZero;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;
use std::thread;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use straum_app::App;
use straum_domain::{Ask, AskError, Definition, Literal};

use crate::host::Hosts;
pub use crate::host::is_host;
use tokio::net::{TcpListener, TcpStream};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::Semaphore;

/// How long to wait before accepting again after an accept fails, as when
/// the process is out of file descriptors.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// How long a stop waits for the requests in flight: inside the 30 s a
/// Kubernetes pod gets before it is killed.
pub const DRAIN_DEADLINE: Duration = Duration::from_secs(10);

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

/// Says what is wrong with an ask that read but doesn't fit its view, in
/// the words of the syntax it was written in.
pub type DescribeAskError = fn(&AskError) -> String;

/// The language asks are written in, which the binary picks: how to read
/// one, and how to say what is wrong with one.
#[derive(Debug, Clone, Copy)]
pub struct AskSyntax {
    pub read: ReadAsk,
    pub describe: DescribeAskError,
}

/// What every request is answered from: the app, the syntax its asks are
/// read in, each view's definition, the hosts it answers to, the most
/// rows an ask may get, and the turns to read the store.
struct Served {
    app: App,
    syntax: AskSyntax,
    definitions: Vec<Definition>,
    hosts: Hosts,
    /// An answer is written whole in memory, so one ask over every key of
    /// a large view could take more than the process may hold; past this
    /// it is refused.
    max_answer_rows: usize,
    /// One per core: a request that reads the store holds one until it is
    /// done, so asks without end can't keep the writer from the store's
    /// lock, which a waiting writer would hold every new reader behind.
    turns: Arc<Semaphore>,
}

/// Serves `app` on `listener`, and each view's `definitions`, until
/// `shutdown` completes, then finishes the
/// requests in flight and returns. Runs on a tokio runtime with I/O and
/// time enabled. With `named_hosts`, each `NAME[:PORT]`, a request whose
/// `Host` is none of them gets 421; with none, the address decides. An
/// ask whose answer has more than `max_answer_rows` rows gets 400.
// `impl Future`: each caller passes an `async` block, whose type has no name.
pub async fn serve(
    listener: net::TcpListener,
    app: App,
    syntax: AskSyntax,
    definitions: Vec<Definition>,
    named_hosts: Vec<String>,
    max_answer_rows: usize,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    let hosts = Hosts::of(listener.local_addr()?, named_hosts);
    listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(listener)?;
    // A core count unknown is taken as one: asks queue, but none fails.
    let cores = thread::available_parallelism().map_or(1, NonZero::get);
    let served = Arc::new(Served {
        app,
        syntax,
        definitions,
        hosts,
        max_answer_rows,
        turns: Arc::new(Semaphore::new(cores)),
    });
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
                spawn_connection(stream, &served, &graceful);
            }
            Some(Err(_)) => tokio::time::sleep(ACCEPT_BACKOFF).await,
        }
    }
    drop(listener);
    // A client may hold a request open for as long as hyper lets it; a
    // stop waits for none past the deadline.
    tokio::time::timeout(DRAIN_DEADLINE, graceful.shutdown())
        .await
        .ok();
    Ok(())
}

/// Serves `app` on `listener` on a tokio runtime of its own until SIGTERM
/// or SIGINT, then finishes the requests in flight, for at most
/// [`DRAIN_DEADLINE`], and returns; a second signal returns at once. The
/// caller stays synchronous, since only this crate runs tokio (ADR-0002).
/// `stopping` runs when the first signal arrives; `named_hosts` and
/// `max_answer_rows` as for [`serve`].
pub fn serve_until_signal(
    listener: net::TcpListener,
    app: App,
    syntax: AskSyntax,
    definitions: Vec<Definition>,
    named_hosts: Vec<String>,
    max_answer_rows: usize,
    stopping: fn(),
) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    // Signals register with the runtime, so they need it entered. Each
    // pair hears every signal: the first pair stops the server, the
    // second counts two.
    let (mut terminate, mut interrupt, mut terminate_again, mut interrupt_again) = {
        let _entered = runtime.enter();
        (
            signal(SignalKind::terminate())?,
            signal(SignalKind::interrupt())?,
            signal(SignalKind::terminate())?,
            signal(SignalKind::interrupt())?,
        )
    };
    let shutdown = async move {
        either_signal(&mut terminate, &mut interrupt).await;
        stopping();
    };
    let second = async move {
        either_signal(&mut terminate_again, &mut interrupt_again).await;
        either_signal(&mut terminate_again, &mut interrupt_again).await;
    };
    let served = serve(
        listener,
        app,
        syntax,
        definitions,
        named_hosts,
        max_answer_rows,
        shutdown,
    );
    let result = runtime.block_on(until_done_or(served, second));
    // An answer still running on a blocking thread is not waited for.
    runtime.shutdown_background();
    result
}

/// `served`'s result, or `Ok` as soon as `cut_short` completes first.
// `impl Future`: both are `async` blocks, whose types have no name.
async fn until_done_or(
    served: impl Future<Output = io::Result<()>>,
    cut_short: impl Future<Output = ()>,
) -> io::Result<()> {
    let mut served = pin!(served);
    let mut cut_short = pin!(cut_short);
    poll_fn(|context| {
        if let Poll::Ready(result) = served.as_mut().poll(context) {
            return Poll::Ready(result);
        }
        cut_short.as_mut().poll(context).map(Ok)
    })
    .await
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

fn spawn_connection(stream: TcpStream, served: &Arc<Served>, graceful: &GracefulShutdown) {
    // Nagle would hold an answer's second write until the client acks
    // the first, which a client that delays acks makes 40 ms.
    stream.set_nodelay(true).ok();
    let served = Arc::clone(served);
    // hyper takes the handler as a closure; each request gets its own
    // handle on what is served.
    let service = service_fn(move |request| routes::handle(request, Arc::clone(&served)));
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
