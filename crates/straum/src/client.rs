//! The other end of `straum-http`, for the commands that talk to a
//! running Straum: one request on a plain socket, since Straum's image
//! holds nothing else to ask with.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::complete::{View, read_views};

const TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn status(url: &str) -> Result<String, String> {
    fetch(url, "its status", "GET /status", "text/plain", None)
}

pub(crate) fn views(url: &str) -> Result<String, String> {
    fetch(url, "its views", "GET /views", "text/plain", None)
}

pub(crate) fn rows(url: &str, view: &str) -> Result<String, String> {
    // The name goes into the request line as it is.
    let named = |c: char| c.is_ascii_alphanumeric() || c == '_';
    if !view.chars().all(named) {
        return Err(format!("{view:?} is not a view's name"));
    }
    let request = format!("GET /views/{view}");
    fetch(url, "its rows", &request, "text/plain", None)
}

pub(crate) fn load_views(url: &str) -> Option<Vec<View>> {
    let json = fetch(url, "its views", "GET /views", "application/json", None).ok()?;
    read_views(&json)
}

pub(crate) fn ask(
    url: &str,
    query: &str,
    parameters: &[impl AsRef<str>],
) -> Result<String, String> {
    let parameters: Vec<String> = parameters
        .iter()
        .map(|text| parameter(text.as_ref()))
        .collect();
    let ask = format!(
        "{{\"query\":{},\"parameters\":[{}]}}",
        json_string(query),
        parameters.join(",")
    );
    fetch(url, "an answer", "POST /ask", "text/plain", Some(&ask))
}

/// A parameter as JSON. A command line holds only text, so the text says
/// which type it is: an integer, `true` or `false`, else a string. Quotes
/// make a string of what would read as one of the others.
fn parameter(text: &str) -> String {
    if let Ok(integer) = text.parse::<i64>() {
        return integer.to_string();
    }
    if text == "true" || text == "false" {
        return text.to_owned();
    }
    let quoted = (text.strip_prefix('\'')).and_then(|rest| rest.strip_suffix('\''));
    json_string(quoted.unwrap_or(text))
}

fn json_string(text: &str) -> String {
    let mut json = String::from('"');
    for character in text.chars() {
        match character {
            '"' | '\\' => json.extend(['\\', character]),
            '\0'..='\x1f' => json.push_str(&format!("\\u{:04x}", u32::from(character))),
            _ => json.push(character),
        }
    }
    json.push('"');
    json
}

