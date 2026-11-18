//! The Straum binary, where Straum SQL, the PostgreSQL source, the use
//! cases and HTTP meet. `main` reads the command line (`command_line.rs`)
//! and `execute` runs the command. `straum run` starts one writer thread,
//! which loads every table, then applies the stream, and ends the process
//! on a failure, while `straum-http` serves asks. The other commands talk to
//! a running Straum through `client.rs`, or prompt at it (`shell.rs`).

#![forbid(unsafe_code)]

mod client;
mod command_line;
mod complete;
mod help;
mod shell;
#[cfg(test)]
mod testing;

use std::env;
use std::fmt::{self, Display};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::net::{SocketAddr, TcpListener};
use std::panic;
use std::process::{self, ExitCode};
use std::thread;

use rustyline::error::ReadlineError;
use straum_app::{App, Step, Writer};
use straum_domain::{Ask, Definition, Literal, Spec};
use straum_http::QueryError;
use straum_notation::SqlError;
use straum_pg::{CopyError, Dsn, DsnError, PgEnv, Postgres};

use crate::client::ClientError;
use crate::command_line::{
    Command, Copy, Environment, Plan, Printed, ROLE, Run, URL_VARIABLE, command,
};
use crate::help::HELP;

// 2, not 1, so a script can tell a misused command line from a failed run.
const USAGE_FAILURE: u8 = 2;
// 3, as systemctl is-active says a unit is not running: Straum is loading
// or too far behind, and a script may ask again later.
const NOT_READY: u8 = 3;

// What a failed writer ends the process with, as every other failure.
const WRITER_FAILED: i32 = 1;

/// A command that started and failed.
#[derive(Debug)]
enum Failure {
    Client(ClientError),
    /// `straum status` printed the status of a Straum that is not ready.
    NotReady,
    Terminal(ReadlineError),
    CommandsFailed(usize),
    ReadingCommands(io::Error),
    WritingAnswer(io::Error),
    ReadingFile {
        path: String,
        error: io::Error,
    },
    WritingFile {
        path: String,
        error: io::Error,
    },
    Spec {
        path: String,
        error: straum_notation::Error,
    },
    Dsn(DsnError),
    NoDsn {
        variable: &'static str,
        /// The file flag, when this command has one.
        flag: Option<&'static str>,
        /// `PGHOST` and the other libpq variables are another way.
        libpq: bool,
    },
    Copy(CopyError),
    /// A role name with a control character, which would end a comment
    /// line of the setup SQL and start a statement.
    RoleControl(String),
    Listening {
        address: String,
        error: io::Error,
    },
    StartingWriter(io::Error),
    Serving(io::Error),
}

impl Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Failure::Client(error) => error.fmt(formatter),
            Failure::NotReady => formatter.write_str("not ready"),
            Failure::Terminal(error) => write!(formatter, "reading the terminal: {error}"),
            Failure::CommandsFailed(count) => write!(formatter, "commands that failed: {count}"),
            Failure::ReadingCommands(error) => write!(formatter, "reading commands: {error}"),
            Failure::WritingAnswer(error) => write!(formatter, "writing the answer: {error}"),
            Failure::ReadingFile { path, error } => write!(formatter, "reading {path}: {error}"),
            Failure::WritingFile { path, error } => write!(formatter, "writing {path}: {error}"),
            Failure::Spec { path, error } => {
                // Each line names the file, as a compiler's errors do.
                let text = error.to_string();
                let mut lines = Vec::new();
                for line in text.lines() {
                    lines.push(format!("{path}: {line}"));
                }
                formatter.write_str(&lines.join("\n"))
            }
            Failure::Dsn(error) => error.fmt(formatter),
            Failure::NoDsn {
                variable,
                flag,
                libpq,
            } => match (flag, libpq) {
                (Some(flag), true) => {
                    write!(formatter, "set {variable}, pass {flag}, or set PGHOST")
                }
                (Some(flag), false) => write!(formatter, "set {variable} or pass {flag}"),
                (None, true) => write!(formatter, "set {variable} or PGHOST"),
                (None, false) => write!(formatter, "set {variable}"),
            },
            Failure::Copy(error) => error.fmt(formatter),
            Failure::RoleControl(role) => {
                write!(formatter, "the role {role:?} holds a control character")
            }
            Failure::Listening { address, error } => {
                write!(formatter, "listening on {address}: {error}")
            }
            Failure::StartingWriter(error) => write!(formatter, "starting the writer: {error}"),
            Failure::Serving(error) => write!(formatter, "serving HTTP: {error}"),
        }
    }
}

