//! Reading a command line into a `Command`. `command` takes the words
//! after `straum`: the first names the command. `run` and `plan` take
//! the spec as the one word that is not a flag; `ask` takes its query
//! and parameters after the flags. `Flags::read` checks the flags
//! against the ones that command takes, and each `*_command` function
//! builds the command from them, filling in defaults. A line Straum
//! can't run becomes a `Usage` error that `main.rs` prints.

use std::fmt::{self, Display};
use std::time::Duration;

use straum_app::Limits;
use straum_domain::closest;
use straum_pg::Replication;

use crate::client::Output;
use crate::help::{ASK_HELP, HELP, PLAN_HELP, RUN_HELP, SHELL_HELP, STATUS_HELP, VIEWS_HELP};

/// Where `straum run` listens unless told.
const LISTEN: &str = "0.0.0.0:8080";
const PUBLICATION: &str = "straum";
/// The role `straum plan` sets up when neither `--role` nor a DSN names one.
pub(crate) const ROLE: &str = "straum";
/// Where the commands that talk to a running Straum find it unless told.
const URL: &str = "http://127.0.0.1:8080";
/// The switch that asks for JSON instead of a table.
const JSON: &str = "--json";
/// The environment variable that names the URL when `--url` doesn't.
pub(crate) const URL_VARIABLE: &str = "STRAUM_URL";
const FOUR_GIBIBYTES: u64 = 4 << 30;
const LIMITS: Limits = Limits {
    max_lag: Duration::from_secs(30),
    max_retained_wal: FOUR_GIBIBYTES,
};
/// Rows one transaction may change before Straum loads again instead of
/// applying it; `bench/taxi.py transactions` measures what a transaction this
/// size holds the write lock for.
const MAX_TRANSACTION_ROWS: usize = 100_000;
/// Rows a physical stream holds before starting over: a guard, far above
/// what a checkpoint cycle's pages and open transactions take.
const MAX_PAGE_MEMORY: usize = 1 << 30;

/// A command's name, the first word after `straum`.
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
        for name in Name::ALL {
            if name.as_str() == text {
                return Some(name);
            }
        }
        None
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

    /// What `straum <name> --help` prints.
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
                "--listen",
                "--wal",
                "--publication",
                "--dsn-file",
                "--max-lag",
                "--max-retained-wal",
                "--max-transaction-rows",
                "--max-page-memory",
            ],
            Name::Plan => &["--wal", "--publication", "--role", "--dsn-file"],
            Name::Status | Name::Views | Name::Ask | Name::Shell => &["--url"],
        }
    }

    /// The flags that take no value.
    fn switches(self) -> &'static [&'static str] {
        match self {
            Name::Status | Name::Views | Name::Ask => &[JSON],
            Name::Run | Name::Plan | Name::Shell => &[],
        }
    }
}

/// A command line Straum can run.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Run(Run),
    Plan(Plan),
    Status {
        url: String,
        output: Output,
    },
    Views {
        url: String,
        output: Output,
    },
    Ask {
        url: String,
        query: String,
        /// As written on the command line.
        parameters: Vec<String>,
        output: Output,
    },
    Shell {
        url: String,
    },
    Help(Option<Name>),
    Version,
}

/// What `straum run` was told.
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

/// What `straum plan` was told.
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

/// What is wrong with a command line.
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
    SwitchTakesNoValue(String),
    BadValue {
        flag: &'static str,
        takes: &'static str,
        value: String,
    },
    NeedsSpec(Name),
    OneSpec(Name),
    BadWal(String),
}

impl Display for UsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            UsageError::UnknownCommand(text) => {
                write!(formatter, "unknown command {text:?}")?;
                let mut names = vec!["help", "version"];
                for name in Name::ALL {
                    names.push(name.as_str());
                }
                did_you_mean(formatter, text, &names)
            }
            UsageError::HelpTakesOneCommand => formatter.write_str("straum help takes one command"),
            UsageError::VersionTakesNothing => formatter.write_str("straum version takes nothing"),
            UsageError::AskNeedsQuery => formatter.write_str("straum ask needs a query"),
            UsageError::PageMemoryNeedsPhysical => {
                formatter.write_str("--max-page-memory takes --wal physical")
            }
            UsageError::FlagNotTaken { command, flag } => {
                write!(formatter, "straum {} takes no {flag:?}", command.as_str())?;
                let mut flags = command.flags().to_vec();
                flags.extend(command.switches());
                did_you_mean(formatter, flag, &flags)
            }
            UsageError::GivenTwice(flag) => write!(formatter, "{flag} given twice"),
            UsageError::NeedsValue(flag) => write!(formatter, "{flag} needs a value"),
            UsageError::SwitchTakesNoValue(flag) => write!(formatter, "{flag} takes no value"),
            UsageError::BadValue { flag, takes, value } => {
                write!(formatter, "{flag} takes {takes}, not {value:?}")
            }
            UsageError::NeedsSpec(command) => {
                write!(formatter, "straum {} needs a spec", command.as_str())
            }
            UsageError::OneSpec(command) => {
                write!(formatter, "straum {} takes one spec", command.as_str())
            }
            UsageError::BadWal(value) => {
                write!(formatter, "--wal takes logical or physical, not {value:?}")
            }
        }
    }
}

