//! The HTTP adapter over a real socket. A scripted source (`fake.rs`)
//! fills the app, `serve` runs `straum_http::serve` on a thread of its
//! own, and the tests send raw HTTP/1.1 and read the reply back. This file
//! holds the server and the request helpers; the tests are split by route:
//! `asks.rs`, `views.rs` and `health.rs`, and the `Host` a request may
//! name in `hosts.rs`.

mod asks;
mod fake;
mod health;
mod hosts;
mod views;

use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::ops::Range;
use std::thread::{self, JoinHandle};

use jiff::{SignedDuration, Timestamp};
use straum_app::{App, Step, Writer};
use straum_domain::{Ask, Literal};
use straum_http::{AskSyntax, QueryError, ReadAsk};
use straum_notation::SqlError;
use tokio::sync::oneshot;

use crate::fake::{Fake, SPEC, START};

/// The most rows an ask may get, as `straum run` has it by default.
const MAX_ANSWER_ROWS: usize = 100_000;

/// A server on its own thread, and how to stop it.
struct Server {
    address: SocketAddr,
    stop: oneshot::Sender<()>,
    thread: JoinHandle<io::Result<()>>,
}

impl Server {
    fn stop(self) {
        self.stop.send(()).unwrap();
        self.thread.join().unwrap().unwrap();
    }
}

/// Reads asks as Straum SQL, as the binary does.
fn read_with_notation(query: &str, parameters: &[Literal]) -> Result<Ask, Vec<QueryError>> {
    let sql_errors = match straum_notation::parse_ask(query, parameters) {
        Ok(ask) => return Ok(ask),
        Err(sql_errors) => sql_errors,
    };
    let mut errors = Vec::new();
    for SqlError { line, message } in sql_errors {
        errors.push(QueryError { line, message });
    }
    Err(errors)
}

/// Straum SQL, as the binary picks it.
fn notation() -> AskSyntax {
    AskSyntax {
        read: read_with_notation,
        describe: straum_notation::describe_ask_error,
    }
}

fn serve(app: App) -> Server {
    serve_in(app, notation())
}

fn serve_with(app: App, read_ask: ReadAsk) -> Server {
    let syntax = AskSyntax {
        read: read_ask,
        ..notation()
    };
    serve_in(app, syntax)
}

fn serve_in(app: App, syntax: AskSyntax) -> Server {
    serve_on(app, syntax, "127.0.0.1:0", Vec::new())
}

/// Serves `app`, refusing an ask whose answer has more than
/// `max_answer_rows` rows.
fn serve_at_most(app: App, max_answer_rows: usize) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    serve_listening(app, notation(), listener, Vec::new(), max_answer_rows)
}

/// Serves `app` on `listen`, answering `named_hosts` if any, reached
/// through loopback whatever address it listens on.
fn serve_on(app: App, syntax: AskSyntax, listen: &str, named_hosts: Vec<String>) -> Server {
    let listener = TcpListener::bind(listen).unwrap();
    serve_listening(app, syntax, listener, named_hosts, MAX_ANSWER_ROWS)
}

fn serve_listening(
    app: App,
    syntax: AskSyntax,
    listener: TcpListener,
    named_hosts: Vec<String>,
    max_answer_rows: usize,
) -> Server {
    let port = listener.local_addr().unwrap().port();
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let (stop, stopped) = oneshot::channel::<()>();
    let thread = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let spec = straum_notation::parse(SPEC).unwrap();
        let definitions = straum_notation::definitions(SPEC, &spec);
        runtime.block_on(straum_http::serve(
            listener,
            app,
            syntax,
            definitions,
            named_hosts,
            max_answer_rows,
            async {
                stopped.await.ok();
            },
        ))
    });
    Server {
        address,
        stop,
        thread,
    }
}

/// Loads every table, then serves the app.
fn started(mut writer: Writer<Fake>) -> (Writer<Fake>, Server) {
    assert_eq!(writer.step().unwrap(), Step::Started(START));
    let server = serve(writer.app().clone());
    (writer, server)
}

/// A reply as read off the socket.
struct Reply {
    status: u16,
    head: String,
    body: String,
}

/// Sends `request` whole and reads the reply until the server closes.
fn send(address: SocketAddr, request: &str) -> Reply {
    let mut stream = TcpStream::connect(address).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    read_reply(&mut stream)
}

/// Where the code sits in a status line such as `HTTP/1.1 200 OK`.
const STATUS_CODE: Range<usize> = 9..12;

fn read_reply(stream: &mut TcpStream) -> Reply {
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    let (head, body) = reply.split_once("\r\n\r\n").unwrap();
    Reply {
        status: head[STATUS_CODE].parse().unwrap(),
        head: head.to_owned(),
        body: body.to_owned(),
    }
}

/// The media type a JSON body is sent as.
const JSON: &str = "application/json";

/// A request with an `Accept` header if one is given, and a POST's body
/// sent as JSON.
fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    accept: Option<&str>,
    body: &str,
) -> Reply {
    let content_type = if method == "POST" { Some(JSON) } else { None };
    let headers = Headers {
        accept,
        content_type,
    };
    request_with(address, method, path, &headers, body)
}

/// The optional headers of a request.
struct Headers<'a> {
    accept: Option<&'a str>,
    content_type: Option<&'a str>,
}

fn request_with(
    address: SocketAddr,
    method: &str,
    path: &str,
    headers: &Headers,
    body: &str,
) -> Reply {
    let mut lines = String::new();
    if let Some(accept) = headers.accept {
        write!(lines, "accept: {accept}\r\n").unwrap();
    }
    if let Some(content_type) = headers.content_type {
        write!(lines, "content-type: {content_type}\r\n").unwrap();
    }
    let length = body.len();
    send(
        address,
        &format!(
            "{method} {path} HTTP/1.1\r\nhost: {address}\r\nconnection: close\r\n{lines}content-length: {length}\r\n\r\n{body}"
        ),
    )
}

fn call(address: SocketAddr, method: &str, path: &str, body: &str) -> Reply {
    request(address, method, path, None, body)
}

/// An ask from a person, who reads plain text.
fn call_as_text(address: SocketAddr, body: &str) -> Reply {
    request(address, "POST", "/ask", Some("text/plain"), body)
}

/// An ask as straum-http reads it: a query and its parameters.
fn ask(query: &str, parameters: &str) -> String {
    format!(r#"{{"query": "{query}", "parameters": {parameters}}}"#)
}

/// Every trip in the first hour.
fn first_hour() -> String {
    ask(
        "SELECT trips FROM trips WHERE bucket >= $1 AND bucket < $2",
        "[0, 3600]",
    )
}

/// The value of the header `name`, written in lower case, if the reply
/// has it.
fn header(head: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}: ");
    for line in head.lines() {
        if let Some(value) = line.strip_prefix(&prefix) {
            return Some(value.to_owned());
        }
    }
    None
}

/// An answer's rows, once its `as_of` is checked to be a time from the last
/// minute.
fn rows_of(body: &str) -> &str {
    let Some(fields) = body.strip_prefix(r#"{"as_of":""#) else {
        panic!("not an answer: {body}");
    };
    let Some(fields) = fields.strip_suffix("}\n") else {
        panic!("not an answer: {body}");
    };
    let Some((as_of, rows)) = fields.split_once(r#"","rows":"#) else {
        panic!("not an answer: {body}");
    };
    let as_of: Timestamp = as_of.parse().unwrap();
    let age = Timestamp::now().duration_since(as_of);
    assert!(
        (SignedDuration::ZERO..SignedDuration::from_secs(60)).contains(&age),
        "{as_of}"
    );
    rows
}
