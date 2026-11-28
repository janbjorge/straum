//! A listener that fails to accept. The test lowers the limit of file
//! descriptors of its whole process, so it has a test binary of its own.

use std::fs::File;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use nix::sys::resource::{Resource, getrlimit, rlim_t, setrlimit};
use straum_app::{App, Limits};
use straum_http::{AskSyntax, Listeners, ServeLimits};

const SPEC: &str = "CREATE VIEW events AS
SELECT buckets(e.at, '1 hour') AS bucket, e.kind AS kind, count(*) AS n
FROM public.events AS e
GROUP BY 1, 2;";

static REPORTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn record(line: &str) {
    REPORTED.lock().unwrap().push(line.to_owned());
}

fn livez(client: &mut TcpStream) -> String {
    let address = client.peer_addr().unwrap();
    let request = format!("GET /livez HTTP/1.1\r\nhost: {address}\r\nconnection: close\r\n\r\n");
    client.write_all(request.as_bytes()).unwrap();
    let mut reply = String::new();
    client.read_to_string(&mut reply).unwrap();
    reply
}

/// While no file descriptor is free, each try to accept fails, ten a
/// second. The server said nothing, so a full table looked like a server
/// that does not answer. It says so once, and serves once one is free.
#[test]
fn a_failed_accept_is_reported_once() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address: SocketAddr = listener.local_addr().unwrap();
    let listeners = Listeners {
        main: listener,
        health: None,
        report: record,
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
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build();
        let served = straum_http::serve(
            listeners,
            app,
            syntax,
            Vec::new(),
            Vec::new(),
            serve_limits,
            std::future::pending(),
        );
        runtime.unwrap().block_on(served)
    });
    assert!(livez(&mut TcpStream::connect(address).unwrap()).starts_with("HTTP/1.1 200"));

    // The lowest free descriptor becomes the last one: a client takes it,
    // and the server has none to accept with. macOS drops a connection
    // whose accept fails, so three clients come in turn.
    let (soft, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
    let lowest_free = File::open("/dev/null").unwrap();
    let limit = rlim_t::try_from(lowest_free.as_raw_fd()).unwrap() + 1;
    setrlimit(Resource::RLIMIT_NOFILE, limit, hard).unwrap();
    drop(lowest_free);
    for _ in 0..3 {
        let waiting = TcpStream::connect(address).unwrap();
        thread::sleep(Duration::from_millis(250));
        drop(waiting);
    }
    setrlimit(Resource::RLIMIT_NOFILE, soft, hard).unwrap();
    assert!(livez(&mut TcpStream::connect(address).unwrap()).starts_with("HTTP/1.1 200"));

    let reported = REPORTED.lock().unwrap().clone();
    assert_eq!(reported.len(), 1, "{reported:?}");
    let failed = format!("accepting a connection on {address} failed: ");
    assert!(reported[0].starts_with(&failed), "{reported:?}");
}
