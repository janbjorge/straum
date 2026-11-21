//! The tests of `command_line.rs`: each command line read into the
//! `Command` it names or the `Usage` error it is, and the help checked
//! against the flags and defaults the code takes.

use std::time::Instant;

use super::*;
use crate::testing::{args, server};

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
            server: server("http://flag:1"),
            output: Output::Text,
        })
    );
    let variable = command(&args("ask SELECT"), &set);
    let Ok(Command::Ask { server: asked, .. }) = variable else {
        panic!("{variable:?}");
    };
    assert_eq!(asked.url, "http://variable:2");
    let empty = command(&args("views"), &with_url(""));
    let views = Command::Views {
        server: server(URL),
        output: Output::Text,
    };
    assert_eq!(empty, Ok(views));
    let unset = command(&args("shell"), &Environment::default());
    assert_eq!(
        unset,
        Ok(Command::Shell {
            server: server(URL)
        })
    );
}

#[test]
fn reads_run_and_plan() {
    assert_eq!(
        command_of(&args("run s.sql --wal logical")),
        Ok(Command::Run(Run {
            spec: "s.sql".into(),
            listen: LISTEN.into(),
            allow_hosts: Vec::new(),
            replication: Replication::Logical,
            publication: PUBLICATION.into(),
            dsn_file: None,
            limits: LIMITS,
            max_transaction_rows: MAX_TRANSACTION_ROWS,
            max_answer_rows: MAX_ANSWER_ROWS,
            max_memory: None,
        }))
    );
    assert_eq!(
        command_of(&args(
            "run --dsn-file d --listen 127.0.0.1:9 s --publication p \
             --max-lag 5 --max-retained-wal 512Mi --max-transaction-rows 2500 --wal physical \
             --max-page-memory 256Mi --max-memory 2Gi --max-answer-rows 50"
        )),
        Ok(Command::Run(Run {
            spec: "s".into(),
            listen: "127.0.0.1:9".into(),
            allow_hosts: Vec::new(),
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
            max_answer_rows: 50,
            max_memory: Some(2 << 30),
        }))
    );
}

/// `--max-memory` is off unless set, and takes bytes with or without a
/// suffix.
#[test]
fn max_memory_is_off_unless_set() {
    let cases = [
        ("run s --wal logical", None),
        ("run s --wal logical --max-memory 4294967296", Some(4 << 30)),
        ("run s --wal physical --max-memory=512Mi", Some(512 << 20)),
    ];
    for (line, expected) in cases {
        let Ok(Command::Run(run)) = command_of(&args(line)) else {
            panic!("{line}");
        };
        assert_eq!(run.max_memory, expected, "{line}");
    }
    assert_eq!(
        usage("plan s --wal logical --max-memory 1Gi"),
        "straum plan takes no \"--max-memory\""
    );
}

/// `--allow-hosts` takes `Host` names split at commas, and is empty
/// unless set.
#[test]
fn allow_hosts_takes_names_with_their_port() {
    let cases = [
        ("run s --wal logical", vec![]),
        (
            "run s --wal logical --allow-hosts localhost:8080,127.0.0.1:8080",
            vec!["localhost:8080", "127.0.0.1:8080"],
        ),
        ("run s --wal logical --allow-hosts=straum", vec!["straum"]),
    ];
    for (line, expected) in cases {
        let Ok(Command::Run(run)) = command_of(&args(line)) else {
            panic!("{line}");
        };
        assert_eq!(run.allow_hosts, expected, "{line}");
    }
    for value in ["localhost:8080,", ",x", "localhost:x", ":8080"] {
        assert_eq!(
            usage(&format!("run s --wal logical --allow-hosts={value}")),
            format!(
                "--allow-hosts takes names with their port, as \
                 localhost:8080,127.0.0.1:8080, not {value:?}"
            ),
            "{value}"
        );
    }
    assert_eq!(
        usage("plan s --wal logical --allow-hosts localhost:8080"),
        "straum plan takes no \"--allow-hosts\""
    );
}

