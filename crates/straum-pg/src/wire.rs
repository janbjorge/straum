//! One connection to PostgreSQL over TCP or TLS: startup and SCRAM, simple
//! queries, and the copy-both stream of a replication connection. Messages
//! are read whole into a buffer, so a read that times out loses nothing;
//! rustls keeps a TLS record it has read part of.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::str;
use std::time::Duration;

use bytes::{Buf, BytesMut};
use fallible_iterator::FallibleIterator;
use postgres_protocol::authentication::sasl::{ChannelBinding, SCRAM_SHA_256, ScramSha256};
use postgres_protocol::message::backend::{ErrorResponseBody, Header, Message};
use postgres_protocol::message::frontend;

use crate::dsn::{Dsn, Tls};
use crate::error::Error;
use crate::tls::{self, TlsStream};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_CHUNK: usize = 64 * 1024;
/// CopyBothResponse, which `postgres_protocol` doesn't parse.
const COPY_BOTH: u8 = b'W';

pub(crate) struct Conn {
    socket: Socket,
    input: BytesMut,
    output: BytesMut,
}

enum Socket {
    Plain(TcpStream),
    Tls(Box<TlsStream>),
}

impl Socket {
    fn tcp(&self) -> &TcpStream {
        match self {
            Socket::Plain(tcp) => tcp,
            Socket::Tls(tls) => tls.get_ref(),
        }
    }
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Socket::Plain(tcp) => tcp.read(buf),
            Socket::Tls(tls) => tls.read(buf),
        }
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Socket::Plain(tcp) => tcp.write(buf),
            Socket::Tls(tls) => tls.write(buf),
        }
    }

    /// TLS learns of a failed write here.
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Socket::Plain(tcp) => tcp.flush(),
            Socket::Tls(tls) => tls.flush(),
        }
    }
}

impl Conn {
    /// Connects and authenticates. A replication connection takes
    /// replication commands as well as SQL. Every connection reads dates as
    /// ISO in UTC, which `value` expects.
    pub(crate) fn connect(dsn: &Dsn, replication: bool) -> Result<Conn, Error> {
        let mut failure = None;
        let tcp = (dsn.host.as_str(), dsn.port)
            .to_socket_addrs()?
            .find_map(|addr| {
                TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
                    .map_err(|e| failure = Some(e))
                    .ok()
            })
            .ok_or_else(|| {
                failure.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address"))
            })?;
        tcp.set_nodelay(true)?;
        let socket = match &dsn.tls {
            Tls::Off => Socket::Plain(tcp),
            Tls::Verify { name, root_cert } => {
                Socket::Tls(Box::new(tls::start(tcp, name, root_cert.as_deref())?))
            }
        };
        let mut conn = Conn {
            socket,
            input: BytesMut::new(),
            output: BytesMut::new(),
        };
        let mut params = vec![
            ("user", dsn.user.as_str()),
            ("database", dsn.database.as_str()),
            ("application_name", "straum"),
            ("client_encoding", "UTF8"),
            ("DateStyle", "ISO"),
            ("TimeZone", "UTC"),
        ];
        if replication {
            params.push(("replication", "database"));
        }
        frontend::startup_message(params, &mut conn.output)?;
        conn.flush()?;
        conn.authenticate(dsn)?;
        Ok(conn)
    }