/// What the environment sets that a command line may leave out.
#[derive(Debug, Default)]
pub(crate) struct Environment {
    /// `STRAUM_URL`, if set.
    pub(crate) url: Option<String>,
}

impl Environment {
    /// The URL a command uses without `--url`: the variable, unless empty.
    fn url(&self) -> &str {
        match self.url.as_deref() {
            Some(url) if !url.is_empty() => url,
            _ => URL,
        }
    }
}

/// `; did you mean x?` when one of `names` is a likely slip for `written`.
fn did_you_mean(formatter: &mut fmt::Formatter, written: &str, names: &[&str]) -> fmt::Result {
    match closest(written, names) {
        Some(name) => write!(formatter, "; did you mean {name}?"),
        None => Ok(()),
    }
}

/// The command the words after `straum` name, or why Straum can't run
/// them.
pub(crate) fn command(args: &[String], environment: &Environment) -> Result<Command, Usage> {
    let Some((first, rest)) = args.split_first() else {
        return Err(Usage::NoCommand);
    };
    let name = Name::parse(first);
    let parsed = match name {
        Some(name) => flags(name, rest, environment.url()),
        None => help_or_version(first, rest),
    };
    match parsed {
        Ok(command) => Ok(command),
        Err(error) => Err(Usage::Wrong {
            error,
            command: name,
        }),
    }
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

/// `--`, after which every word is taken as written, never as a flag.
const END_OF_FLAGS: &str = "--";

/// Whether a flag before any `--` asks for help.
fn asks_for_help(args: &[String]) -> bool {
    for arg in args {
        if arg == END_OF_FLAGS {
            return false;
        }
        if is_help(arg) {
            return true;
        }
    }
    false
}

/// Where an ask's flags end and its query begins: each flag takes the
/// next word as its value unless it has one after `=`; a call for help or
/// a switch takes none.
fn query_at(rest: &[String]) -> usize {
    let mut at = 0;
    while let Some(arg) = rest.get(at) {
        if !arg.starts_with('-') || arg == END_OF_FLAGS {
            break;
        }
        let alone = is_help(arg) || Name::Ask.switches().contains(&arg.as_str());
        at += if alone || Written::of(arg).attached.is_some() {
            1
        } else {
            2
        };
    }
    at.min(rest.len())
}

fn flags(name: Name, rest: &[String], url: &str) -> Result<Command, UsageError> {
    // The query and its parameters are read as written, so a parameter
    // may be `-h` or a negative number. Everywhere else, -h anywhere
    // asks for help.
    if name != Name::Ask && asks_for_help(rest) {
        return Ok(Command::Help(Some(name)));
    }
    match name {
        Name::Status => {
            let flags = Flags::read(name, rest)?;
            Ok(Command::Status {
                url: flags.url(url),
                output: flags.output(),
            })
        }
        Name::Views => {
            let flags = Flags::read(name, rest)?;
            Ok(Command::Views {
                url: flags.url(url),
                output: flags.output(),
            })
        }
        Name::Shell => Ok(Command::Shell {
            url: Flags::read(name, rest)?.url(url),
        }),
        Name::Ask => {
            let (rest, query) = rest.split_at(query_at(rest));
            if asks_for_help(rest) {
                return Ok(Command::Help(Some(name)));
            }
            // A query that starts with `-` follows a `--`.
            let query = match query.split_first() {
                Some((first, after)) if first == END_OF_FLAGS => after,
                _ => query,
            };
            ask_command(&Flags::read(name, rest)?, query, url)
        }
        Name::Plan => plan_command(&Flags::read(name, rest)?),
        Name::Run => run_command(&Flags::read(name, rest)?),
    }
}

fn ask_command(flags: &Flags, query: &[String], url: &str) -> Result<Command, UsageError> {
    let Some((query, parameters)) = query.split_first() else {
        return Err(UsageError::AskNeedsQuery);
    };
    Ok(Command::Ask {
        url: flags.url(url),
        query: query.clone(),
        parameters: parameters.to_vec(),
        output: flags.output(),
    })
}

fn plan_command(flags: &Flags) -> Result<Command, UsageError> {
    let Some(spec) = flags.spec.clone() else {
        return Err(UsageError::NeedsSpec(Name::Plan));
    };
    Ok(Command::Plan(Plan {
        spec,
        replication: flags.replication()?,
        publication: flags.publication(),
        role: flags.text("--role"),
        dsn_file: flags.text("--dsn-file"),
    }))
}

fn run_command(flags: &Flags) -> Result<Command, UsageError> {
    let replication = flags.replication()?;
    let is_physical = matches!(replication, Replication::Physical { .. });
    if flags.value("--max-page-memory").is_some() && !is_physical {
        return Err(UsageError::PageMemoryNeedsPhysical);
    }
    let Some(spec) = flags.spec.clone() else {
        return Err(UsageError::NeedsSpec(Name::Run));
    };
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
        listen: flags.text_or("--listen", LISTEN),
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

/// A word that starts a flag: `--url`, or `--url=VALUE` with its value
/// attached, as getopt reads a long option.
struct Written<'a> {
    flag: &'a str,
    attached: Option<&'a str>,
}

impl Written<'_> {
    // A lifetime: the flag and its value are slices of the word read.
    fn of(word: &str) -> Written<'_> {
        if word.starts_with("--")
            && let Some((flag, value)) = word.split_once('=')
        {
            return Written {
                flag,
                attached: Some(value),
            };
        }
        Written {
            flag: word,
            attached: None,
        }
    }
}

