//! The HTTP adapter over a real socket. A scripted source (`fake.rs`)
//! fills the app, `serve` runs `straum_http::serve` on a thread of its
//! own, and the tests send raw HTTP/1.1 and read the reply back. This file
//! holds the server and the request helpers; the tests are split by route:
//! `asks.rs`, `views.rs` and `health.rs`.

mod asks;
mod fake;
mod health;
mod views;

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::ops::Range;
use std::thread::{self, JoinHandle};

use jiff::{SignedDuration, Timestamp};
use straum_app::{App, Step, Writer};
use straum_domain::{Ask, Literal};
use straum_http::{QueryError, ReadAsk};
use straum_notation::SqlError;
use tokio::sync::oneshot;

use crate::fake::{Fake, SPEC, START};

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

fn serve(app: App) -> Server {
    serve_with(app, read_with_notation)
}

fn serve_with(app: App, read_ask: ReadAsk) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
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
            read_ask,
            definitions,
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

/// A request with an `Accept` header if one is given.
fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    accept: Option<&str>,
    body: &str,
) -> Reply {
    let accept = match accept {
        Some(accept) => format!("accept: {accept}\r\n"),
        None => String::new(),
    };
    let length = body.len();
    send(
        address,
        &format!(
            "{method} {path} HTTP/1.1\r\nhost: straum\r\nconnection: close\r\n{accept}content-length: {length}\r\n\r\n{body}"
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
    let Some(fields) = fields.strip_suffix('}') else {
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
