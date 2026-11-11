//! Where and as whom Straum connects: a `postgres://` URL, as libpq takes it.

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
    pub fn user(&self) -> &str {
        &self.user
    }

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
        let rest = ["postgres://", "postgresql://"]
            .iter()
            .find_map(|scheme| url.strip_prefix(scheme))
            .ok_or(DsnError::Scheme)?;
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (authority, database) = rest.split_once('/').unwrap_or((rest, ""));
        let (userinfo, hostport) = authority.rsplit_once('@').ok_or(DsnError::User)?;
        let (user, password) = match userinfo.split_once(':') {
            Some((user, password)) => (user, Some(decode(password)?)),
            None => (userinfo, None),
        };
        let user = decode(user)?;
        if user.is_empty() {
            return Err(DsnError::User);
        }
        let (host, port) = match hostport.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().map_err(|_| DsnError::Port)?),
            None => (hostport, DEFAULT_PORT),
        };
        if host.is_empty() {
            return Err(DsnError::Host);
        }
        let mut plain = false;
        let mut root_cert = None;
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            match pair.split_once('=') {
                Some(("sslmode", "disable")) => plain = true,
                Some(("sslmode", "require" | "verify-ca" | "verify-full")) => plain = false,
                Some(("sslmode", mode @ ("allow" | "prefer"))) => {
                    return Err(DsnError::Fallback(mode.to_owned()));
                }
                Some(("sslrootcert", file)) if !file.is_empty() => root_cert = Some(decode(file)?),
                _ => return Err(DsnError::Parameter(pair.to_owned())),
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
            d if d.is_empty() => user.clone(),
            d => d,
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
fn decode(s: &str) -> Result<String, DsnError> {
    let mut bytes = Vec::with_capacity(s.len());
    let mut rest = s.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        if b == b'%' {
            let hex = tail.get(..2).ok_or(DsnError::Escape)?;
            let hex = std::str::from_utf8(hex).map_err(|_| DsnError::Escape)?;
            bytes.push(u8::from_str_radix(hex, 16).map_err(|_| DsnError::Escape)?);
            rest = &tail[2..];
        } else {
            bytes.push(b);
            rest = tail;
        }
    }
    String::from_utf8(bytes).map_err(|_| DsnError::Escape)
}

/// Says what Straum does: `require` and `verify-ca` show as `verify-full`.
impl fmt::Display for Dsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "postgres://{}@{}:{}/{}",
            self.user, self.host, self.port, self.database
        )?;
        match &self.tls {
            Tls::Off => f.write_str("?sslmode=disable"),
            Tls::Verify {
                root_cert: None, ..
            } => f.write_str("?sslmode=verify-full"),
            Tls::Verify {
                root_cert: Some(file),
                ..
            } => write!(f, "?sslmode=verify-full&sslrootcert={file}"),
        }
    }
}

/// Never shows the password.
impl fmt::Debug for Dsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Dsn({self})")
    }
}

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
    Parameter(String),
}

impl fmt::Display for DsnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DsnError::Scheme => f.write_str("DSN must start with postgres:// or postgresql://"),
            DsnError::User => f.write_str("DSN must name a user, as user@host"),
            DsnError::Host => f.write_str("DSN must name a host"),
            DsnError::Port => f.write_str("DSN port is not a port number"),
            DsnError::Escape => f.write_str("DSN has a bad percent escape"),
            DsnError::Fallback(mode) => write!(
                f,
                "sslmode={mode} may connect without TLS; use sslmode=verify-full, \
                 or sslmode=disable on a trusted network"
            ),
            DsnError::RootCert => f.write_str("DSN gives sslrootcert with sslmode=disable"),
            DsnError::TlsHost(host) => write!(
                f,
                "TLS needs a DNS name or IP address as the DSN host, not {host:?}"
            ),
            DsnError::Parameter(p) => write!(f, "DSN parameter {p} is not supported"),
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

    #[test]
    fn every_mode_but_disable_verifies() {
        let verify = |root_cert: Option<&str>| Tls::Verify {
            name: ServerName::try_from("h").unwrap(),
            root_cert: root_cert.map(str::to_owned),
        };
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
                DsnError::Parameter("sslmode=Disable".into()),
            ),
            (
                "postgres://straum@h/taxi?sslmode=disable&sslrootcert=ca.pem",
                DsnError::RootCert,
            ),
            (
                "postgres://straum@h/taxi?sslrootcert=",
                DsnError::Parameter("sslrootcert=".into()),
            ),
            ("postgres://straum@h/taxi?sslrootcert=%g", DsnError::Escape),
            ("postgres://straum@-h/taxi", DsnError::TlsHost("-h".into())),
            (
                "postgres://straum@a..b/taxi?sslmode=require",
                DsnError::TlsHost("a..b".into()),
            ),
            (
                "postgres://straum@h/taxi?sslcert=me.pem",
                DsnError::Parameter("sslcert=me.pem".into()),
            ),
            (
                "postgres://straum@h/taxi?connect_timeout=5",
                DsnError::Parameter("connect_timeout=5".into()),
            ),
            (
                "postgres://straum@h/taxi?sslmode",
                DsnError::Parameter("sslmode".into()),
            ),
        ];
        for (url, err) in cases {
            assert_eq!(Dsn::parse(url), Err(err), "{url}");
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
                DsnError::Parameter("x=1".into()),
                "DSN parameter x=1 is not supported",
            ),
        ];
        for (err, msg) in cases {
            assert_eq!(err.to_string(), msg);
        }
    }
}
