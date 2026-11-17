//! The other end of `straum-http`, for the commands that talk to a
//! running Straum: `straum status`, `views`, `ask` and the shell. Each
//! public function builds one `Request`, `fetch` writes it as HTTP/1.1 on
//! a plain socket and reads the reply until Straum closes the connection,
//! and the body comes back as text. An ask and a view's rows come back
//! with a `Timing` too: how long the request took at this end, and the
//! phases the reply's `Server-Timing` header names, which the shell
//! prints. A plain socket, since Straum's image holds nothing else to ask
//! with.

use std::fmt::{self, Display, Write as _};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use serde_json::Value;
use straum_domain::Identifier;

use crate::complete::{View, read_views};

const TIMEOUT: Duration = Duration::from_secs(10);

/// The header in which Straum says where an answer's time went.
const SERVER_TIMING: &str = "server-timing";

/// What follows a phase's name in `Server-Timing`, before its milliseconds.
const PHASE_TOOK: &str = ";dur=";

const MILLISECONDS_PER_SECOND: f64 = 1e3;

/// A request to a running Straum that got no answer.
#[derive(Debug)]
pub(crate) enum ClientError {
    NotAViewName(String),
    NotAnHttpUrl(String),
    Unreachable {
        url: String,
        wanted: Wanted,
        error: io::Error,
    },
    Refused {
        url: String,
        status_line: String,
        body: String,
    },
    NoReply {
        url: String,
    },
    /// Connected, but no reply came within [`TIMEOUT`].
    Silent {
        url: String,
        wanted: Wanted,
    },
    /// A chunked reply, which Straum never sends: something stands
    /// between.
    Chunked {
        url: String,
    },
    /// A reply whose body ended before its `content-length`.
    CutShort {
        url: String,
        expected: usize,
        got: usize,
    },
}

impl ClientError {
    /// Straum answered 503: it is loading or too far behind, and may
    /// answer if asked again later.
    pub(crate) fn is_not_ready(&self) -> bool {
        match self {
            ClientError::Refused { status_line, .. } => status_code(status_line) == Some(503),
            _ => false,
        }
    }
}

impl Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ClientError::NotAViewName(name) => write!(formatter, "{name:?} is not a view's name"),
            ClientError::NotAnHttpUrl(url) => {
                write!(
                    formatter,
                    "--url or STRAUM_URL takes http://HOST:PORT, not {url:?}"
                )
            }
            ClientError::Unreachable { url, wanted, error } => {
                let wanted = wanted.as_str();
                write!(formatter, "asking {url} for {wanted}: {error}")?;
                formatter.write_str("\ntry: --url URL, or set STRAUM_URL")
            }
            ClientError::Refused {
                url,
                status_line,
                body,
            } => {
                // The body says why: the errors of an ask, or why not ready.
                let answered = format!("{url} answered {status_line:?}\n{body}");
                formatter.write_str(answered.trim_end())
            }
            ClientError::NoReply { url } => write!(formatter, "{url} sent no HTTP reply"),
            ClientError::Silent { url, wanted } => write!(
                formatter,
                "asking {url} for {}: no reply came within {} s",
                wanted.as_str(),
                TIMEOUT.as_secs()
            ),
            ClientError::Chunked { url } => write!(
                formatter,
                "{url} sent a chunked reply, which Straum never sends; give the URL Straum \
                 listens on, not a proxy's"
            ),
            ClientError::CutShort { url, expected, got } => write!(
                formatter,
                "{url} sent {got} of the {expected} bytes its reply said it would"
            ),
        }
    }
}

/// What a request asks a running Straum for, as an error names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wanted {
    Status,
    Readiness,
    Views,
    Rows,
    Spec,
    Answer,
}

