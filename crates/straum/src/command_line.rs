//! Reading a command line into a `Command`: which command, its flags
//! and their values, or why the line is wrong.

use std::fmt::{self, Display};
use std::time::Duration;

use straum_app::Limits;
use straum_pg::Replication;

use crate::help::{ASK_HELP, HELP, PLAN_HELP, RUN_HELP, SHELL_HELP, STATUS_HELP, VIEWS_HELP};

const LISTEN: &str = "0.0.0.0:8080";
const PUBLICATION: &str = "straum";
pub(crate) const ROLE: &str = "straum";
const URL: &str = "http://127.0.0.1:8080";
const LIMITS: Limits = Limits {
    max_lag: Duration::from_secs(30),
    max_retained_wal: 4 << 30,
};
/// Rows one transaction may change before Straum loads again instead of
/// applying it; `bench/taxi.py transactions` measures what a transaction this
/// size holds the write lock for.
const MAX_TRANSACTION_ROWS: usize = 100_000;
/// Rows a physical stream holds before starting over: a guard, far above
/// what a checkpoint cycle's pages and open transactions take.
const MAX_PAGE_MEMORY: usize = 1 << 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Name {
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

    pub(crate) fn help(self) -> &'static str {
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
                "--wal",
                "--publication",
                "--dsn-file",
                "--max-lag",
                "--max-retained-wal",
                "--max-transaction-rows",
                "--max-page-memory",
            ],
            Name::Plan => &["--spec", "--wal", "--publication", "--role", "--dsn-file"],
            Name::Status | Name::Views | Name::Ask | Name::Shell => &["--url"],
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Run(Run),
    Plan(Plan),
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
pub(crate) struct Run {
    pub(crate) spec: String,
    pub(crate) listen: String,
    pub(crate) replication: Replication,
    pub(crate) publication: String,
    pub(crate) dsn_file: Option<String>,
    pub(crate) limits: Limits,
    pub(crate) max_transaction_rows: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) spec: String,
    pub(crate) replication: Replication,
    pub(crate) publication: String,
    pub(crate) role: Option<String>,
    pub(crate) dsn_file: Option<String>,
}

/// A command line Straum can't run, printed with a hint at the help.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Usage {
    NoCommand,
    Wrong {
        error: UsageError,
        command: Option<Name>,
    },
}

impl Display for Usage {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Usage::NoCommand => formatter.write_str(HELP.trim_end()),
            Usage::Wrong { error, command } => {
                let hint = match command {
                    Some(name) => format!("{} --help", name.as_str()),
                    None => "help".to_owned(),
                };
                write!(formatter, "straum: {error}\ntry: straum {hint}")
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UsageError {
    UnknownCommand(String),
    HelpTakesOneCommand,
    VersionTakesNothing,
    AskNeedsQuery,
    PageMemoryNeedsPhysical,
    FlagNotTaken {
        command: Name,
        flag: String,
    },
    GivenTwice(String),
    NeedsValue(String),
    BadValue {
        flag: &'static str,
        takes: &'static str,
        value: String,
    },
    NeedsSpec(Name),
    BadWal(String),
}

impl Display for UsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            UsageError::UnknownCommand(text) => write!(formatter, "unknown command {text:?}"),
            UsageError::HelpTakesOneCommand => formatter.write_str("straum help takes one command"),
            UsageError::VersionTakesNothing => formatter.write_str("straum version takes nothing"),
            UsageError::AskNeedsQuery => formatter.write_str("straum ask needs a query"),
            UsageError::PageMemoryNeedsPhysical => {
                formatter.write_str("--max-page-memory takes --wal physical")
            }
            UsageError::FlagNotTaken { command, flag } => {
                write!(formatter, "straum {} takes no {flag:?}", command.as_str())
            }
            UsageError::GivenTwice(flag) => write!(formatter, "{flag} given twice"),
            UsageError::NeedsValue(flag) => write!(formatter, "{flag} needs a value"),
            UsageError::BadValue { flag, takes, value } => {
                write!(formatter, "{flag} takes {takes}, not {value:?}")
            }
            UsageError::NeedsSpec(command) => {
                write!(formatter, "straum {} needs --spec", command.as_str())
            }
            UsageError::BadWal(value) => {
                write!(formatter, "--wal takes logical or physical, not {value:?}")
            }
        }
    }
}