/// The body of the reply to one request, in the form `accept` names.
fn fetch(
    url: &str,
    what: &str,
    request: &str,
    accept: &str,
    ask: Option<&str>,
) -> Result<String, String> {
    let address = (url.strip_prefix("http://"))
        .map(|rest| rest.trim_end_matches('/'))
        .filter(|address| !address.is_empty() && !address.contains('/'))
        .ok_or_else(|| format!("--url takes http://HOST:PORT, not {url:?}"))?;
    let failed = |e: io::Error| format!("asking {url} for {what}: {e}");
    let mut stream = TcpStream::connect(address).map_err(failed)?;
    stream.set_read_timeout(Some(TIMEOUT)).map_err(failed)?;
    let body_headers = ask.map_or_else(String::new, |ask| {
        let length = ask.len();
        format!("content-type: application/json\r\ncontent-length: {length}\r\n")
    });
    let body = ask.unwrap_or_default();
    let request = format!(
        "{request} HTTP/1.1\r\nhost: {address}\r\nconnection: close\r\naccept: {accept}\r\n\
         {body_headers}\r\n{body}"
    );
    stream.write_all(request.as_bytes()).map_err(failed)?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).map_err(failed)?;
    match reply.split_once("\r\n\r\n") {
        Some((head, body)) if head.starts_with("HTTP/1.1 200 ") => Ok(body.to_owned()),
        Some((head, body)) => {
            let first = head.lines().next().unwrap_or_default();
            // The body says why: the errors of an ask, or why not ready.
            Err(format!("{url} answered {first:?}\n{body}")
                .trim_end()
                .to_owned())
        }
        None => Err(format!("{url} sent no HTTP reply")),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    /// A server that sends `reply` to one request, if it is `request`.
    pub(crate) fn answering(request: &'static str, reply: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let url = format!("http://{address}");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = request.replace("ADDRESS", &address.to_string());
            let mut read = vec![0; request.len()];
            if stream.read_exact(&mut read).is_ok() && read == request.as_bytes() {
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        url
    }

    const GET_STATUS: &str =
        "GET /status HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\naccept: text/plain\r\n\r\n";

    #[test]
    fn asks_for_the_status() {
        let reply = "HTTP/1.1 200 OK\r\ncontent-length: 11\r\n\r\nready: yes\n";
        let url = answering(GET_STATUS, reply);
        assert_eq!(status(&url), Ok("ready: yes\n".to_owned()));
        let url = answering(GET_STATUS, "HTTP/1.1 200 OK\r\n\r\n");
        assert_eq!(status(&format!("{url}/")), Ok(String::new()));
    }

    #[test]
    fn asks_for_the_views() {
        let url = answering(
            "GET /views HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\naccept: text/plain\r\n\r\n",
            "HTTP/1.1 200 OK\r\n\r\nview trips\n",
        );
        assert_eq!(views(&url), Ok("view trips\n".to_owned()));
        let url = answering(
            GET_STATUS,
            "HTTP/1.1 503 Service Unavailable\r\n\r\nnot ready: loading\n",
        );
        let answered =
            format!("{url} answered \"HTTP/1.1 503 Service Unavailable\"\nnot ready: loading");
        assert_eq!(status(&url), Err(answered));
    }

    pub(crate) const GET_ROWS: &str = "GET /views/trips HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\naccept: text/plain\r\n\r\n";

    #[test]
    fn asks_for_a_views_rows() {
        let url = answering(GET_ROWS, "HTTP/1.1 200 OK\r\n\r\ntrips\n");
        assert_eq!(rows(&url, "trips"), Ok("trips\n".to_owned()));
        for name in ["trips HTTP/1.1", "a/b", "tri\rps", "trips;"] {
            let error = format!("{name:?} is not a view's name");
            assert_eq!(rows("http://127.0.0.1:1", name), Err(error));
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

    pub(crate) const POST_ASK: &str = "POST /ask HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\n\
        accept: text/plain\r\ncontent-type: application/json\r\ncontent-length: 53\r\n\r\n\
        {\"query\":\"SELECT \\\"x\\\"\",\"parameters\":[7,\"paid\",true]}";

    #[test]
    fn sends_an_ask_and_prints_its_table() {
        let url = answering(POST_ASK, "HTTP/1.1 200 OK\r\n\r\ntrips\n-----\n    1\n");
        let parameters = args("7 paid true");
        assert_eq!(
            ask(&url, "SELECT \"x\"", &parameters),
            Ok("trips\n-----\n    1\n".to_owned())
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
            ask(&url, "SELECT \"x\"", &args("7 paid true")),
            Err(refused)
        );
        // Nothing listens on port 1.
        let err = ask("http://127.0.0.1:1", "SELECT", &args("")).unwrap_err();
        let asking = "asking http://127.0.0.1:1 for an answer: ";
        assert!(err.starts_with(asking), "{err}");
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
            ("1.5", "\"1.5\""),
            ("True", "\"True\""),
            ("", "\"\""),
            // Quotes make a string of an integer or a boolean.
            ("'7'", "\"7\""),
            ("'true'", "\"true\""),
            ("''", "\"\""),
            ("'", "\"'\""),
            ("it's", "\"it's\""),
            ("a\"b\\c", "\"a\\\"b\\\\c\""),
            ("a\nb\u{1}", "\"a\\u000ab\\u0001\""),
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
        assert_eq!(status(&url), Err(answered));
        let url = answering(GET_STATUS, "HTTP/1.1 404 Not Found\r\n\r\n");
        let answered = format!("{url} answered \"HTTP/1.1 404 Not Found\"");
        assert_eq!(status(&url), Err(answered));
        let url = answering(GET_STATUS, "garbage");
        assert_eq!(status(&url), Err(format!("{url} sent no HTTP reply")));
        for bad in [
            "https://x:1",
            "x:1",
            "http://",
            "http:///",
            "http://x:1/status",
        ] {
            let error = format!("--url takes http://HOST:PORT, not {bad:?}");
            assert_eq!(status(bad), Err(error), "{bad}");
        }
        // Nothing listens on port 1.
        let err = status("http://127.0.0.1:1").unwrap_err();
        let asking = "asking http://127.0.0.1:1 for its status: ";
        assert!(err.starts_with(asking), "{err}");
    }
}
