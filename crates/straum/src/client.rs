//! The other end of `straum-http`, for the commands that talk to a
//! running Straum: `straum status`, `views`, `ask` and the shell. Each
//! public function builds one `Request`, `fetch` writes it as HTTP/1.1 on
//! a plain socket and reads the reply until Straum closes the connection,
//! and the body comes back as text. A plain socket, since Straum's image
//! holds nothing else to ask with.

use std::fmt::{self, Display, Write as _};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::Value;
use straum_domain::Identifier;

use crate::complete::{View, read_views};

const TIMEOUT: Duration = Duration::from_secs(10);

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

impl Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ClientError::NotAViewName(name) => write!(formatter, "{name:?} is not a view's name"),
            ClientError::NotAnHttpUrl(url) => {
                write!(formatter, "--url takes http://HOST:PORT, not {url:?}")
            }
            ClientError::Unreachable { url, wanted, error } => {
                write!(formatter, "asking {url} for {}: {error}", wanted.as_str())
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
    Views,
    Rows,
    Answer,
}

impl Wanted {
    fn as_str(self) -> &'static str {
        match self {
            Wanted::Status => "its status",
            Wanted::Views => "its views",
            Wanted::Rows => "its rows",
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

/// The form of reply a request accepts.
#[derive(Debug, Clone, Copy)]
enum Accept {
    Text,
    Json,
}

impl Accept {
    fn as_str(self) -> &'static str {
        match self {
            Accept::Text => "text/plain",
            Accept::Json => "application/json",
        }
    }
}

/// One HTTP request to a running Straum, before it is written out.
struct Request {
    wanted: Wanted,
    method: Method,
    path: String,
    accept: Accept,
    /// A JSON body, sent with its length.
    body: Option<String>,
}

impl Request {
    /// A `GET` with no body.
    fn get(wanted: Wanted, path: String, accept: Accept) -> Request {
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
pub(crate) fn status(url: &str) -> Result<String, ClientError> {
    let request = Request::get(Wanted::Status, "/status".to_owned(), Accept::Text);
    fetch(url, &request)
}

/// The views a running Straum holds, as `GET /views` says them.
pub(crate) fn views(url: &str) -> Result<String, ClientError> {
    let request = Request::get(Wanted::Views, "/views".to_owned(), Accept::Text);
    fetch(url, &request)
}

/// A view's rows, as `GET /views/NAME` writes them.
pub(crate) fn rows(url: &str, view: &str) -> Result<String, ClientError> {
    // The name goes into the request line as it is.
    if Identifier::new(view).is_none() {
        return Err(ClientError::NotAViewName(view.to_owned()));
    }
    let request = Request::get(Wanted::Rows, format!("/views/{view}"), Accept::Text);
    fetch(url, &request)
}

/// The views as JSON, for the shell to complete from; `None` when they
/// can't be had.
pub(crate) fn load_views(url: &str) -> Option<Vec<View>> {
    let request = Request::get(Wanted::Views, "/views".to_owned(), Accept::Json);
    let json = fetch(url, &request).ok()?;
    read_views(&json)
}

/// Sends an ask and returns its answer as a table.
pub(crate) fn ask(url: &str, query: &str, parameters: &[String]) -> Result<String, ClientError> {
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
        accept: Accept::Text,
        body: Some(body),
    };
    fetch(url, &request)
}

/// A parameter as JSON. A command line holds only text, so the text says
/// which type it is: an integer, a decimal such as `20.5`, `true` or
/// `false`, else a string. Quotes make a string of what would read as one
/// of the others.
fn parameter(text: &str) -> String {
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
    let Some(address) = address_of(url) else {
        return Err(ClientError::NotAnHttpUrl(url.to_owned()));
    };
    let reply = match exchange(address, &request_text(address, request)) {
        Ok(reply) => reply,
        Err(error) => {
            let url = url.to_owned();
            let wanted = request.wanted;
            return Err(ClientError::Unreachable { url, wanted, error });
        }
    };
    let Some((head, body)) = reply.split_once("\r\n\r\n") else {
        return Err(ClientError::NoReply {
            url: url.to_owned(),
        });
    };
    if head.starts_with("HTTP/1.1 200 ") {
        return Ok(body.to_owned());
    }
    Err(ClientError::Refused {
        url: url.to_owned(),
        status_line: head.lines().next().unwrap_or_default().to_owned(),
        body: body.to_owned(),
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
    use crate::testing::{GET_ROWS, GET_STATUS, GET_VIEWS, POST_ASK, answering, args};

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

    #[test]
    fn asks_for_the_status() {
        let reply = "HTTP/1.1 200 OK\r\ncontent-length: 11\r\n\r\nready: yes\n";
        let url = answering(GET_STATUS, reply);
        assert_eq!(said(status(&url)), Said::Body("ready: yes\n".to_owned()));
        let url = answering(GET_STATUS, "HTTP/1.1 200 OK\r\n\r\n");
        assert_eq!(said(status(&format!("{url}/"))), Said::Body(String::new()));
    }

    #[test]
    fn asks_for_the_views() {
        let url = answering(GET_VIEWS, "HTTP/1.1 200 OK\r\n\r\nview trips\n");
        assert_eq!(said(views(&url)), Said::Body("view trips\n".to_owned()));
        let url = answering(
            GET_STATUS,
            "HTTP/1.1 503 Service Unavailable\r\n\r\nnot ready: loading\n",
        );
        let answered =
            format!("{url} answered \"HTTP/1.1 503 Service Unavailable\"\nnot ready: loading");
        assert_eq!(said(status(&url)), Said::Error(answered));
    }

    #[test]
    fn asks_for_a_views_rows() {
        let url = answering(GET_ROWS, "HTTP/1.1 200 OK\r\n\r\ntrips\n");
        assert_eq!(said(rows(&url, "trips")), Said::Body("trips\n".to_owned()));
        for name in ["trips HTTP/1.1", "a/b", "tri\rps", "trips;", "", "Trips"] {
            let error = format!("{name:?} is not a view's name");
            assert_eq!(said(rows("http://127.0.0.1:1", name)), Said::Error(error));
        }
        // Nothing listens on port 1.
        let error = rows("http://127.0.0.1:1", "trips").unwrap_err();
        let asking = "asking http://127.0.0.1:1 for its rows: ";
        assert!(error.to_string().starts_with(asking), "{error}");
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
    fn sends_an_ask_and_prints_its_table() {
        let url = answering(POST_ASK, "HTTP/1.1 200 OK\r\n\r\ntrips\n-----\n    1\n");
        let parameters = args("7 paid true");
        assert_eq!(
            said(ask(&url, "SELECT \"x\"", &parameters)),
            Said::Body("trips\n-----\n    1\n".to_owned())
        );
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
        assert_eq!(
            said(ask(&url, "SELECT \"x\"", &args("7 paid true"))),
            Said::Error(refused)
        );
        // Nothing listens on port 1.
        let error = ask("http://127.0.0.1:1", "SELECT", &args("")).unwrap_err();
        let asking = "asking http://127.0.0.1:1 for an answer: ";
        assert!(error.to_string().starts_with(asking), "{error}");
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
    fn status_errors() {
        let url = answering(
            GET_STATUS,
            "HTTP/1.1 404 Not Found\r\n\r\nno /status here\n",
        );
        let answered = format!("{url} answered \"HTTP/1.1 404 Not Found\"\nno /status here");
        assert_eq!(said(status(&url)), Said::Error(answered));
        let url = answering(GET_STATUS, "HTTP/1.1 404 Not Found\r\n\r\n");
        let answered = format!("{url} answered \"HTTP/1.1 404 Not Found\"");
        assert_eq!(said(status(&url)), Said::Error(answered));
        let url = answering(GET_STATUS, "garbage");
        assert_eq!(
            said(status(&url)),
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
            assert_eq!(said(status(bad)), Said::Error(error), "{bad}");
        }
        // Nothing listens on port 1.
        let error = status("http://127.0.0.1:1").unwrap_err();
        let asking = "asking http://127.0.0.1:1 for its status: ";
        assert!(error.to_string().starts_with(asking), "{error}");
    }
}
