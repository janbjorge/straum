//! The `Host` a request may name, end to end. On loopback Straum answers
//! only the names of this machine with its port, so a page whose name was
//! rebound to 127.0.0.1 can't read it; on any other address it answers
//! every name, unless told which names to answer.

use std::net::SocketAddr;

use crate::fake::writer;
use crate::{Reply, Server, notation, send, serve, serve_on};

/// `GET /livez` naming `host`, which `{port}` in it becomes the port.
fn livez(address: SocketAddr, host: &str) -> Reply {
    let host = host.replace("{port}", &address.port().to_string());
    send(
        address,
        &format!("GET /livez HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\n\r\n"),
    )
}

#[test]
fn on_loopback_the_names_of_this_machine_are_answered() {
    let server = serve(writer().app().clone());
    for host in [
        "127.0.0.1:{port}",
        "localhost:{port}",
        "LocalHost:{port}",
        "[::1]:{port}",
    ] {
        let reply = livez(server.address, host);
        assert_eq!(
            (reply.status, reply.body.as_str()),
            (200, "live\n"),
            "{host}"
        );
    }
    server.stop();
}

#[test]
fn on_loopback_any_other_name_is_misdirected() {
    let server = serve(writer().app().clone());
    for host in [
        "evil.example:{port}",
        "localhost.evil.example:{port}",
        "127.0.0.2:{port}",
        "localhost:1",
        "localhost",
        "[::1]",
        "straum",
        "",
    ] {
        let reply = livez(server.address, host);
        assert_eq!(reply.status, 421, "{host}: {}", reply.body);
        let port = server.address.port();
        let name = host.replace("{port}", &port.to_string());
        let body = format!(
            "Straum on {} answers Host localhost:{port}, 127.0.0.1:{port} or \
             [::1]:{port}, not {name:?}\n",
            server.address
        );
        assert_eq!(reply.body, body, "{host}");
    }
    server.stop();
}

#[test]
fn off_loopback_every_name_is_answered() {
    let server = serve_on(writer().app().clone(), notation(), "0.0.0.0:0", Vec::new());
    for host in ["evil.example:{port}", "straum", "10.0.0.7:8080"] {
        let reply = livez(server.address, host);
        assert_eq!(
            (reply.status, reply.body.as_str()),
            (200, "live\n"),
            "{host}"
        );
    }
    server.stop();
}

/// Straum on every address, told to answer `localhost:8080` and
/// `straum`, whatever port it listens on: behind a published port the
/// `Host` names the port published, not Straum's.
fn serve_named() -> Server {
    let named = vec!["localhost:8080".to_owned(), "straum".to_owned()];
    serve_on(writer().app().clone(), notation(), "0.0.0.0:0", named)
}

#[test]
fn told_names_are_answered_off_loopback() {
    let server = serve_named();
    for host in ["localhost:8080", "LOCALHOST:8080", "straum", "straum:80"] {
        let reply = livez(server.address, host);
        assert_eq!(
            (reply.status, reply.body.as_str()),
            (200, "live\n"),
            "{host}"
        );
    }
    server.stop();
}

#[test]
fn any_name_not_told_is_misdirected_off_loopback() {
    let server = serve_named();
    for host in [
        "evil.example:8080",
        "127.0.0.1:8080",
        "localhost:{port}",
        "localhost",
        "straum:8080",
    ] {
        let reply = livez(server.address, host);
        let name = host.replace("{port}", &server.address.port().to_string());
        let body = format!("Straum answers Host localhost:8080 or straum, not {name:?}\n");
        assert_eq!((reply.status, reply.body), (421, body), "{host}");
    }
    server.stop();
}
