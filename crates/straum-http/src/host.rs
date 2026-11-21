//! The `Host` a request may name. `Hosts::of` reads it from the names
//! Straum was told to answer, else from the address it listens on, and
//! `routes.rs` asks `Hosts::refusal` before any route. When told names, only
//! those pass. On loopback only the names of this machine with Straum's
//! port pass: a page whose name an attacker rebinds to 127.0.0.1 makes
//! the browser send that name, and is refused (DNS rebinding). On any
//! other address every name passes: Straum is meant to be reached by
//! whatever name points at it. A port published on the host's loopback
//! is reached as loopback is, so there the names are told.

use std::net::SocketAddr;

/// The names of this machine a request on loopback may name.
const LOOPBACK_NAMES: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// The port a `Host` without one means.
const HTTP_PORT: u16 = 80;

/// Which `Host` a request may name, from the names told or where Straum
/// listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Hosts {
    /// Listening on another address than loopback: any name.
    Any,
    /// Listening on loopback at this address: the names of this machine
    /// with its port.
    Loopback(SocketAddr),
    /// Told which: these names, each `NAME[:PORT]` as a `Host` writes it.
    Named(Vec<String>),
}

impl Hosts {
    /// What a request to `address` may name: `named` when it holds any.
    pub(crate) fn of(address: SocketAddr, named: Vec<String>) -> Hosts {
        if !named.is_empty() {
            Hosts::Named(named)
        } else if address.ip().is_loopback() {
            Hosts::Loopback(address)
        } else {
            Hosts::Any
        }
    }

    /// What the reply to a request naming `host` says, if it is not
    /// answered.
    pub(crate) fn refusal(&self, host: Option<&[u8]>) -> Option<String> {
        let shown = String::from_utf8_lossy(host.unwrap_or_default());
        match self {
            Hosts::Any => None,
            Hosts::Loopback(address) => {
                if allows(*address, host) {
                    return None;
                }
                let answered = answered_names(*address);
                Some(format!(
                    "Straum on {address} answers Host {answered}, not {shown:?}\n"
                ))
            }
            Hosts::Named(names) => {
                if allows_named(names, host) {
                    return None;
                }
                let answered = listed(names);
                Some(format!("Straum answers Host {answered}, not {shown:?}\n"))
            }
        }
    }
}

/// Whether `text` is a name `Hosts::Named` can hold: a name, then a
/// port that is a number if it has one.
pub fn is_host(text: &str) -> bool {
    let (name, port) = name_and_port(text);
    !name.is_empty() && port.is_some()
}

/// Whether a request naming `host` is one of `names`, the port of each
/// 80 when it writes none. One with no `Host` is, as on loopback.
fn allows_named(names: &[String], host: Option<&[u8]>) -> bool {
    let Some(host) = host else {
        return true;
    };
    let Ok(host) = std::str::from_utf8(host) else {
        return false;
    };
    let (name, port) = name_and_port(host);
    for allowed in names {
        let (allowed_name, allowed_port) = name_and_port(allowed);
        if port.is_some() && port == allowed_port && name.eq_ignore_ascii_case(allowed_name) {
            return true;
        }
    }
    false
}

/// The names Straum on loopback at `address` answers, each with its
/// port, listed as "a, b or c": those of `LOOPBACK_NAMES`, then the
/// address it listens on when that is none of them.
fn answered_names(address: SocketAddr) -> String {
    let port = address.port();
    let mut names = Vec::new();
    for loopback in LOOPBACK_NAMES {
        names.push(format!("{loopback}:{port}"));
    }
    let listen = format!("{}:{port}", listen_name(address));
    if !names.contains(&listen) {
        names.push(listen);
    }
    listed(&names)
}

/// `names` as "a, b or c".
fn listed(names: &[String]) -> String {
    let mut answered = String::new();
    for (index, name) in names.iter().enumerate() {
        if index > 0 {
            let is_last = index + 1 == names.len();
            answered.push_str(if is_last { " or " } else { ", " });
        }
        answered.push_str(name);
    }
    answered
}

/// Whether Straum on loopback at `address` answers a request naming
/// `host`. One with no `Host` at all is: a browser always sends one, so
/// it is no rebound page.
fn allows(address: SocketAddr, host: Option<&[u8]>) -> bool {
    let Some(host) = host else {
        return true;
    };
    let Ok(host) = std::str::from_utf8(host) else {
        return false;
    };
    let (name, port) = name_and_port(host);
    if port != Some(address.port()) {
        return false;
    }
    if name.eq_ignore_ascii_case(&listen_name(address)) {
        return true;
    }
    for loopback in LOOPBACK_NAMES {
        if name.eq_ignore_ascii_case(loopback) {
            return true;
        }
    }
    false
}

/// The address Straum listens on as a `Host` names it, without the port:
/// an IPv6 one in brackets.
fn listen_name(address: SocketAddr) -> String {
    match address {
        SocketAddr::V4(v4) => v4.ip().to_string(),
        SocketAddr::V6(v6) => format!("[{}]", v6.ip()),
    }
}

