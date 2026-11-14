//! The Straum binary, where Straum SQL, the PostgreSQL source, the use cases and
//! HTTP meet. One thread writes: it loads every table, then applies the
//! stream, and starts over after a failure. tokio serves asks meanwhile.

#![forbid(unsafe_code)]

mod client;
mod complete;
mod shell;

use std::env;
use std::fmt::{self, Display};
use std::fs;
use std::future::poll_fn;
use std::io::{self, IsTerminal, Write};
use std::net::TcpListener;
use std::panic;
use std::process::{self, ExitCode};
use std::task::Poll;
use std::thread;
use std::time::Duration;

use straum_app::{App, Limits, Step, Writer};
use straum_domain::{Ask, Literal, Spec};
use straum_pg::{Dsn, Postgres};
use tokio::signal::unix::{SignalKind, signal};

const HELP: &str = "\
Straum follows tables in PostgreSQL and answers asks over HTTP.

usage: straum <command> [flags]

  run     load the spec's tables, follow their changes, answer asks
  plan    print the SQL that sets PostgreSQL up for the spec
  status  print what a running Straum has done
  views   print the views a running Straum holds, and how much of each
  ask     ask a running Straum and print the answer as a table
  shell   a prompt at a running Straum: its views, their rows, asks
  version print Straum's version
  help    print this; straum help <command> prints its flags

Guide: https://janbjorge.github.io/straum/
";

const RUN_HELP: &str = "\
usage: straum run --spec FILE [flags]

Loads every table FILE names, follows their changes and answers asks over
HTTP. SIGTERM or SIGINT stops it.

  --spec FILE                  the spec; required
  --listen ADDRESS             where to serve HTTP (0.0.0.0:8080)
  --publication NAME           the publication straum plan made (straum)
  --dsn-file FILE              a file holding the DSN; else STRAUM_DSN
  --max-lag SECONDS            lag before /readyz turns off (30)
  --max-retained-wal BYTES     WAL retained for the slot before Straum starts
                               over (4Gi; takes Ki, Mi, Gi or Ti)
  --max-transaction-rows ROWS  rows one transaction may change before
                               Straum starts over (100000)

Guide: https://janbjorge.github.io/straum/running.html#straum-run
";

const PLAN_HELP: &str = "\
usage: straum plan --spec FILE [flags]

Prints the SQL that sets PostgreSQL up for FILE: logical decoding, the
role, its grants and the publication. With a DSN the role is its user and
gets its password, as a SCRAM-SHA-256 verifier.

  --spec FILE         the spec; required
  --publication NAME  the publication to make (straum)
  --role NAME         the role straum run connects as (the DSN's user,
                      else straum)
  --dsn-file FILE     a file holding the DSN; else STRAUM_DSN, if set

Guide: https://janbjorge.github.io/straum/postgresql.html#2-run-the-setup-sql
";

const STATUS_HELP: &str = "\
usage: straum status [flags]

