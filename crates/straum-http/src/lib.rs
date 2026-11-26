//! Straum over HTTP, on tokio and hyper. This file holds the server loop:
//! `serve` accepts connections until told to stop, at most
//! `max_connections` open at once and each closed after its deadline, hands
//! each request to `routes.rs`, and on shutdown finishes the requests in
//! flight. The other
//! files each write one kind of reply: `answer.rs` an answer, `views.rs`
//! the views, `status.rs` the status. `ask.rs` reads an ask; `json.rs`
//! parses JSON for it and writes the strings and nulls of every reply;
//! `time.rs` writes times as RFC 3339.
//!
//! On loopback a request whose `Host` names another machine or port gets
//! 421 (see `host.rs`).
//!
//! A health listener, if `serve` is given one, serves `/livez` and
//! `/readyz` alone, with [`MAX_HEALTH_CONNECTIONS`] of its own. Idle
//! connections that fill the main listener then cannot fail a health check.
//!
//! - `POST /ask`: a query and its parameters as JSON (see `ask.rs`), the
//!   query read by the `AskSyntax` that `serve` is given; the answer as JSON
//!   (see `answer.rs`), with the LSN it reflects in `Straum-LSN` and where
//!   the time went in `Server-Timing`. 400 for an ask Straum can't answer
//!   or whose answer has more rows than `serve` is told it may, 503 while a
//!   table it reads isn't ready or while every core answers another ask.
//!   It is 503 too while unread answers hold the bytes this answer needs.
//!   With `Accept: text/plain` the answer is a table and the errors are
//!   lines, for people (ADR-0011).
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

/// Straum's version: the release's, which CI builds with, else
/// Cargo.toml's, which never names one.
pub const VERSION: &str = match option_env!("STRAUM_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

use std::fmt;
use std::future::{Future, poll_fn};
use std::io;
use std::net;
use std::num::NonZero;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::Poll;
use std::thread;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use straum_app::App;
use straum_domain::{Ask, AskError, Definition, Literal};

use crate::host::Hosts;
pub use crate::host::is_host;
use tokio::net::{TcpListener, TcpStream};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

/// How long to wait before accepting again after an accept fails, as when
/// the process is out of file descriptors.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// How long a stop waits for the requests in flight: inside the 30 s a
/// Kubernetes pod gets before it is killed.
pub const DRAIN_DEADLINE: Duration = Duration::from_secs(10);
/// The connections `straum run` serves at once: each holds a file
/// descriptor, and its answer within `max_answer_bytes`.
pub const MAX_CONNECTIONS: usize = 1024;
/// How long `straum run` keeps a connection open: far past any answer a
/// client reads, so only a client that stalls is cut.
pub const CONNECTION_DEADLINE: Duration = Duration::from_secs(300);
/// How long `straum run` lets a connection past its deadline finish the
/// request in flight: far past any answer a client reads.
pub const CONNECTION_GRACE: Duration = Duration::from_secs(60);
/// How long a connection may take to send a request head, at its start
/// and after each reply. hyper's own 30 s let idle connections fill
/// [`MAX_CONNECTIONS`] and hold `/livez` past a health check's timeout.
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
/// The connections the health listener serves at once: a few probes, and
/// apart from [`MAX_CONNECTIONS`], so a flood of the main listener does
/// not reach them.
pub const MAX_HEALTH_CONNECTIONS: usize = 16;

/// Where `serve` listens: the main listener for every route, and, if
/// given, a health listener for `/livez` and `/readyz` alone.
#[derive(Debug)]
pub struct Listeners {
    pub main: net::TcpListener,
    pub health: Option<net::TcpListener>,
}

/// What one client may hold of the server, and for how long.
#[derive(Debug, Clone, Copy)]
pub struct ServeLimits {
    /// An answer is written whole in memory, so one ask over every key of
    /// a large view could take more than the process may hold; past this
    /// it is refused.
    pub max_answer_rows: usize,
    /// The bytes that all answers not yet sent may hold. A client that
    /// does not read keeps its answer in memory until its connection
    /// closes; an answer past this gets 503.
    pub max_answer_bytes: usize,
    /// The connections served at once. A connection past them waits in
    /// the listen backlog until one closes.
    pub max_connections: usize,
    /// How long a connection may stay open. hyper has no deadline on a
    /// reply's write, so a client that never reads would hold its answer
    /// in memory without end.
    pub connection_deadline: Duration,
    /// How long a connection past its deadline has to finish the request
    /// in flight before it is cut.
    pub connection_grace: Duration,
}

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
    /// See [`ServeLimits`].
    max_answer_rows: usize,
    /// One per core: a request that reads the store holds one until it is
    /// done, so asks without end can't keep the writer from the store's
    /// lock, which a waiting writer would hold every new reader behind.
    turns: Arc<Semaphore>,
    /// One per byte of [`ServeLimits::max_answer_bytes`]: an answer holds
    /// one per byte of its body until hyper sends the last byte.
    answer_bytes: Arc<Semaphore>,
    /// On the health listener: every route but `/livez` and `/readyz`
    /// gets 404.
    health_only: bool,
}