/// The address `straum run` serves on for `line`.
fn listen_of(line: &str) -> std::net::SocketAddr {
    let Ok(Command::Run(run)) = command_of(&args(line)) else {
        panic!("{line}");
    };
    run.listen.parse().unwrap()
}

/// Straum has no authentication, so unless told it serves this host only.
#[test]
fn listens_on_this_host_only_unless_told() {
    assert!(listen_of("run s.sql --wal logical").ip().is_loopback());
    let every = listen_of("run s.sql --wal logical --listen 0.0.0.0:8080");
    assert!(!every.ip().is_loopback());
}

#[test]
fn reads_plan() {
    assert_eq!(
        command_of(&args("plan s --wal logical")),
        Ok(Command::Plan(Plan {
            specs: vec!["s".into()],
            publication: PUBLICATION.into(),
            role: None,
            dsn_file: None,
            out: None,
            printed: Printed::Setup {
                replication: Replication::Logical,
            },
        }))
    );
    assert_eq!(
        command_of(&args("plan --role r s --dsn-file d --wal physical")),
        Ok(Command::Plan(Plan {
            specs: vec!["s".into()],
            publication: PUBLICATION.into(),
            role: Some("r".into()),
            dsn_file: Some("d".into()),
            out: None,
            printed: Printed::Setup {
                replication: Replication::Physical {
                    max_page_memory: MAX_PAGE_MEMORY,
                },
            },
        }))
    );
    assert_eq!(
        command_of(&args("plan s --wal logical --out setup.sql")),
        Ok(Command::Plan(Plan {
            specs: vec!["s".into()],
            publication: PUBLICATION.into(),
            role: None,
            dsn_file: None,
            out: Some("setup.sql".into()),
            printed: Printed::Setup {
                replication: Replication::Logical,
            },
        }))
    );
    assert_eq!(
        command_of(&args("plan s --tables --out t.txt")),
        Ok(Command::Plan(Plan {
            specs: vec!["s".into()],
            publication: PUBLICATION.into(),
            role: None,
            dsn_file: None,
            out: Some("t.txt".into()),
            printed: Printed::Tables,
        }))
    );
    let refused = command_of(&args("run --tables s"));
    assert!(
        matches!(
            refused,
            Err(Usage::Wrong {
                error: UsageError::FlagNotTaken { .. },
                ..
            })
        ),
        "{refused:?}"
    );
}

/// `--copy` and `--tables` take several specs.
#[test]
fn reads_plan_with_several_specs() {
    assert_eq!(
        command_of(&args("plan a.sql b.sql --copy out")),
        Ok(Command::Plan(Plan {
            specs: vec!["a.sql".into(), "b.sql".into()],
            publication: PUBLICATION.into(),
            role: None,
            dsn_file: None,
            out: None,
            printed: Printed::Copy {
                directory: "out".into(),
            },
        }))
    );
    assert_eq!(
        command_of(&args("plan a b --tables")),
        Ok(Command::Plan(Plan {
            specs: vec!["a".into(), "b".into()],
            publication: PUBLICATION.into(),
            role: None,
            dsn_file: None,
            out: None,
            printed: Printed::Tables,
        }))
    );
}

