//! An accept that fails for a reason of Straum ends `serve`. The test
//! opens files until no file descriptor is free, so it has a test binary
//! of its own.

use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use straum_app::{App, Limits};
use straum_http::{AskSyntax, Listeners, ServeLimits};

const SPEC: &str = "CREATE VIEW events AS
SELECT buckets(e.at, '1 hour') AS bucket, e.kind AS kind, count(*) AS n
FROM public.events AS e
GROUP BY 1, 2;";

/// With no file descriptor free, each accept failed, and Straum tried
/// again ten times a second without a word: a full table looked like a
/// server that does not answer. Now `serve` returns the error.
#[test]
fn serve_returns_an_accept_that_fails_for_want_of_files() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let listeners = Listeners {
        main: listener,
        health: None,
    };
    let limits = Limits {
        max_lag: Duration::from_secs(30),
        max_retained_wal: 1 << 30,
        max_stream_memory: 1 << 30,
    };
    let app = App::new(straum_notation::parse(SPEC).unwrap(), limits);
    let syntax = AskSyntax {
        read: |_, _| Err(Vec::new()),
        describe: straum_notation::describe_ask_error,
    };
    let serve_limits = ServeLimits {
        max_answer_rows: 1,
        max_answer_bytes: 1 << 20,
        max_connections: straum_http::MAX_CONNECTIONS,
        connection_deadline: straum_http::CONNECTION_DEADLINE,
        connection_grace: straum_http::CONNECTION_GRACE,
    };
    let (done, served) = mpsc::channel();
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build();
        let serving = straum_http::serve(
            listeners,
            app,
            syntax,
            Vec::new(),
            Vec::new(),
            serve_limits,
            std::future::pending(),
        );
        done.send(runtime.unwrap().block_on(serving)).ok();
    });
    let mut client = TcpStream::connect(address).unwrap();
    let request = format!("GET /livez HTTP/1.1\r\nhost: {address}\r\nconnection: close\r\n\r\n");
    client.write_all(request.as_bytes()).unwrap();
    let mut reply = String::new();
    client.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");

    // Copies of one file take every descriptor but one: a copy costs a
    // slot of the table and no file of the kernel. The client takes the
    // last one, and the server has none to accept with.
    let null = File::open("/dev/null").unwrap();
    let mut files = Vec::new();
    while let Ok(copy) = null.try_clone() {
        files.push(copy);
    }
    files.pop();
    let waiting = TcpStream::connect(address);
    let result = served.recv_timeout(Duration::from_secs(10));
    let held = files.len();
    drop(files);
    drop(waiting);
    let Ok(Err(error)) = result else {
        panic!("serve went on after {held} files: {result:?}");
    };
    let failed = format!("accepting a connection on {address}: ");
    assert!(error.to_string().starts_with(&failed), "{error}");
}
