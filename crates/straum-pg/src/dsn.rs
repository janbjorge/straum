//! Where and as whom Straum connects: a `postgres://` URL, as libpq
//! (PostgreSQL's client library) takes it. `Dsn::parse` reads the URL once
//! at start; `wire.rs` connects with it and `tls.rs` checks the server's
//! certificate against what it says.

use std::fmt;

use rustls::pki_types::ServerName;

const DEFAULT_PORT: u16 = 5432;

/// A parsed connection URL. Its `Display` leaves out the password.
#[derive(Clone, PartialEq, Eq)]
pub struct Dsn {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) user: String,
    pub(crate) password: Option<String>,
    pub(crate) database: String,
    pub(crate) tls: Tls,
}

/// How Straum reaches PostgreSQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Tls {
    /// Plain TCP.
    Off,
    /// TLS, with a certificate that chains to a root Straum trusts and names
    /// the host, as `name`: the roots in the PEM file `root_cert`, else the
    /// roots built in.
    Verify {
        name: ServerName<'static>,
        root_cert: Option<String>,
    },
}

impl Dsn {
    /// The role to connect as.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// The password, if the URL has one.
    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    /// `postgres://user:password@host:port/database?sslmode=verify-full`.
    /// The port defaults to 5432 and the database to the user.
    ///
    /// `sslmode=disable` is plain TCP. Any other connection is TLS and
    /// checks the server's certificate and name: `require`, `verify-ca`,
    /// `verify-full` and no `sslmode` all mean that. `allow` and `prefer`,
    /// which may fall back to plain TCP, are refused. `sslrootcert=FILE`
    /// trusts the roots in FILE instead of the ones built in. With TLS the
    /// host must be a DNS name or IP address, which a certificate can name.
    pub fn parse(url: &str) -> Result<Dsn, DsnError> {
        let rest = match url.strip_prefix("postgres://") {
            Some(rest) => rest,
            None => match url.strip_prefix("postgresql://") {
                Some(rest) => rest,
                None => return Err(DsnError::Scheme),
            },
        };
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (authority, database) = rest.split_once('/').unwrap_or((rest, ""));
        // A password with a raw `?` or `/` before its `@` puts the `@` in
        // the parameters or the database: refused, or the host after it
        // would be read from the password, and errors would quote it.
        if database.contains('@') || query.contains('@') {
            return Err(DsnError::StrayAt);
        }
        let (userinfo, hostport) = authority.rsplit_once('@').ok_or(DsnError::User)?;
        let (user, password) = match userinfo.split_once(':') {
            Some((user, password)) => (user, Some(decode(password)?)),
            None => (userinfo, None),
        };
        let user = decode(user)?;
        if user.is_empty() {
            return Err(DsnError::User);
        }
        let (host, port) = host_and_port(hostport)?;
        if host.is_empty() {
            return Err(DsnError::Host);
        }
        let mut plain = false;
        let mut root_cert = None;
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            match pair.split_once('=') {
                Some(("sslmode", "disable")) => plain = true,
                Some(("sslmode", "require" | "verify-ca" | "verify-full")) => plain = false,
                Some(("sslmode", mode @ ("allow" | "prefer"))) => {
                    return Err(DsnError::Fallback(mode.to_owned()));
                }
                // libpq's word for the system's roots: Straum's are built in.
                Some(("sslrootcert", "system")) => root_cert = None,
                Some(("sslrootcert", file)) if !file.is_empty() => root_cert = Some(decode(file)?),
                _ => return Err(DsnError::Parameter(parameter_name(pair))),
            }
        }
        let tls = match (plain, root_cert) {
            (true, Some(_)) => return Err(DsnError::RootCert),
            (true, None) => Tls::Off,
            (false, root_cert) => match ServerName::try_from(host.to_owned()) {
                Ok(name) => Tls::Verify { name, root_cert },
                Err(_) => return Err(DsnError::TlsHost(host.to_owned())),
            },
        };
        let database = match decode(database)? {
            decoded if decoded.is_empty() => user.clone(),
            decoded => decoded,
        };
        Ok(Dsn {
            host: host.to_owned(),
            port,
            user,
            password,
            database,
            tls,
        })
    }
}