/// Serves `app` on `listeners`, and each view's `definitions`, until
/// `shutdown` completes, then finishes the
/// requests in flight and returns. Runs on a tokio runtime with I/O and
/// time enabled. With `named_hosts`, each `NAME[:PORT]`, a request whose
/// `Host` is none of them gets 421; with none, the address decides. On
/// the health listener its address always decides.
/// `limits` caps each answer's rows, the bytes of unread answers, the
/// connections served at once and how long each stays open.
// `impl Future`: each caller passes an `async` block, whose type has no name.
pub async fn serve(
    listeners: Listeners,
    app: App,
    syntax: AskSyntax,
    definitions: Vec<Definition>,
    named_hosts: Vec<String>,
    limits: ServeLimits,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    let Listeners { main, health } = listeners;
    let hosts = Hosts::of(main.local_addr()?, named_hosts);
    let main = to_tokio(main)?;
    // A core count unknown is taken as one: asks queue, but none fails.
    let cores = thread::available_parallelism().map_or(1, NonZero::get);
    let served = Served {
        app,
        syntax,
        definitions,
        hosts,
        max_answer_rows: limits.max_answer_rows,
        turns: Arc::new(Semaphore::new(cores)),
        answer_bytes: Arc::new(Semaphore::new(
            limits.max_answer_bytes.min(Semaphore::MAX_PERMITS),
        )),
        health_only: false,
    };
    let (stop, stopping) = watch::channel(false);
    let health_connections = Arc::new(Semaphore::new(MAX_HEALTH_CONNECTIONS));
    if let Some(health) = health {
        // Probes name the pod's address, not the names told for asks.
        let health_served = Served {
            app: served.app.clone(),
            definitions: Vec::new(),
            hosts: Hosts::of(health.local_addr()?, Vec::new()),
            turns: Arc::clone(&served.turns),
            answer_bytes: Arc::clone(&served.answer_bytes),
            health_only: true,
            ..served
        };
        let mut stopped = stopping.clone();
        let accepting = accept_until(
            to_tokio(health)?,
            Arc::clone(&health_connections),
            Arc::new(health_served),
            limits,
            stopping.clone(),
            async move {
                stopped.wait_for(is_true).await.ok();
            },
        );
        tokio::spawn(accepting);
    }
    let connections = Arc::new(Semaphore::new(limits.max_connections));
    let accepting = accept_until(
        main,
        Arc::clone(&connections),
        Arc::new(served),
        limits,
        stopping,
        shutdown,
    );
    accepting.await;
    stop.send_replace(true);
    // Each connection holds a permit until it closes, so all of them free
    // means none is open. A stop waits for none past the deadline.
    let every_connection_closed = async {
        let held = [
            (&connections, limits.max_connections),
            (&health_connections, MAX_HEALTH_CONNECTIONS),
        ];
        for (permits, count) in held {
            let count = u32::try_from(count).unwrap_or(u32::MAX);
            permits.acquire_many(count).await.ok();
        }
    };
    tokio::time::timeout(DRAIN_DEADLINE, every_connection_closed)
        .await
        .ok();
    Ok(())
}

