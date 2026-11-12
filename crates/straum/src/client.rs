//! The other end of `straum-http`, for the commands that talk to a
//! running Straum: one request on a plain socket, since Straum's image
//! holds nothing else to ask with.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const STATUS_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn status(url: &str) -> Result<String, String> {
    let address = (url.strip_prefix("http://"))
        .map(|rest| rest.trim_end_matches('/'))
        .filter(|address| !address.is_empty() && !address.contains('/'))
        .ok_or_else(|| format!("--url takes http://HOST:PORT, not {url:?}"))?;
    let failed = |e: io::Error| format!("asking {url} for its status: {e}");
    let mut stream = TcpStream::connect(address).map_err(failed)?;
    stream
        .set_read_timeout(Some(STATUS_TIMEOUT))
        .map_err(failed)?;
    let request = format!("GET /status HTTP/1.1\r\nhost: {address}\r\nconnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).map_err(failed)?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).map_err(failed)?;
    match reply.split_once("\r\n\r\n") {
        Some((head, body)) if head.starts_with("HTTP/1.1 200 ") => Ok(body.to_owned()),
        Some((head, _)) => {
            let first = head.lines().next().unwrap_or_default();
            Err(format!("{url} answered {first:?}"))
        }
        None => Err(format!("{url} sent no HTTP reply")),
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    fn answering(reply: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                request.push(byte[0]);
            }
            if request.starts_with(b"GET /status HTTP/1.1\r\n") {
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        url
    }

    #[test]
    fn asks_for_the_status() {
        let url = answering("HTTP/1.1 200 OK\r\ncontent-length: 11\r\n\r\nready: yes\n");
        assert_eq!(status(&url), Ok("ready: yes\n".to_owned()));
        let url = answering("HTTP/1.1 200 OK\r\n\r\n");
        assert_eq!(status(&format!("{url}/")), Ok(String::new()));
    }

    #[test]
    fn status_errors() {
        let url = answering("HTTP/1.1 404 Not Found\r\n\r\nno /status here\n");
        let answered = format!("{url} answered \"HTTP/1.1 404 Not Found\"");
        assert_eq!(status(&url), Err(answered));
        let url = answering("garbage");
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
