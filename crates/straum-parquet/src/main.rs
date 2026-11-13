//! `straum-parquet`, a feeder: it connects to the feed of a running
//! Straum and answers each table Straum asks for out of the Parquet files
//! its manifest names.

#![forbid(unsafe_code)]

mod file;
mod manifest;
mod value;

use std::env;
use std::fmt::{self, Display};
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use straum_domain::{ColumnType, Identifier, Table};
use straum_feed::{Error, Feeder, Request};

use crate::manifest::{Column, Manifest};

const HELP: &str = "\
usage: straum-parquet --manifest FILE --feed SOCKET

Feeds the tables FILE names, out of their Parquet files, to the Straum
that listens on SOCKET, started with straum run --feed SOCKET. It exits
once that Straum has every table of its spec.

  --manifest FILE  the tables: their files and how their columns read;
                   required
  --feed SOCKET    the Unix socket Straum listens on; required

Guide: https://janbjorge.github.io/straum/feed.html
";

const FLAGS: [&str; 2] = ["--manifest", "--feed"];

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Feed { manifest: String, feed: String },
    Help,
    Version,
}

#[derive(Debug, PartialEq, Eq)]
enum Exit {
    Usage(String),
    Runtime(String),
}

impl Exit {
    fn exit_code(&self) -> ExitCode {
        match self {
            Exit::Usage(_) => ExitCode::from(2),
            Exit::Runtime(_) => ExitCode::FAILURE,
        }
    }
}

impl Display for Exit {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Exit::Usage(message) => {
                write!(
                    formatter,
                    "straum-parquet: {message}\ntry: straum-parquet --help"
                )
            }
            Exit::Runtime(message) => write!(formatter, "straum-parquet: {message}"),
        }
    }
}

/// What stops a table: this end, which tells Straum why, or the feed.
#[derive(Debug)]
enum Failure {
    Table(String),
    Feed(Error),
}

impl From<String> for Failure {
    fn from(why: String) -> Failure {
        Failure::Table(why)
    }
}

impl From<Error> for Failure {
    fn from(e: Error) -> Failure {
        Failure::Feed(e)
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

fn command(args: &[String]) -> Result<Command, Exit> {
    // -h anywhere asks for help, whatever else is on the line.
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Command::Help);
    }
    if let [only] = args
        && (only == "--version" || only == "-V")
    {
        return Ok(Command::Version);
    }
    let mut given: Vec<(&str, &str)> = Vec::new();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        if !FLAGS.contains(&flag.as_str()) {
            return Err(Exit::Usage(format!("straum-parquet takes no {flag:?}")));
        }
        if given.iter().any(|(seen, _)| seen == flag) {
            return Err(Exit::Usage(format!("{flag} given twice")));
        }
        let value = rest
            .next()
            .ok_or_else(|| Exit::Usage(format!("{flag} needs a value")))?;
        given.push((flag, value));
    }
    let value = |flag: &str| {
        (given.iter())
            .find(|(seen, _)| *seen == flag)
            .map(|(_, value)| (*value).to_owned())
            .ok_or_else(|| Exit::Usage(format!("straum-parquet needs {flag}")))
    };
    Ok(Command::Feed {
        manifest: value("--manifest")?,
        feed: value("--feed")?,
    })
}

fn execute(command: Command) -> Result<(), String> {
    match command {
        Command::Help => {
            print!("{HELP}");
            Ok(())
        }
        Command::Version => {
            // CI builds with the release's version; Cargo.toml never names it.
            let version = option_env!("STRAUM_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
            println!("straum-parquet {version}");
            Ok(())
        }
        Command::Feed { manifest, feed } => {
            let manifest = read_manifest(&manifest)?;
            let mut feeder = Feeder::connect(&feed).map_err(|e| match e {
                Error::Io(e) => format!("connecting to the feed at {feed}: {e}"),
                e => e.to_string(),
            })?;
            serve(&manifest, &mut feeder, &mut log)?;
            log("Straum has every table".into());
            Ok(())
        }
    }
}

fn read_manifest(path: &str) -> Result<Manifest, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
    // A path with no directory is in the current one.
    let directory = Path::new(path).parent().unwrap_or(Path::new(""));
    manifest::parse(&text, directory).map_err(|e| format!("{path}: {e}"))
}

/// Answers every table Straum asks for until it is done. `fed` hears of
/// each table once it is whole.
fn serve(
    manifest: &Manifest,
    feeder: &mut Feeder,
    fed: &mut dyn FnMut(String),
) -> Result<(), String> {
    loop {
        let (table, columns) = match feeder.next_request().map_err(|e| e.to_string())? {
            Request::Done => return Ok(()),
            Request::Load { table, columns } => (table, columns),
        };
        match load(manifest, feeder, &table, &columns) {
            Ok(rows) => fed(format!(
                "fed {rows} rows of {}.{}",
                table.schema, table.name
            )),
            Err(Failure::Feed(e)) => return Err(e.to_string()),
            Err(Failure::Table(why)) => {
                // Straum may be gone, and this end's failure stands alone.
                let _ = feeder.fail(&why);
                return Err(why);
            }
        }
    }
}