impl Failure {
    /// What the process exits with: not ready apart from every other
    /// failure.
    fn exit_code(&self) -> ExitCode {
        match self {
            Failure::NotReady => ExitCode::from(NOT_READY),
            Failure::Client(error) if error.is_not_ready() => ExitCode::from(NOT_READY),
            _ => ExitCode::FAILURE,
        }
    }
}

impl From<ClientError> for Failure {
    fn from(error: ClientError) -> Failure {
        Failure::Client(error)
    }
}

impl From<DsnError> for Failure {
    fn from(error: DsnError) -> Failure {
        Failure::Dsn(error)
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let environment = Environment {
        url: env::var(URL_VARIABLE).ok(),
    };
    let command = match command(&args, &environment) {
        Ok(command) => command,
        Err(usage) => {
            // Nothing more to do if stderr is gone too.
            writeln!(io::stderr(), "{usage}").ok();
            return ExitCode::from(USAGE_FAILURE);
        }
    };
    match execute(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            log(&failure.to_string());
            failure.exit_code()
        }
    }
}

fn execute(command: Command) -> Result<(), Failure> {
    match command {
        Command::Help(topic) => {
            let help = match topic {
                Some(name) => name.help(),
                None => HELP,
            };
            print(help)
        }
        Command::Version => {
            // CI builds with the release's version; Cargo.toml never names it.
            let version = match option_env!("STRAUM_VERSION") {
                Some(version) => version,
                None => env!("CARGO_PKG_VERSION"),
            };
            print(&format!("straum {version}\n"))
        }
        Command::Plan(args) => plan(args),
        Command::Copy(args) => copy_tables(args),
        Command::Run(args) => run(args),
        Command::Status { server, output } => {
            print(&client::status(&server, output)?)?;
            if !client::is_ready(&server)? {
                return Err(Failure::NotReady);
            }
            Ok(())
        }
        Command::Views { server, output } => print(&client::views(&server, output)?),
        Command::Ask {
            server,
            query,
            parameters,
            output,
        } => print(&client::ask(&server, &query, &parameters, output)?.text),
        Command::Shell { server } => {
            let (input, output) = (io::stdin(), io::stdout());
            // A prompt is for a person at both ends: not for a script
            // that feeds lines, nor for output that goes to a file.
            if input.is_terminal() && output.is_terminal() {
                return shell::interactive(&server).map_err(Failure::Terminal);
            }
            match shell::piped(&server, input.lock(), output.lock(), io::stderr().lock()) {
                Ok(0) => Ok(()),
                Ok(failed) => Err(Failure::CommandsFailed(failed)),
                Err(error) => Err(Failure::ReadingCommands(error)),
            }
        }
    }
}

/// Writes an answer to stdout. A reader that has left, as `head` does
/// after its lines, is no failure.
fn print(text: &str) -> Result<(), Failure> {
    match io::stdout().write_all(text.as_bytes()) {
        Err(error) if error.kind() != io::ErrorKind::BrokenPipe => {
            Err(Failure::WritingAnswer(error))
        }
        _ => Ok(()),
    }
}

fn read_file(path: &str) -> Result<String, Failure> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) => {
            let path = path.to_owned();
            Err(Failure::ReadingFile { path, error })
        }
    }
}

/// A spec file, read and checked, and each view's statement in it.
struct SpecFile {
    spec: Spec,
    definitions: Vec<Definition>,
}

fn read_spec(path: &str) -> Result<SpecFile, Failure> {
    let text = read_file(path)?;
    match straum_notation::parse(&text) {
        Ok(spec) => {
            let definitions = straum_notation::definitions(&text, &spec);
            Ok(SpecFile { spec, definitions })
        }
        Err(error) => {
            let path = path.to_owned();
            Err(Failure::Spec { path, error })
        }
    }
}

/// The role and its password come from the DSN when there is one, so the
/// setup and `straum run` agree on them without being told twice.
/// `--out` writes the SQL there. Otherwise it is printed.
fn plan(
    Plan {
        spec,
        replication,
        publication,
        role,
        dsn_file,
        out,
        printed,
    }: Plan,
) -> Result<(), Failure> {
    let spec = read_spec(&spec)?.spec;
    if printed == Printed::Tables {
        let mut names = straum_pg::table_names(&spec).join("\n");
        names.push('\n');
        return write_or_print(out.as_deref(), &names);
    }
    let dsn = read_dsn("STRAUM_DSN", dsn_file.as_deref(), true)?;
    let role = match (&role, &dsn) {
        (Some(role), _) => role.as_str(),
        (None, Some(dsn)) => dsn.user(),
        (None, None) => ROLE,
    };
    if role.chars().any(char::is_control) {
        return Err(Failure::RoleControl(role.to_owned()));
    }
    let password = match &dsn {
        Some(dsn) => dsn.password(),
        None => None,
    };
    let sql = straum_pg::plan(&spec, &publication, role, password, replication);
    write_or_print(out.as_deref(), &sql)
}

