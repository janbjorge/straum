//! Reading a command line into a `Command`. `command` takes the words
//! after `straum`: the first names the command. `run` and `plan` take
//! the spec as the word that is not a flag, `plan --copy` and `plan
//! --tables` several; `ask` takes its query
//! and parameters after the flags. `Flags::read` checks the flags
//! against the ones that command takes, and each `*_command` function
//! builds the command from them, filling in defaults. A line Straum
//! can't run becomes a `Usage` error that `main.rs` prints.

use std::fmt::{self, Display};
use std::time::Duration;

use straum_app::Limits;
use straum_domain::closest;
use straum_pg::Replication;

use crate::client::{Output, Server, TIMEOUT};
use crate::help::{ASK_HELP, HELP, PLAN_HELP, RUN_HELP, SHELL_HELP, STATUS_HELP, VIEWS_HELP};

/// Where `straum run` listens unless told: this host only, as Straum
/// has no authentication or TLS.
const LISTEN: &str = "127.0.0.1:8080";
const PUBLICATION: &str = "straum";
/// The role `straum plan` sets up when neither `--role` nor a DSN names one.
pub(crate) const ROLE: &str = "straum";
/// Where the commands that talk to a running Straum find it unless told.
const URL: &str = "http://127.0.0.1:8080";
/// The switch that asks for JSON instead of a table.
const JSON_FLAG: &str = "--json";
/// The switch that makes `plan` print the spec's tables, not its SQL.
const TABLES_FLAG: &str = "--tables";
/// The flag that makes `plan` write the copy scripts into a directory.
const COPY_FLAG: &str = "--copy";
/// Where `plan` writes the SQL or the tables instead of stdout.
const OUT_FLAG: &str = "--out";
// Each flag is written once, here: a misspelt name at a read site
// fails to compile, where a literal would silently take the default.
const LISTEN_FLAG: &str = "--listen";
const ALLOW_HOSTS_FLAG: &str = "--allow-hosts";
const WAL_FLAG: &str = "--wal";
const PUBLICATION_FLAG: &str = "--publication";
const ROLE_FLAG: &str = "--role";
const DSN_FILE_FLAG: &str = "--dsn-file";
const MAX_LAG_FLAG: &str = "--max-lag";
const MAX_RETAINED_WAL_FLAG: &str = "--max-retained-wal";
const MAX_TRANSACTION_ROWS_FLAG: &str = "--max-transaction-rows";
const MAX_ANSWER_ROWS_FLAG: &str = "--max-answer-rows";
const MAX_PAGE_MEMORY_FLAG: &str = "--max-page-memory";
const MAX_STREAM_MEMORY_FLAG: &str = "--max-stream-memory";
const MAX_MEMORY_FLAG: &str = "--max-memory";
const URL_FLAG: &str = "--url";
const TIMEOUT_FLAG: &str = "--timeout";
/// The environment variable that names the URL when `--url` doesn't.
pub(crate) const URL_VARIABLE: &str = "STRAUM_URL";
const FOUR_GIBIBYTES: u64 = 4 << 30;
const LIMITS: Limits = Limits {
    max_lag: Duration::from_secs(30),
    max_retained_wal: FOUR_GIBIBYTES,
    max_stream_memory: MAX_STREAM_MEMORY,
};
/// Rows one transaction may change before Straum stops instead of applying
/// it, so the next start brings its rows in through the load;
/// `just taxi transactions` measures what a transaction this size holds
/// the write lock for.
const MAX_TRANSACTION_ROWS: usize = 100_000;
/// The rows `POST /ask` answers at most. An answer is written whole in
/// memory, so one ask over every key of a large view could take more than
/// the process may hold; past this it is refused.
const MAX_ANSWER_ROWS: usize = 100_000;
/// What `--max-transaction-rows` and `--max-answer-rows` take, as their
/// errors say.
const ROW_COUNT_TAKES: &str = "a count of rows above 0";
/// Bytes of rows a physical stream holds before Straum stops: a guard,
/// far above what a checkpoint cycle's pages and open transactions take.
const MAX_PAGE_MEMORY: usize = 1 << 30;
/// Bytes of commits the stream holds while the tables load before
/// Straum stops: a guard on a long load of busy tables.
const MAX_STREAM_MEMORY: usize = 1 << 30;
/// What `--max-page-memory` and `--max-stream-memory` take, as their
/// errors say.
const MEMORY_LIMIT_TAKES: &str = "bytes above 0, as 1073741824 or 1Gi";
/// The longest `--timeout`: a day. A timeout is added to the clock, and
/// one near `u64::MAX` seconds would overflow it.
const MAX_TIMEOUT: Duration = Duration::from_hours(24);
/// What `--timeout` takes, as its error says.
const TIMEOUT_TAKES: &str = "whole seconds from 1 to 86400";
/// What `--max-retained-wal` and `--max-memory` take, as their errors
/// say.
const BYTE_LIMIT_TAKES: &str = "bytes above 0, as 4294967296 or 4Gi";
/// What `--allow-hosts` takes, as its error says.
const ALLOW_HOSTS_TAKES: &str = "names with their port, as localhost:8080,127.0.0.1:8080";

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
                LISTEN_FLAG,
                ALLOW_HOSTS_FLAG,
                WAL_FLAG,
                PUBLICATION_FLAG,
                DSN_FILE_FLAG,
                MAX_LAG_FLAG,
                MAX_RETAINED_WAL_FLAG,
                MAX_TRANSACTION_ROWS_FLAG,
                MAX_ANSWER_ROWS_FLAG,
                MAX_PAGE_MEMORY_FLAG,
                MAX_STREAM_MEMORY_FLAG,
                MAX_MEMORY_FLAG,
            ],
            Name::Plan => &[
                WAL_FLAG,
                PUBLICATION_FLAG,
                ROLE_FLAG,
                DSN_FILE_FLAG,
                OUT_FLAG,
                COPY_FLAG,
            ],
            Name::Status | Name::Views | Name::Ask | Name::Shell => &[URL_FLAG, TIMEOUT_FLAG],
        }
    }

    /// The flags that take no value.
    fn switches(self) -> &'static [&'static str] {
        match self {
            Name::Status | Name::Views | Name::Ask => &[JSON_FLAG],
            Name::Plan => &[TABLES_FLAG],
            Name::Run | Name::Shell => &[],
        }
    }
}