/// Percent-decoding, for user names and passwords with reserved characters.
fn decode(text: &str) -> Result<String, DsnError> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut rest = text.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%' {
            let hex = tail.get(..2).ok_or(DsnError::Escape)?;
            let hex = std::str::from_utf8(hex).map_err(|_| DsnError::Escape)?;
            bytes.push(u8::from_str_radix(hex, 16).map_err(|_| DsnError::Escape)?);
            rest = &tail[2..];
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    String::from_utf8(bytes).map_err(|_| DsnError::Escape)
}

/// Says what Straum does: `require` and `verify-ca` show as `verify-full`.
impl fmt::Display for Dsn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // An IPv6 address goes in brackets, so its colons stay its own.
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        write!(
            formatter,
            "postgres://{}@{host}:{}/{}",
            self.user, self.port, self.database
        )?;
        match &self.tls {
            Tls::Off => formatter.write_str("?sslmode=disable"),
            Tls::Verify {
                root_cert: None, ..
            } => formatter.write_str("?sslmode=verify-full"),
            Tls::Verify {
                root_cert: Some(file),
                ..
            } => write!(formatter, "?sslmode=verify-full&sslrootcert={file}"),
        }
    }
}

/// Never shows the password.
impl fmt::Debug for Dsn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Dsn({self})")
    }
}

/// Why a connection URL doesn't parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DsnError {
    Scheme,
    User,
    Host,
    Port,
    Escape,
    /// `sslmode=allow` or `prefer`, which may connect without TLS.
    Fallback(String),
    /// `sslrootcert` with `sslmode=disable`.
    RootCert,
    /// A host no certificate can name.
    TlsHost(String),
    /// A parameter not taken, or not with that value: by name when it is
    /// one libpq knows, never by its text, which may be a password's tail.
    Parameter(Option<&'static str>),
    /// An `@` past the host, as an unescaped `?` or `/` in the password
    /// puts it.
    StrayAt,
}

/// The host and port of `hostport`: `host`, `host:port`, or an IPv6
/// address in brackets, `[::1]` or `[::1]:5432`, whose colons are its
/// own.
fn host_and_port(hostport: &str) -> Result<(&str, u16), DsnError> {
    let (host, port) = match hostport.strip_prefix('[') {
        Some(bracketed) => {
            let (host, after) = bracketed.split_once(']').ok_or(DsnError::Host)?;
            match after.strip_prefix(':') {
                Some(port) => (host, Some(port)),
                None if after.is_empty() => (host, None),
                None => return Err(DsnError::Port),
            }
        }
        None => match hostport.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (hostport, None),
        },
    };
    let port = match port {
        Some(port) => port.parse().map_err(|_| DsnError::Port)?,
        None => DEFAULT_PORT,
    };
    Ok((host, port))
}

/// libpq's parameters a DSN may name, which an error may repeat.
const KNOWN_PARAMETERS: [&str; 16] = [
    "sslmode",
    "sslrootcert",
    "sslcert",
    "sslkey",
    "sslpassword",
    "sslnegotiation",
    "sslcrl",
    "connect_timeout",
    "application_name",
    "options",
    "target_session_attrs",
    "keepalives",
    "channel_binding",
    "gssencmode",
    "requiressl",
    "client_encoding",
];

/// The name of `pair`'s parameter, if it is one libpq knows.
fn parameter_name(pair: &str) -> Option<&'static str> {
    let name = match pair.split_once('=') {
        Some((name, _)) => name,
        None => pair,
    };
    for known in KNOWN_PARAMETERS {
        if known == name {
            return Some(known);
        }
    }
    None
}

impl fmt::Display for DsnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DsnError::Scheme => {
                formatter.write_str("DSN must start with postgres:// or postgresql://")
            }
            DsnError::User => formatter.write_str("DSN must name a user, as user@host"),
            DsnError::Host => formatter.write_str("DSN must name a host"),
            DsnError::Port => formatter.write_str("DSN port is not a port number"),
            DsnError::Escape => formatter.write_str("DSN has a bad percent escape"),
            DsnError::Fallback(mode) => write!(
                formatter,
                "sslmode={mode} may connect without TLS; use sslmode=verify-full, \
                 or sslmode=disable on a trusted network"
            ),
            DsnError::RootCert => formatter.write_str("DSN gives sslrootcert with sslmode=disable"),
            DsnError::TlsHost(host) => write!(
                formatter,
                "TLS needs a DNS name or IP address as the DSN host, not {host:?}"
            ),
            DsnError::Parameter(Some(name)) => write!(
                formatter,
                "DSN parameter {name} is not supported as given; Straum takes sslmode and \
                 sslrootcert"
            ),
            DsnError::Parameter(None) => formatter.write_str(
                "DSN has a parameter Straum doesn't know; it takes sslmode and sslrootcert",
            ),
            DsnError::StrayAt => formatter.write_str(
                "DSN has an @ past its host; percent-encode @, ? and / in the password as \
                 %40, %3F and %2F",
            ),
        }
    }
}