/// Writes `text` to `path`, or prints it when there is no path.
fn write_or_print(path: Option<&str>, text: &str) -> Result<(), Failure> {
    let Some(path) = path else {
        return print(text);
    };
    match fs::write(path, text) {
        Ok(()) => Ok(()),
        Err(error) => {
            let path = path.to_owned();
            Err(Failure::WritingFile { path, error })
        }
    }
}

/// Reads an ask's query as Straum SQL, for `straum-http`.
fn read_ask(query: &str, parameters: &[Literal]) -> Result<Ask, Vec<QueryError>> {
    let sql_errors = match straum_notation::parse_ask(query, parameters) {
        Ok(ask) => return Ok(ask),
        Err(sql_errors) => sql_errors,
    };
    let mut errors = Vec::with_capacity(sql_errors.len());
    for SqlError { line, message } in sql_errors {
        errors.push(QueryError { line, message });
    }
    Err(errors)
}

/// From the file, else from `variable`, else from libpq's variables when
/// `libpq` says so. Never from the command line, where other users of the
/// machine can read it. A set Straum variable wins, even when libpq's are
/// set too.
fn read_dsn(variable: &str, file: Option<&str>, libpq: bool) -> Result<Option<Dsn>, Failure> {
    let text = match (file, env::var(variable)) {
        (Some(path), _) => read_file(path)?,
        (None, Ok(text)) => text,
        (None, Err(_)) => {
            if !libpq {
                return Ok(None);
            }
            return Dsn::from_pg(&pg_env()).map_err(Failure::Dsn);
        }
    };
    Ok(Some(Dsn::parse(text.trim())?))
}

/// What libpq would read from the environment.
fn pg_env() -> PgEnv {
    PgEnv {
        host: env::var("PGHOST").ok(),
        host_addr: env::var("PGHOSTADDR").ok(),
        port: env::var("PGPORT").ok(),
        user: env::var("PGUSER").ok(),
        os_user: env::var("USER").ok(),
        password: env::var("PGPASSWORD").ok(),
        database: env::var("PGDATABASE").ok(),
        sslmode: env::var("PGSSLMODE").ok(),
        ssl_root_cert: env::var("PGSSLROOTCERT").ok(),
    }
}

/// `variable`. The URL stays out of the command line, where other users
/// of the machine can read it.
fn require_dsn(
    variable: &'static str,
    flag: Option<&'static str>,
    libpq: bool,
) -> Result<Dsn, Failure> {
    match read_dsn(variable, None, libpq)? {
        Some(dsn) => Ok(dsn),
        None => Err(Failure::NoDsn {
            variable,
            flag,
            libpq,
        }),
    }
}

fn copy_tables(Copy { spec }: Copy) -> Result<(), Failure> {
    let spec = read_spec(&spec)?.spec;
    let from = require_dsn("STRAUM_FROM", None, true)?;
    let to = require_dsn("STRAUM_TO", None, false)?;
    straum_pg::copy(&spec, &from, &to).map_err(Failure::Copy)
}

fn run(
    Run {
        spec,
        listen,
        replication,
        publication,
        dsn_file,
        limits,
        max_transaction_rows,
    }: Run,
) -> Result<(), Failure> {
    let SpecFile { spec, definitions } = read_spec(&spec)?;
    let Some(dsn) = read_dsn("STRAUM_DSN", dsn_file.as_deref(), true)? else {
        return Err(Failure::NoDsn {
            variable: "STRAUM_DSN",
            flag: Some("--dsn-file"),
            libpq: true,
        });
    };
    let Bound { listener, address } = match bind(&listen) {
        Ok(bound) => bound,
        Err(error) => {
            let address = listen;
            return Err(Failure::Listening { address, error });
        }
    };

    // After a panic the store may be half written, or nothing writes it
    // any more. Stop, rather than answer from it.
    let report = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        report(info);
        process::abort();
    }));

    let app = App::new(spec, limits);
    let writer = Writer::new(
        app.clone(),
        Postgres::new(
            dsn,
            &publication,
            max_transaction_rows,
            limits.max_retained_wal,
            replication,
        ),
    )
    .reporting(log);
    // Before the writer, whose failure may end the process at once.
    log(&format!("listening on {address}"));
    thread::Builder::new()
        .name("straum-writer".into())
        .spawn(move || run_writer(writer))
        .map_err(Failure::StartingWriter)?;

    straum_http::serve_until_signal(listener, app, read_ask, definitions, say_stopping)
        .map_err(Failure::Serving)?;
    log("stopped");
    // Leaving closes the replication connections, and PostgreSQL drops
    // their temporary slots.
    Ok(())
}