/// A command line Straum can run.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Run(Run),
    Plan(Plan),
    Status {
        server: Server,
        output: Output,
    },
    Views {
        server: Server,
        output: Output,
    },
    Ask {
        server: Server,
        query: String,
        /// As written on the command line.
        parameters: Vec<String>,
        output: Output,
    },
    Shell {
        server: Server,
    },
    Help(Option<Name>),
    Version,
}

/// What `straum run` was told.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub(crate) spec: String,
    pub(crate) listen: String,
    /// The `Host` names, each `NAME[:PORT]`, that requests may name;
    /// empty, the default, leaves it to the listen address.
    pub(crate) allow_hosts: Vec<String>,
    pub(crate) replication: Replication,
    pub(crate) publication: String,
    pub(crate) dsn_file: Option<String>,
    pub(crate) limits: Limits,
    pub(crate) max_transaction_rows: usize,
    /// The rows an ask may get before it is refused.
    pub(crate) max_answer_rows: usize,
    /// Bytes of resident memory before Straum stops; `None`, the default,
    /// sets no limit and never reads it.
    pub(crate) max_memory: Option<u64>,
}

/// What `straum plan` was told.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// One spec, or several with `--copy` or `--tables`.
    pub(crate) specs: Vec<String>,
    pub(crate) publication: String,
    pub(crate) role: Option<String>,
    pub(crate) dsn_file: Option<String>,
    /// Where to write the setup SQL. `None` prints it.
    pub(crate) out: Option<String>,
    pub(crate) printed: Printed,
}

