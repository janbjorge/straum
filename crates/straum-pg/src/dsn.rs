//! Where and as whom straum connects: a `postgres://` URL, as libpq takes it.

use std::fmt;

const DEFAULT_PORT: u16 = 5432;

/// A parsed connection URL. Its `Display` leaves out the password.
#[derive(Clone, PartialEq, Eq)]
pub struct Dsn {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) user: String,
    pub(crate) password: Option<String>,
    pub(crate) database: String,
}

impl Dsn {
    /// `postgres://user:password@host:port/database?sslmode=disable`. The
    /// port defaults to 5432 and the database to the user. straum has no TLS
    /// yet, so an `sslmode` that requires it is refused.
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
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            match pair.split_once('=') {
                Some(("sslmode", "disable" | "allow" | "prefer")) => {}
                Some(("sslmode", mode)) => return Err(DsnError::Tls(mode.to_owned())),
                _ => return Err(DsnError::Parameter(pair.to_owned())),
            }
        }
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

impl fmt::Display for Dsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "postgres://{}@{}:{}/{}",
            self.user, self.host, self.port, self.database
        )
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
    /// An `sslmode` that needs TLS.
    Tls(String),
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
            DsnError::Tls(mode) => write!(
                f,
                "sslmode={mode} needs TLS, which straum does not support yet; \
                 use sslmode=disable on a trusted network"
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
        assert_eq!(dsn.database, "taxi");
        assert_eq!(dsn.to_string(), "postgres://straum@db.local:6543/taxi");
        assert_eq!(
            format!("{dsn:?}"),
            "Dsn(postgres://straum@db.local:6543/taxi)"
        );

        let dsn = Dsn::parse("postgresql://ann@localhost").unwrap();
        assert_eq!(
            (dsn.port, dsn.password, dsn.database),
            (5432, None, "ann".to_owned())
        );
        for ok in ["sslmode=allow", "sslmode=prefer", ""] {
            assert!(
                Dsn::parse(&format!("postgres://a@h/d?{ok}")).is_ok(),
                "{ok}"
            );
        }
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
                "postgres://straum@h/taxi?sslmode=require",
                DsnError::Tls("require".into()),
            ),
            (
                "postgres://straum@h/taxi?sslmode=verify-full",
                DsnError::Tls("verify-full".into()),
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
                DsnError::Tls("require".into()),
                "sslmode=require needs TLS, which straum does not support yet; \
                 use sslmode=disable on a trusted network",
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
