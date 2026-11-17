//! One connection to PostgreSQL over TCP or TLS, speaking its frontend and
//! backend protocol: startup and SCRAM (password sign-in), simple queries,
//! `COPY` out for loads, and the copy-both stream of a replication
//! connection. Every other module talks to PostgreSQL through
//! `Connection`. Messages are read whole into a buffer, so a read that
//! times out loses nothing; rustls keeps a TLS record it has read part of.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::str;
use std::time::Duration;

use bytes::{Buf, BytesMut};
use fallible_iterator::FallibleIterator;
use postgres_protocol::authentication::sasl::{ChannelBinding, SCRAM_SHA_256, ScramSha256};
use postgres_protocol::message::backend::{DataRowBody, ErrorResponseBody, Header, Message};
use postgres_protocol::message::frontend;

use crate::dsn::{Dsn, Tls};
use crate::error::Error;
use crate::tls::{self, TlsStream};
use crate::unexpected::Unexpected;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the server may go silent while a connection starts: TLS,
/// startup and sign-in. A server that takes the socket and then says
/// nothing would otherwise hang a start, the watchdog's own among them.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const READ_CHUNK: usize = 64 * 1024;
/// `CopyBothResponse`, which `postgres_protocol` doesn't parse.
const COPY_BOTH: u8 = b'W';

/// What a connection takes: SQL, or replication commands as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionKind {
    Sql,
    Replication,
}

/// An open, signed-in connection to PostgreSQL.
pub(crate) struct Connection {
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
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Socket::Plain(tcp) => tcp.read(buffer),
            Socket::Tls(tls) => tls.read(buffer),
        }
    }
}

