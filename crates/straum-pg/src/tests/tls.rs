//! Connections over TLS: Straum follows a server with ssl on, trusts
//! only the roots it is given, and names what went wrong when the TLS
//! handshake fails against a fake server.

use std::path::Path;
use std::thread;
use std::time::Duration;

use straum_app::Step;

use super::references::*;
use super::support::*;
use crate::dsn::Dsn;
use crate::error::Error;
use crate::wire::{Connection, ConnectionKind};

#[test]
fn follows_the_database_over_tls() {
    let pg = postgres_tls();
    let mut db = Db::new(&pg, "tls", TABLES);
    let mut rng = Rng::new(11);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5), (2, 'Bronx', NULL)");
    for id in 0..50 {
        db.run(&rng.trip(id));
    }
    db.run("INSERT INTO $S.logins VALUES (1, to_timestamp(1705276800), 1)");
    let mut writer = db.writer(SPEC);
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the load");
    for id in 50..60 {
        db.run(&rng.trip(id));
        applied(&mut writer);
    }
    // Past wal_sender_timeout (4 s): reads time out and keepalives are
    // answered, over TLS.
    thread::sleep(Duration::from_secs(6));
    // An altered table stops the writer; the next one loads again, on a
    // new TLS connection. The new Relation comes with the next change to
    // the table.
    db.run("ALTER TABLE $S.logins ADD COLUMN note text");
    db.run("INSERT INTO $S.logins VALUES (2, to_timestamp(1705276800), 1)");
    applied_or_started_again(&db, SPEC, &mut writer);
    for id in 60..70 {
        db.run(&rng.trip(id));
        applied(&mut writer);
    }
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");
    let tls = db.rows(
        "SELECT DISTINCT a.backend_type, s.ssl FROM pg_stat_activity a
         JOIN pg_stat_ssl s USING (pid) WHERE a.application_name = 'straum' ORDER BY 1",
    );
    assert_eq!(tls, [over_tls("client backend"), over_tls("walsender")]);
}

/// A `pg_stat_activity` row of `backend_type` that runs over TLS.
fn over_tls(backend_type: &str) -> Vec<Option<String>> {
    vec![Some(backend_type.to_owned()), Some("t".to_owned())]
}

/// Whether the session `dsn` opens runs over TLS, as `pg_stat_ssl` says.
fn ssl(dsn: &str) -> Result<String, Error> {
    let mut connection = Connection::connect(&Dsn::parse(dsn).unwrap(), ConnectionKind::Sql)?;
    let rows = connection.rows("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")?;
    Ok(rows[0][0].clone().unwrap())
}

#[test]
fn tls_trusts_only_its_roots() {
    let pg = postgres_tls();
    // Display leaves the password out.
    let dsn = (pg.dsn.to_string()).replace("postgres@", &format!("postgres:{PASSWORD}@"));
    assert_eq!(ssl(&dsn).unwrap(), "t");
    let (plain, _) = dsn.split_once('?').unwrap();
    assert_eq!(ssl(&format!("{plain}?sslmode=disable")).unwrap(), "f");

    // The roots built in don't know the test's CA.
    let error = ssl(&format!("{plain}?sslmode=require")).unwrap_err();
    assert_eq!(
        error.to_string(),
        "TLS to PostgreSQL: invalid peer certificate: UnknownIssuer"
    );
    // The certificate names 127.0.0.1, not localhost; verify-ca checks the
    // name too.
    let localhost = dsn.replace("127.0.0.1", "localhost") + "&sslmode=verify-ca";
    let error = ssl(&localhost).unwrap_err();
    assert_eq!(
        error.to_string(),
        "TLS to PostgreSQL: invalid peer certificate: certificate not valid for name \"localhost\"; \
         certificate is only valid for IpAddress(127.0.0.1)"
    );
}

#[test]
fn tls_start_errors() {
    for case in refusals_without_a_ca_file() {
        expect_tls_start_error(&case);
    }
    let missing = std::env::temp_dir().join("straum-no-such-ca.pem");
    let empty = TempFile::new("empty.pem", b"");
    let junk = TempFile::new(
        "junk.pem",
        b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
    );
    let open = TempFile::new("open.pem", b"-----BEGIN CERTIFICATE-----\nAAAA\n");
    let cases = [
        trusting(
            &missing,
            "TLS to PostgreSQL: reading {}: I/O error: No such file or directory (os error 2)",
        ),
        trusting(&empty.0, "TLS to PostgreSQL: {} holds no CA certificate"),
        trusting(&junk.0, "TLS to PostgreSQL: {} holds no CA certificate"),
        trusting(
            &open.0,
            "TLS to PostgreSQL: reading {}: missing section end marker",
        ),
    ];
    for case in cases {
        expect_tls_start_error(&case);
    }
}

/// A fake server's answer to `SSLRequest`, the CA file the client trusts,
/// or the roots built in, and the start of the error the client gets.
struct TlsCase {
    answer: Vec<u8>,
    ca: Option<String>,
    expected: String,
}

/// Answers that fail before any certificate is read.
fn refusals_without_a_ca_file() -> [TlsCase; 4] {
    [
        answered(
            b"N",
            "TLS to PostgreSQL: PostgreSQL has ssl off; turn it on, \
             or use sslmode=disable on a trusted network",
        ),
        answered(b"E", "PostgreSQL sent 0x45 as its answer to SSLRequest"),
        answered(b"", "connection to PostgreSQL: failed to fill whole buffer"),
        answered(
            b"Snot a TLS record",
            "TLS to PostgreSQL: received corrupt message of type InvalidContentType",
        ),
    ]
}

/// The server answers `answer`; the client trusts the roots built in.
fn answered(answer: &[u8], expected: &str) -> TlsCase {
    TlsCase {
        answer: answer.to_vec(),
        ca: None,
        expected: expected.to_owned(),
    }
}

/// The server agrees to TLS; the client trusts the CA file `file`, whose
/// path takes the place of `{}` in `expected`.
fn trusting(file: &Path, expected: &str) -> TlsCase {
    let path = file.display().to_string();
    TlsCase {
        answer: b"S".to_vec(),
        expected: expected.replace("{}", &path),
        ca: Some(path),
    }
}

fn expect_tls_start_error(case: &TlsCase) {
    let error = tls_start_error(case.answer.clone(), case.ca.clone());
    let expected = &case.expected;
    assert!(error.starts_with(expected), "{error:?} is not {expected:?}");
}

/// The error a TLS connection gets from a server that answers `answer`,
/// trusting `ca`, or the roots built in.
fn tls_start_error(answer: Vec<u8>, ca: Option<String>) -> String {
    let server = scripted(vec![now(answer)]);
    let tls = match ca {
        Some(ca) => format!("sslrootcert={ca}"),
        None => "sslmode=require".to_owned(),
    };
    let dsn = Dsn::parse(&server.dsn.to_string().replace("sslmode=disable", &tls)).unwrap();
    Connection::connect(&dsn, ConnectionKind::Sql)
        .err()
        .unwrap()
        .to_string()
}