pub(crate) fn command(args: &[String]) -> Result<Command, Usage> {
    let Some((first, rest)) = args.split_first() else {
        return Err(Usage::NoCommand);
    };
    let name = Name::parse(first);
    let parsed = match name {
        Some(name) => flags(name, rest),
        None => help_or_version(first, rest),
    };
    parsed.map_err(|error| Usage::Wrong {
        error,
        command: name,
    })
}

fn help_or_version(word: &str, rest: &[String]) -> Result<Command, UsageError> {
    if ["help", "--help", "-h"].contains(&word) {
        return match rest {
            [] => Ok(Command::Help(None)),
            [topic] => match Name::parse(topic) {
                Some(name) => Ok(Command::Help(Some(name))),
                None => Err(UsageError::UnknownCommand(topic.clone())),
            },
            _ => Err(UsageError::HelpTakesOneCommand),
        };
    }
    if ["version", "--version", "-V"].contains(&word) {
        return match rest {
            [] => Ok(Command::Version),
            _ => Err(UsageError::VersionTakesNothing),
        };
    }
    Err(UsageError::UnknownCommand(word.to_owned()))
}

fn is_help(arg: &str) -> bool {
    arg == "-h" || arg == "--help"
}

/// Where an ask's flags end and its query begins: each flag takes one
/// value, a call for help none.
fn query_at(rest: &[String]) -> usize {
    let mut at = 0;
    while let Some(flag) = rest.get(at).filter(|arg| arg.starts_with('-')) {
        at += if is_help(flag) { 1 } else { 2 };
    }
    at.min(rest.len())
}

fn flags(name: Name, rest: &[String]) -> Result<Command, UsageError> {
    // The query and its parameters are read as written, so a parameter
    // may be `-h` or a negative number.
    let (rest, query) = match name {
        Name::Ask => rest.split_at(query_at(rest)),
        _ => (rest, &[][..]),
    };
    // -h anywhere asks for help, whatever else is on the line.
    if rest.iter().any(|arg| is_help(arg)) {
        return Ok(Command::Help(Some(name)));
    }
    let flags = Flags::read(name, rest)?;
    match name {
        Name::Status => Ok(Command::Status { url: flags.url() }),
        Name::Views => Ok(Command::Views { url: flags.url() }),
        Name::Shell => Ok(Command::Shell { url: flags.url() }),
        Name::Ask => ask_command(&flags, query),
        Name::Plan => plan_command(&flags),
        Name::Run => run_command(&flags),
    }
}

fn ask_command(flags: &Flags, query: &[String]) -> Result<Command, UsageError> {
    let Some((query, parameters)) = query.split_first() else {
        return Err(UsageError::AskNeedsQuery);
    };
    Ok(Command::Ask {
        url: flags.url(),
        query: query.clone(),
        parameters: parameters.to_vec(),
    })
}

fn plan_command(flags: &Flags) -> Result<Command, UsageError> {
    Ok(Command::Plan(Plan {
        spec: flags.spec()?,
        replication: flags.replication()?,
        publication: flags.publication(),
        role: flags.text("--role"),
        dsn_file: flags.text("--dsn-file"),
    }))
}