impl Wanted {
    fn as_str(self) -> &'static str {
        match self {
            Wanted::Status => "its status",
            Wanted::Readiness => "whether it is ready",
            Wanted::Views => "its views",
            Wanted::Rows => "its rows",
            Wanted::Spec => "a view's spec",
            Wanted::Answer => "an answer",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Method {
    Get,
    Post,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

/// What a command prints: a table for people, or the JSON Straum
/// answers programs with. It is the form of reply the request accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Output {
    Text,
    Json,
}

impl Output {
    fn as_str(self) -> &'static str {
        match self {
            Output::Text => "text/plain",
            Output::Json => "application/json",
        }
    }
}

/// One HTTP request to a running Straum, before it is written out.
struct Request {
    wanted: Wanted,
    method: Method,
    path: String,
    accept: Output,
    /// A JSON body, sent with its length.
    body: Option<String>,
}

impl Request {
    /// A `GET` with no body.
    fn get(wanted: Wanted, path: String, accept: Output) -> Request {
        Request {
            wanted,
            method: Method::Get,
            path,
            accept,
            body: None,
        }
    }
}

/// What a running Straum has done, as `GET /status` says it.
pub(crate) fn status(url: &str, output: Output) -> Result<String, ClientError> {
    let request = Request::get(Wanted::Status, "/status".to_owned(), output);
    fetch(url, &request)
}

/// Whether a running Straum is ready, as `GET /readyz` says.
pub(crate) fn is_ready(url: &str) -> Result<bool, ClientError> {
    let request = Request::get(Wanted::Readiness, "/readyz".to_owned(), Output::Text);
    match fetch(url, &request) {
        Ok(_) => Ok(true),
        Err(error) if error.is_not_ready() => Ok(false),
        Err(error) => Err(error),
    }
}

/// A reply's body, and how long it took to get.
#[derive(Debug)]
pub(crate) struct Timed {
    pub(crate) text: String,
    pub(crate) timing: Timing,
}

/// How long a request took: all of it as this end measured, from
/// connecting until Straum closed the connection, and the phases of it
/// in Straum as the reply's `Server-Timing` header names them.
#[derive(Debug)]
pub(crate) struct Timing {
    pub(crate) round_trip: Duration,
    pub(crate) phases: Vec<Phase>,
}

/// A part of answering in Straum, and how long it took.
#[derive(Debug, PartialEq)]
pub(crate) struct Phase {
    pub(crate) name: String,
    pub(crate) took: Duration,
}

/// `Time: 3.412 ms (in Straum: parse 0.031, answer 2.480, total 2.950)`,
/// the first as psql writes it; the request alone for a reply that names
/// no phase.
impl Display for Timing {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "Time: {} ms", milliseconds(self.round_trip))?;
        let mut opening = " (in Straum: ";
        for phase in &self.phases {
            let took = milliseconds(phase.took);
            write!(formatter, "{opening}{} {took}", phase.name)?;
            opening = ", ";
        }
        if self.phases.is_empty() {
            return Ok(());
        }
        formatter.write_str(")")
    }
}

/// Milliseconds to three places, as `Server-Timing` writes them: an ask
/// takes microseconds.
fn milliseconds(took: Duration) -> String {
    format!("{:.3}", took.as_secs_f64() * MILLISECONDS_PER_SECOND)
}

/// The views a running Straum holds, as `GET /views` says them.
pub(crate) fn views(url: &str, output: Output) -> Result<String, ClientError> {
    let request = Request::get(Wanted::Views, "/views".to_owned(), output);
    fetch(url, &request)
}

/// Which of a view's rows to ask for: from `start`, a negative one
/// counted back from the end, and at most `count`. Straum picks either
/// one not given: the first row, and as many as it shows.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct RowWindow {
    pub(crate) start: Option<i64>,
    pub(crate) count: Option<u64>,
}

/// A view's rows, as `GET /views/NAME` writes them.
pub(crate) fn rows(url: &str, view: &str, window: RowWindow) -> Result<Timed, ClientError> {
    // The name goes into the request line as it is.
    if Identifier::new(view).is_none() {
        return Err(ClientError::NotAViewName(view.to_owned()));
    }
    let mut path = format!("/views/{view}");
    let mut separator = '?';
    if let Some(start) = window.start {
        let _ = write!(path, "{separator}start={start}");
        separator = '&';
    }
    if let Some(count) = window.count {
        let _ = write!(path, "{separator}count={count}");
    }
    let request = Request::get(Wanted::Rows, path, Output::Text);
    fetch_timed(url, &request)
}

/// A view's `CREATE VIEW` statement, as `GET /views/NAME/spec` writes it.
pub(crate) fn spec(url: &str, view: &str) -> Result<String, ClientError> {
    // The name goes into the request line as it is.
    if Identifier::new(view).is_none() {
        return Err(ClientError::NotAViewName(view.to_owned()));
    }
    let request = Request::get(Wanted::Spec, format!("/views/{view}/spec"), Output::Text);
    fetch(url, &request)
}

/// The views as JSON, for the shell to complete from; `None` when they
/// can't be had.
pub(crate) fn load_views(url: &str) -> Option<Vec<View>> {
    let request = Request::get(Wanted::Views, "/views".to_owned(), Output::Json);
    let json = fetch(url, &request).ok()?;
    read_views(&json)
}

/// Sends an ask and returns its answer, as a table or as JSON.
pub(crate) fn ask(
    url: &str,
    query: &str,
    parameters: &[String],
    output: Output,
) -> Result<Timed, ClientError> {
    let mut values = Vec::with_capacity(parameters.len());
    for text in parameters {
        values.push(parameter(text));
    }
    let body = format!(
        "{{\"query\":{},\"parameters\":[{}]}}",
        json_string(query),
        values.join(",")
    );
    let request = Request {
        wanted: Wanted::Answer,
        method: Method::Post,
        path: "/ask".to_owned(),
        accept: output,
        body: Some(body),
    };
    fetch_timed(url, &request)
}