    fn authenticate(&mut self, dsn: &Dsn) -> Result<(), Error> {
        let mut scram: Option<ScramSha256> = None;
        loop {
            match self.read()? {
                Message::AuthenticationOk
                | Message::ParameterStatus(_)
                | Message::BackendKeyData(_)
                | Message::NoticeResponse(_) => {}
                Message::AuthenticationSasl(body) => {
                    let offered: Vec<&str> = body.mechanisms().collect()?;
                    if !offered.contains(&SCRAM_SHA_256) {
                        return Err(Error::Protocol(format!(
                            "SASL mechanisms {offered:?}; straum needs {SCRAM_SHA_256}"
                        )));
                    }
                    let password = dsn
                        .password
                        .as_deref()
                        .ok_or_else(|| Error::protocol("a password request; the DSN has none"))?;
                    let state =
                        ScramSha256::new(password.as_bytes(), ChannelBinding::unsupported());
                    frontend::sasl_initial_response(
                        SCRAM_SHA_256,
                        state.message(),
                        &mut self.output,
                    )?;
                    self.flush()?;
                    scram = Some(state);
                }
                Message::AuthenticationSaslContinue(body) => {
                    let state = scram
                        .as_mut()
                        .ok_or_else(|| Error::protocol("SASL continue"))?;
                    state.update(body.data())?;
                    frontend::sasl_response(state.message(), &mut self.output)?;
                    self.flush()?;
                }
                Message::AuthenticationSaslFinal(body) => {
                    let state = scram
                        .as_mut()
                        .ok_or_else(|| Error::protocol("SASL final"))?;
                    state.finish(body.data())?;
                }
                Message::ReadyForQuery(_) => return Ok(()),
                Message::ErrorResponse(body) => return Err(server_error(&body)),
                _ => {
                    return Err(Error::Protocol(
                        "an authentication request straum doesn't support; \
                         straum signs in with SCRAM-SHA-256 or trust"
                            .into(),
                    ));
                }
            }
        }
    }

    /// Runs one simple query and returns every row of its result, owned,
    /// so it suits a small result. The rest of the result is read even
    /// after an error, so the connection stays usable.
    pub(crate) fn rows(&mut self, sql: &str) -> Result<Vec<Vec<Option<String>>>, Error> {
        frontend::query(sql, &mut self.output)?;
        self.flush()?;
        let mut rows = Vec::new();
        let mut error = None;
        loop {
            match self.read()? {
                Message::DataRow(row) => {
                    let buffer = row.buffer();
                    let fields = row
                        .ranges()
                        .map(|range| {
                            range
                                .map(|r| str::from_utf8(&buffer[r]).map(str::to_owned))
                                .transpose()
                                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
                        })
                        .collect()?;
                    rows.push(fields);
                }
                Message::RowDescription(_)
                | Message::CommandComplete(_)
                | Message::EmptyQueryResponse
                | Message::NoticeResponse(_)
                | Message::ParameterStatus(_) => {}
                Message::ErrorResponse(body) => {
                    error.get_or_insert_with(|| server_error(&body));
                }
                Message::ReadyForQuery(_) => return error.map_or(Ok(rows), Err),
                _ => return Err(Error::protocol("a message during a query")),
            }
        }
    }

    /// Runs a `COPY ... TO STDOUT`, calling `each` with every CopyData as
    /// it comes, so a load holds one message of the table at a time. After
    /// an error the connection is mid-copy; drop it.
    pub(crate) fn copy_out(
        &mut self,
        sql: &str,
        mut each: impl FnMut(&[u8]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        frontend::query(sql, &mut self.output)?;
        self.flush()?;
        loop {
            match self.read()? {
                Message::CopyData(body) => each(body.data())?,
                Message::CopyOutResponse(_)
                | Message::CopyDone
                | Message::CommandComplete(_)
                | Message::NoticeResponse(_)
                | Message::ParameterStatus(_) => {}
                Message::ErrorResponse(body) => return Err(server_error(&body)),
                Message::ReadyForQuery(_) => return Ok(()),
                _ => return Err(Error::protocol("a message during a copy")),
            }
        }
    }

    pub(crate) fn execute(&mut self, sql: &str) -> Result<(), Error> {
        self.rows(sql).map(drop)
    }

    /// Sends a command that answers with CopyBothResponse, such as
    /// `START_REPLICATION`, and waits for that answer.
    pub(crate) fn copy_both(&mut self, command: &str) -> Result<(), Error> {
        frontend::query(command, &mut self.output)?;
        self.flush()?;
        loop {
            if self.next_tag()? == COPY_BOTH {
                let header = Header::parse(&self.input)?.expect("a whole message is buffered");
                self.input.advance(frame_len(header));
                return Ok(());
            }
            match self.read()? {
                Message::NoticeResponse(_) | Message::ParameterStatus(_) => {}
                Message::ErrorResponse(body) => return Err(server_error(&body)),
                _ => return Err(Error::protocol("a message instead of CopyBothResponse")),
            }
        }
    }

    pub(crate) fn send_copy_data(&mut self, data: &[u8]) -> Result<(), Error> {
        frontend::CopyData::new(data)?.write(&mut self.output);
        self.flush()
    }

    /// The next message, or `None` if none came within the read timeout.
    pub(crate) fn try_read(&mut self) -> Result<Option<Message>, Error> {
        if !self.whole()? {
            match self.fill() {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                other => other?,
            }
            if !self.whole()? {
                return Ok(None);
            }
        }
        self.read().map(Some)
    }

    pub(crate) fn set_read_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.socket.tcp().set_read_timeout(Some(timeout))
    }

