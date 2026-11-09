//! The straum binary, where straum SQL, the PostgreSQL source, the use cases and
//! HTTP meet. One thread writes: it loads every table, then applies the
//! stream, and starts over after a failure. tokio serves asks meanwhile.

#![forbid(unsafe_code)]

use std::env;
use std::fmt::Display;
use std::fs;
use std::future::poll_fn;
use std::net::TcpListener;
use std::panic;
use std::process::{self, ExitCode};
use std::task::Poll;
use std::thread;
use std::time::Duration;

use straum_app::{App, Step, Writer};
use straum_domain::Spec;
use straum_pg::{Dsn, Pg};
use tokio::signal::unix::{SignalKind, signal};

const USAGE: &str = "\
usage:
  straum run --spec FILE [--listen ADDRESS] [--publication NAME] [--dsn-file FILE]
  straum plan --spec FILE [--publication NAME] [--role NAME]
  straum help

straum run loads every table FILE names, follows PostgreSQL's changes to them
and answers asks over HTTP on ADDRESS (0.0.0.0:8080). It reads the
publication NAME (straum) as a replication user. The DSN,
postgres://user:password@host:port/database, comes from --dsn-file or the
STRAUM_DSN environment variable. straum connects with TLS and checks the
server's certificate and name against the roots built in, or against the CA
file given by ?sslrootcert=FILE; ?sslmode=disable connects without TLS.
SIGTERM or SIGINT stops it.

straum plan prints the SQL that sets PostgreSQL up for FILE: logical
decoding, the role NAME (straum), its grants and the publication.
";

const LISTEN: &str = "0.0.0.0:8080";
const PUBLICATION: &str = "straum";
const ROLE: &str = "straum";

/// The wait after a failure, doubled after each failure in a row up to
/// [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Run {
        spec: String,
        listen: String,
        publication: String,
        dsn_file: Option<String>,
    },
    Plan {
        spec: String,
        publication: String,
        role: String,
    },
    Help,
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let done = match command(&args) {
        Ok(Command::Help) => {
            print!("{USAGE}");
            Ok(())
        }
        Ok(Command::Plan {
            spec,
            publication,
            role,
        }) => {
            read_spec(&spec).map(|spec| print!("{}", straum_pg::plan(&spec, &publication, &role)))
        }
        Ok(Command::Run {
            spec,
            listen,
            publication,
            dsn_file,
        }) => run(&spec, &listen, &publication, dsn_file.as_deref()),
        Err(usage) => {
            eprint!("straum: {usage}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log(e);
            ExitCode::FAILURE
        }
    }
}

fn command(args: &[String]) -> Result<Command, String> {
    let Some((name, rest)) = args.split_first() else {
        return Err("no command".into());
    };
    let allowed: &[&str] = match name.as_str() {
        "run" => &["--spec", "--listen", "--publication", "--dsn-file"],
        "plan" => &["--spec", "--publication", "--role"],
        "help" | "--help" | "-h" if rest.is_empty() => return Ok(Command::Help),
        other => return Err(format!("unknown command {other:?}")),
    };
    let mut flags: Vec<(&str, &str)> = Vec::new();
    let mut rest = rest.iter();
    while let Some(flag) = rest.next() {
        if !allowed.contains(&flag.as_str()) {
            return Err(format!("straum {name} takes no {flag:?}"));
        }
        if flags.iter().any(|(f, _)| f == flag) {
            return Err(format!("{flag} given twice"));
        }
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        flags.push((flag, value));
    }
    let flag = |name: &str| {
        (flags.iter())
            .find(|(f, _)| *f == name)
            .map(|(_, value)| (*value).to_owned())
    };
    let spec = flag("--spec").ok_or_else(|| format!("straum {name} needs --spec"))?;
    let publication = flag("--publication").unwrap_or_else(|| PUBLICATION.into());
    Ok(if name == "run" {
        Command::Run {
            spec,
            listen: flag("--listen").unwrap_or_else(|| LISTEN.into()),
            publication,
            dsn_file: flag("--dsn-file"),
        }
    } else {
        Command::Plan {
            spec,
            publication,
            role: flag("--role").unwrap_or_else(|| ROLE.into()),
        }
    })
}