fn run_command(flags: &Flags) -> Result<Command, UsageError> {
    let is_physical = matches!(flags.replication()?, Replication::Physical { .. });
    if flags.value("--max-page-memory").is_some() && !is_physical {
        return Err(UsageError::PageMemoryNeedsPhysical);
    }
    // The spec comes first: without one, nothing else on the line matters.
    let spec = flags.spec()?;
    let replication = flags.replication()?;
    let max_lag = flags.parsed(
        "--max-lag",
        LIMITS.max_lag,
        seconds,
        "whole seconds above 0",
    )?;
    let max_retained_wal = flags.parsed(
        "--max-retained-wal",
        LIMITS.max_retained_wal,
        bytes,
        "bytes above 0, as 4294967296 or 4Gi",
    )?;
    let max_transaction_rows = flags.parsed(
        "--max-transaction-rows",
        MAX_TRANSACTION_ROWS,
        count,
        "a count of rows above 0",
    )?;
    Ok(Command::Run(Run {
        spec,
        listen: flags.text("--listen").unwrap_or_else(|| LISTEN.into()),
        replication,
        publication: flags.publication(),
        dsn_file: flags.text("--dsn-file"),
        limits: Limits {
            max_lag,
            max_retained_wal,
        },
        max_transaction_rows,
    }))
}

struct Given<'a> {
    flag: &'a str,
    value: &'a str,
}

/// The flags on one command's line, each with its value.
struct Flags<'a> {
    name: Name,
    given: Vec<Given<'a>>,
}

impl<'a> Flags<'a> {
    fn read(name: Name, rest: &'a [String]) -> Result<Flags<'a>, UsageError> {
        let mut flags = Flags {
            name,
            given: Vec::new(),
        };
        let mut rest = rest.iter();
        while let Some(flag) = rest.next() {
            if !name.flags().contains(&flag.as_str()) {
                return Err(UsageError::FlagNotTaken {
                    command: name,
                    flag: flag.clone(),
                });
            }
            if flags.value(flag).is_some() {
                return Err(UsageError::GivenTwice(flag.clone()));
            }
            let Some(value) = rest.next() else {
                return Err(UsageError::NeedsValue(flag.clone()));
            };
            flags.given.push(Given { flag, value });
        }
        Ok(flags)
    }

    fn value(&self, flag: &str) -> Option<&'a str> {
        let given = self.given.iter().find(|given| given.flag == flag)?;
        Some(given.value)
    }

    fn text(&self, flag: &str) -> Option<String> {
        self.value(flag).map(str::to_owned)
    }

    fn parsed<T>(
        &self,
        flag: &'static str,
        default: T,
        parse: fn(&str) -> Option<T>,
        takes: &'static str,
    ) -> Result<T, UsageError> {
        let Some(value) = self.value(flag) else {
            return Ok(default);
        };
        let Some(parsed) = parse(value) else {
            let value = value.to_owned();
            return Err(UsageError::BadValue { flag, takes, value });
        };
        Ok(parsed)
    }

    fn spec(&self) -> Result<String, UsageError> {
        let Some(spec) = self.text("--spec") else {
            return Err(UsageError::NeedsSpec(self.name));
        };
        Ok(spec)
    }

    fn publication(&self) -> String {
        self.text("--publication")
            .unwrap_or_else(|| PUBLICATION.into())
    }

    fn url(&self) -> String {
        self.text("--url").unwrap_or_else(|| URL.into())
    }

    fn replication(&self) -> Result<Replication, UsageError> {
        match self.value("--wal") {
            None | Some("logical") => Ok(Replication::Logical),
            Some("physical") => {
                let max_page_memory = self.parsed(
                    "--max-page-memory",
                    MAX_PAGE_MEMORY,
                    memory,
                    "bytes above 0, as 1073741824 or 1Gi",
                )?;
                Ok(Replication::Physical { max_page_memory })
            }
            Some(other) => Err(UsageError::BadWal(other.to_owned())),
        }
    }
}

fn positive(text: &str) -> Option<u64> {
    text.parse().ok().filter(|&number| number > 0)
}

fn seconds(text: &str) -> Option<Duration> {
    positive(text).map(Duration::from_secs)
}

fn count(text: &str) -> Option<usize> {
    usize::try_from(positive(text)?).ok()
}

fn memory(text: &str) -> Option<usize> {
    usize::try_from(bytes(text)?).ok()
}