Prints what the Straum at URL has done: ready or why not, how far behind
PostgreSQL, the WAL retained for it, its starts and its failures. In a pod:
kubectl exec deployment/straum -- /straum status

  --url URL  where Straum serves HTTP (http://127.0.0.1:8080)

Guide: https://janbjorge.github.io/straum/running.html#straum-status
";

const VIEWS_HELP: &str = "\
usage: straum views [flags]

Prints every view of the Straum at URL: its bucket widths, the cells and
the range of buckets it holds, its keys with their types and its measures.
Then each dim and its rows.

  --url URL  where Straum serves HTTP (http://127.0.0.1:8080)

Guide: https://janbjorge.github.io/straum/running.html#straum-views
";

const ASK_HELP: &str = "\
usage: straum ask [flags] QUERY [PARAMETER...]

Sends QUERY to the Straum at URL and prints the answer as a table. Each
PARAMETER is the value of $1, $2 and on: 7 is an integer, true and false
are booleans, anything else is a string, and '7' in quotes is the string 7.

  --url URL  where Straum serves HTTP (http://127.0.0.1:8080)

Guide: https://janbjorge.github.io/straum/running.html#straum-ask
";

const SHELL_HELP: &str = "\
usage: straum shell [flags]

Reads commands for the Straum at URL, a line each, until \\q or the end
of input. At a terminal Tab completes views, keys, measures, conditions
and the bounds of the buckets held.

  \\d                    every view, as straum views prints them
  \\rows VIEW            the rows PostgreSQL would give for VIEW, the
                        first 1000
  QUERY; PARAMETER...   an ask, as straum ask takes it: the query, a
                        semicolon, then the values of $1, $2 and on
  \\?                    these commands
  \\q                    leave

  --url URL  where Straum serves HTTP (http://127.0.0.1:8080)

Guide: https://janbjorge.github.io/straum/running.html#straum-shell
";

const LISTEN: &str = "0.0.0.0:8080";
const PUBLICATION: &str = "straum";
const ROLE: &str = "straum";
const URL: &str = "http://127.0.0.1:8080";
const LIMITS: Limits = Limits {
    max_lag: Duration::from_secs(30),
    max_retained_wal: 4 << 30,
};
/// Rows one transaction may change before Straum loads again instead of
/// applying it; `bench/taxi.py transactions` measures what a transaction this
/// size holds the write lock for.
const MAX_TRANSACTION_ROWS: usize = 100_000;

const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Name {
    Run,
    Plan,
    Status,
    Views,
    Ask,
    Shell,
}

impl Name {
    const ALL: [Name; 6] = [
        Name::Run,
        Name::Plan,
        Name::Status,
        Name::Views,
        Name::Ask,
        Name::Shell,
    ];

    fn parse(text: &str) -> Option<Name> {
        Name::ALL.into_iter().find(|name| name.as_str() == text)
    }

    fn as_str(self) -> &'static str {
        match self {
            Name::Run => "run",
            Name::Plan => "plan",
            Name::Status => "status",
            Name::Views => "views",
            Name::Ask => "ask",
            Name::Shell => "shell",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Name::Run => RUN_HELP,
            Name::Plan => PLAN_HELP,
            Name::Status => STATUS_HELP,
            Name::Views => VIEWS_HELP,
            Name::Ask => ASK_HELP,
            Name::Shell => SHELL_HELP,
        }
    }

    fn flags(self) -> &'static [&'static str] {
        match self {
            Name::Run => &[
                "--spec",
                "--listen",
                "--publication",
                "--dsn-file",
                "--max-lag",
                "--max-retained-wal",
                "--max-transaction-rows",
            ],
            Name::Plan => &["--spec", "--publication", "--role", "--dsn-file"],
            Name::Status | Name::Views | Name::Ask | Name::Shell => &["--url"],
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Run(Run),
    Plan {
        spec: String,
        publication: String,
        role: Option<String>,
        dsn_file: Option<String>,
    },
    Status {
        url: String,
    },
    Views {
        url: String,
    },
    Ask {
        url: String,
        query: String,
        /// As written on the command line.
        parameters: Vec<String>,
    },
    Shell {
        url: String,
    },
    Help(Option<Name>),
    Version,
}

#[derive(Debug, PartialEq, Eq)]
struct Run {
    spec: String,
    listen: String,
    publication: String,
    dsn_file: Option<String>,
    limits: Limits,
    max_transaction_rows: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum Exit {
    NoCommand,
    Usage {
        message: String,
        command: Option<Name>,
    },
    Runtime(String),
}

impl Exit {
    fn exit_code(&self) -> ExitCode {
        match self {
            Exit::NoCommand | Exit::Usage { .. } => ExitCode::from(2),
            Exit::Runtime(_) => ExitCode::FAILURE,
        }
    }
}

impl Display for Exit {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Exit::NoCommand => formatter.write_str(HELP.trim_end()),
            Exit::Usage { message, command } => {
                let hint =
                    command.map_or("help".into(), |name| format!("{} --help", name.as_str()));
                write!(formatter, "straum: {message}\ntry: straum {hint}")
            }
            Exit::Runtime(message) => write!(formatter, "straum: {message}"),
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match command(&args).and_then(|command| execute(command).map_err(Exit::Runtime)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(exit) => {
            eprintln!("{exit}");
            exit.exit_code()
        }
    }
}

fn execute(command: Command) -> Result<(), String> {
    match command {
        Command::Help(topic) => {
            print!("{}", topic.map_or(HELP, Name::help));
            Ok(())
        }
        Command::Version => {
            // CI builds with the release's version; Cargo.toml never names it.
            let version = option_env!("STRAUM_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
            println!("straum {version}");
            Ok(())
        }
        Command::Plan {
            spec,
            publication,
            role,
            dsn_file,
        } => plan(&spec, &publication, role.as_deref(), dsn_file.as_deref()),
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
                return shell::interactive(&url);
            }
            match shell::piped(&url, input.lock(), output.lock(), io::stderr().lock()) {
                Ok(0) => Ok(()),
                Ok(failed) => Err(format!("lines that failed: {failed}")),
                Err(e) => Err(format!("reading commands: {e}")),
            }
        }
    }
}

/// Writes an answer to stdout. A reader that has left, as `head` does
/// after its lines, is no failure.
fn print(text: &str) -> Result<(), String> {
    match io::stdout().write_all(text.as_bytes()) {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(format!("writing the answer: {e}")),
        _ => Ok(()),
    }
}

fn command(args: &[String]) -> Result<Command, Exit> {
    let Some((first, rest)) = args.split_first() else {
        return Err(Exit::NoCommand);
    };
    let name = Name::parse(first);
    let parsed = match name {
        Some(name) => flags(name, rest),
        None => help_or_version(first, rest),
    };
    parsed.map_err(|message| Exit::Usage {
        message,
        command: name,
    })
}

fn unknown(text: &str) -> String {
    format!("unknown command {text:?}")
}

fn help_or_version(word: &str, rest: &[String]) -> Result<Command, String> {
    if ["help", "--help", "-h"].contains(&word) {
        return match rest {
            [] => Ok(Command::Help(None)),
            [topic] => Name::parse(topic)
                .map(|name| Command::Help(Some(name)))
                .ok_or_else(|| unknown(topic)),
            _ => Err("straum help takes one command".into()),
        };
    }
    if ["version", "--version", "-V"].contains(&word) {
        return match rest {
            [] => Ok(Command::Version),
            _ => Err("straum version takes nothing".into()),
        };
    }
    Err(unknown(word))
}

struct Given<'a> {
    flag: &'a str,
    value: &'a str,
}

/// Where an ask's flags end and its query begins: each flag takes one
/// value, a call for help none.
fn query_at(rest: &[String]) -> usize {
    let mut at = 0;
    while let Some(flag) = rest.get(at).filter(|arg| arg.starts_with('-')) {
        at += if flag == "-h" || flag == "--help" {
            1
        } else {
            2
        };
    }
    at.min(rest.len())
}

fn flags(name: Name, rest: &[String]) -> Result<Command, String> {
    // The query and its parameters are read as written, so a parameter
    // may be `-h` or a negative number.
    let (rest, query) = match name {
        Name::Ask => rest.split_at(query_at(rest)),
        _ => (rest, &[][..]),
    };
    // -h anywhere asks for help, whatever else is on the line.
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Command::Help(Some(name)));
    }
    let mut given: Vec<Given> = Vec::new();
    let mut rest = rest.iter();
    while let Some(flag) = rest.next() {
        if !name.flags().contains(&flag.as_str()) {
            return Err(format!("straum {} takes no {flag:?}", name.as_str()));
        }
        if value(&given, flag).is_some() {
            return Err(format!("{flag} given twice"));
        }
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        given.push(Given { flag, value });
    }
    let text = |flag| value(&given, flag).map(str::to_owned);
    let spec = || text("--spec").ok_or_else(|| format!("straum {} needs --spec", name.as_str()));
    let publication = || text("--publication").unwrap_or_else(|| PUBLICATION.into());
    let url = || text("--url").unwrap_or_else(|| URL.into());
    Ok(match name {
        Name::Status => Command::Status { url: url() },
        Name::Views => Command::Views { url: url() },
        Name::Shell => Command::Shell { url: url() },
        Name::Ask => match query {
            [query, parameters @ ..] => Command::Ask {
                url: url(),
                query: query.clone(),
                parameters: parameters.to_vec(),
            },
            [] => return Err("straum ask needs a query".into()),
        },
        Name::Plan => Command::Plan {
            spec: spec()?,
            publication: publication(),
            role: text("--role"),
            dsn_file: text("--dsn-file"),
        },
        Name::Run => Command::Run(Run {
            spec: spec()?,
            listen: text("--listen").unwrap_or_else(|| LISTEN.into()),
            publication: publication(),
            dsn_file: text("--dsn-file"),
            limits: Limits {
                max_lag: parsed(
                    &given,
                    "--max-lag",
                    LIMITS.max_lag,
                    seconds,
                    "whole seconds above 0",
                )?,
                max_retained_wal: parsed(
                    &given,
                    "--max-retained-wal",
                    LIMITS.max_retained_wal,
                    bytes,
                    "bytes above 0, as 4294967296 or 4Gi",
                )?,
            },
            max_transaction_rows: parsed(
                &given,
                "--max-transaction-rows",
                MAX_TRANSACTION_ROWS,
                count,
                "a count of rows above 0",
            )?,
        }),
    })
}

fn value<'a>(given: &[Given<'a>], flag: &str) -> Option<&'a str> {
    (given.iter())
        .find(|given| given.flag == flag)
        .map(|given| given.value)
}