#[test]
fn reads_the_commands_that_talk_to_a_running_straum() {
    assert_eq!(
        command_of(&args("status")),
        Ok(Command::Status {
            server: server(URL),
            output: Output::Text,
        })
    );
    assert_eq!(
        command_of(&args("status --url http://straum:9")),
        Ok(Command::Status {
            server: server("http://straum:9"),
            output: Output::Text,
        })
    );
    assert_eq!(
        command_of(&args("views --url http://straum:9")),
        Ok(Command::Views {
            server: server("http://straum:9"),
            output: Output::Text,
        })
    );
    assert_eq!(
        command_of(&args("shell")),
        Ok(Command::Shell {
            server: server(URL)
        })
    );
    assert_eq!(
        command_of(&args("ask SELECT")),
        Ok(Command::Ask {
            server: server(URL),
            query: "SELECT".into(),
            parameters: Vec::new(),
            output: Output::Text,
        })
    );
    // What follows the query is a parameter, whatever it looks like.
    assert_eq!(
        command_of(&args("ask --url http://straum:9 SELECT -h --url -7")),
        Ok(Command::Ask {
            server: server("http://straum:9"),
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
            server: server("http://straum:9"),
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
        server: asked,
        query,
        parameters,
        ..
    }) = ask
    else {
        panic!("{ask:?}");
    };
    assert_eq!(
        (asked.url.as_str(), query.as_str()),
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

/// After `--`, a word that starts with `-` is a spec or a query, not a
/// flag; before it, it is a flag.
#[test]
fn a_double_dash_ends_the_flags() {
    let Ok(Command::Run(run)) = command_of(&args("run --wal logical --listen a -- -spec.sql"))
    else {
        panic!("run");
    };
    assert_eq!((run.spec.as_str(), run.listen.as_str()), ("-spec.sql", "a"));
    let Ok(Command::Plan(plan)) = command_of(&args("plan --wal logical -- -h")) else {
        panic!("plan -- -h asks for no help");
    };
    assert_eq!(plan.specs, ["-h"]);
    let ask = command_of(&args("ask --json -- -x $1 -y"));
    let Ok(Command::Ask {
        query, parameters, ..
    }) = ask
    else {
        panic!("{ask:?}");
    };
    assert_eq!((query.as_str(), parameters), ("-x", args("$1 -y")));
    for (line, error) in [
        ("run -spec.sql", "straum run takes no \"-spec.sql\""),
        ("plan -- a b", "straum plan takes one spec"),
        ("ask --", "straum ask needs a query"),
    ] {
        assert!(usage(line).starts_with(error), "{line}: {}", usage(line));
    }
}

#[test]
fn json_is_a_switch_of_the_commands_that_print_what_straum_says() {
    let status = command_of(&args("status --json --url http://straum:9"));
    let json_status = Command::Status {
        server: server("http://straum:9"),
        output: Output::Json,
    };
    assert_eq!(status, Ok(json_status));
    let views = command_of(&args("views --json"));
    let json_views = Command::Views {
        server: server(URL),
        output: Output::Json,
    };
    assert_eq!(views, Ok(json_views));
    // Before the query a switch takes no value; after it, --json is
    // a parameter like any other word.
    let ask = command_of(&args("ask --json SELECT --json"));
    let json_ask = Command::Ask {
        server: server(URL),
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
        format!("({MAX_ANSWER_ROWS})"),
        format!("({})", gibibytes(MAX_PAGE_MEMORY as u64)),
        "(off;".to_owned(),
    ] {
        assert!(run.contains(&default), "run {default}");
    }
    let plan = Name::Plan.help();
    assert!(plan.contains(&format!("({PUBLICATION})")));
    assert!(plan.contains(&format!("else {ROLE})")));
    for name in [Name::Status, Name::Views, Name::Ask, Name::Shell] {
        let default = format!("({URL_VARIABLE}, else {URL})");
        assert!(name.help().contains(&default), "{name:?}");
        let timeout = format!("the reply may take ({})", TIMEOUT.as_secs());
        assert!(name.help().contains(&timeout), "{name:?}");
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
        (
            "plan a b",
            "straum plan takes one spec; --copy and --tables take several",
        ),
        (
            "plan s --copy d --tables",
            "--copy and --tables don't go together",
        ),
        (
            "plan s --copy d --out f",
            "--copy and --out don't go together",
        ),
        ("plan s --copy", "--copy needs a value"),
        ("plan s --listen x", "straum plan takes no \"--listen\""),
        ("plan --out", "--out needs a value"),
        ("run s --out setup.sql", "straum run takes no \"--out\""),
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
        ("run s", "straum run needs --wal logical or --wal physical"),
        (
            "run s --max-page-memory 1Gi",
            "straum run needs --wal logical or --wal physical",
        ),
        (
            "plan s",
            "straum plan needs --wal logical or --wal physical",
        ),
        (
            "plan s --out setup.sql",
            "straum plan needs --wal logical or --wal physical",
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
fn a_switch_is_on_only_when_given() {
    let flags = Flags::read(Name::Plan, &args("s --tables")).unwrap();
    assert!(flags.switch(TABLES_FLAG));
    assert!(!flags.switch(JSON_FLAG));
    let none = Flags::read(Name::Status, &[]).unwrap();
    assert!(!none.switch(JSON_FLAG));
    assert_eq!(none.output(), Output::Text);
}

/// A command that talks to a running Straum waits `--timeout` seconds
/// for it, else ten.
#[test]
fn the_timeout_is_the_flag_else_ten_seconds() {
    for line in ["status", "views", "ask SELECT", "shell"] {
        let given = format!("{line} --timeout 3");
        let given = given.replacen(" SELECT --timeout 3", " --timeout 3 SELECT", 1);
        let timeout = match command_of(&args(&given)) {
            Ok(
                Command::Status { server, .. }
                | Command::Views { server, .. }
                | Command::Ask { server, .. }
                | Command::Shell { server },
            ) => server.timeout,
            other => panic!("{given}: {other:?}"),
        };
        assert_eq!(timeout, Duration::from_secs(3), "{given}");
    }
    let Ok(Command::Status { server, .. }) = command_of(&args("status")) else {
        panic!("status");
    };
    assert_eq!(server.timeout, Duration::from_secs(10));
    assert_eq!(
        usage("run s --timeout 3"),
        "straum run takes no \"--timeout\""
    );
}

/// A timeout is added to the clock: one past the maximum is refused on
/// the command line, before it can overflow the deadline.
#[test]
fn the_timeout_has_a_maximum() {
    let most = MAX_TIMEOUT.as_secs();
    assert!(
        TIMEOUT_TAKES.ends_with(&format!(" {most}")),
        "{TIMEOUT_TAKES}"
    );
    let Ok(Command::Status { server, .. }) = command_of(&args(&format!("status --timeout {most}")))
    else {
        panic!("status --timeout {most}");
    };
    assert_eq!(server.timeout, MAX_TIMEOUT);
    assert!(Instant::now().checked_add(server.timeout).is_some());
    for timeout in [most + 1, u64::MAX] {
        let line = format!("status --timeout {timeout}");
        let error = format!("--timeout takes {TIMEOUT_TAKES}, not \"{timeout}\"");
        assert_eq!(usage(&line), error, "{timeout}");
    }
}

#[test]
fn number_flags_say_what_they_take() {
    for lag in ["0", "-1", "1.5", "30s", ""] {
        let line = format!("run s --wal logical --max-lag {lag}");
        let error = if lag.is_empty() {
            "--max-lag needs a value".to_owned()
        } else {
            format!("--max-lag takes whole seconds above 0, not {lag:?}")
        };
        assert_eq!(usage(&line), error, "{lag}");
    }
    for timeout in ["0", "-1", "1.5", "10s"] {
        let line = format!("status --timeout {timeout}");
        let error = format!("--timeout takes {TIMEOUT_TAKES}, not {timeout:?}");
        assert_eq!(usage(&line), error, "{timeout}");
    }
    for size in ["0", "0Gi", "4GB", "4gi", "Gi", "16777216Ti"] {
        let line = format!("run s --wal logical --max-retained-wal {size}");
        let error =
            format!("--max-retained-wal takes bytes above 0, as 4294967296 or 4Gi, not {size:?}");
        assert_eq!(usage(&line), error, "{size}");
    }
    for size in ["0", "0Gi", "4GB", "4gi", "Gi", "16777216Ti", "-1", "lots"] {
        let line = format!("run s --wal logical --max-memory {size}");
        let error = format!("--max-memory takes bytes above 0, as 4294967296 or 4Gi, not {size:?}");
        assert_eq!(usage(&line), error, "{size}");
    }
    for rows in ["0", "-1", "1.5", "10k", "1Ki"] {
        let line = format!("run s --wal logical --max-transaction-rows {rows}");
        let error = format!("--max-transaction-rows takes a count of rows above 0, not {rows:?}");
        assert_eq!(usage(&line), error, "{rows}");
    }
    for rows in ["0", "-1", "1.5", "10k", "1Ki", "lots"] {
        let line = format!("run s --wal logical --max-answer-rows {rows}");
        let error = format!("--max-answer-rows takes a count of rows above 0, not {rows:?}");
        assert_eq!(usage(&line), error, "{rows}");
    }
    assert_eq!(
        usage("plan s --wal logical --max-answer-rows 5"),
        "straum plan takes no \"--max-answer-rows\""
    );
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

/// Pieces a command line is built from: every command and flag, the
/// shapes values take, and the characters a parser trips on.
const PIECES: [&str; 35] = [
    "run",
    "plan",
    "--copy",
    "status",
    "views",
    "ask",
    "shell",
    "help",
    "version",
    "--url",
    "--timeout",
    "--json",
    "--wal",
    "--listen",
    "--max-lag",
    "--max-retained-wal",
    "--max-page-memory",
    "--max-memory",
    "--max-answer-rows",
    "-h",
    "-V",
    "--",
    "-",
    "=",
    "",
    "physical",
    "4Gi",
    "0",
    "-7",
    "18446744073709551615",
    "http://x:1",
    "SELECT",
    "$1",
    "é",
    " ",
];

/// A small seeded generator, so a failing line comes back on every run.
struct Words {
    state: u64,
}

impl Words {
    fn next(&mut self) -> u64 {
        // xorshift64: enough spread for picking pieces, and no dependency.
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    fn below(&mut self, bound: usize) -> usize {
        let bound = u64::try_from(bound).expect("a bound fits a u64");
        usize::try_from(self.next() % bound).expect("below a usize bound")
    }

    /// One word: a piece, or two joined, as `--url=x` or `-hx`.
    fn word(&mut self) -> String {
        let first = PIECES[self.below(PIECES.len())];
        if self.below(3) > 0 {
            return first.to_owned();
        }
        let second = PIECES[self.below(PIECES.len())];
        format!("{first}{second}")
    }

    fn line(&mut self) -> Vec<String> {
        let length = self.below(7);
        let mut words = Vec::with_capacity(length);
        for _ in 0..length {
            words.push(self.word());
        }
        words
    }
}

/// Everything a command line reaches: the parse, the error's words, the
/// help it may ask for, and the deadline a timeout sets.
fn read_all_of(words: &[String], environment: &Environment) {
    match command(words, environment) {
        Ok(Command::Help(Some(name))) => assert_ne!(name.help(), ""),
        Ok(
            Command::Status { server, .. }
            | Command::Views { server, .. }
            | Command::Ask { server, .. }
            | Command::Shell { server },
        ) => assert!(Instant::now().checked_add(server.timeout).is_some()),
        Ok(_) => {}
        Err(usage) => assert_ne!(usage.to_string(), ""),
    }
}

#[test]
fn no_command_line_panics() {
    let mut words = Words {
        state: 0x9E37_79B9_7F4A_7C15,
    };
    for _ in 0..20_000 {
        let line = words.line();
        let url = if words.below(2) == 0 {
            None
        } else {
            Some(words.word())
        };
        let environment = Environment { url };
        let read = std::panic::catch_unwind(|| read_all_of(&line, &environment));
        assert!(read.is_ok(), "panicked on {line:?} with {environment:?}");
    }
}
