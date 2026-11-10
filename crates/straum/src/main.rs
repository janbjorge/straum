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

const USAGE: &str = "\
usage:
  straum run --spec FILE [--listen ADDRESS] [--publication NAME] [--dsn-file FILE]
             [--max-lag SECONDS] [--max-retained-wal BYTES]
             [--max-transaction-rows ROWS]
  straum plan --spec FILE [--publication NAME] [--role NAME]
  straum status [--url URL]
  straum help

straum run loads every table FILE names, follows PostgreSQL's changes to them
and answers asks over HTTP on ADDRESS (0.0.0.0:8080). It reads the
publication NAME (straum) as a replication user. The DSN,
postgres://user:password@host:port/database, comes from --dsn-file or the
STRAUM_DSN environment variable. straum connects with TLS and checks the
server's certificate and name against the roots built in, or against the CA
file given by ?sslrootcert=FILE; ?sslmode=disable connects without TLS.
/readyz turns off while straum is more than SECONDS (30) behind PostgreSQL.
Once PostgreSQL holds more than BYTES (4Gi) of WAL for straum's slot, straum
drops the slot and loads again, so it can't fill the server's disk; BYTES
takes a Ki, Mi, Gi or Ti suffix. A transaction that changes more than ROWS
(100000) rows of the spec's tables makes straum load again too, from a
snapshot after it, instead of holding and applying it all at once. SIGTERM
or SIGINT stops it.

straum plan prints the SQL that sets PostgreSQL up for FILE: logical
decoding, the role NAME (straum), its grants and the publication.

straum status prints what the straum at URL (http://127.0.0.1:8080) has
done: whether it is ready and why not, how far behind PostgreSQL it is, the
WAL PostgreSQL holds for it, its starts and its failures. In a pod:
kubectl exec deploy/straum -- /straum status.
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
    Run {
        spec: String,
        listen: String,
        publication: String,
        dsn_file: Option<String>,
        limits: Limits,
        max_transaction_rows: usize,
    },
    Plan {
        spec: String,
        publication: String,
        role: String,
    },
    Status {
        url: String,
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
            limits,
            max_transaction_rows,
        }) => run(
            &spec,
            &listen,
            &publication,
            dsn_file.as_deref(),
            limits,
            max_transaction_rows,
        ),
        Ok(Command::Status { url }) => status(&url).map(|text| print!("{text}")),
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
        "run" => &[
            "--spec",
            "--listen",
            "--publication",
            "--dsn-file",
            "--max-lag",
            "--max-retained-wal",
            "--max-transaction-rows",
        ],
        "plan" => &["--spec", "--publication", "--role"],
        "status" => &["--url"],
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
        Command::Run {
            spec,
            listen: flag("--listen").unwrap_or_else(|| LISTEN.into()),
            publication,
            dsn_file: flag("--dsn-file"),
            limits: Limits {
                max_lag,
                max_retained_wal,
            },
            max_transaction_rows,
        }
    } else {
        Command::Plan {
            spec,
            publication,
            role: flag("--role").unwrap_or_else(|| ROLE.into()),
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

fn run(
    spec: &str,
    listen: &str,
    publication: &str,
    dsn_file: Option<&str>,
    limits: Limits,
    max_transaction_rows: usize,
) -> Result<(), String> {
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

    let app = App::new(spec, limits);
    let writer = Writer::new(app.clone(), Pg::new(dsn, publication, max_transaction_rows));
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
            Ok(Command::Run {
                spec: "s.sql".into(),
                listen: LISTEN.into(),
                publication: PUBLICATION.into(),
                dsn_file: None,
                limits: LIMITS,
                max_transaction_rows: MAX_TRANSACTION_ROWS,
            })
        );
        assert_eq!(
            command(&args(
                "run --dsn-file d --listen 127.0.0.1:9 --publication p --spec s \
                 --max-lag 5 --max-retained-wal 512Mi --max-transaction-rows 2500"
            )),
            Ok(Command::Run {
                spec: "s".into(),
                listen: "127.0.0.1:9".into(),
                publication: "p".into(),
                dsn_file: Some("d".into()),
                limits: Limits {
                    max_lag: Duration::from_secs(5),
                    max_retained_wal: 536_870_912,
                },
                max_transaction_rows: 2500,
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