fn parsed<T>(
    given: &[Given],
    flag: &str,
    default: T,
    parse: fn(&str) -> Option<T>,
    takes: &str,
) -> Result<T, String> {
    match value(given, flag) {
        None => Ok(default),
        Some(value) => parse(value).ok_or_else(|| format!("{flag} takes {takes}, not {value:?}")),
    }
}

fn positive(text: &str) -> Option<u64> {
    text.parse().ok().filter(|&n| n > 0)
}

fn seconds(text: &str) -> Option<Duration> {
    positive(text).map(Duration::from_secs)
}

fn count(text: &str) -> Option<usize> {
    usize::try_from(positive(text)?).ok()
}

// The suffixes are Kubernetes' own, so a chart's `4Gi` passes as written.
fn bytes(text: &str) -> Option<u64> {
    let (count, shift) = [("Ki", 10), ("Mi", 20), ("Gi", 30), ("Ti", 40)]
        .into_iter()
        .find_map(|(unit, shift)| Some((text.strip_suffix(unit)?, shift)))
        .unwrap_or((text, 0));
    positive(count)?.checked_mul(1 << shift)
}

fn read_file(path: &str) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))
}

fn read_spec(path: &str) -> Result<Spec, String> {
    let text = read_file(path)?;
    straum_notation::parse(&text).map_err(|e| {
        let lines: Vec<String> = (e.to_string().lines())
            .map(|line| format!("{path}: {line}"))
            .collect();
        lines.join("\n")
    })
}