/// One flag on the line and the value after it.
struct Given {
    flag: String,
    value: String,
}

/// The flags on one command's line, each with its value, the switches,
/// and the spec for the commands that take one.
struct Flags {
    given: Vec<Given>,
    switches: Vec<String>,
    spec: Option<String>,
}

impl Flags {
    /// A flag takes the next word, so for `run` and `plan` the spec is
    /// whichever word is left.
    fn read(name: Name, rest: &[String]) -> Result<Flags, UsageError> {
        let takes_spec = matches!(name, Name::Run | Name::Plan);
        let mut flags = Flags {
            given: Vec::new(),
            switches: Vec::new(),
            spec: None,
        };
        let mut rest = rest.iter();
        let mut flags_ended = false;
        while let Some(word) = rest.next() {
            if word == END_OF_FLAGS && !flags_ended {
                flags_ended = true;
                continue;
            }
            if takes_spec && (flags_ended || !word.starts_with('-')) {
                if flags.spec.is_some() {
                    return Err(UsageError::OneSpec(name));
                }
                flags.spec = Some(word.clone());
                continue;
            }
            let Written { flag, attached } = Written::of(word);
            if name.switches().contains(&flag) {
                if attached.is_some() {
                    return Err(UsageError::SwitchTakesNoValue(flag.to_owned()));
                }
                if flags.switches.contains(word) {
                    return Err(UsageError::GivenTwice(flag.to_owned()));
                }
                flags.switches.push(word.clone());
                continue;
            }
            if !name.flags().contains(&flag) {
                return Err(UsageError::FlagNotTaken {
                    command: name,
                    flag: flag.to_owned(),
                });
            }
            if flags.value(flag).is_some() {
                return Err(UsageError::GivenTwice(flag.to_owned()));
            }
            let value = match attached {
                Some(value) => value,
                None => match rest.next() {
                    Some(next) => next.as_str(),
                    None => "",
                },
            };
            if value.is_empty() {
                return Err(UsageError::NeedsValue(flag.to_owned()));
            }
            flags.given.push(Given {
                flag: flag.to_owned(),
                value: value.to_owned(),
            });
        }
        Ok(flags)
    }

    fn value(&self, flag: &str) -> Option<&str> {
        for given in &self.given {
            if given.flag == flag {
                return Some(&given.value);
            }
        }
        None
    }

    fn text(&self, flag: &str) -> Option<String> {
        self.value(flag).map(str::to_owned)
    }

    fn text_or(&self, flag: &str, default: &str) -> String {
        match self.value(flag) {
            Some(value) => value.to_owned(),
            None => default.to_owned(),
        }
    }

    // Generic: the flags parse to seconds, bytes and counts, each a type
    // of its own.
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

    fn publication(&self) -> String {
        self.text_or("--publication", PUBLICATION)
    }

    /// JSON with `--json`, else a table.
    fn output(&self) -> Output {
        for switch in &self.switches {
            if switch == JSON {
                return Output::Json;
            }
        }
        Output::Text
    }

    /// `--url`, else `default`, which the environment may have set.
    fn url(&self, default: &str) -> String {
        self.text_or("--url", default)
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
    let Ok(number) = text.parse() else {
        return None;
    };
    if number == 0 {
        return None;
    }
    Some(number)
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

/// A binary unit a size may end in, and the power of two it stands for.
struct Unit {
    suffix: &'static str,
    shift: u32,
}

// The suffixes are Kubernetes' own, so a chart's `4Gi` passes as written.
const UNITS: [Unit; 4] = [
    Unit {
        suffix: "Ki",
        shift: 10,
    },
    Unit {
        suffix: "Mi",
        shift: 20,
    },
    Unit {
        suffix: "Gi",
        shift: 30,
    },
    Unit {
        suffix: "Ti",
        shift: 40,
    },
];

fn bytes(text: &str) -> Option<u64> {
    let mut count = text;
    let mut shift = 0;
    for unit in &UNITS {
        if let Some(number) = text.strip_suffix(unit.suffix) {
            count = number;
            shift = unit.shift;
            break;
        }
    }
    positive(count)?.checked_mul(1 << shift)
}

#[cfg(test)]
mod tests;
