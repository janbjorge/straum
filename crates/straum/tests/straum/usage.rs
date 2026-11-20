//! What a person reads when the command line is wrong or asks for help:
//! help and version, usage errors with exit 2, and the errors of a run
//! that can't start.

use std::net::TcpListener;

use crate::{SPEC, Scratch, output, straum};

#[test]
fn prints_help_and_version() {
    let (code, stdout, _) = output(&mut straum(&["help"]));
    assert_eq!(code, Some(0));
    assert!(stdout.contains("usage: straum <command>"), "{stdout}");
    let (code, stdout, _) = output(&mut straum(&["version"]));
    assert_eq!(code, Some(0));
    assert_eq!(stdout, format!("straum {}\n", env!("CARGO_PKG_VERSION")));
    for (command, usage) in [
        ("views", "usage: straum views [flags]"),
        ("ask", "usage: straum ask [flags] QUERY [PARAMETER...]"),
        ("shell", "usage: straum shell [flags]"),
    ] {
        let (code, stdout, _) = output(&mut straum(&[command, "--help"]));
        assert_eq!(code, Some(0));
        assert!(stdout.starts_with(usage), "{stdout}");
    }
    let (code, stdout, _) = output(&mut straum(&["plan", "--help"]));
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("usage: straum plan FILE"), "{stdout}");
}

#[test]
fn rejects_bad_usage() {
    let (code, _, stderr) = output(&mut straum(&[]));
    assert_eq!(code, Some(2));
    assert!(stderr.contains("usage: straum <command>"), "{stderr}");
    let (code, _, stderr) = output(&mut straum(&["run", "--port", "1"]));
    assert_eq!(code, Some(2));
    assert_eq!(
        stderr,
        "straum: straum run takes no \"--port\"\ntry: straum run --help\n"
    );
    let (code, _, stderr) = output(&mut straum(&["ask"]));
    assert_eq!(code, Some(2));
    assert_eq!(
        stderr,
        "straum: straum ask needs a query\ntry: straum ask --help\n"
    );
    let (code, _, stderr) = output(&mut straum(&["serve"]));
    assert_eq!(code, Some(2));
    assert_eq!(
        stderr,
        "straum: unknown command \"serve\"\ntry: straum help\n"
    );
}

#[test]
fn says_what_is_wrong() {
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let bad = scratch.file("bad.sql", "CREATE VIEW v AS SELECT 1;\n");
    let missing = scratch.path.join("missing").to_str().unwrap().to_owned();
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = taken.local_addr().unwrap().to_string();
    let not_found = format!("straum: reading {missing}: No such file or directory (os error 2)\n");

    let cases: &[(&[&str], Option<&str>, String)] = &[
        (
            &["plan", "--wal", "logical", &missing],
            None,
            not_found.clone(),
        ),
        (
            &["plan", "--wal", "logical", &bad],
            None,
            format!("straum: {bad}: line 1: "),
        ),
        (
            &["run", "--wal", "logical", &spec],
            None,
            "straum: set STRAUM_DSN, pass --dsn-file, or set PGHOST\n".to_owned(),
        ),
        (
            &["run", "--wal", "logical", &spec],
            Some("mysql://straum@db/straum"),
            "straum: DSN must start with postgres:// or postgresql://\n".to_owned(),
        ),
        (
            &["run", "--wal", "logical", &spec, "--dsn-file", &missing],
            Some("postgres://straum@db/straum"),
            not_found,
        ),
        (
            &["run", "--wal", "logical", &spec, "--listen", "nowhere"],
            Some("postgres://straum@db/straum"),
            "straum: listening on nowhere: invalid socket address\n".to_owned(),
        ),
        (
            &["run", "--wal", "logical", &spec, "--listen", &taken],
            Some("postgres://straum:secret@db/straum"),
            format!("straum: listening on {taken}: Address already in use"),
        ),
    ];
    for (args, dsn, error) in cases {
        let mut command = straum(args);
        if let Some(dsn) = dsn {
            command.env("STRAUM_DSN", dsn);
        }
        let (code, stdout, stderr) = output(&mut command);
        assert_eq!(code, Some(1), "{args:?}: {stderr}");
        assert_eq!(stdout, "");
        assert!(
            stderr.starts_with(error.as_str()),
            "{args:?}: {stderr:?} is not {error:?}"
        );
        assert!(!stderr.contains("secret"), "{stderr}");
    }
}