impl std::error::Error for DsnError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let dsn =
            Dsn::parse("postgres://straum:s%40cr%3At@db.local:6543/taxi?sslmode=disable").unwrap();
        assert_eq!(dsn.host, "db.local");
        assert_eq!(dsn.port, 6543);
        assert_eq!(dsn.user, "straum");
        assert_eq!(dsn.password.as_deref(), Some("s@cr:t"));
        assert_eq!((dsn.user(), dsn.password()), ("straum", Some("s@cr:t")));
        assert_eq!(dsn.database, "taxi");
        assert_eq!(dsn.tls, Tls::Off);
        assert_eq!(
            dsn.to_string(),
            "postgres://straum@db.local:6543/taxi?sslmode=disable"
        );
        assert_eq!(
            format!("{dsn:?}"),
            "Dsn(postgres://straum@db.local:6543/taxi?sslmode=disable)"
        );

        let dsn = Dsn::parse("postgresql://ann@localhost").unwrap();
        assert_eq!(
            dsn.to_string(),
            "postgres://ann@localhost:5432/ann?sslmode=verify-full"
        );
        assert_eq!(
            dsn.tls,
            Tls::Verify {
                name: ServerName::try_from("localhost").unwrap(),
                root_cert: None
            }
        );
        assert_eq!(
            (dsn.port, dsn.password, dsn.database),
            (5432, None, "ann".to_owned())
        );
    }

    /// An IPv6 address goes in brackets, with or without a port, and
    /// `sslrootcert=system` trusts the roots built in, as libpq's word
    /// for the system's.
    #[test]
    fn reads_ipv6_hosts_and_system_roots() {
        for (url, port) in [
            ("postgres://ann@[::1]:6543/taxi?sslmode=disable", 6543),
            ("postgres://ann@[::1]/taxi?sslmode=disable", 5432),
        ] {
            let dsn = Dsn::parse(url).unwrap();
            assert_eq!((dsn.host.as_str(), dsn.port), ("::1", port), "{url}");
            assert!(
                dsn.to_string().starts_with("postgres://ann@[::1]:"),
                "{dsn}"
            );
        }
        let verified = Dsn::parse("postgres://ann@[2001:db8::1]/taxi").unwrap();
        assert!(matches!(verified.tls, Tls::Verify { .. }));
        let system = Dsn::parse("postgres://ann@h/taxi?sslrootcert=system").unwrap();
        assert_eq!(system.tls, verify(None));
        for (url, error) in [
            ("postgres://ann@[::1/taxi", DsnError::Host),
            ("postgres://ann@[::1]x/taxi", DsnError::Port),
            ("postgres://ann@[::1]:x/taxi", DsnError::Port),
        ] {
            assert_eq!(Dsn::parse(url), Err(error), "{url}");
        }
    }

    /// TLS to host `h`, trusting the roots in `root_cert` if given.
    fn verify(root_cert: Option<&str>) -> Tls {
        Tls::Verify {
            name: ServerName::try_from("h").unwrap(),
            root_cert: root_cert.map(str::to_owned),
        }
    }

    #[test]
    fn every_mode_but_disable_verifies() {
        let cases = [
            ("", verify(None)),
            ("sslmode=require", verify(None)),
            ("sslmode=verify-ca", verify(None)),
            ("sslmode=verify-full", verify(None)),
            ("sslmode=disable", Tls::Off),
            ("sslmode=disable&sslmode=require", verify(None)),
            ("sslmode=require&sslmode=disable", Tls::Off),
            ("sslrootcert=/ca%20s/a.pem", verify(Some("/ca s/a.pem"))),
            (
                "sslmode=verify-ca&sslrootcert=ca.pem",
                verify(Some("ca.pem")),
            ),
        ];
        for (query, tls) in cases {
            let dsn = Dsn::parse(&format!("postgres://a@h/d?{query}")).unwrap();
            assert_eq!(dsn.tls, tls, "{query}");
        }
        let dsn = Dsn::parse("postgres://a@h/d?sslmode=require&sslrootcert=ca.pem").unwrap();
        assert_eq!(
            dsn.to_string(),
            "postgres://a@h:5432/d?sslmode=verify-full&sslrootcert=ca.pem"
        );
        assert_eq!(Dsn::parse(&dsn.to_string()), Ok(dsn));
        // Without TLS no certificate names the host.
        let dsn = Dsn::parse("postgres://a@-h/d?sslmode=disable").unwrap();
        assert_eq!((dsn.host.as_str(), dsn.tls), ("-h", Tls::Off));
    }

    #[test]
    fn rejects_urls() {
        let cases = [
            ("host=localhost user=straum", DsnError::Scheme),
            ("postgres://localhost/taxi", DsnError::User),
            ("postgres://:pw@localhost/taxi", DsnError::User),
            ("postgres://straum@/taxi", DsnError::Host),
            ("postgres://straum@h:99999/taxi", DsnError::Port),
            ("postgres://straum@h:x/taxi", DsnError::Port),
            ("postgres://we%2@h/taxi", DsnError::Escape),
            ("postgres://we%zz@h/taxi", DsnError::Escape),
            ("postgres://we%ff@h/taxi", DsnError::Escape),
            (
                "postgres://straum@h/taxi?sslmode=allow",
                DsnError::Fallback("allow".into()),
            ),
            (
                "postgres://straum@h/taxi?sslmode=prefer",
                DsnError::Fallback("prefer".into()),
            ),
            (
                "postgres://straum@h/taxi?sslmode=Disable",
                DsnError::Parameter(Some("sslmode")),
            ),
            (
                "postgres://straum@h/taxi?sslmode=disable&sslrootcert=ca.pem",
                DsnError::RootCert,
            ),
            (
                "postgres://straum@h/taxi?sslrootcert=",
                DsnError::Parameter(Some("sslrootcert")),
            ),
            ("postgres://straum@h/taxi?sslrootcert=%g", DsnError::Escape),
            ("postgres://straum@-h/taxi", DsnError::TlsHost("-h".into())),
            (
                "postgres://straum@a..b/taxi?sslmode=require",
                DsnError::TlsHost("a..b".into()),
            ),
            (
                "postgres://straum@h/taxi?sslcert=me.pem",
                DsnError::Parameter(Some("sslcert")),
            ),
            (
                "postgres://straum@h/taxi?connect_timeout=5",
                DsnError::Parameter(Some("connect_timeout")),
            ),
            (
                "postgres://straum@h/taxi?sslmode",
                DsnError::Parameter(Some("sslmode")),
            ),
            // A password's tail is never repeated, nor read as the host.
            (
                "postgres://straum@h/taxi?secret=1",
                DsnError::Parameter(None),
            ),
            ("postgres://u:pa?ss@h/taxi", DsnError::StrayAt),
            ("postgres://u:pa/ss@evil/taxi", DsnError::StrayAt),
        ];
        for (url, error) in cases {
            assert_eq!(Dsn::parse(url), Err(error), "{url}");
        }
    }

    #[test]
    fn error_display() {
        let cases = [
            (
                DsnError::Scheme,
                "DSN must start with postgres:// or postgresql://",
            ),
            (DsnError::User, "DSN must name a user, as user@host"),
            (DsnError::Host, "DSN must name a host"),
            (DsnError::Port, "DSN port is not a port number"),
            (DsnError::Escape, "DSN has a bad percent escape"),
            (
                DsnError::Fallback("prefer".into()),
                "sslmode=prefer may connect without TLS; use sslmode=verify-full, \
                 or sslmode=disable on a trusted network",
            ),
            (
                DsnError::RootCert,
                "DSN gives sslrootcert with sslmode=disable",
            ),
            (
                DsnError::TlsHost("-h".into()),
                "TLS needs a DNS name or IP address as the DSN host, not \"-h\"",
            ),
            (
                DsnError::Parameter(Some("sslcert")),
                "DSN parameter sslcert is not supported as given; Straum takes sslmode and \
                 sslrootcert",
            ),
            (
                DsnError::Parameter(None),
                "DSN has a parameter Straum doesn't know; it takes sslmode and sslrootcert",
            ),
            (
                DsnError::StrayAt,
                "DSN has an @ past its host; percent-encode @, ? and / in the password as \
                 %40, %3F and %2F",
            ),
        ];
        for (error, message) in cases {
            assert_eq!(error.to_string(), message);
        }
    }
}