/// Feeds one table: its types, then its rows out of each file. The rows
/// fed.
fn load(
    manifest: &Manifest,
    feeder: &mut Feeder,
    table: &Table,
    columns: &[Identifier],
) -> Result<u64, Failure> {
    let (schema, name) = (&table.schema, &table.name);
    let held = manifest
        .table(schema, name)
        .ok_or_else(|| format!("the manifest has no table {schema}.{name}"))?;
    let mut asked: Vec<&Column> = Vec::new();
    for column in columns {
        let found = held.column(column).ok_or_else(|| {
            format!("the manifest has no column {column} for table {schema}.{name}")
        })?;
        asked.push(found);
    }
    let types: Vec<ColumnType> = asked.iter().map(|column| column.column_type).collect();
    feeder.types(&types)?;
    let mut rows = 0;
    for path in &held.files {
        rows += file::read(path, &asked, |batch| {
            feeder.rows(batch).map_err(Failure::Feed)
        })?;
    }
    feeder.end()?;
    Ok(rows)
}

fn log(message: String) {
    eprintln!("straum-parquet: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn reads_commands() {
        let feed = Command::Feed {
            manifest: "m".into(),
            feed: "s".into(),
        };
        assert_eq!(command(&args("--manifest m --feed s")), Ok(feed));
        let feed = Command::Feed {
            manifest: "m".into(),
            feed: "s".into(),
        };
        assert_eq!(command(&args("--feed s --manifest m")), Ok(feed));
        for help in ["-h", "--help", "--manifest m -h", "--nonsense --help"] {
            assert_eq!(command(&args(help)), Ok(Command::Help), "{help}");
        }
        for version in ["-V", "--version"] {
            assert_eq!(command(&args(version)), Ok(Command::Version));
        }
    }

    #[test]
    fn help_names_every_flag() {
        assert!(HELP.starts_with("usage: straum-parquet --manifest FILE --feed SOCKET\n"));
        for flag in FLAGS {
            assert!(HELP.contains(&format!("\n  {flag} ")), "{flag}");
        }
        assert!(HELP.lines().all(|line| line.len() <= 78));
    }

    #[test]
    fn command_errors() {
        let cases = [
            ("", "straum-parquet needs --manifest"),
            ("--manifest m", "straum-parquet needs --feed"),
            ("--feed s", "straum-parquet needs --manifest"),
            ("--manifest", "--manifest needs a value"),
            ("--manifest m --feed", "--feed needs a value"),
            ("--manifest a --manifest b", "--manifest given twice"),
            ("--feed=s", "straum-parquet takes no \"--feed=s\""),
            ("m s", "straum-parquet takes no \"m\""),
            (
                "--version --feed s",
                "straum-parquet takes no \"--version\"",
            ),
            (
                "--manifest m --feed s --spec x",
                "straum-parquet takes no \"--spec\"",
            ),
        ];
        for (line, message) in cases {
            let exit = command(&args(line)).unwrap_err();
            assert_eq!(exit, Exit::Usage(message.into()), "{line}");
        }
    }

    #[test]
    fn failures_say_what_to_try() {
        let usage = Exit::Usage("straum-parquet needs --feed".into());
        assert_eq!(
            usage.to_string(),
            "straum-parquet: straum-parquet needs --feed\ntry: straum-parquet --help"
        );
        assert_eq!(usage.exit_code(), ExitCode::from(2));
        let runtime = Exit::Runtime("reading m: gone".into());
        assert_eq!(runtime.to_string(), "straum-parquet: reading m: gone");
        assert_eq!(runtime.exit_code(), ExitCode::FAILURE);
    }

    #[test]
    fn a_manifest_is_read_from_its_own_directory() {
        let scratch = file::tests::Scratch::new();
        let path = scratch.0.join("taxi.manifest");
        let text = "table public.t\nfile t.parquet\ncolumn a int from a\n";
        fs::write(&path, text).unwrap();
        let manifest = read_manifest(path.to_str().unwrap()).unwrap();
        assert_eq!(manifest.tables[0].files, [scratch.0.join("t.parquet")]);

        fs::write(&path, "table public.t\n").unwrap();
        let path = path.to_str().unwrap();
        assert_eq!(
            read_manifest(path).unwrap_err(),
            format!("{path}: line 1: table public.t has no file")
        );
        let missing = scratch.0.join("missing");
        let error = read_manifest(missing.to_str().unwrap()).unwrap_err();
        assert!(
            error.starts_with(&format!("reading {}: ", missing.display())),
            "{error}"
        );
    }
}