/// The role and its password come from the DSN when there is one, so the
/// setup and `straum run` agree on them without being told twice.
fn plan(
    spec: &str,
    publication: &str,
    role: Option<&str>,
    dsn_file: Option<&str>,
) -> Result<(), String> {
    let spec = read_spec(spec)?;
    let dsn = read_dsn(dsn_file)?;
    let role = role.or_else(|| dsn.as_ref().map(Dsn::user)).unwrap_or(ROLE);
    let password = dsn.as_ref().and_then(Dsn::password);
    print!("{}", straum_pg::plan(&spec, publication, role, password));
    Ok(())
}

fn read_ask(query: &str, parameters: &[Literal]) -> Result<Ask, Vec<String>> {
    straum_notation::parse_ask(query, parameters)
        .map_err(|errors| errors.iter().map(ToString::to_string).collect())
}

/// From the file, else from `STRAUM_DSN`. Never from the command line, where
/// other users of the machine can read it.
fn read_dsn(file: Option<&str>) -> Result<Option<Dsn>, String> {
    let text = match (file, env::var("STRAUM_DSN")) {
        (Some(path), _) => read_file(path)?,
        (None, Ok(text)) => text,
        (None, Err(_)) => return Ok(None),
    };
    Dsn::parse(text.trim()).map(Some).map_err(|e| e.to_string())
}