impl Write for Socket {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Socket::Plain(tcp) => tcp.write(buffer),
            Socket::Tls(tls) => tls.write(buffer),
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

impl Connection {
    /// Connects and authenticates. A replication connection takes
    /// replication commands as well as SQL. Every connection reads dates as
    /// ISO in UTC, which `value` expects.
    pub(crate) fn connect(dsn: &Dsn, kind: ConnectionKind) -> Result<Connection, Error> {
        let tcp = connect_tcp(dsn)?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        tcp.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let connection = match Connection::start(tcp, dsn, kind) {
            Ok(connection) => connection,
            Err(Error::Io(error)) if timed_out(&error) => {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "PostgreSQL sent nothing for {} s while the connection started",
                        HANDSHAKE_TIMEOUT.as_secs()
                    ),
                )));
            }
            Err(other) => return Err(other),
        };
        // A replication connection waits on the server for as long as
        // the database is quiet.
        connection.socket.tcp().set_read_timeout(None)?;
        connection.socket.tcp().set_write_timeout(None)?;
        Ok(connection)
    }

    /// TLS if the DSN asks for it, then startup and sign-in on `tcp`.
    fn start(tcp: TcpStream, dsn: &Dsn, kind: ConnectionKind) -> Result<Connection, Error> {
        let socket = match &dsn.tls {
            Tls::Off => Socket::Plain(tcp),
            Tls::Verify { name, root_cert } => {
                Socket::Tls(Box::new(tls::start(tcp, name, root_cert.as_deref())?))
            }
        };
        let mut connection = Connection {
            socket,
            input: BytesMut::new(),
            output: BytesMut::new(),
        };
        let mut parameters = vec![
            ("user", dsn.user.as_str()),
            ("database", dsn.database.as_str()),
            ("application_name", "straum"),
            ("client_encoding", "UTF8"),
            ("DateStyle", "ISO"),
            ("TimeZone", "UTC"),
            // A policy that would hide a row makes a read fail, never come
            // back short.
            ("row_security", "off"),
            // A load runs as long as its table takes, and its snapshot
            // stays open between tables; a timeout set on the database or
            // the role would cancel it on every start, as pg_dump knows.
            ("statement_timeout", "0"),
            ("lock_timeout", "0"),
            ("idle_in_transaction_session_timeout", "0"),
            ("transaction_timeout", "0"),
        ];
        if kind == ConnectionKind::Replication {
            parameters.push(("replication", "database"));
        }
        frontend::startup_message(parameters, &mut connection.output)?;
        connection.flush()?;
        connection.authenticate(dsn)?;
        Ok(connection)
    }

    fn authenticate(&mut self, dsn: &Dsn) -> Result<(), Error> {
        let mut scram: Option<ScramSha256> = None;
        // The server's proof, in SASL final. Without TLS, a server that
        // skips it may be anyone between Straum and PostgreSQL.
        let mut proven = false;
        loop {
            match self.read()? {
                Message::AuthenticationOk if scram.is_some() && !proven => {
                    return Err(Error::Protocol(Unexpected::ScramUnproven));
                }
                Message::AuthenticationOk
                | Message::ParameterStatus(_)
                | Message::BackendKeyData(_)
                | Message::NoticeResponse(_) => {}
                Message::AuthenticationSasl(body) => {
                    let offered: Vec<&str> = body.mechanisms().collect()?;
                    if !offered.contains(&SCRAM_SHA_256) {
                        return Err(Error::Protocol(Unexpected::SaslMechanisms(owned(&offered))));
                    }
                    let Some(password) = dsn.password.as_deref() else {
                        return Err(Error::Protocol(Unexpected::PasswordRequestWithoutPassword));
                    };
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
                    let Some(state) = scram.as_mut() else {
                        return Err(Error::Protocol(Unexpected::SaslContinue));
                    };
                    state.update(body.data())?;
                    frontend::sasl_response(state.message(), &mut self.output)?;
                    self.flush()?;
                }
                Message::AuthenticationSaslFinal(body) => {
                    let Some(state) = scram.as_mut() else {
                        return Err(Error::Protocol(Unexpected::SaslFinal));
                    };
                    state.finish(body.data())?;
                    proven = true;
                }
                Message::ReadyForQuery(_) => return Ok(()),
                Message::ErrorResponse(body) => return Err(server_error(&body)),
                _ => {
                    return Err(Error::Protocol(Unexpected::AuthenticationRequest));
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
            // After a FATAL error the server hangs up with no
            // ReadyForQuery: its error says why, not the hang-up.
            let message = match self.read() {
                Ok(message) => message,
                Err(read_error) => return Err(error.unwrap_or(read_error)),
            };
            match message {
                Message::DataRow(row) => rows.push(row_fields(&row)?),
                Message::RowDescription(_)
                | Message::CommandComplete(_)
                | Message::EmptyQueryResponse
                | Message::NoticeResponse(_)
                | Message::ParameterStatus(_) => {}
                // The first error is the one that says why.
                Message::ErrorResponse(body) => {
                    if error.is_none() {
                        error = Some(server_error(&body));
                    }
                }
                Message::ReadyForQuery(_) => match error {
                    None => return Ok(rows),
                    Some(error) => return Err(error),
                },
                _ => return Err(Error::Protocol(Unexpected::MessageDuringQuery)),
            }
        }
    }

    /// Runs a `COPY ... TO STDOUT`, calling `each` with every `CopyData` as
    /// it comes, so a load holds one message of the table at a time. After
    /// an error the connection is mid-copy; drop it.
    // `impl FnMut`: the caller's code runs on each message in the buffer it
    // came in, which is reused for the next.
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
                _ => return Err(Error::Protocol(Unexpected::MessageDuringCopy)),
            }
        }
    }

    /// Runs one simple query, ignoring any rows.
    pub(crate) fn execute(&mut self, sql: &str) -> Result<(), Error> {
        self.rows(sql).map(drop)
    }

    /// Sends a command that answers with `CopyBothResponse`, such as
    /// `START_REPLICATION`, and waits for that answer.
    pub(crate) fn copy_both(&mut self, command: &str) -> Result<(), Error> {
        frontend::query(command, &mut self.output)?;
        self.flush()?;
        loop {
            if self.next_tag()? == COPY_BOTH {
                let header = Header::parse(&self.input)?.expect("a whole message is buffered");
                self.input.advance(frame_length(header));
                return Ok(());
            }
            match self.read()? {
                Message::NoticeResponse(_) | Message::ParameterStatus(_) => {}
                Message::ErrorResponse(body) => return Err(server_error(&body)),
                _ => return Err(Error::Protocol(Unexpected::NoCopyBothResponse)),
            }
        }
    }

    /// Sends `data` as one `CopyData` message on a copy-both stream.
    pub(crate) fn send_copy_data(&mut self, data: &[u8]) -> Result<(), Error> {
        frontend::CopyData::new(data)?.write(&mut self.output);
        self.flush()
    }

    /// The next message, or `None` if none came within the read timeout.
    pub(crate) fn try_read(&mut self) -> Result<Option<Message>, Error> {
        if !self.whole()? {
            match self.fill() {
                Err(error) if timed_out(&error) => {
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

    /// How long a read waits before `try_read` gives up.
    pub(crate) fn set_read_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.socket.tcp().set_read_timeout(Some(timeout))
    }

    /// A handle that can close the socket from another thread.
    pub(crate) fn closer(&self) -> io::Result<Closer> {
        self.socket.tcp().try_clone().map(Closer)
    }

    /// The next message, waiting for it.
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
        let Some(header) = Header::parse(&self.input)? else {
            return Ok(false);
        };
        Ok(self.input.len() >= frame_length(header))
    }

    fn fill(&mut self) -> io::Result<()> {
        let start = self.input.len();
        self.input.resize(start + READ_CHUNK, 0);
        let read = loop {
            match self.socket.read(&mut self.input[start..]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                other => break other,
            }
        };
        let received = match read {
            Ok(received) => received,
            Err(error) => {
                self.input.truncate(start);
                return Err(error);
            }
        };
        self.input.truncate(start + received);
        if received == 0 {
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

impl Drop for Connection {
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
    /// Shuts the socket both ways.
    pub(crate) fn close(&self) {
        // Already closed is fine.
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

/// The tag byte plus the length, which counts itself.
fn frame_length(header: Header) -> usize {
    1 + usize::try_from(header.len()).expect("Header::parse rejects lengths below 4")
}

/// Whether a read or write gave up at its timeout; platforms say so in
/// either of two ways.
fn timed_out(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// The first of the host's addresses that answers, or the last failure.
fn connect_tcp(dsn: &Dsn) -> io::Result<TcpStream> {
    let mut failure = io::Error::new(io::ErrorKind::NotFound, "no address");
    for address in (dsn.host.as_str(), dsn.port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(tcp) => return Ok(tcp),
            Err(error) => failure = error,
        }
    }
    Err(failure)
}

/// A row's values as text; `None` for a null.
fn row_fields(row: &DataRowBody) -> Result<Vec<Option<String>>, Error> {
    let buffer = row.buffer();
    let mut fields = Vec::new();
    let mut ranges = row.ranges();
    while let Some(range) = ranges.next()? {
        let Some(range) = range else {
            fields.push(None);
            continue;
        };
        let text = str::from_utf8(&buffer[range])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fields.push(Some(text.to_owned()));
    }
    Ok(fields)
}

/// The value in `column` of the first of `rows`, if there is one.
pub(crate) fn first_row_value(rows: &[Vec<Option<String>>], column: usize) -> Option<&str> {
    let row = rows.first()?;
    row.get(column)?.as_deref()
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

/// The SASL mechanism names, copied out of the message they borrow from.
fn owned(names: &[&str]) -> Vec<String> {
    let mut owned = Vec::new();
    for name in names {
        owned.push((*name).to_owned());
    }
    owned
}