fn read_spec(path: &str) -> Result<Spec, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
    straum_notation::parse(&text).map_err(|e| {
        let lines: Vec<String> = (e.to_string().lines())
            .map(|line| format!("{path}: {line}"))
            .collect();
        lines.join("\n")
    })
}

/// From the file, else from `STRAUM_DSN`. Never from the command line, where
/// other users of the machine can read it.
fn read_dsn(file: Option<&str>) -> Result<Dsn, String> {
    let text = match file {
        Some(path) => fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?,
        None => env::var("STRAUM_DSN").map_err(|_| "set STRAUM_DSN or pass --dsn-file")?,
    };
    Dsn::parse(text.trim()).map_err(|e| e.to_string())
}

fn run(spec: &str, listen: &str, publication: &str, dsn_file: Option<&str>) -> Result<(), String> {
    let spec = read_spec(spec)?;
    let dsn = read_dsn(dsn_file)?;
    let listener = TcpListener::bind(listen).map_err(|e| format!("listening on {listen}: {e}"))?;
    let address = listener
        .local_addr()
        .map_err(|e| format!("listening on {listen}: {e}"))?;

    // After a panic the store may be half written, or nothing writes it
    // any more. Stop, rather than answer from it.
    let report = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        report(info);
        process::abort();
    }));

    let app = App::new(spec);
    let writer = Writer::new(app.clone(), Pg::new(dsn, publication));
    thread::Builder::new()
        .name("writer".into())
        .spawn(move || write(writer))
        .map_err(|e| format!("starting the writer: {e}"))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("starting tokio: {e}"))?;
    runtime
        .block_on(async {
            let mut terminate = signal(SignalKind::terminate())?;
            let mut interrupt = signal(SignalKind::interrupt())?;
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
            straum_http::serve(listener, app, stop).await
        })
        .map_err(|e| format!("serving HTTP: {e}"))?;
    log("stopped");
    // Leaving closes the replication connections, and PostgreSQL drops
    // their temporary slots.
    Ok(())
}

/// Steps the writer for as long as the process runs.
fn write(mut writer: Writer<Pg>) {
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
            Ok(Command::Run {
                spec: "s.sql".into(),
                listen: LISTEN.into(),
                publication: PUBLICATION.into(),
                dsn_file: None,
            })
        );
        assert_eq!(
            command(&args(
                "run --dsn-file d --listen 127.0.0.1:9 --publication p --spec s"
            )),
            Ok(Command::Run {
                spec: "s".into(),
                listen: "127.0.0.1:9".into(),
                publication: "p".into(),
                dsn_file: Some("d".into()),
            })
        );
        assert_eq!(
            command(&args("plan --role r --spec s")),
            Ok(Command::Plan {
                spec: "s".into(),
                publication: PUBLICATION.into(),
                role: "r".into(),
            })
        );
        for help in ["help", "--help", "-h"] {
            assert_eq!(command(&args(help)), Ok(Command::Help));
        }
    }

    #[test]
    fn command_errors() {
        let cases = [
            ("", "no command"),
            ("serve", "unknown command \"serve\""),
            ("help --spec s", "unknown command \"help\""),
            ("run --spec", "--spec needs a value"),
            ("run --spec a --spec b", "--spec given twice"),
            (
                "plan --spec s --listen x",
                "straum plan takes no \"--listen\"",
            ),
            ("run --spec=s", "straum run takes no \"--spec=s\""),
            ("run", "straum run needs --spec"),
            ("plan --role r", "straum plan needs --spec"),
        ];
        for (line, error) in cases {
            assert_eq!(command(&args(line)), Err(error.to_owned()), "{line}");
        }
    }
}