fn run(args: Run) -> Result<(), String> {
    let Run {
        spec,
        listen,
        publication,
        dsn_file,
        limits,
        max_transaction_rows,
    } = args;
    let spec = read_spec(&spec)?;
    let dsn = read_dsn(dsn_file.as_deref())?.ok_or("set STRAUM_DSN or pass --dsn-file")?;
    let listening = |e: io::Error| format!("listening on {listen}: {e}");
    let listener = TcpListener::bind(&listen).map_err(listening)?;
    let address = listener.local_addr().map_err(listening)?;

    // After a panic the store may be half written, or nothing writes it
    // any more. Stop, rather than answer from it.
    let report = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        report(info);
        process::abort();
    }));

    let app = App::new(spec, limits, None);
    let writer = Writer::new(
        app.clone(),
        Postgres::new(dsn, &publication, max_transaction_rows),
    );
    thread::Builder::new()
        .name("straum-writer".into())
        .spawn(move || run_writer(writer))
        .map_err(|e| format!("starting the writer: {e}"))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("starting tokio: {e}"))?;
    let handling =
        |name: &str, kind: SignalKind| signal(kind).map_err(|e| format!("handling {name}: {e}"));
    let (mut terminate, mut interrupt) = {
        let _entered = runtime.enter();
        (
            handling("SIGTERM", SignalKind::terminate())?,
            handling("SIGINT", SignalKind::interrupt())?,
        )
    };
    runtime
        .block_on(async {
            let stop = async move {
                poll_fn(|cx| {
                    let terminated = terminate.poll_recv(cx).is_ready();
                    if interrupt.poll_recv(cx).is_ready() || terminated {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
                log("stopping: finishing the asks in flight");
            };
            log(format!("listening on {address}"));
            straum_http::serve(listener, app, read_ask, stop).await
        })
        .map_err(|e| format!("serving HTTP: {e}"))?;
    log("stopped");
    // Leaving closes the replication connections, and PostgreSQL drops
    // their temporary slots.
    Ok(())
}

fn run_writer(mut writer: Writer<Postgres>) {
    let mut backoff = FIRST_BACKOFF;
    loop {
        match writer.step() {
            Ok(Step::Started(lsn)) => {
                backoff = FIRST_BACKOFF;
                log(format!("loaded every table; streaming from {lsn}"));
            }
            Ok(Step::Applied(_)) => {}
            Err(e) => {
                log(format!("{e}\nstarting over in {} s", backoff.as_secs()));
                thread::sleep(backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

fn log(message: impl Display) {
    eprintln!("straum: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn reads_commands() {
        assert_eq!(
            command(&args("run --spec s.sql")),
            Ok(Command::Run(Run {
                spec: "s.sql".into(),
                listen: LISTEN.into(),
                publication: PUBLICATION.into(),
                dsn_file: None,
                limits: LIMITS,
                max_transaction_rows: MAX_TRANSACTION_ROWS,
            }))
        );
        assert_eq!(
            command(&args(
                "run --dsn-file d --listen 127.0.0.1:9 --publication p --spec s \
                 --max-lag 5 --max-retained-wal 512Mi --max-transaction-rows 2500"
            )),
            Ok(Command::Run(Run {
                spec: "s".into(),
                listen: "127.0.0.1:9".into(),
                publication: "p".into(),
                dsn_file: Some("d".into()),
                limits: Limits {
                    max_lag: Duration::from_secs(5),
                    max_retained_wal: 536_870_912,
                },
                max_transaction_rows: 2500,
            }))
        );
        assert_eq!(
            command(&args("plan --spec s")),
            Ok(Command::Plan {
                spec: "s".into(),
                publication: PUBLICATION.into(),
                role: None,
                dsn_file: None,
            })
        );
        assert_eq!(
            command(&args("plan --role r --spec s --dsn-file d")),
            Ok(Command::Plan {
                spec: "s".into(),
                publication: PUBLICATION.into(),
                role: Some("r".into()),
                dsn_file: Some("d".into()),
            })
        );
        assert_eq!(
            command(&args("status")),
            Ok(Command::Status { url: URL.into() })
        );
        assert_eq!(
            command(&args("status --url http://straum:9")),
            Ok(Command::Status {
                url: "http://straum:9".into()
            })
        );
        assert_eq!(
            command(&args("views --url http://straum:9")),
            Ok(Command::Views {
                url: "http://straum:9".into()
            })
        );
        assert_eq!(
            command(&args("shell")),
            Ok(Command::Shell { url: URL.into() })
        );
        assert_eq!(
            command(&args("ask SELECT")),
            Ok(Command::Ask {
                url: URL.into(),
                query: "SELECT".into(),
                parameters: Vec::new(),
            })
        );
        // What follows the query is a parameter, whatever it looks like.
        assert_eq!(
            command(&args("ask --url http://straum:9 SELECT -h --url -7")),
            Ok(Command::Ask {
                url: "http://straum:9".into(),
                query: "SELECT".into(),
                parameters: args("-h --url -7"),
            })
        );
        for help in ["help", "--help", "-h"] {
            assert_eq!(command(&args(help)), Ok(Command::Help(None)));
        }
        for version in ["version", "--version", "-V"] {
            assert_eq!(command(&args(version)), Ok(Command::Version));
        }
        for (line, name) in [
            ("help run", Name::Run),
            ("run --spec s -h", Name::Run),
            ("plan --help", Name::Plan),
            ("status -h --url", Name::Status),
            ("views -h", Name::Views),
            ("ask --help", Name::Ask),
            ("shell -h", Name::Shell),
            ("ask --url u -h SELECT", Name::Ask),
        ] {
            assert_eq!(
                command(&args(line)),
                Ok(Command::Help(Some(name))),
                "{line}"
            );
        }
    }

    #[test]
    fn help_names_every_flag() {
        assert!(HELP.contains("usage: straum <command>"));
        for name in Name::ALL {
            let text = name.help();
            let word = name.as_str();
            assert!(text.starts_with(&format!("usage: straum {word}")), "{word}");
            assert!(HELP.contains(&format!("\n  {word} ")), "{word}");
            for flag in name.flags() {
                assert!(text.contains(&format!("  {flag} ")), "{word} {flag}");
            }
            for other in Name::ALL.into_iter().filter(|&other| other != name) {
                for flag in other.flags().iter().filter(|f| !name.flags().contains(f)) {
                    assert!(!text.contains(flag), "{word} {flag}");
                }
            }
        }
    }

    fn usage(line: &str) -> String {
        match command(&args(line)) {
            Err(Exit::Usage { message, .. }) => message,
            other => panic!("{line}: {other:?}"),
        }
    }

    #[test]
    fn failures_say_what_to_try() {
        assert_eq!(command(&[]), Err(Exit::NoCommand));
        assert_eq!(Exit::NoCommand.to_string(), HELP.trim_end());
        let cases = [
            (
                "serve",
                "straum: unknown command \"serve\"\ntry: straum help",
            ),
            (
                "help serve",
                "straum: unknown command \"serve\"\ntry: straum help",
            ),
            (
                "run",
                "straum: straum run needs --spec\ntry: straum run --help",
            ),
            (
                "status --spec s",
                "straum: straum status takes no \"--spec\"\ntry: straum status --help",
            ),
        ];
        for (line, printed) in cases {
            let exit = command(&args(line)).unwrap_err();
            assert_eq!(exit.to_string(), printed, "{line}");
            assert_eq!(exit.exit_code(), ExitCode::from(2), "{line}");
        }
        let exit = Exit::Runtime("reading s: gone".into());
        assert_eq!(exit.to_string(), "straum: reading s: gone");
        assert_eq!(exit.exit_code(), ExitCode::FAILURE);
    }

    #[test]
    fn command_errors() {
        let cases = [
            ("serve", "unknown command \"serve\""),
            ("help --spec s", "straum help takes one command"),
            ("help --spec", "unknown command \"--spec\""),
            ("help serve", "unknown command \"serve\""),
            ("help run plan", "straum help takes one command"),
            ("version run", "straum version takes nothing"),
            ("-V --spec s", "straum version takes nothing"),
            ("run --spec", "--spec needs a value"),
            ("run --spec a --spec b", "--spec given twice"),
            (
                "plan --spec s --listen x",
                "straum plan takes no \"--listen\"",
            ),
            ("run --spec=s", "straum run takes no \"--spec=s\""),
            ("run", "straum run needs --spec"),
            ("plan --role r", "straum plan needs --spec"),
            (
                "plan --spec s --max-lag 5",
                "straum plan takes no \"--max-lag\"",
            ),
            ("status --spec s", "straum status takes no \"--spec\""),
            ("status --url", "--url needs a value"),
            ("views --spec s", "straum views takes no \"--spec\""),
            ("shell now", "straum shell takes no \"now\""),
            ("ask", "straum ask needs a query"),
            ("ask --url u", "straum ask needs a query"),
            ("ask --url", "--url needs a value"),
            ("ask --spec s SELECT", "straum ask takes no \"--spec\""),
        ];
        for (line, error) in cases {
            assert_eq!(usage(line), error, "{line}");
        }
        for lag in ["0", "-1", "1.5", "30s", ""] {
            let line = format!("run --spec s --max-lag {lag}");
            let error = format!("--max-lag takes whole seconds above 0, not {lag:?}");
            let error = if lag.is_empty() {
                "--max-lag needs a value".into()
            } else {
                error
            };
            assert_eq!(usage(&line), error, "{lag}");
        }
        for size in ["0", "0Gi", "4GB", "4gi", "Gi", "16777216Ti"] {
            let line = format!("run --spec s --max-retained-wal {size}");
            let error = format!(
                "--max-retained-wal takes bytes above 0, as 4294967296 or 4Gi, not {size:?}"
            );
            assert_eq!(usage(&line), error, "{size}");
        }
        for rows in ["0", "-1", "1.5", "10k", "1Ki"] {
            let line = format!("run --spec s --max-transaction-rows {rows}");
            let error =
                format!("--max-transaction-rows takes a count of rows above 0, not {rows:?}");
            assert_eq!(usage(&line), error, "{rows}");
        }
    }

    #[test]
    fn reads_sizes() {
        let cases = [
            ("1", 1),
            ("4294967296", 4_294_967_296),
            ("1Ki", 1_024),
            ("3Mi", 3_145_728),
            ("4Gi", 4_294_967_296),
            ("2Ti", 2_199_023_255_552),
            ("16777215Ti", 18_446_742_974_197_923_840),
        ];
        for (text, expected) in cases {
            assert_eq!(bytes(text), Some(expected), "{text}");
        }
    }
}
