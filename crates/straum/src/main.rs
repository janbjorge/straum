//! The straum binary, where straum SQL, the PostgreSQL source, the use cases and
//! HTTP meet. One thread writes: it loads every table, then applies the
//! stream, and starts over after a failure. tokio serves asks meanwhile.

#![forbid(unsafe_code)]

use std::env;
use std::fmt::Display;
use std::fs;
use std::future::poll_fn;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::panic;
use std::process::{self, ExitCode};
use std::task::Poll;
use std::thread;
use std::time::Duration;

use straum_app::{App, Limits, Step, Writer};
use straum_domain::{Ask, Literal, Spec};
use straum_pg::{Dsn, Pg};
use tokio::signal::unix::{SignalKind, signal};

const COMMANDS: [&str; 3] = ["run", "plan", "status"];

const HELP: &str = "\
straum follows tables in PostgreSQL and answers asks over HTTP.

usage: straum <command> [flags]

  run     load the spec's tables, follow their changes, answer asks
  plan    print the SQL that sets PostgreSQL up for the spec
  status  print what a running straum has done
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
  --max-retained-wal BYTES     WAL held for the slot before straum starts
                               over (4Gi; takes Ki, Mi, Gi or Ti)
  --max-transaction-rows ROWS  rows one transaction may change before
                               straum starts over (100000)

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

Prints what the straum at URL has done: ready or why not, how far behind
PostgreSQL, the WAL held for it, its starts and its failures. In a pod:
kubectl exec deploy/straum -- /straum status

  --url URL  where straum serves HTTP (http://127.0.0.1:8080)

Guide: https://janbjorge.github.io/straum/running.html#straum-status
";

const LISTEN: &str = "0.0.0.0:8080";
const PUBLICATION: &str = "straum";
const ROLE: &str = "straum";
const URL: &str = "http://127.0.0.1:8080";
/// How long `straum status` waits for an answer.
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
const LIMITS: Limits = Limits {
    max_lag: Duration::from_secs(30),
    max_retained_wal: 4 << 30,
};
/// Rows one transaction may change before straum loads again instead of
/// applying it; `bench/transaction.py` measures what a transaction this
/// size holds the write lock for.
const MAX_TRANSACTION_ROWS: usize = 100_000;

/// The wait after a failure, doubled after each failure in a row up to
/// [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

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
    /// The help of one command, or of straum.
    Help(Option<&'static str>),
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

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let done = match command(&args) {
        Ok(Command::Help(topic)) => {
            print!("{}", help(topic));
            Ok(())
        }
        Ok(Command::Plan {
            spec,
            publication,
            role,
            dsn_file,
        }) => plan(&spec, &publication, role.as_deref(), dsn_file.as_deref()),
        Ok(Command::Run(args)) => run(args),
        Ok(Command::Status { url }) => status(&url).map(|text| print!("{text}")),
        Err(_) if args.is_empty() => {
            eprint!("{HELP}");
            return ExitCode::from(2);
        }
        Err(usage) => {
            let hint = match known(&args[0]) {
                Ok(name) => format!("straum {name} --help"),
                Err(_) => "straum help".into(),
            };
            eprintln!("straum: {usage}\ntry: {hint}");
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

fn help(topic: Option<&str>) -> &'static str {
    match topic {
        None => HELP,
        Some("run") => RUN_HELP,
        Some("plan") => PLAN_HELP,
        Some(_) => STATUS_HELP,
    }
}

fn known(name: &str) -> Result<&'static str, String> {
    (COMMANDS.into_iter())
        .find(|&command| command == name)
        .ok_or_else(|| format!("unknown command {name:?}"))
}

fn flags(name: &str) -> &'static [&'static str] {
    match name {
        "run" => &[
            "--spec",
            "--listen",
            "--publication",
            "--dsn-file",
            "--max-lag",
            "--max-retained-wal",
            "--max-transaction-rows",
        ],
        "plan" => &["--spec", "--publication", "--role", "--dsn-file"],
        _ => &["--url"],
    }
}

fn command(args: &[String]) -> Result<Command, String> {
    let Some((name, rest)) = args.split_first() else {
        return Err("no command".into());
    };
    if ["help", "--help", "-h"].contains(&name.as_str()) {
        return match rest {
            [] => Ok(Command::Help(None)),
            [topic] => known(topic).map(|name| Command::Help(Some(name))),
            _ => Err("straum help takes one command".into()),
        };
    }
    let name = known(name)?;
    // -h anywhere asks for help, whatever else is on the line.
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Command::Help(Some(name)));
    }
    let allowed = flags(name);
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
    if name == "status" {
        let url = flag("--url").unwrap_or_else(|| URL.into());
        return Ok(Command::Status { url });
    }
    let spec = flag("--spec").ok_or_else(|| format!("straum {name} needs --spec"))?;
    let publication = flag("--publication").unwrap_or_else(|| PUBLICATION.into());
    Ok(if name == "run" {
        let max_lag = match flag("--max-lag") {
            None => LIMITS.max_lag,
            Some(value) => seconds(&value)
                .ok_or_else(|| format!("--max-lag takes whole seconds above 0, not {value:?}"))?,
        };
        let max_retained_wal = match flag("--max-retained-wal") {
            None => LIMITS.max_retained_wal,
            Some(value) => bytes(&value).ok_or_else(|| {
                format!(
                    "--max-retained-wal takes bytes above 0, as 4294967296 or 4Gi, not {value:?}"
                )
            })?,
        };
        let max_transaction_rows = match flag("--max-transaction-rows") {
            None => MAX_TRANSACTION_ROWS,
            Some(value) => count(&value).ok_or_else(|| {
                format!("--max-transaction-rows takes a count of rows above 0, not {value:?}")
            })?,
        };
        Command::Run(Run {
            spec,
            listen: flag("--listen").unwrap_or_else(|| LISTEN.into()),
            publication,
            dsn_file: flag("--dsn-file"),
            limits: Limits {
                max_lag,
                max_retained_wal,
            },
            max_transaction_rows,
        })
    } else {
        Command::Plan {
            spec,
            publication,
            role: flag("--role"),
            dsn_file: flag("--dsn-file"),
        }
    })
}

