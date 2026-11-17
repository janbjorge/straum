//! Helpers the unit tests of several files share, compiled only for
//! tests. `answering` stands in for a running Straum: it takes one request
//! on a socket and sends a scripted reply, so `client.rs` and `shell.rs`
//! can be tested without a server. The request constants are the exact
//! bytes the client sends, with `ADDRESS` where the socket's goes.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

/// A command line split at its spaces, as the shell would.
pub(crate) fn args(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_owned).collect()
}

/// A server that sends `reply` to one request, if it is `request`; its URL.
pub(crate) fn answering(request: &str, reply: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let url = format!("http://{address}");
    let request = request.replace("ADDRESS", &address.to_string());
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut read = vec![0; request.len()];
        if stream.read_exact(&mut read).is_ok() && read == request.as_bytes() {
            stream.write_all(reply.as_bytes()).unwrap();
        }
    });
    url
}

/// `straum status`'s request.
pub(crate) const GET_STATUS: &str =
    "GET /status HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\naccept: text/plain\r\n\r\n";

/// What `straum status` asks after the status, to know its exit code.
pub(crate) const GET_READYZ: &str =
    "GET /readyz HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\naccept: text/plain\r\n\r\n";

/// `straum views`'s request, and the shell's `\d`.
pub(crate) const GET_VIEWS: &str =
    "GET /views HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\naccept: text/plain\r\n\r\n";

/// The shell's `\rows trips`.
pub(crate) const GET_ROWS: &str =
    "GET /views/trips HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\naccept: text/plain\r\n\r\n";

/// An ask of `SELECT "x"` with the parameters `7 paid true`.
pub(crate) const POST_ASK: &str = "POST /ask HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\n\
    accept: text/plain\r\ncontent-type: application/json\r\ncontent-length: 53\r\n\r\n\
    {\"query\":\"SELECT \\\"x\\\"\",\"parameters\":[7,\"paid\",true]}";

/// The shell's ask of `SELECT "x"` with `7 'Staten Island' true`.
pub(crate) const POST_ASK_SPACED: &str = "POST /ask HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\n\
    accept: text/plain\r\ncontent-type: application/json\r\ncontent-length: 62\r\n\r\n\
    {\"query\":\"SELECT \\\"x\\\"\",\"parameters\":[7,\"Staten Island\",true]}";

/// The same ask as `POST_ASK`, as `straum ask --json` sends it.
pub(crate) const POST_ASK_JSON: &str = "POST /ask HTTP/1.1\r\nhost: ADDRESS\r\nconnection: close\r\n\
    accept: application/json\r\ncontent-type: application/json\r\ncontent-length: 53\r\n\r\n\
    {\"query\":\"SELECT \\\"x\\\"\",\"parameters\":[7,\"paid\",true]}";