/// A socket bound for HTTP, and the address it took: with port 0 the
/// system picks the port.
struct Bound {
    listener: TcpListener,
    address: SocketAddr,
}

fn bind(listen: &str) -> io::Result<Bound> {
    let listener = TcpListener::bind(listen)?;
    let address = listener.local_addr()?;
    Ok(Bound { listener, address })
}

fn say_stopping() {
    log("stopping: finishing the asks in flight");
}

/// Steps the writer until a step fails, then ends the process: Straum
/// fails hard, and whatever runs it starts it again, from a new slot. No
/// state of a failed run carries over to answer from. Ending the process
/// closes the replication connections, and PostgreSQL drops their
/// temporary slots.
fn run_writer(mut writer: Writer<Postgres>) {
    loop {
        match writer.step() {
            Ok(Step::Started(lsn)) => log(&format!("loaded every table; streaming from {lsn}")),
            Ok(Step::Applied(_)) => {}
            Err(error) => {
                log(&error.to_string());
                process::exit(WRITER_FAILED);
            }
        }
    }
}

/// Each line of `message` to stderr, prefixed `straum: `, so a reader
/// can grep for every line Straum logs. A stderr nobody reads any more is
/// no reason to stop: `eprintln!` would panic, and a panic aborts.
fn log(message: &str) {
    let mut stderr = io::stderr().lock();
    for line in message.lines() {
        if writeln!(stderr, "straum: {line}").is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_asks_as_straum_sql() {
        let Err(errors) = read_ask("SELECT trips", &[]) else {
            panic!("SELECT trips is an ask");
        };
        let first = errors[0].to_string();
        assert!(first.starts_with("line 1: "), "{first}");
    }

    fn gone() -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, "gone")
    }

    #[test]
    fn failures_say_what_failed() {
        let cases = [
            (
                Failure::ReadingFile {
                    path: "s".into(),
                    error: gone(),
                },
                "reading s: gone",
            ),
            (
                Failure::NoDsn {
                    variable: "STRAUM_DSN",
                    flag: Some("--dsn-file"),
                    libpq: true,
                },
                "set STRAUM_DSN, pass --dsn-file, or set PGHOST",
            ),
            (
                Failure::NoDsn {
                    variable: "STRAUM_FROM",
                    flag: None,
                    libpq: true,
                },
                "set STRAUM_FROM or PGHOST",
            ),
            (
                Failure::NoDsn {
                    variable: "STRAUM_TO",
                    flag: None,
                    libpq: false,
                },
                "set STRAUM_TO",
            ),
            (
                Failure::RoleControl("a\nb".into()),
                "the role \"a\\nb\" holds a control character",
            ),
            (Failure::NotReady, "not ready"),
            (Failure::CommandsFailed(2), "commands that failed: 2"),
            (Failure::ReadingCommands(gone()), "reading commands: gone"),
            (Failure::WritingAnswer(gone()), "writing the answer: gone"),
            (
                Failure::WritingFile {
                    path: "s".into(),
                    error: gone(),
                },
                "writing s: gone",
            ),
            (
                Failure::Listening {
                    address: "x".into(),
                    error: gone(),
                },
                "listening on x: gone",
            ),
        ];
        for (failure, said) in cases {
            assert_eq!(failure.to_string(), said);
        }
        let error = straum_notation::parse("nonsense").unwrap_err();
        let failure = Failure::Spec {
            path: "s".into(),
            error,
        };
        for line in failure.to_string().lines() {
            assert!(line.starts_with("s: "), "{line}");
        }
    }

    fn refused(status_line: &str) -> Failure {
        Failure::Client(ClientError::Refused {
            url: "u".into(),
            status_line: status_line.into(),
            body: String::new(),
        })
    }

    #[test]
    fn not_ready_exits_with_its_own_code() {
        let loading = refused("HTTP/1.1 503 Service Unavailable");
        assert_eq!(loading.exit_code(), ExitCode::from(NOT_READY));
        assert_eq!(Failure::NotReady.exit_code(), ExitCode::from(NOT_READY));
        assert!(HELP.contains(&format!("{USAGE_FAILURE} a command line misused")));
        assert!(HELP.contains(&format!("{NOT_READY} Straum not\nready")));
        let wrong = refused("HTTP/1.1 400 Bad Request");
        assert_eq!(wrong.exit_code(), ExitCode::FAILURE);
        assert_eq!(
            Failure::NoDsn {
                variable: "STRAUM_DSN",
                flag: Some("--dsn-file"),
                libpq: true,
            }
            .exit_code(),
            ExitCode::FAILURE
        );
    }
}
