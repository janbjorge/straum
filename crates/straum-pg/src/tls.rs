//! TLS to PostgreSQL: rustls on graviola, which is pure Rust. The server's
//! certificate must chain to a root straum trusts and name the host.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use bytes::BytesMut;
use postgres_protocol::message::frontend;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

use crate::error::Error;

pub(crate) type TlsStream = StreamOwned<ClientConnection, TcpStream>;

/// Asks PostgreSQL for TLS on `tcp` and shakes hands, so a certificate
/// straum doesn't trust fails the connect. The roots are those in the PEM
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
            return Err(Error::Tls(
                "PostgreSQL has ssl off; turn it on, \
                 or use sslmode=disable on a trusted network"
                    .into(),
            ));
        }
        other => {
            return Err(Error::Protocol(format!(
                "{other:#04x} as its answer to SSLRequest"
            )));
        }
    }
    let mut conn = ClientConnection::new(config, name.clone()).expect("the config is complete");
    while conn.is_handshaking() {
        conn.complete_io(&mut tcp)
            .map_err(|e| Error::Tls(e.to_string()))?;
    }
    Ok(StreamOwned::new(conn, tcp))
}

/// Built for every connection, so a CA file replaced on disk counts from
/// the next connect.
fn config(root_cert: Option<&str>) -> Result<Arc<ClientConfig>, Error> {
    let mut roots = RootCertStore::empty();
    match root_cert {
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        Some(file) => {
            let certs: Vec<CertificateDer> = CertificateDer::pem_file_iter(file)
                .and_then(Iterator::collect)
                .map_err(|e| Error::Tls(format!("reading {file}: {e}")))?;
            roots.add_parsable_certificates(certs);
            if roots.is_empty() {
                return Err(Error::Tls(format!("{file} holds no CA certificate")));
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