    /// A handle that can close the socket from another thread.
    pub(crate) fn closer(&self) -> io::Result<Closer> {
        self.socket.tcp().try_clone().map(Closer)
    }

    pub(crate) fn read(&mut self) -> Result<Message, Error> {
        self.next_tag()?;
        Ok(Message::parse(&mut self.input)?.expect("a whole message is buffered"))
    }

    /// The tag of the next message, once it is whole in the buffer.
    fn next_tag(&mut self) -> Result<u8, Error> {
        while !self.whole()? {
            self.fill()?;
        }
        Ok(self.input[0])
    }

    fn whole(&self) -> io::Result<bool> {
        Ok(Header::parse(&self.input)?.is_some_and(|h| self.input.len() >= frame_len(h)))
    }

    fn fill(&mut self) -> io::Result<()> {
        let start = self.input.len();
        self.input.resize(start + READ_CHUNK, 0);
        let read = loop {
            match self.socket.read(&mut self.input[start..]) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                other => break other,
            }
        };
        let n = read.inspect_err(|_| self.input.truncate(start))?;
        self.input.truncate(start + n);
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "PostgreSQL closed the connection",
            ));
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.socket.write_all(&self.output)?;
        self.socket.flush()?;
        self.output.clear();
        Ok(())
    }
}

impl Drop for Conn {
    /// Says goodbye, so PostgreSQL ends the session, and drops its temporary
    /// slot, without waiting for the socket to time out.
    fn drop(&mut self) {
        self.output.clear();
        frontend::terminate(&mut self.output);
        // The connection may already be gone.
        let _ = self.socket.write_all(&self.output);
        let _ = self.socket.flush();
    }
}

/// Closes a connection's socket from another thread, which wakes a read
/// blocked on it.
pub(crate) struct Closer(TcpStream);

impl Closer {
    pub(crate) fn close(&self) {
        // Already closed is fine.
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

/// The tag byte plus the length, which counts itself.
fn frame_len(header: Header) -> usize {
    1 + usize::try_from(header.len()).expect("Header::parse rejects lengths below 4")
}

/// The message and, when given, the detail.
pub(crate) fn server_error(body: &ErrorResponseBody) -> Error {
    let mut code = String::new();
    let mut message = String::new();
    let mut detail = String::new();
    let mut fields = body.fields();
    while let Ok(Some(field)) = fields.next() {
        let value = String::from_utf8_lossy(field.value_bytes()).into_owned();
        match field.type_() {
            b'C' => code = value,
            b'M' => message = value,
            b'D' => detail = value,
            _ => {}
        }
    }
    if !detail.is_empty() {
        message = format!("{message}: {detail}");
    }
    Error::Server { code, message }
}
