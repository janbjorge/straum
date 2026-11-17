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
use std::net::TcpStream;
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
}

impl ClientError {
    /// Straum answered 503: it is loading or too far behind, and may
    /// answer if asked again later.
    pub(crate) fn is_not_ready(&self) -> bool {
        match self {
            ClientError::Refused { status_line, .. } => status_line.starts_with("HTTP/1.1 503 "),
            _ => false,
        }
    }
}

impl Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ClientError::NotAViewName(name) => write!(formatter, "{name:?} is not a view's name"),
            ClientError::NotAnHttpUrl(url) => {
                write!(formatter, "--url takes http://HOST:PORT, not {url:?}")
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
    let reply = match exchange(address, &request_text(address, request)) {
        Ok(reply) => reply,
        Err(error) => {
            let url = url.to_owned();
            let wanted = request.wanted;
            return Err(ClientError::Unreachable { url, wanted, error });
        }
    };
    let round_trip = sent.elapsed();
    let Some((head, body)) = reply.split_once("\r\n\r\n") else {
        return Err(ClientError::NoReply {
            url: url.to_owned(),
        });
    };
    if head.starts_with("HTTP/1.1 200 ") {
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
fn exchange(address: &str, request: &str) -> io::Result<String> {
    let mut stream = TcpStream::connect(address)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.write_all(request.as_bytes())?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{
        GET_READYZ, GET_ROWS, GET_STATUS, GET_VIEWS, POST_ASK, POST_ASK_JSON, answering, args,
    };

    /// What the user reads: a reply's body, or an error's words.
    #[derive(Debug, PartialEq)]
    enum Said {
        Body(String),
        Error(String),
    }

    fn said(result: Result<String, ClientError>) -> Said {
        match result {
            Ok(text) => Said::Body(text),
            Err(error) => Said::Error(error.to_string()),
        }
    }

    /// The body of a reply that was timed, without its timing.
    fn text(result: Result<Timed, ClientError>) -> Result<String, ClientError> {
        Ok(result?.text)
    }

    fn phase_of(name: &str, took_us: u64) -> Phase {
        Phase {
            name: name.to_owned(),
            took: Duration::from_micros(took_us),
        }
    }

    #[test]
    fn asks_for_the_status() {
        let reply = "HTTP/1.1 200 OK\r\ncontent-length: 11\r\n\r\nready: yes\n";
        let url = answering(GET_STATUS, reply);
        assert_eq!(
            said(status(&url, Output::Text)),
            Said::Body("ready: yes\n".to_owned())
        );
        let url = answering(GET_STATUS, "HTTP/1.1 200 OK\r\n\r\n");
        assert_eq!(
            said(status(&format!("{url}/"), Output::Text)),
            Said::Body(String::new())
        );
    }

    #[test]
    fn asks_for_the_views() {
        let url = answering(GET_VIEWS, "HTTP/1.1 200 OK\r\n\r\nview trips\n");
        assert_eq!(
            said(views(&url, Output::Text)),
            Said::Body("view trips\n".to_owned())
        );
        let url = answering(
            GET_STATUS,
            "HTTP/1.1 503 Service Unavailable\r\n\r\nnot ready: loading\n",
        );
        let answered =
            format!("{url} answered \"HTTP/1.1 503 Service Unavailable\"\nnot ready: loading");
        assert_eq!(said(status(&url, Output::Text)), Said::Error(answered));
    }

    #[test]
    fn asks_for_a_views_rows() {
        let url = answering(GET_ROWS, "HTTP/1.1 200 OK\r\n\r\ntrips\n");
        let all = RowWindow::default();
        assert_eq!(
            said(text(rows(&url, "trips", all))),
            Said::Body("trips\n".to_owned())
        );
        for name in ["trips HTTP/1.1", "a/b", "tri\rps", "trips;", "", "Trips"] {
            let error = format!("{name:?} is not a view's name");
            assert_eq!(
                said(text(rows("http://127.0.0.1:1", name, all))),
                Said::Error(error)
            );
        }
        // Nothing listens on port 1.
        let error = rows("http://127.0.0.1:1", "trips", all).unwrap_err();
        let asking = "asking http://127.0.0.1:1 for its rows: ";
        let said = error.to_string();
        assert!(said.starts_with(asking), "{said}");
        assert!(
            said.ends_with("\ntry: --url URL, or set STRAUM_URL"),
            "{said}"
        );
    }

    #[test]
    fn asks_for_the_rows_a_window_picks() {
        let asked = [
            (Some(-20), None, "?start=-20"),
            (Some(5), Some(10), "?start=5&count=10"),
            (None, Some(0), "?count=0"),
        ];
        for (start, count, query) in asked {
            let request = GET_ROWS.replacen("trips", &format!("trips{query}"), 1);
            let url = answering(&request, "HTTP/1.1 200 OK\r\n\r\nsome\n");
            let window = RowWindow { start, count };
            assert_eq!(
                said(text(rows(&url, "trips", window))),
                Said::Body("some\n".to_owned()),
                "{query}"
            );
        }
    }

    #[test]
    fn loads_the_views_as_json() {
        let url = answering(
            "GET /views HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\naccept: application/json\r\n\r\n",
            "HTTP/1.1 200 OK\r\n\r\n{\"views\":[],\"dims\":[]}",
        );
        assert_eq!(load_views(&url), Some(Vec::new()));
        assert_eq!(load_views("http://127.0.0.1:1"), None);
    }

    #[test]
    fn says_whether_straum_is_ready() {
        let url = answering(GET_READYZ, "HTTP/1.1 200 OK\r\n\r\nready\n");
        assert!(is_ready(&url).unwrap());
        let loading = "HTTP/1.1 503 Service Unavailable\r\n\r\nnot ready: loading\n";
        let url = answering(GET_READYZ, loading);
        assert!(!is_ready(&url).unwrap());
        let url = answering(GET_READYZ, "HTTP/1.1 404 Not Found\r\n\r\n");
        let error = is_ready(&url).unwrap_err();
        assert!(!error.is_not_ready(), "{error}");
        let error = is_ready("http://127.0.0.1:1").unwrap_err();
        assert!(
            error.to_string().contains("for whether it is ready: "),
            "{error}"
        );
        // Only a 503 says not ready; an ask refused for itself is a 400.
        let url = answering(GET_VIEWS, loading);
        assert!(views(&url, Output::Text).unwrap_err().is_not_ready());
        assert!(!ClientError::NotAViewName("x".into()).is_not_ready());
    }

    #[test]
    fn json_asks_straum_for_json_and_passes_it_through() {
        let json_status = "GET /status HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\naccept: application/json\r\n\r\n";
        let url = answering(json_status, "HTTP/1.1 200 OK\r\n\r\n{\"ready\":true}\n");
        let body = Said::Body("{\"ready\":true}\n".to_owned());
        assert_eq!(said(status(&url, Output::Json)), body);
        let json_views = "GET /views HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\naccept: application/json\r\n\r\n";
        let url = answering(json_views, "HTTP/1.1 200 OK\r\n\r\n{\"views\":[]}");
        let body = Said::Body("{\"views\":[]}".to_owned());
        assert_eq!(said(views(&url, Output::Json)), body);
        let url = answering(POST_ASK_JSON, "HTTP/1.1 200 OK\r\n\r\n{\"rows\":[]}");
        let parameters = args("7 paid true");
        let body = Said::Body("{\"rows\":[]}".to_owned());
        let answered = ask(&url, "SELECT \"x\"", &parameters, Output::Json);
        assert_eq!(said(text(answered)), body);
    }

    #[test]
    fn sends_an_ask_and_prints_its_table() {
        let url = answering(POST_ASK, "HTTP/1.1 200 OK\r\n\r\ntrips\n-----\n    1\n");
        let parameters = args("7 paid true");
        let answered = ask(&url, "SELECT \"x\"", &parameters, Output::Text);
        assert_eq!(
            said(text(answered)),
            Said::Body("trips\n-----\n    1\n".to_owned())
        );
    }

    #[test]
    fn an_answer_comes_with_how_long_it_took() {
        let reply = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\
            Server-Timing: parse;dur=0.031, answer;dur=2.480, text;dur=0.401, total;dur=2.950\r\n\
            \r\n    1\n";
        let url = answering(POST_ASK, reply);
        let sent = Instant::now();
        let answered = ask(&url, "SELECT \"x\"", &args("7 paid true"), Output::Text).unwrap();
        let waited = sent.elapsed();
        assert_eq!(answered.text, "    1\n");
        let phases = [
            phase_of("parse", 31),
            phase_of("answer", 2480),
            phase_of("text", 401),
            phase_of("total", 2950),
        ];
        assert_eq!(answered.timing.phases, phases);
        let round_trip = answered.timing.round_trip;
        assert!(round_trip > Duration::ZERO && round_trip <= waited);
        // A view's rows are an ask the app builds, timed the same.
        let reply = "HTTP/1.1 200 OK\r\nserver-timing: total;dur=1.000\r\n\r\ntrips\n";
        let url = answering(GET_ROWS, reply);
        let answered = rows(&url, "trips", RowWindow::default()).unwrap();
        assert_eq!(answered.timing.phases, [phase_of("total", 1000)]);
    }

    #[test]
    fn a_reply_without_phases_is_timed_at_this_end_alone() {
        let url = answering(POST_ASK, "HTTP/1.1 200 OK\r\n\r\n    1\n");
        let answered = ask(&url, "SELECT \"x\"", &args("7 paid true"), Output::Text).unwrap();
        assert_eq!(answered.timing.phases, []);
        assert!(answered.timing.round_trip > Duration::ZERO);
        // Only Server-Timing is read, and only entries that say a time.
        let head = "HTTP/1.1 200 OK\r\nx-timing: parse;dur=1\r\n\
            server-timing: total, cache;desc=hit, parse;dur=x, answer;dur=-1, \
            text;dur=inf, json;dur=NaN, db;dur=1e400, miss;dur=, total;dur=2";
        assert_eq!(phases(head), [phase_of("total", 2000)]);
    }

    #[test]
    fn a_timing_is_written_as_a_line() {
        let timing = Timing {
            round_trip: Duration::from_micros(3412),
            phases: vec![
                phase_of("parse", 31),
                phase_of("answer", 2480),
                phase_of("total", 2950),
            ],
        };
        assert_eq!(
            timing.to_string(),
            "Time: 3.412 ms (in Straum: parse 0.031, answer 2.480, total 2.950)"
        );
        let timing = Timing {
            round_trip: Duration::from_millis(1500),
            phases: Vec::new(),
        };
        assert_eq!(timing.to_string(), "Time: 1500.000 ms");
    }

    #[test]
    fn an_ask_refused_says_why() {
        let url = answering(
            POST_ASK,
            "HTTP/1.1 400 Bad Request\r\n\r\nkind asked twice\ntrips asked twice\n",
        );
        let refused = format!(
            "{url} answered \"HTTP/1.1 400 Bad Request\"\nkind asked twice\ntrips asked twice"
        );
        let answered = ask(&url, "SELECT \"x\"", &args("7 paid true"), Output::Text);
        assert_eq!(said(text(answered)), Said::Error(refused));
        // Nothing listens on port 1.
        let error = ask("http://127.0.0.1:1", "SELECT", &args(""), Output::Text).unwrap_err();
        let asking = "asking http://127.0.0.1:1 for an answer: ";
        let said = error.to_string();
        assert!(said.starts_with(asking), "{said}");
        assert!(
            said.ends_with("\ntry: --url URL, or set STRAUM_URL"),
            "{said}"
        );
    }

    #[test]
    fn a_parameter_says_its_type() {
        let cases = [
            ("7", "7"),
            ("-7", "-7"),
            ("+7", "7"),
            ("true", "true"),
            ("false", "false"),
            ("paid", "\"paid\""),
            ("2026-01-01T00:00:00Z", "\"2026-01-01T00:00:00Z\""),
            ("1.5", "1.5"),
            ("-0.05", "-0.05"),
            // A decimal as JSON writes one, else a string.
            ("1.", "\"1.\""),
            (".5", "\".5\""),
            ("00.5", "\"00.5\""),
            ("+1.5", "\"+1.5\""),
            ("1.2.3", "\"1.2.3\""),
            ("1e5", "\"1e5\""),
            ("True", "\"True\""),
            ("", "\"\""),
            // Quotes make a string of an integer or a boolean.
            ("'7'", "\"7\""),
            ("'true'", "\"true\""),
            ("'1.5'", "\"1.5\""),
            ("''", "\"\""),
            ("'", "\"'\""),
            ("it's", "\"it's\""),
            ("a\"b\\c", "\"a\\\"b\\\\c\""),
            ("a\nb\u{1}", "\"a\\nb\\u0001\""),
            ("blåbær", "\"blåbær\""),
        ];
        for (text, json) in cases {
            assert_eq!(parameter(text), json, "{text}");
        }
    }

    #[test]
    fn a_quoted_parameter_word_holds_its_spaces() {
        let cases: [(&str, &[&str]); 7] = [
            (" 7  paid\ttrue ", &["7", "paid", "true"]),
            ("'Staten Island' 7", &["'Staten Island'", "7"]),
            ("'O'Brien  St' x", &["'O'Brien  St'", "x"]),
            ("'' 'blå bær'", &["''", "'blå bær'"]),
            ("'7' '", &["'7'", "'"]),
            ("it's 'a", &["it's", "'a"]),
            ("", &[]),
        ];
        for (text, words) in cases {
            assert_eq!(parameter_words(text), words, "{text}");
        }
        // The quoted word is one string, its quotes gone.
        assert_eq!(parameter("'Staten Island'"), "\"Staten Island\"");
    }

    #[test]
    fn a_quote_that_never_closes_is_a_word_like_any_other() {
        assert_eq!(parameter_words("'Staten Island"), ["'Staten", "Island"]);
        assert_eq!(parameter_words("'a'b c"), ["'a'b", "c"]);
        for open in ["'", "x 'Staten", "'Staten Is", "'a'b"] {
            assert!(has_open_quote(open), "{open}");
        }
        for closed in ["", "x", "it's", "'Staten Island'", "'' y", "'a' 'b'"] {
            assert!(!has_open_quote(closed), "{closed}");
        }
    }

    #[test]
    fn status_errors() {
        let url = answering(
            GET_STATUS,
            "HTTP/1.1 404 Not Found\r\n\r\nno /status here\n",
        );
        let answered = format!("{url} answered \"HTTP/1.1 404 Not Found\"\nno /status here");
        assert_eq!(said(status(&url, Output::Text)), Said::Error(answered));
        let url = answering(GET_STATUS, "HTTP/1.1 404 Not Found\r\n\r\n");
        let answered = format!("{url} answered \"HTTP/1.1 404 Not Found\"");
        assert_eq!(said(status(&url, Output::Text)), Said::Error(answered));
        let url = answering(GET_STATUS, "garbage");
        assert_eq!(
            said(status(&url, Output::Text)),
            Said::Error(format!("{url} sent no HTTP reply"))
        );
        for bad in [
            "https://x:1",
            "x:1",
            "http://",
            "http:///",
            "http://x:1/status",
        ] {
            let error = format!("--url takes http://HOST:PORT, not {bad:?}");
            assert_eq!(said(status(bad, Output::Text)), Said::Error(error), "{bad}");
        }
        // Nothing listens on port 1.
        let error = status("http://127.0.0.1:1", Output::Text).unwrap_err();
        let asking = "asking http://127.0.0.1:1 for its status: ";
        let said = error.to_string();
        assert!(said.starts_with(asking), "{said}");
        assert!(
            said.ends_with("\ntry: --url URL, or set STRAUM_URL"),
            "{said}"
        );
        // A server that answers, even with a refusal, needs no such hint.
        let url = answering(GET_STATUS, "HTTP/1.1 404 Not Found\r\n\r\n");
        assert!(
            !status(&url, Output::Text)
                .unwrap_err()
                .to_string()
                .contains("STRAUM_URL")
        );
    }
}
