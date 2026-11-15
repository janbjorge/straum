//! TLS to PostgreSQL: rustls on graviola, which is pure Rust. `wire.rs`
//! calls `start` on a new TCP socket: it sends PostgreSQL's `SSLRequest`,
//! then shakes hands. The server's certificate must chain to a root Straum
//! trusts and name the host.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use bytes::BytesMut;
use postgres_protocol::message::frontend;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

use crate::error::Error;
use crate::unexpected::Unexpected;

/// Why TLS to PostgreSQL didn't start.
#[derive(Debug)]
pub(crate) enum TlsProblem {
    SslOff,
    Handshake(std::io::Error),
    ReadingRoots {
        file: String,
        error: rustls::pki_types::pem::Error,
    },
    NoRoots(String),
}

impl std::fmt::Display for TlsProblem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsProblem::SslOff => formatter.write_str(
                "PostgreSQL has ssl off; turn it on, \
                 or use sslmode=disable on a trusted network",
            ),
            TlsProblem::Handshake(error) => write!(formatter, "{error}"),
            TlsProblem::ReadingRoots { file, error } => {
                write!(formatter, "reading {file}: {error}")
            }
            TlsProblem::NoRoots(file) => write!(formatter, "{file} holds no CA certificate"),
        }
    }
}

/// A TCP socket with TLS on it, which reads and writes plain bytes.
pub(crate) type TlsStream = StreamOwned<ClientConnection, TcpStream>;

/// Asks PostgreSQL for TLS on `tcp` and shakes hands, so a certificate
/// Straum doesn't trust fails the connect. The roots are those in the PEM
/// file `root_cert`, else Mozilla's, built in.
pub(crate) fn start(
    mut tcp: TcpStream,
    name: &ServerName<'static>,
    root_cert: Option<&str>,
) -> Result<TlsStream, Error> {
    let config = config(root_cert)?;
    let mut request = BytesMut::new();
    frontend::ssl_request(&mut request);
    tcp.write_all(&request)?;
    let mut answer = [0];
    tcp.read_exact(&mut answer)?;
    match answer[0] {
        b'S' => {}
        b'N' => {
            return Err(Error::Tls(TlsProblem::SslOff));
        }
        other => {
            return Err(Error::Protocol(Unexpected::SslAnswer(other)));
        }
    }
    let mut connection =
        ClientConnection::new(config, name.clone()).expect("the config is complete");
    while connection.is_handshaking() {
        if let Err(error) = connection.complete_io(&mut tcp) {
            return Err(Error::Tls(TlsProblem::Handshake(error)));
        }
    }
    Ok(StreamOwned::new(connection, tcp))
}

/// Built for every connection, so a CA file replaced on disk counts from
/// the next connect.
fn config(root_cert: Option<&str>) -> Result<Arc<ClientConfig>, Error> {
    let mut roots = RootCertStore::empty();
    match root_cert {
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        Some(file) => {
            let certs = match read_roots(file) {
                Ok(certs) => certs,
                Err(error) => {
                    let file = file.to_owned();
                    return Err(Error::Tls(TlsProblem::ReadingRoots { file, error }));
                }
            };
            roots.add_parsable_certificates(certs);
            if roots.is_empty() {
                return Err(Error::Tls(TlsProblem::NoRoots(file.to_owned())));
            }
        }
    }
    let provider = Arc::new(rustls_graviola::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("graviola does TLS 1.2 and 1.3")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// The certificates in the PEM file `file`.
fn read_roots(file: &str) -> Result<Vec<CertificateDer<'static>>, rustls::pki_types::pem::Error> {
    let mut certs = Vec::new();
    for cert in CertificateDer::pem_file_iter(file)? {
        certs.push(cert?);
    }
    Ok(certs)
}