/// A parameter as JSON. A command line holds only text, so the text says
/// which type it is: an integer, a decimal such as `20.5`, `true` or
/// `false`, else a string. Quotes make a string of what would read as one
/// of the others.
pub(crate) fn parameter(text: &str) -> String {
    if let Ok(integer) = text.parse::<i64>() {
        return integer.to_string();
    }
    if is_decimal(text) || text == "true" || text == "false" {
        return text.to_owned();
    }
    json_string(unquoted(text))
}

/// Digits, a point and digits, maybe after a minus, written as JSON
/// writes a number: the whole part `0` or without a leading zero.
fn is_decimal(text: &str) -> bool {
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    let Some((whole, fraction)) = unsigned.split_once('.') else {
        return false;
    };
    all_digits(whole) && all_digits(fraction) && (whole == "0" || !whole.starts_with('0'))
}

/// At least one character, every one an ASCII digit.
fn all_digits(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            return false;
        }
    }
    true
}

/// The parameters of a shell line, the text past its semicolon, a word
/// each. A word that opens with a single quote runs to the quote that
/// closes it, spaces and all: the next `'` with whitespace or the end
/// after it. One that never closes is read to the next whitespace, as any
/// other word is, so `'` and `it's` mean what they mean to `straum ask`.
pub(crate) fn parameter_words(text: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let length = match quoted_length(rest) {
            Some(length) => length,
            None => plain_length(rest),
        };
        words.push(&rest[..length]);
        rest = rest[length..].trim_start();
    }
    words
}

/// Whether `text` opens a quote it doesn't close: the user is still
/// typing a value. No later quote can close it, or it would be closed.
pub(crate) fn has_open_quote(text: &str) -> bool {
    for word in parameter_words(text) {
        if word.starts_with('\'') && quoted_length(word).is_none() {
            return true;
        }
    }
    false
}

/// The bytes of the quoted word `text` starts with, quotes included, if
/// it starts with one that closes.
fn quoted_length(text: &str) -> Option<usize> {
    let inside = text.strip_prefix('\'')?;
    for (at, character) in inside.char_indices() {
        if character != '\'' {
            continue;
        }
        let after = &inside[at + 1..];
        if after.is_empty() || after.starts_with(char::is_whitespace) {
            // The opening quote, what it holds and the closing quote.
            return Some(at + 2);
        }
    }
    None
}

/// The bytes of `text` before its first whitespace.
fn plain_length(text: &str) -> usize {
    match text.find(char::is_whitespace) {
        Some(at) => at,
        None => text.len(),
    }
}

/// `text` without the single quotes around it, if it has them.
fn unquoted(text: &str) -> &str {
    let Some(rest) = text.strip_prefix('\'') else {
        return text;
    };
    let Some(inside) = rest.strip_suffix('\'') else {
        return text;
    };
    inside
}

/// `text` as a JSON string, quoted and escaped.
fn json_string(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

/// The `HOST:PORT` of an `http://HOST:PORT` URL, which may end in `/`.
fn address_of(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("http://")?;
    let address = rest.trim_end_matches('/');
    if address.is_empty() || address.contains('/') {
        return None;
    }
    Some(address)
}

/// The body of the reply to one request.
fn fetch(url: &str, request: &Request) -> Result<String, ClientError> {
    Ok(fetch_timed(url, request)?.text)
}

/// The body of the reply to one request, and how long it took.
fn fetch_timed(url: &str, request: &Request) -> Result<Timed, ClientError> {
    let Some(address) = address_of(url) else {
        return Err(ClientError::NotAnHttpUrl(url.to_owned()));
    };
    let sent = Instant::now();
    let url_owned = url.to_owned();
    let wanted = request.wanted;
    let reply = match exchange(address, &request_text(address, request)) {
        Ok(reply) => reply,
        Err(Exchange::Failed(error)) => {
            return Err(ClientError::Unreachable {
                url: url_owned,
                wanted,
                error,
            });
        }
        Err(Exchange::Silent) => {
            return Err(ClientError::Silent {
                url: url_owned,
                wanted,
            });
        }
    };
    let round_trip = sent.elapsed();
    let Some((head, body)) = reply.split_once("\r\n\r\n") else {
        return Err(ClientError::NoReply {
            url: url.to_owned(),
        });
    };
    if header(head, "transfer-encoding").is_some_and(is_chunked) {
        return Err(ClientError::Chunked {
            url: url.to_owned(),
        });
    }
    // A connection that closed early ends the read as a whole reply
    // would: only the length the head gave tells.
    if let Some(expected) = content_length(head)
        && body.len() < expected
    {
        return Err(ClientError::CutShort {
            url: url.to_owned(),
            expected,
            got: body.len(),
        });
    }
    let status_line = head.lines().next().unwrap_or_default();
    if status_code(status_line) == Some(200) {
        let timing = Timing {
            round_trip,
            phases: phases(head),
        };
        return Ok(Timed {
            text: body.to_owned(),
            timing,
        });
    }
    Err(ClientError::Refused {
        url: url.to_owned(),
        status_line: head.lines().next().unwrap_or_default().to_owned(),
        body: body.to_owned(),
    })
}

/// The `content-length` a reply's head gives, if any.
fn content_length(head: &str) -> Option<usize> {
    header(head, "content-length")?.parse().ok()
}

/// The value of the header `name`, in any case, in a reply's head.
// 'a: the value lies inside `head`.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    for line in head.lines().skip(1) {
        let Some((written, value)) = line.split_once(':') else {
            continue;
        };
        if written.eq_ignore_ascii_case(name) {
            return Some(value.trim());
        }
    }
    None
}

