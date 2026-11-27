//! `straum plan`: the setup SQL, printed or written, from the DSN or
//! libpq's variables, the tables of several specs, and the copy scripts.

use std::fs;

use crate::{SPEC, Scratch, output, straum};

/// `plan --copy` writes both scripts for every spec into the directory,
/// made if missing, and needs no database.
#[test]
fn plan_copy_writes_the_scripts() {
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let other = scratch.file("other.sql", SPEC);
    let directory = scratch.path.join("copy/my out");
    let directory = directory.to_str().unwrap();
    let mut plan = straum(&["plan", &spec, &other, "--copy", directory]);
    let (code, stdout, stderr) = output(&mut plan);
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    let full = fs::canonicalize(directory).unwrap();
    let full = full.to_str().unwrap();
    let parsed = straum_notation::parse(SPEC).unwrap();
    let scripts = straum_pg::copy_scripts(&[parsed.clone(), parsed], full);
    assert_eq!(
        fs::read_to_string(format!("{full}/export.sql")).unwrap(),
        scripts.export
    );
    assert_eq!(
        fs::read_to_string(format!("{full}/load.sql")).unwrap(),
        scripts.load
    );
    assert!(
        stdout.contains(&format!("psql \"$SOURCE\" -f '{full}/export.sql'\n")),
        "{stdout}"
    );

    // A spec that doesn't parse writes nothing and names its file.
    let broken = scratch.file("broken.sql", "CREATE VIEW");
    let elsewhere = scratch.path.join("never");
    let elsewhere = elsewhere.to_str().unwrap();
    let mut refused = straum(&["plan", &spec, &broken, "--copy", elsewhere]);
    let (code, _, stderr) = output(&mut refused);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("broken.sql"), "{stderr}");
    assert!(!std::path::Path::new(elsewhere).exists());
}

/// `plan --tables` names each table of several specs once.
#[test]
fn plan_tables_names_each_table_once() {
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let other = scratch.file("other.sql", SPEC);
    let parsed = straum_notation::parse(SPEC).unwrap();
    let mut expected = straum_pg::table_names(&parsed).join("\n");
    expected.push('\n');
    let (code, stdout, stderr) = output(&mut straum(&["plan", &spec, &other, "--tables"]));
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    assert_eq!(stdout, expected);
    let (code, _, stderr) = output(&mut straum(&["plan", "--wal", "logical", &spec, &other]));
    assert_eq!(code, Some(2), "{stderr}");
}

#[test]
fn plan_prints_the_setup_sql() {
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let parsed = straum_notation::parse(SPEC).unwrap();
    let (code, stdout, stderr) = output(&mut straum(&["plan", "--wal", "logical", &spec]));
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    assert_eq!(
        stdout,
        straum_pg::plan(
            &parsed,
            "straum",
            "straum",
            None,
            straum_pg::Replication::Logical
        )
    );
    let mut custom = straum(&[
        "plan",
        "--wal",
        "logical",
        "--publication",
        "p",
        &spec,
        "--role",
        "r",
    ]);
    let (_, stdout, _) = output(&mut custom);
    assert_eq!(
        stdout,
        straum_pg::plan(&parsed, "p", "r", None, straum_pg::Replication::Logical)
    );

    // With a DSN the role is its user, with its password as a verifier;
    // --role still names the role. The password has a hyphen, which no
    // base64 verifier can contain, so the check can't trip on chance.
    let dsn = scratch.file("dsn", "postgres://reader:plain-pw@db/app\n");
    let mut with_dsn = straum(&["plan", "--wal", "logical", &spec, "--dsn-file", &dsn]);
    let (code, stdout, stderr) = output(&mut with_dsn);
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    let role = "CREATE ROLE \"reader\" LOGIN REPLICATION PASSWORD 'SCRAM-SHA-256$4096:";
    assert!(stdout.contains(role), "{stdout}");
    assert!(
        !stdout.contains("plain-pw") && !stdout.contains("\\password"),
        "{stdout}"
    );
    let mut named = straum(&["plan", "--wal", "logical", &spec, "--role", "r"]);
    named.env("STRAUM_DSN", "postgres://reader@db/app");
    let (_, stdout, _) = output(&mut named);
    assert!(
        stdout.contains("CREATE ROLE \"r\" LOGIN REPLICATION;\n"),
        "{stdout}"
    );
    // A line break in the role would end a comment line and start a
    // statement: refused, from the flag or the DSN's user.
    let broken = "r\nDROP TABLE t; --";
    let (code, stdout, stderr) = output(&mut straum(&[
        "plan", "--wal", "logical", &spec, "--role", broken,
    ]));
    assert_eq!((code, stdout.as_str()), (Some(1), ""));
    assert_eq!(
        stderr,
        "straum: the role \"r\\nDROP TABLE t; --\" holds a control character\n"
    );
    let mut from_dsn = straum(&["plan", "--wal", "logical", &spec]);
    from_dsn.env("STRAUM_DSN", "postgres://r%0ADROP@db/app");
    assert_eq!(output(&mut from_dsn).0, Some(1));
    let mut bad = straum(&["plan", "--wal", "logical", &spec]);
    bad.env("STRAUM_DSN", "mysql://reader@db/app");
    let (code, _, stderr) = output(&mut bad);
    assert_eq!(code, Some(1));
    assert_eq!(
        stderr,
        "straum: DSN must start with postgres:// or postgresql://\n"
    );
}

