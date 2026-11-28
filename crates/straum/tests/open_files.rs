//! The limit of open files that `straum run` needs. The test lowers the
//! limit of its own process, which a child process takes, so it has a
//! test binary of its own.

use std::process::Command;

use nix::sys::resource::{Resource, getrlimit, setrlimit};

const SPEC: &str = "CREATE VIEW events AS
SELECT buckets(e.at, '1 hour') AS bucket, e.kind AS kind, count(*) AS n
FROM public.events AS e
GROUP BY 1, 2;";

/// `straum run` on `spec` with this process's limits, against a port
/// where no PostgreSQL listens: its exit status and its log.
fn run(spec: &str) -> (Option<i32>, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_straum"))
        .args(["run", spec, "--wal", "logical", "--listen", "127.0.0.1:0"])
        .args(["--health-listen", "127.0.0.1:0"])
        .env(
            "STRAUM_DSN",
            "postgres://straum:straum@127.0.0.1:1/postgres",
        )
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.code(), log)
}

/// A container gets a soft limit of 1024 open files by default, and the
/// connections took every one: accepts failed, and the memory watch
/// ended Straum. Straum raises the soft limit, and does not start when
/// the hard limit cannot hold its connections.
#[test]
fn run_raises_the_limit_of_open_files_or_does_not_start() {
    let spec = std::env::temp_dir().join(format!("straum-open-files-{}.sql", std::process::id()));
    std::fs::write(&spec, SPEC).unwrap();
    let spec = spec.to_str().unwrap();
    let (_, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();

    setrlimit(Resource::RLIMIT_NOFILE, 64, hard).unwrap();
    let (code, log) = run(spec);
    assert_eq!(code, Some(1), "{log}");
    assert!(log.contains("straum: listening on 127.0.0.1:"), "{log}");
    assert!(!log.contains("open files"), "{log}");

    setrlimit(Resource::RLIMIT_NOFILE, 64, 64).unwrap();
    let (code, log) = run(spec);
    assert_eq!(code, Some(1), "{log}");
    let refused = "straum: the limit of open files (RLIMIT_NOFILE) is 64, under the ";
    assert!(log.starts_with(refused), "{log}");
    assert!(!log.contains("listening on"), "{log}");
    std::fs::remove_file(spec).unwrap();
}