fn seconds(text: &str) -> Option<Duration> {
    let seconds = text.parse().ok().filter(|&n| n > 0)?;
    Some(Duration::from_secs(seconds))
}

fn count(text: &str) -> Option<usize> {
    text.parse().ok().filter(|&n| n > 0)
}

/// A count of bytes, or of `Ki`, `Mi`, `Gi` or `Ti` as Kubernetes writes
/// them: `4Gi`.
fn bytes(text: &str) -> Option<u64> {
    let (count, shift) = [("Ki", 10), ("Mi", 20), ("Gi", 30), ("Ti", 40)]
        .into_iter()
        .find_map(|(unit, shift)| Some((text.strip_suffix(unit)?, shift)))
        .unwrap_or((text, 0));
    let count: u64 = count.parse().ok().filter(|&n| n > 0)?;
    count.checked_mul(1 << shift)
}

/// What the straum at `url` says it has done: GET /status over a plain
/// socket, since its image holds nothing else to ask with.
fn status(url: &str) -> Result<String, String> {
    let address = (url.strip_prefix("http://"))
        .map(|rest| rest.trim_end_matches('/'))
        .filter(|address| !address.is_empty() && !address.contains('/'))
        .ok_or_else(|| format!("--url takes http://HOST:PORT, not {url:?}"))?;
    let failed = |e: io::Error| format!("asking {url} for its status: {e}");
    let mut stream = TcpStream::connect(address).map_err(failed)?;
    stream
        .set_read_timeout(Some(STATUS_TIMEOUT))
        .map_err(failed)?;
    let request = format!("GET /status HTTP/1.1\r\nhost: {address}\r\nconnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).map_err(failed)?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).map_err(failed)?;
    match reply.split_once("\r\n\r\n") {
        Some((head, body)) if head.starts_with("HTTP/1.1 200 ") => Ok(body.to_owned()),
        Some((head, _)) => {
            let first = head.lines().next().unwrap_or_default();
            Err(format!("{url} answered {first:?}"))
        }
        None => Err(format!("{url} sent no HTTP reply")),
    }
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

/// The role and its password come from the DSN when there is one, so the
/// setup and `straum run` agree on them without being told twice.
fn plan(
    spec: &str,
    publication: &str,
    role: Option<&str>,
    dsn_file: Option<&str>,
) -> Result<(), String> {
    let spec = read_spec(spec)?;
    let dsn = match (dsn_file, env::var("STRAUM_DSN").is_ok()) {
        (None, false) => None,
        _ => Some(read_dsn(dsn_file)?),
    };
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
fn read_dsn(file: Option<&str>) -> Result<Dsn, String> {
    let text = match file {
        Some(path) => fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?,
        None => env::var("STRAUM_DSN").map_err(|_| "set STRAUM_DSN or pass --dsn-file")?,
    };
    Dsn::parse(text.trim()).map_err(|e| e.to_string())
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
    let dsn = read_dsn(dsn_file.as_deref())?;
    let listener = TcpListener::bind(&listen).map_err(|e| format!("listening on {listen}: {e}"))?;
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

    let app = App::new(spec, limits);
    let writer = Writer::new(
        app.clone(),
        Pg::new(dsn, &publication, max_transaction_rows),
    );
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
            straum_http::serve(listener, app, read_ask, stop).await
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
        for help in ["help", "--help", "-h"] {
            assert_eq!(command(&args(help)), Ok(Command::Help(None)));
        }
        for (line, name) in [
            ("help run", "run"),
            ("run --spec s -h", "run"),
            ("plan --help", "plan"),
            ("status -h --url", "status"),
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
        assert!(help(None).contains("usage: straum <command>"));
        for name in COMMANDS {
            let text = help(Some(name));
            assert!(text.starts_with(&format!("usage: straum {name}")), "{name}");
            assert!(help(None).contains(&format!("\n  {name} ")), "{name}");
            for flag in flags(name) {
                assert!(text.contains(&format!("  {flag} ")), "{name} {flag}");
            }
            for other in COMMANDS.into_iter().filter(|&other| other != name) {
                for flag in flags(other).iter().filter(|f| !flags(name).contains(f)) {
                    assert!(!text.contains(flag), "{name} {flag}");
                }
            }
        }
    }

    #[test]
    fn command_errors() {
        let cases = [
            ("", "no command"),
            ("serve", "unknown command \"serve\""),
            ("help --spec s", "straum help takes one command"),
            ("help --spec", "unknown command \"--spec\""),
            ("help serve", "unknown command \"serve\""),
            ("help run plan", "straum help takes one command"),
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
        ];
        for (line, error) in cases {
            assert_eq!(command(&args(line)), Err(error.to_owned()), "{line}");
        }
        for lag in ["0", "-1", "1.5", "30s", ""] {
            let line = args(&format!("run --spec s --max-lag {lag}"));
            let error = format!("--max-lag takes whole seconds above 0, not {lag:?}");
            let error = if lag.is_empty() {
                "--max-lag needs a value".into()
            } else {
                error
            };
            assert_eq!(command(&line), Err(error), "{lag}");
        }
        for size in ["0", "0Gi", "4GB", "4gi", "Gi", "16777216Ti"] {
            let line = args(&format!("run --spec s --max-retained-wal {size}"));
            let error = format!(
                "--max-retained-wal takes bytes above 0, as 4294967296 or 4Gi, not {size:?}"
            );
            assert_eq!(command(&line), Err(error), "{size}");
        }
        for rows in ["0", "-1", "1.5", "10k", "1Ki"] {
            let line = args(&format!("run --spec s --max-transaction-rows {rows}"));
            let error =
                format!("--max-transaction-rows takes a count of rows above 0, not {rows:?}");
            assert_eq!(command(&line), Err(error), "{rows}");
        }
    }

    /// A server that answers one GET /status with `reply`; its URL.
    fn answering(reply: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                request.push(byte[0]);
            }
            if request.starts_with(b"GET /status HTTP/1.1\r\n") {
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        url
    }

    #[test]
    fn asks_for_the_status() {
        let url = answering("HTTP/1.1 200 OK\r\ncontent-length: 11\r\n\r\nready: yes\n");
        assert_eq!(status(&url), Ok("ready: yes\n".to_owned()));
        let url = answering("HTTP/1.1 200 OK\r\n\r\n");
        assert_eq!(status(&format!("{url}/")), Ok(String::new()));
    }

    #[test]
    fn status_errors() {
        let url = answering("HTTP/1.1 404 Not Found\r\n\r\nno /status here\n");
        let answered = format!("{url} answered \"HTTP/1.1 404 Not Found\"");
        assert_eq!(status(&url), Err(answered));
        let url = answering("garbage");
        assert_eq!(status(&url), Err(format!("{url} sent no HTTP reply")));
        for bad in [
            "https://x:1",
            "x:1",
            "http://",
            "http:///",
            "http://x:1/status",
        ] {
            let error = format!("--url takes http://HOST:PORT, not {bad:?}");
            assert_eq!(status(bad), Err(error), "{bad}");
        }
        // Nothing listens on port 1.
        let err = status("http://127.0.0.1:1").unwrap_err();
        let asking = "asking http://127.0.0.1:1 for its status: ";
        assert!(err.starts_with(asking), "{err}");
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