/// Whether a `transfer-encoding` ends in chunks.
fn is_chunked(encoding: &str) -> bool {
    encoding.to_ascii_lowercase().contains("chunked")
}

/// The status code of `HTTP/1.x CODE REASON`, HTTP/1.0 or 1.1.
fn status_code(status_line: &str) -> Option<u16> {
    let rest = status_line.strip_prefix("HTTP/1.")?;
    let (_minor, rest) = rest.split_once(' ')?;
    let code = rest.split(' ').next()?;
    code.parse().ok()
}

/// The phases `Server-Timing` names in a reply's head, in the order
/// written: `parse;dur=0.031, answer;dur=2.480` and on, in milliseconds.
/// None for a reply without the header. An entry written another way is
/// left out, since its time can't be told.
fn phases(head: &str) -> Vec<Phase> {
    let mut phases = Vec::new();
    for line in head.lines() {
        let Some((name, entries)) = line.split_once(':') else {
            continue;
        };
        // A header's name is in any case; a proxy may write its own.
        if !name.eq_ignore_ascii_case(SERVER_TIMING) {
            continue;
        }
        for entry in entries.split(',') {
            if let Some(phase) = phase(entry.trim()) {
                phases.push(phase);
            }
        }
    }
    phases
}

/// One entry of `Server-Timing`, `name;dur=milliseconds`, if it is one.
fn phase(entry: &str) -> Option<Phase> {
    let (name, took_ms) = entry.split_once(PHASE_TOOK)?;
    let took_ms = took_ms.parse::<f64>().ok()?;
    // Refuses a time below zero and one that is no number.
    let took = Duration::try_from_secs_f64(took_ms / MILLISECONDS_PER_SECOND).ok()?;
    Some(Phase {
        name: name.to_owned(),
        took,
    })
}

/// The request as HTTP/1.1 text, sent to `address`.
fn request_text(address: &str, request: &Request) -> String {
    let method = request.method.as_str();
    let path = &request.path;
    let accept = request.accept.as_str();
    let mut text = format!(
        "{method} {path} HTTP/1.1\r\nhost: {address}\r\nconnection: close\r\naccept: {accept}\r\n"
    );
    if let Some(body) = &request.body {
        let length = body.len();
        let _ = write!(
            text,
            "content-type: application/json\r\ncontent-length: {length}\r\n\r\n{body}"
        );
    } else {
        text.push_str("\r\n");
    }
    text
}

/// Sends `request` to `address` and reads the reply until Straum closes
/// the connection.
fn exchange(address: &str, request: &str) -> Result<String, Exchange> {
    let mut stream = connect(address).map_err(Exchange::Failed)?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .map_err(Exchange::Failed)?;
    stream
        .write_all(request.as_bytes())
        .map_err(Exchange::Failed)?;
    let mut reply = String::new();
    match stream.read_to_string(&mut reply) {
        Ok(_) => Ok(reply),
        // The read timeout ends a read as either, by platform.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            Err(Exchange::Silent)
        }
        Err(error) => Err(Exchange::Failed(error)),
    }
}

/// Why an exchange got no reply.
enum Exchange {
    /// No connection, or it broke.
    Failed(io::Error),
    /// Connected, but nothing came within [`TIMEOUT`].
    Silent,
}

/// A connection to `address`, each of its addresses tried for at most
/// [`TIMEOUT`]: a host that drops packets would hold `connect` for
/// minutes.
fn connect(address: &str) -> io::Result<TcpStream> {
    let mut failure = io::Error::new(io::ErrorKind::NotFound, "no address");
    for socket_address in address.to_socket_addrs()? {
        match TcpStream::connect_timeout(&socket_address, TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => failure = error,
        }
    }
    Err(failure)
}

#[cfg(test)]
mod tests;