/// A `Host` split into its name and port: 80 when it names none, `None`
/// when the port is no number. The port follows the last `:` outside an
/// IPv6 address's brackets.
fn name_and_port(host: &str) -> (&str, Option<u16>) {
    match host.rsplit_once(':') {
        Some((name, port)) if !port.contains(']') => (name, port.parse().ok()),
        _ => (host, Some(HTTP_PORT)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback(address: &str) -> Hosts {
        Hosts::of(address.parse().unwrap(), Vec::new())
    }

    fn named(address: &str, names: &[&str]) -> Hosts {
        let mut owned = Vec::new();
        for name in names {
            owned.push((*name).to_owned());
        }
        Hosts::of(address.parse().unwrap(), owned)
    }

    /// Whether `hosts` answers a request naming `host`.
    fn answers(hosts: &Hosts, host: Option<&[u8]>) -> bool {
        hosts.refusal(host).is_none()
    }

    #[test]
    fn named_hosts_answer_only_those_names_with_their_port() {
        for address in ["0.0.0.0:8080", "127.0.0.1:8080"] {
            let hosts = named(address, &["localhost:8080", "straum"]);
            for host in ["localhost:8080", "LocalHost:8080", "straum", "straum:80"] {
                assert!(answers(&hosts, Some(host.as_bytes())), "{host}");
            }
            assert!(answers(&hosts, None));
            for host in [
                "evil.example:8080",
                "127.0.0.1:8080",
                "localhost",
                "localhost:8081",
                "straum:8080",
                "localhost:x",
                "",
            ] {
                assert!(!answers(&hosts, Some(host.as_bytes())), "{host}");
            }
            assert!(!answers(&hosts, Some(b"localhost:\xff")));
        }
    }

    #[test]
    fn a_named_refusal_names_what_is_answered() {
        let hosts = named("0.0.0.0:8080", &["localhost:8080", "127.0.0.1:8080"]);
        assert_eq!(
            hosts.refusal(Some(b"evil.example:8080")).unwrap(),
            "Straum answers Host localhost:8080 or 127.0.0.1:8080, not \"evil.example:8080\"\n"
        );
        assert_eq!(
            Hosts::of("0.0.0.0:8080".parse().unwrap(), Vec::new()),
            Hosts::Any
        );
    }

    #[test]
    fn is_host_takes_a_name_and_a_number_port() {
        for text in ["localhost:8080", "straum", "[::1]:8080", "[::1]"] {
            assert!(is_host(text), "{text}");
        }
        for text in ["", ":8080", "localhost:", "localhost:x", "localhost:70000"] {
            assert!(!is_host(text), "{text}");
        }
    }

    #[test]
    fn only_loopback_checks_the_host() {
        assert_eq!(loopback("0.0.0.0:8080"), Hosts::Any);
        assert_eq!(loopback("10.0.0.7:8080"), Hosts::Any);
        assert_eq!(loopback("[::]:8080"), Hosts::Any);
        assert!(answers(&Hosts::Any, Some(b"evil.example")));
        assert!(matches!(loopback("127.0.0.1:8080"), Hosts::Loopback(_)));
        assert!(matches!(loopback("[::1]:8080"), Hosts::Loopback(_)));
    }

    #[test]
    fn loopback_answers_the_names_of_this_machine_with_its_port() {
        let hosts = loopback("127.0.0.5:8080");
        for host in [
            "localhost:8080",
            "LOCALHOST:8080",
            "127.0.0.1:8080",
            "[::1]:8080",
            "127.0.0.5:8080",
        ] {
            assert!(answers(&hosts, Some(host.as_bytes())), "{host}");
        }
        assert!(answers(&hosts, None));
        let on_80 = loopback("[::1]:80");
        assert!(answers(&on_80, Some(b"localhost")));
        assert!(answers(&on_80, Some(b"[::1]")));
    }

    #[test]
    fn loopback_refuses_any_other_name_or_port() {
        let hosts = loopback("127.0.0.1:8080");
        for host in [
            "evil.example:8080",
            "localhost.evil.example:8080",
            "127.0.0.2:8080",
            "localhost:8081",
            "localhost:",
            "localhost:x",
            "localhost",
            "[::1]",
            "::1",
            "",
        ] {
            assert!(!answers(&hosts, Some(host.as_bytes())), "{host}");
        }
        assert!(!answers(&hosts, Some(b"localhost:\xff")));
    }

    #[test]
    fn a_refusal_names_what_is_answered() {
        let hosts = loopback("127.0.0.1:8080");
        assert_eq!(
            hosts.refusal(Some(b"evil.example")).unwrap(),
            "Straum on 127.0.0.1:8080 answers Host localhost:8080, 127.0.0.1:8080 or \
             [::1]:8080, not \"evil.example\"\n"
        );
        assert_eq!(hosts.refusal(Some(b"localhost:8080")), None);
        assert_eq!(Hosts::Any.refusal(Some(b"evil.example")), None);
    }

    #[test]
    fn a_refusal_names_the_listen_address_once() {
        let hosts = loopback("127.0.0.5:8080");
        assert_eq!(
            hosts.refusal(Some(b"evil.example:8080")).unwrap(),
            "Straum on 127.0.0.5:8080 answers Host localhost:8080, 127.0.0.1:8080, \
             [::1]:8080 or 127.0.0.5:8080, not \"evil.example:8080\"\n"
        );
        assert!(answers(&hosts, Some(b"127.0.0.5:8080")));
        assert_eq!(
            loopback("[::1]:8080")
                .refusal(Some(b"evil.example:8080"))
                .unwrap(),
            "Straum on [::1]:8080 answers Host localhost:8080, 127.0.0.1:8080 or \
             [::1]:8080, not \"evil.example:8080\"\n"
        );
    }
}