/// Accepts connections on `listener` until `shutdown` completes, each
/// served from `served` while it holds one of `connections`' permits.
// `impl Future`: each caller passes an `async` block, whose type has no name.
async fn accept_until(
    listener: TcpListener,
    connections: Arc<Semaphore>,
    served: Arc<Served>,
    limits: ServeLimits,
    stopping: watch::Receiver<bool>,
    shutdown: impl Future<Output = ()>,
) {
    let mut shutdown = pin!(shutdown);
    loop {
        // Past the limit nothing is accepted: the listen backlog holds the
        // next connections until one closes.
        let free = Arc::clone(&connections).acquire_owned();
        let Some(Ok(permit)) = before_shutdown(shutdown.as_mut(), free).await else {
            return;
        };
        let Some(accepted) = before_shutdown(shutdown.as_mut(), listener.accept()).await else {
            return;
        };
        match accepted {
            Ok((stream, _)) => {
                let connection = Connection {
                    stream,
                    permit,
                    deadline: limits.connection_deadline,
                    grace: limits.connection_grace,
                    stopping: stopping.clone(),
                };
                spawn_connection(connection, &served);
            }
            Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
        }
    }
}

/// `listener` as tokio takes it. Needs the runtime entered.
fn to_tokio(listener: net::TcpListener) -> io::Result<TcpListener> {
    listener.set_nonblocking(true)?;
    TcpListener::from_std(listener)
}

/// Serves `app` on `listeners` on a tokio runtime of its own until SIGTERM
/// or SIGINT, then finishes the requests in flight, for at most
/// [`DRAIN_DEADLINE`], and returns; a second signal returns at once. The
/// caller stays synchronous, since only this crate runs tokio (ADR-0002).
/// `stopping` runs when the first signal arrives; `named_hosts` and
/// `limits` as for [`serve`].
pub fn serve_until_signal(
    listeners: Listeners,
    app: App,
    syntax: AskSyntax,
    definitions: Vec<Definition>,
    named_hosts: Vec<String>,
    limits: ServeLimits,
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
        listeners,
        app,
        syntax,
        definitions,
        named_hosts,
        limits,
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

/// What `wanted` gives, or `None` when `shutdown` completes first.
// Generic: the accept loop waits this way on a permit and on a connection.
async fn before_shutdown<T>(
    mut shutdown: Pin<&mut impl Future<Output = ()>>,
    wanted: impl Future<Output = T>,
) -> Option<T> {
    let mut wanted = pin!(wanted);
    poll_fn(|context| match shutdown.as_mut().poll(context) {
        Poll::Ready(()) => Poll::Ready(None),
        Poll::Pending => wanted.as_mut().poll(context).map(Some),
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

/// An accepted connection, the permit it holds while it is open, when it
/// stops taking requests and how long it then has, and whether the server
/// stops.
struct Connection {
    stream: TcpStream,
    permit: OwnedSemaphorePermit,
    deadline: Duration,
    grace: Duration,
    stopping: watch::Receiver<bool>,
}

/// Why a connection stopped serving.
enum Ended {
    /// The client closed it, or hyper did.
    Closed,
    /// Its deadline came, or the server stops: it finishes what is in
    /// flight and takes no more.
    Closing,
}

fn spawn_connection(connection: Connection, served: &Arc<Served>) {
    let Connection {
        stream,
        permit,
        deadline,
        grace,
        mut stopping,
    } = connection;
    // Nagle would hold an answer's second write until the client acks
    // the first, which a client that delays acks makes 40 ms.
    stream.set_nodelay(true).ok();
    let served = Arc::clone(served);
    // hyper takes the handler as a closure; each request gets its own
    // handle on what is served.
    let service = service_fn(move |request| routes::handle(request, Arc::clone(&served)));
    let connection = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_TIMEOUT)
        .serve_connection(TokioIo::new(stream), service);
    // A client that hangs up mid-request is its own problem. A client
    // that never reads its reply is cut at the deadline and its grace,
    // which drops the connection and frees its permit.
    tokio::spawn(async move {
        let _permit = permit;
        let mut connection = pin!(connection);
        let mut deadline = pin!(tokio::time::sleep(deadline));
        let mut stop = pin!(stopping.wait_for(is_true));
        let ended = poll_fn(|context| {
            if connection.as_mut().poll(context).is_ready() {
                return Poll::Ready(Ended::Closed);
            }
            let late = deadline.as_mut().poll(context).is_ready();
            if late || stop.as_mut().poll(context).is_ready() {
                return Poll::Ready(Ended::Closing);
            }
            Poll::Pending
        })
        .await;
        let Ended::Closing = ended else {
            return;
        };
        connection.as_mut().graceful_shutdown();
        tokio::time::timeout(grace, connection).await.ok();
    });
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "watch::Receiver::wait_for hands the value by reference"
)]
fn is_true(value: &bool) -> bool {
    *value
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