#[test]
fn plan_reads_the_libpq_variables() {
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let mut command = straum(&["plan", "--wal", "logical", &spec]);
    command
        .env("PGHOST", "db.example")
        .env("PGUSER", "reader")
        .env("PGPASSWORD", "secret")
        .env("PGDATABASE", "app")
        .env("PGSSLMODE", "disable");
    let (code, stdout, stderr) = output(&mut command);
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    assert!(
        stdout.contains("CREATE ROLE \"reader\" LOGIN REPLICATION PASSWORD "),
        "{stdout}"
    );
    assert!(!stdout.contains("secret"), "{stdout}");
    let mut both = straum(&["plan", "--wal", "logical", &spec]);
    both.env(
        "STRAUM_DSN",
        "postgres://owner@db.example/app?sslmode=disable",
    )
    .env("PGUSER", "reader")
    .env("PGHOST", "other.example");
    let (code, stdout, stderr) = output(&mut both);
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    assert!(stdout.contains("CREATE ROLE \"owner\""), "{stdout}");
    assert!(!stdout.contains("\"reader\""), "{stdout}");
    // An empty STRAUM_DSN is unset, as an empty STRAUM_ variable is.
    let mut empty = straum(&["plan", "--wal", "logical", &spec]);
    empty
        .env("STRAUM_DSN", "")
        .env("PGHOST", "db.example")
        .env("PGUSER", "reader");
    let (code, stdout, stderr) = output(&mut empty);
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    assert!(stdout.contains("CREATE ROLE \"reader\""), "{stdout}");
}

#[test]
fn plan_gives_the_dsn_password_only_to_its_user() {
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let with_role = |role: &str| {
        let mut command = straum(&["plan", "--wal", "logical", &spec, "--role", role]);
        command
            .env("PGHOST", "db.example")
            .env("PGUSER", "postgres")
            .env("PGPASSWORD", "adminpw");
        output(&mut command)
    };
    let (code, stdout, stderr) = with_role("postgres");
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    let verifier = "CREATE ROLE \"postgres\" LOGIN REPLICATION PASSWORD 'SCRAM-SHA-256$";
    assert!(stdout.contains(verifier), "{stdout}");
    // The password is the DSN user's, so another role gets none.
    let (code, stdout, stderr) = with_role("straum");
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    assert!(
        stdout.contains("CREATE ROLE \"straum\" LOGIN REPLICATION;\n"),
        "{stdout}"
    );
    assert!(!stdout.contains("PASSWORD"), "{stdout}");
}

#[test]
fn plan_can_write_the_setup_sql() {
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let out = scratch.path.join("setup.sql");
    let out = out.to_str().unwrap();
    let parsed = straum_notation::parse(SPEC).unwrap();
    let (code, stdout, stderr) = output(&mut straum(&[
        "plan", "--wal", "logical", &spec, "--out", out,
    ]));
    assert_eq!((code, stdout.as_str(), stderr.as_str()), (Some(0), "", ""));
    let wrote = fs::read_to_string(out).unwrap();
    assert_eq!(
        wrote,
        straum_pg::plan(
            &parsed,
            "straum",
            "straum",
            None,
            straum_pg::Replication::Logical
        )
    );
    let missing = scratch.path.join("missing").join("setup.sql");
    let missing = missing.to_str().unwrap();
    let (code, stdout, stderr) = output(&mut straum(&[
        "plan", "--wal", "logical", &spec, "--out", missing,
    ]));
    assert_eq!((code, stdout.as_str()), (Some(1), ""));
    let start = format!("straum: writing {missing}: ");
    assert!(stderr.starts_with(&start), "{stderr}");
}