/// What `straum plan` makes: the setup SQL, the specs' tables, one per
/// line, for a dump to take, or the copy scripts, in a directory.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Printed {
    Setup { replication: Replication },
    Tables,
    Copy { directory: String },
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
    /// `plan` was given several specs without `--copy` or `--tables`.
    SetupTakesOneSpec,
    /// Two flags that ask for different things.
    Together {
        flag: &'static str,
        other: &'static str,
    },
    BadWal(String),
    /// `run`, or `plan` printing the setup, without `--wal`.
    NeedsWal(Name),
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
            UsageError::SetupTakesOneSpec => {
                formatter.write_str("straum plan takes one spec; --copy and --tables take several")
            }
            UsageError::Together { flag, other } => {
                write!(formatter, "{flag} and {other} don't go together")
            }
            UsageError::BadWal(value) => {
                write!(formatter, "--wal takes logical or physical, not {value:?}")
            }
            UsageError::NeedsWal(command) => write!(
                formatter,
                "straum {} needs --wal logical or --wal physical",
                command.as_str()
            ),
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
                server: flags.server(url)?,
                output: flags.output(),
            })
        }
        Name::Views => {
            let flags = Flags::read(name, rest)?;
            Ok(Command::Views {
                server: flags.server(url)?,
                output: flags.output(),
            })
        }
        Name::Shell => Ok(Command::Shell {
            server: Flags::read(name, rest)?.server(url)?,
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
        server: flags.server(url)?,
        query: query.clone(),
        parameters: parameters.to_vec(),
        output: flags.output(),
    })
}

fn plan_command(flags: &Flags) -> Result<Command, UsageError> {
    if flags.specs.is_empty() {
        return Err(UsageError::NeedsSpec(Name::Plan));
    }
    let printed = printed(flags)?;
    Ok(Command::Plan(Plan {
        specs: flags.specs.clone(),
        publication: flags.publication(),
        role: flags.text(ROLE_FLAG),
        dsn_file: flags.text(DSN_FILE_FLAG),
        out: flags.text(OUT_FLAG),
        printed,
    }))
}

/// What `plan` makes, from `--copy` and `--tables`. The copy scripts go
/// into their directory, so `--out` has nothing to write.
fn printed(flags: &Flags) -> Result<Printed, UsageError> {
    let tables = flags.switch(TABLES_FLAG);
    let Some(directory) = flags.text(COPY_FLAG) else {
        if tables {
            return Ok(Printed::Tables);
        }
        if flags.specs.len() > 1 {
            return Err(UsageError::SetupTakesOneSpec);
        }
        let replication = flags.replication(Name::Plan)?;
        return Ok(Printed::Setup { replication });
    };
    if tables {
        return Err(UsageError::Together {
            flag: COPY_FLAG,
            other: TABLES_FLAG,
        });
    }
    if flags.value(OUT_FLAG).is_some() {
        return Err(UsageError::Together {
            flag: COPY_FLAG,
            other: OUT_FLAG,
        });
    }
    Ok(Printed::Copy { directory })
}

