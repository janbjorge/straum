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

fn asks_for_help(args: &[String]) -> bool {
    for arg in args {
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
        if !arg.starts_with('-') {
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
        while let Some(word) = rest.next() {
            if takes_spec && !word.starts_with('-') {
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
mod tests {
    use super::*;

    /// A command line read with no `STRAUM_URL` set.
    fn command_of(args: &[String]) -> Result<Command, Usage> {
        command(args, &Environment::default())
    }

    fn with_url(url: &str) -> Environment {
        Environment {
            url: Some(url.to_owned()),
        }
    }

    #[test]
    fn the_url_is_the_flag_else_straum_url_else_the_default() {
        let line = args("status --url http://flag:1");
        let set = with_url("http://variable:2");
        let flag = command(&line, &set);
        assert_eq!(
            flag,
            Ok(Command::Status {
                url: "http://flag:1".into(),
                output: Output::Text,
            })
        );
        let variable = command(&args("ask SELECT"), &set);
        let Ok(Command::Ask { url, .. }) = variable else {
            panic!("{variable:?}");
        };
        assert_eq!(url, "http://variable:2");
        let empty = command(&args("views"), &with_url(""));
        let views = Command::Views {
            url: URL.into(),
            output: Output::Text,
        };
        assert_eq!(empty, Ok(views));
        let unset = command(&args("shell"), &Environment::default());
        assert_eq!(unset, Ok(Command::Shell { url: URL.into() }));
    }
    use crate::testing::args;

    #[test]
    fn reads_run_and_plan() {
        assert_eq!(
            command_of(&args("run s.sql")),
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
            command_of(&args(
                "run --dsn-file d --listen 127.0.0.1:9 s --publication p \
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
    }

    #[test]
    fn reads_plan() {
        assert_eq!(
            command_of(&args("plan s")),
            Ok(Command::Plan(Plan {
                spec: "s".into(),
                replication: Replication::Logical,
                publication: PUBLICATION.into(),
                role: None,
                dsn_file: None,
            }))
        );
        assert_eq!(
            command_of(&args("plan --role r s --dsn-file d --wal physical")),
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
            command_of(&args("status")),
            Ok(Command::Status {
                url: URL.into(),
                output: Output::Text,
            })
        );
        assert_eq!(
            command_of(&args("status --url http://straum:9")),
            Ok(Command::Status {
                url: "http://straum:9".into(),
                output: Output::Text,
            })
        );
        assert_eq!(
            command_of(&args("views --url http://straum:9")),
            Ok(Command::Views {
                url: "http://straum:9".into(),
                output: Output::Text,
            })
        );
        assert_eq!(
            command_of(&args("shell")),
            Ok(Command::Shell { url: URL.into() })
        );
        assert_eq!(
            command_of(&args("ask SELECT")),
            Ok(Command::Ask {
                url: URL.into(),
                query: "SELECT".into(),
                parameters: Vec::new(),
                output: Output::Text,
            })
        );
        // What follows the query is a parameter, whatever it looks like.
        assert_eq!(
            command_of(&args("ask --url http://straum:9 SELECT -h --url -7")),
            Ok(Command::Ask {
                url: "http://straum:9".into(),
                query: "SELECT".into(),
                parameters: args("-h --url -7"),
                output: Output::Text,
            })
        );
    }

    #[test]
    fn a_long_flag_takes_its_value_after_an_equals_sign_too() {
        for line in [
            "status --url=http://straum:9",
            "status --url http://straum:9",
        ] {
            let status = Command::Status {
                url: "http://straum:9".into(),
                output: Output::Text,
            };
            assert_eq!(command_of(&args(line)), Ok(status), "{line}");
        }
        // The value is all after the first =, which a URL may hold again.
        let Ok(Command::Run(run)) = command_of(&args("run s --listen=a=b --wal=physical")) else {
            panic!("run");
        };
        assert_eq!(run.listen, "a=b");
        assert!(matches!(run.replication, Replication::Physical { .. }));
        // An ask's query starts after a flag with its value attached.
        let ask = command_of(&args("ask --url=http://straum:9 SELECT --url=x"));
        let Ok(Command::Ask {
            url,
            query,
            parameters,
            ..
        }) = ask
        else {
            panic!("{ask:?}");
        };
        assert_eq!(
            (url.as_str(), query.as_str()),
            ("http://straum:9", "SELECT")
        );
        assert_eq!(parameters, args("--url=x"));
        for (line, error) in [
            ("status --url=", "--url needs a value"),
            ("status --url=a --url b", "--url given twice"),
            ("status --json=yes", "--json takes no value"),
            (
                "ask --ulr=x SELECT",
                "straum ask takes no \"--ulr\"; did you mean --url?",
            ),
        ] {
            assert_eq!(usage(line), error, "{line}");
        }
    }

    #[test]
    fn json_is_a_switch_of_the_commands_that_print_what_straum_says() {
        let status = command_of(&args("status --json --url http://straum:9"));
        let json_status = Command::Status {
            url: "http://straum:9".into(),
            output: Output::Json,
        };
        assert_eq!(status, Ok(json_status));
        let views = command_of(&args("views --json"));
        let json_views = Command::Views {
            url: URL.into(),
            output: Output::Json,
        };
        assert_eq!(views, Ok(json_views));
        // Before the query a switch takes no value; after it, --json is
        // a parameter like any other word.
        let ask = command_of(&args("ask --json SELECT --json"));
        let json_ask = Command::Ask {
            url: URL.into(),
            query: "SELECT".into(),
            parameters: args("--json"),
            output: Output::Json,
        };
        assert_eq!(ask, Ok(json_ask));
        for (line, error) in [
            ("status --json --json", "--json given twice"),
            ("shell --json", "straum shell takes no \"--json\""),
            ("plan s --json", "straum plan takes no \"--json\""),
        ] {
            assert_eq!(usage(line), error, "{line}");
        }
    }

    #[test]
    fn reads_calls_for_help_and_the_version() {
        for help in ["help", "--help", "-h"] {
            assert_eq!(command_of(&args(help)), Ok(Command::Help(None)));
        }
        for version in ["version", "--version", "-V"] {
            assert_eq!(command_of(&args(version)), Ok(Command::Version));
        }
        for (line, name) in [
            ("help run", Name::Run),
            ("run s -h", Name::Run),
            ("plan --help", Name::Plan),
            ("status -h --url", Name::Status),
            ("views -h", Name::Views),
            ("ask --help", Name::Ask),
            ("shell -h", Name::Shell),
            ("ask --url u -h SELECT", Name::Ask),
        ] {
            assert_eq!(
                command_of(&args(line)),
                Ok(Command::Help(Some(name))),
                "{line}"
            );
        }
    }

    /// The flags other commands take and `name` doesn't.
    fn flags_of_others(name: Name) -> Vec<&'static str> {
        let mut flags = Vec::new();
        let mut taken = name.flags().to_vec();
        taken.extend(name.switches());
        for other in Name::ALL {
            for flag in other.flags().iter().chain(other.switches()) {
                if !taken.contains(flag) && !flags.contains(flag) {
                    flags.push(*flag);
                }
            }
        }
        flags
    }

    #[test]
    fn help_names_every_flag() {
        assert!(HELP.contains("usage: straum <command>"));
        for name in Name::ALL {
            let text = name.help();
            let word = name.as_str();
            assert!(text.starts_with(&format!("usage: straum {word}")), "{word}");
            assert!(HELP.contains(&format!("\n  {word} ")), "{word}");
            for flag in name.flags().iter().chain(name.switches()) {
                assert!(text.contains(&format!("  {flag} ")), "{word} {flag}");
            }
            for flag in flags_of_others(name) {
                assert!(!text.contains(flag), "{word} {flag}");
            }
        }
    }

    /// `bytes` in whole gibibytes, as the help writes a default.
    fn gibibytes(bytes: u64) -> String {
        format!("{}Gi", bytes >> 30)
    }

    /// Each default the help shows is the one the code fills in.
    #[test]
    fn help_names_each_default() {
        let run = Name::Run.help();
        for default in [
            format!("({LISTEN})"),
            format!("({PUBLICATION})"),
            format!("({})", LIMITS.max_lag.as_secs()),
            format!("({};", gibibytes(LIMITS.max_retained_wal)),
            format!("({MAX_TRANSACTION_ROWS})"),
            format!("({})", gibibytes(MAX_PAGE_MEMORY as u64)),
        ] {
            assert!(run.contains(&default), "run {default}");
        }
        let plan = Name::Plan.help();
        assert!(plan.contains(&format!("({PUBLICATION})")));
        assert!(plan.contains(&format!("else {ROLE})")));
        for name in [Name::Status, Name::Views, Name::Ask, Name::Shell] {
            let default = format!("({URL_VARIABLE}, else {URL})");
            assert!(name.help().contains(&default), "{name:?}");
        }
    }

    fn usage(line: &str) -> String {
        match command_of(&args(line)) {
            Err(Usage::Wrong { error, .. }) => error.to_string(),
            other => panic!("{line}: {other:?}"),
        }
    }

    #[test]
    fn failures_say_what_to_try() {
        assert_eq!(command_of(&[]), Err(Usage::NoCommand));
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
                "straum: straum run needs a spec\ntry: straum run --help",
            ),
            (
                "status --spec s",
                "straum: straum status takes no \"--spec\"\ntry: straum status --help",
            ),
        ];
        for (line, printed) in cases {
            let usage = command_of(&args(line)).unwrap_err();
            assert_eq!(usage.to_string(), printed, "{line}");
        }
    }

    #[test]
    fn command_errors() {
        let cases = [
            ("serve", "unknown command \"serve\""),
            ("stauts", "unknown command \"stauts\"; did you mean status?"),
            ("help vews", "unknown command \"vews\"; did you mean views?"),
            (
                "verison",
                "unknown command \"verison\"; did you mean version?",
            ),
            (
                "ask --ulr x SELECT",
                "straum ask takes no \"--ulr\"; did you mean --url?",
            ),
            (
                "status --jsno",
                "straum status takes no \"--jsno\"; did you mean --json?",
            ),
            (
                "run s --max-lags 1",
                "straum run takes no \"--max-lags\"; did you mean --max-lag?",
            ),
            ("help --spec s", "straum help takes one command"),
            ("help --spec", "unknown command \"--spec\""),
            ("help serve", "unknown command \"serve\""),
            ("help run plan", "straum help takes one command"),
            ("version run", "straum version takes nothing"),
            ("-V --spec s", "straum version takes nothing"),
            ("run --spec", "straum run takes no \"--spec\""),
            ("run --spec a --spec b", "straum run takes no \"--spec\""),
            ("run a b", "straum run takes one spec"),
            ("plan a b", "straum plan takes one spec"),
            ("plan s --listen x", "straum plan takes no \"--listen\""),
            ("run --spec=s", "straum run takes no \"--spec\""),
            ("run", "straum run needs a spec"),
            ("plan --role r", "straum plan needs a spec"),
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
    fn wal_flags_say_what_they_take() {
        let cases = [
            (
                "run s --wal Physical",
                "--wal takes logical or physical, not \"Physical\"",
            ),
            (
                "plan s --wal both",
                "--wal takes logical or physical, not \"both\"",
            ),
            ("run s --wal", "--wal needs a value"),
            (
                "run s --max-page-memory 1Gi",
                "--max-page-memory takes --wal physical",
            ),
            (
                "run --max-page-memory 1Gi",
                "--max-page-memory takes --wal physical",
            ),
            (
                "run s --wal logical --max-page-memory 1Gi",
                "--max-page-memory takes --wal physical",
            ),
            (
                "run s --wal physical --max-page-memory 0",
                "--max-page-memory takes bytes above 0, as 1073741824 or 1Gi, not \"0\"",
            ),
            (
                "run s --wal physical --max-page-memory 1Pi",
                "--max-page-memory takes bytes above 0, as 1073741824 or 1Gi, not \"1Pi\"",
            ),
            (
                "plan s --max-page-memory 1Gi",
                "straum plan takes no \"--max-page-memory\"",
            ),
            ("plan s --max-lag 5", "straum plan takes no \"--max-lag\""),
        ];
        for (line, error) in cases {
            assert_eq!(usage(line), error, "{line}");
        }
    }

    #[test]
    fn number_flags_say_what_they_take() {
        for lag in ["0", "-1", "1.5", "30s", ""] {
            let line = format!("run s --max-lag {lag}");
            let error = if lag.is_empty() {
                "--max-lag needs a value".to_owned()
            } else {
                format!("--max-lag takes whole seconds above 0, not {lag:?}")
            };
            assert_eq!(usage(&line), error, "{lag}");
        }
        for size in ["0", "0Gi", "4GB", "4gi", "Gi", "16777216Ti"] {
            let line = format!("run s --max-retained-wal {size}");
            let error = format!(
                "--max-retained-wal takes bytes above 0, as 4294967296 or 4Gi, not {size:?}"
            );
            assert_eq!(usage(&line), error, "{size}");
        }
        for rows in ["0", "-1", "1.5", "10k", "1Ki"] {
            let line = format!("run s --max-transaction-rows {rows}");
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
