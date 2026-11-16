//! The Straum binary, where Straum SQL, the PostgreSQL source, the use
//! cases and HTTP meet. `main` reads the command line (`command_line.rs`)
//! and `execute` runs the command. `straum run` starts one writer thread,
//! which loads every table, then applies the stream, and starts over after
//! a failure, while `straum-http` serves asks. The other commands talk to
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
use std::time::Duration;

use rustyline::error::ReadlineError;
use straum_app::{App, Step, Writer};
use straum_domain::{Ask, Literal, Spec};
use straum_http::QueryError;
use straum_notation::SqlError;
use straum_pg::{Dsn, DsnError, Postgres};

use crate::client::ClientError;
use crate::command_line::{Command, Environment, Plan, ROLE, Run, URL_VARIABLE, command};
use crate::help::HELP;

// 2, not 1, so a script can tell a misused command line from a failed run.
const USAGE_FAILURE: u8 = 2;

const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// A command that started and failed.
#[derive(Debug)]
enum Failure {
    Client(ClientError),
    Terminal(ReadlineError),
    LinesFailed(usize),
    ReadingCommands(io::Error),
    WritingAnswer(io::Error),
    ReadingFile {
        path: String,
        error: io::Error,
    },
    Spec {
        path: String,
        error: straum_notation::Error,
    },
    Dsn(DsnError),
    NoDsn,
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
            Failure::Terminal(error) => write!(formatter, "reading the terminal: {error}"),
            Failure::LinesFailed(count) => write!(formatter, "lines that failed: {count}"),
            Failure::ReadingCommands(error) => write!(formatter, "reading commands: {error}"),
            Failure::WritingAnswer(error) => write!(formatter, "writing the answer: {error}"),
            Failure::ReadingFile { path, error } => write!(formatter, "reading {path}: {error}"),
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
            Failure::NoDsn => formatter.write_str("set STRAUM_DSN or pass --dsn-file"),
            Failure::Listening { address, error } => {
                write!(formatter, "listening on {address}: {error}")
            }
            Failure::StartingWriter(error) => write!(formatter, "starting the writer: {error}"),
            Failure::Serving(error) => write!(formatter, "serving HTTP: {error}"),
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
            eprintln!("{usage}");
            return ExitCode::from(USAGE_FAILURE);
        }
    };
    match execute(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            log(&failure.to_string());
            ExitCode::FAILURE
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
            print!("{help}");
            Ok(())
        }
        Command::Version => {
            // CI builds with the release's version; Cargo.toml never names it.
            let version = match option_env!("STRAUM_VERSION") {
                Some(version) => version,
                None => env!("CARGO_PKG_VERSION"),
            };
            println!("straum {version}");
            Ok(())
        }
        Command::Plan(args) => plan(args),
        Command::Run(args) => run(args),
        Command::Status { url } => print(&client::status(&url)?),
        Command::Views { url } => print(&client::views(&url)?),
        Command::Ask {
            url,
            query,
            parameters,
        } => print(&client::ask(&url, &query, &parameters)?),
        Command::Shell { url } => {
            let (input, output) = (io::stdin(), io::stdout());
            // A prompt is for a person at both ends: not for a script
            // that feeds lines, nor for output that goes to a file.
            if input.is_terminal() && output.is_terminal() {
                return shell::interactive(&url).map_err(Failure::Terminal);
            }
            match shell::piped(&url, input.lock(), output.lock(), io::stderr().lock()) {
                Ok(0) => Ok(()),
                Ok(failed) => Err(Failure::LinesFailed(failed)),
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

fn read_spec(path: &str) -> Result<Spec, Failure> {
    let text = read_file(path)?;
    match straum_notation::parse(&text) {
        Ok(spec) => Ok(spec),
        Err(error) => {
            let path = path.to_owned();
            Err(Failure::Spec { path, error })
        }
    }
}

/// The role and its password come from the DSN when there is one, so the
/// setup and `straum run` agree on them without being told twice.
fn plan(
    Plan {
        spec,
        replication,
        publication,
        role,
        dsn_file,
    }: Plan,
) -> Result<(), Failure> {
    let spec = read_spec(&spec)?;
    let dsn = read_dsn(dsn_file.as_deref())?;
    let role = match (&role, &dsn) {
        (Some(role), _) => role.as_str(),
        (None, Some(dsn)) => dsn.user(),
        (None, None) => ROLE,
    };
    let password = match &dsn {
        Some(dsn) => dsn.password(),
        None => None,
    };
    let sql = straum_pg::plan(&spec, &publication, role, password, replication);
    print!("{sql}");
    Ok(())
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

/// From the file, else from `STRAUM_DSN`. Never from the command line, where
/// other users of the machine can read it.
fn read_dsn(file: Option<&str>) -> Result<Option<Dsn>, Failure> {
    let text = match (file, env::var("STRAUM_DSN")) {
        (Some(path), _) => read_file(path)?,
        (None, Ok(text)) => text,
        (None, Err(_)) => return Ok(None),
    };
    Ok(Some(Dsn::parse(text.trim())?))
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
    let spec = read_spec(&spec)?;
    let Some(dsn) = read_dsn(dsn_file.as_deref())? else {
        return Err(Failure::NoDsn);
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
    );
    thread::Builder::new()
        .name("straum-writer".into())
        .spawn(move || run_writer(writer))
        .map_err(Failure::StartingWriter)?;

    log(&format!("listening on {address}"));
    straum_http::serve_until_signal(listener, app, read_ask, say_stopping)
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

fn run_writer(mut writer: Writer<Postgres>) {
    let mut backoff = FIRST_BACKOFF;
    loop {
        match writer.step() {
            Ok(Step::Started(lsn)) => {
                backoff = FIRST_BACKOFF;
                log(&format!("loaded every table; streaming from {lsn}"));
            }
            Ok(Step::Applied(_)) => {}
            Err(error) => {
                log(&format!(
                    "{error}\nstarting over in {} s",
                    backoff.as_secs()
                ));
                thread::sleep(backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

fn log(message: &str) {
    eprintln!("straum: {message}");
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
            (Failure::NoDsn, "set STRAUM_DSN or pass --dsn-file"),
            (Failure::LinesFailed(2), "lines that failed: 2"),
            (Failure::ReadingCommands(gone()), "reading commands: gone"),
            (Failure::WritingAnswer(gone()), "writing the answer: gone"),
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
}