fn run_command(flags: &Flags) -> Result<Command, UsageError> {
    let Some(spec) = flags.specs.first().cloned() else {
        return Err(UsageError::NeedsSpec(Name::Run));
    };
    let replication = flags.replication(Name::Run)?;
    let is_physical = matches!(replication, Replication::Physical { .. });
    if flags.value(MAX_PAGE_MEMORY_FLAG).is_some() && !is_physical {
        return Err(UsageError::PageMemoryNeedsPhysical);
    }
    let max_lag = flags.parsed(
        MAX_LAG_FLAG,
        LIMITS.max_lag,
        seconds,
        "whole seconds above 0",
    )?;
    let max_retained_wal = flags.parsed(
        MAX_RETAINED_WAL_FLAG,
        LIMITS.max_retained_wal,
        bytes,
        BYTE_LIMIT_TAKES,
    )?;
    let max_transaction_rows = flags.parsed(
        MAX_TRANSACTION_ROWS_FLAG,
        MAX_TRANSACTION_ROWS,
        count,
        ROW_COUNT_TAKES,
    )?;
    let max_answer_rows = flags.parsed(
        MAX_ANSWER_ROWS_FLAG,
        MAX_ANSWER_ROWS,
        count,
        ROW_COUNT_TAKES,
    )?;
    let max_stream_memory = flags.parsed(
        MAX_STREAM_MEMORY_FLAG,
        MAX_STREAM_MEMORY,
        memory,
        MEMORY_LIMIT_TAKES,
    )?;
    Ok(Command::Run(Run {
        spec,
        listen: flags.text_or(LISTEN_FLAG, LISTEN),
        allow_hosts: flags
            .parsed_if_given(ALLOW_HOSTS_FLAG, host_names, ALLOW_HOSTS_TAKES)?
            .unwrap_or_default(),
        replication,
        publication: flags.publication(),
        dsn_file: flags.text(DSN_FILE_FLAG),
        limits: Limits {
            max_lag,
            max_retained_wal,
            max_stream_memory,
        },
        max_transaction_rows,
        max_answer_rows,
        max_memory: flags.parsed_if_given(MAX_MEMORY_FLAG, bytes, BYTE_LIMIT_TAKES)?,
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
/// and the specs for the commands that take them.
struct Flags {
    given: Vec<Given>,
    switches: Vec<String>,
    specs: Vec<String>,
}

impl Flags {
    /// A flag takes the next word, so for `run` and `plan` the spec is
    /// whichever word is left. Only `plan` takes several, and
    /// `plan_command` says when.
    fn read(name: Name, rest: &[String]) -> Result<Flags, UsageError> {
        let takes_spec = matches!(name, Name::Run | Name::Plan);
        let mut flags = Flags {
            given: Vec::new(),
            switches: Vec::new(),
            specs: Vec::new(),
        };
        let mut rest = rest.iter();
        let mut flags_ended = false;
        while let Some(word) = rest.next() {
            if word == END_OF_FLAGS && !flags_ended {
                flags_ended = true;
                continue;
            }
            if takes_spec && (flags_ended || !word.starts_with('-')) {
                if !flags.specs.is_empty() && name != Name::Plan {
                    return Err(UsageError::OneSpec(name));
                }
                flags.specs.push(word.clone());
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
        match self.parsed_if_given(flag, parse, takes)? {
            Some(parsed) => Ok(parsed),
            None => Ok(default),
        }
    }

    /// The flag's value, parsed, or `None` when the line leaves it out:
    /// for a limit that is off unless set.
    // Generic, as `parsed` is.
    fn parsed_if_given<T>(
        &self,
        flag: &'static str,
        parse: fn(&str) -> Option<T>,
        takes: &'static str,
    ) -> Result<Option<T>, UsageError> {
        let Some(value) = self.value(flag) else {
            return Ok(None);
        };
        let Some(parsed) = parse(value) else {
            let value = value.to_owned();
            return Err(UsageError::BadValue { flag, takes, value });
        };
        Ok(Some(parsed))
    }

    fn publication(&self) -> String {
        self.text_or(PUBLICATION_FLAG, PUBLICATION)
    }

    /// Whether the switch `flag` is on the line.
    fn switch(&self, flag: &str) -> bool {
        for switch in &self.switches {
            if switch == flag {
                return true;
            }
        }
        false
    }

    /// JSON with `--json`, else a table.
    fn output(&self) -> Output {
        if self.switch(JSON_FLAG) {
            Output::Json
        } else {
            Output::Text
        }
    }

    /// `--url`, else `default`, which the environment may have set, and
    /// `--timeout`.
    fn server(&self, default: &str) -> Result<Server, UsageError> {
        Ok(Server {
            url: self.text_or(URL_FLAG, default),
            timeout: self.parsed(TIMEOUT_FLAG, TIMEOUT, timeout, TIMEOUT_TAKES)?,
        })
    }

    fn replication(&self, command: Name) -> Result<Replication, UsageError> {
        match self.value(WAL_FLAG) {
            None => Err(UsageError::NeedsWal(command)),
            Some("logical") => Ok(Replication::Logical),
            Some("physical") => {
                let max_page_memory = self.parsed(
                    MAX_PAGE_MEMORY_FLAG,
                    MAX_PAGE_MEMORY,
                    memory,
                    MEMORY_LIMIT_TAKES,
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

fn timeout(text: &str) -> Option<Duration> {
    let timeout = seconds(text)?;
    if timeout > MAX_TIMEOUT {
        return None;
    }
    Some(timeout)
}

/// `--allow-hosts`, split at its commas, or `None` if any part is no
/// `Host` name.
fn host_names(text: &str) -> Option<Vec<String>> {
    let mut names = Vec::new();
    for name in text.split(',') {
        if !straum_http::is_host(name) {
            return None;
        }
        names.push(name.to_owned());
    }
    Some(names)
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