// The suffixes are Kubernetes' own, so a chart's `4Gi` passes as written.
fn bytes(text: &str) -> Option<u64> {
    let (count, shift) = [("Ki", 10), ("Mi", 20), ("Gi", 30), ("Ti", 40)]
        .into_iter()
        .find_map(|(unit, shift)| Some((text.strip_suffix(unit)?, shift)))
        .unwrap_or((text, 0));
    positive(count)?.checked_mul(1 << shift)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn reads_run_and_plan() {
        assert_eq!(
            command(&args("run --spec s.sql")),
            Ok(Command::Run(Run {
                spec: "s.sql".into(),
                listen: LISTEN.into(),
                replication: Replication::Logical,
                publication: PUBLICATION.into(),
                dsn_file: None,
                limits: LIMITS,
                max_transaction_rows: MAX_TRANSACTION_ROWS,
            }))
        );
        assert_eq!(
            command(&args(
                "run --dsn-file d --listen 127.0.0.1:9 --publication p --spec s \
                 --max-lag 5 --max-retained-wal 512Mi --max-transaction-rows 2500 --wal physical \
                 --max-page-memory 256Mi"
            )),
            Ok(Command::Run(Run {
                spec: "s".into(),
                listen: "127.0.0.1:9".into(),
                replication: Replication::Physical {
                    max_page_memory: 256 << 20,
                },
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
            Ok(Command::Plan(Plan {
                spec: "s".into(),
                replication: Replication::Logical,
                publication: PUBLICATION.into(),
                role: None,
                dsn_file: None,
            }))
        );
        assert_eq!(
            command(&args("plan --role r --spec s --dsn-file d --wal physical")),
            Ok(Command::Plan(Plan {
                spec: "s".into(),
                replication: Replication::Physical {
                    max_page_memory: MAX_PAGE_MEMORY,
                },
                publication: PUBLICATION.into(),
                role: Some("r".into()),
                dsn_file: Some("d".into()),
            }))
        );
    }

    #[test]
    fn reads_the_commands_that_talk_to_a_running_straum() {
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
    }

    #[test]
    fn reads_calls_for_help_and_the_version() {
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
                for flag in other
                    .flags()
                    .iter()
                    .filter(|flag| !name.flags().contains(flag))
                {
                    assert!(!text.contains(flag), "{word} {flag}");
                }
            }
        }
    }

    fn usage(line: &str) -> String {
        match command(&args(line)) {
            Err(Usage::Wrong { error, .. }) => error.to_string(),
            other => panic!("{line}: {other:?}"),
        }
    }

    #[test]
    fn failures_say_what_to_try() {
        assert_eq!(command(&[]), Err(Usage::NoCommand));
        assert_eq!(Usage::NoCommand.to_string(), HELP.trim_end());
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
            let usage = command(&args(line)).unwrap_err();
            assert_eq!(usage.to_string(), printed, "{line}");
        }
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
                "run --spec s --wal Physical",
                "--wal takes logical or physical, not \"Physical\"",
            ),
            (
                "plan --spec s --wal both",
                "--wal takes logical or physical, not \"both\"",
            ),
            ("run --spec s --wal", "--wal needs a value"),
            (
                "run --spec s --max-page-memory 1Gi",
                "--max-page-memory takes --wal physical",
            ),
            (
                "run --spec s --wal logical --max-page-memory 1Gi",
                "--max-page-memory takes --wal physical",
            ),
            (
                "run --spec s --wal physical --max-page-memory 0",
                "--max-page-memory takes bytes above 0, as 1073741824 or 1Gi, not \"0\"",
            ),
            (
                "run --spec s --wal physical --max-page-memory 1Pi",
                "--max-page-memory takes bytes above 0, as 1073741824 or 1Gi, not \"1Pi\"",
            ),
            (
                "plan --spec s --max-page-memory 1Gi",
                "straum plan takes no \"--max-page-memory\"",
            ),
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
    }

    #[test]
    fn number_flags_say_what_they_take() {
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
