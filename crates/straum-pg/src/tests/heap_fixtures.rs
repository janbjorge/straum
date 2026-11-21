//! Writes the heap fixtures `wal/heap/fixture.rs` reads: one table's
//! records after a checkpoint, through inserts, a bulk `COPY`, updates
//! on and off the page, deletes and a vacuum that prunes and freezes, and
//! the rows a `SELECT` reads after. Run once per major when the fixtures
//! change: `just postgres=18 heap-fixtures`.

use std::fs;
use std::path::PathBuf;

use super::streams::*;
use super::support::*;
use crate::physical::Physical;
use crate::wal::heap::fixture::{self, Fixture, FixtureRow, LoggedRecord};

/// What the fixture's table goes through after the checkpoint.
const CHURN: &str = "
INSERT INTO $S.t SELECT i, 'row ' || i FROM generate_series(1, 300) AS i;
COPY $S.t FROM PROGRAM 'seq 301 400 | sed s/$/,copied/' WITH (FORMAT csv);
UPDATE $S.t SET body = body || '!' WHERE id % 7 = 0;
UPDATE $S.t SET body = repeat('long', 200) WHERE id % 50 = 0;
DELETE FROM $S.t WHERE id % 11 = 0;
VACUUM (FREEZE) $S.t;
UPDATE $S.t SET body = 'after' WHERE id % 13 = 0;
VACUUM $S.t;
";

#[test]
#[ignore = "writes the heap fixture of the major under test"]
fn capture_heap_fixture() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(
        &pg,
        "fixture",
        "CREATE TABLE $S.t (id int PRIMARY KEY, body text)
             WITH (fillfactor = 70, autovacuum_enabled = off);",
    );
    let (mut physical, _) = Physical::start(&pg.dsn).unwrap();
    db.run("CHECKPOINT");
    // One at a time: VACUUM runs in no transaction block.
    for statement in CHURN.split(";\n") {
        if !statement.trim().is_empty() {
            db.run(statement);
        }
    }
    let TableFile { relation, .. } = relation_file(&mut db, "t");
    let mut records = Vec::new();
    for record in &records_to_switch(&mut db, &mut physical).records {
        let mut ours = false;
        for block in &record.blocks {
            ours = ours || block.file == relation;
        }
        if ours {
            records.push(LoggedRecord {
                lsn: record.lsn,
                end: record.end,
                bytes: record.logged().to_vec(),
            });
        }
    }
    let mut rows = Vec::new();
    for row in db.rows("SELECT id, body FROM $S.t ORDER BY id") {
        rows.push(FixtureRow {
            id: row[0].as_deref().unwrap().parse().unwrap(),
            body: row[1].clone().unwrap(),
        });
    }
    let major = physical.major();
    let captured = Fixture {
        file: relation,
        major,
        records,
        rows,
    };
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("src/wal/heap/fixtures/pg{major}.bin"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, fixture::write(&captured)).unwrap();
    eprintln!(
        "{}: {} records, {} rows",
        path.display(),
        captured.records.len(),
        captured.rows.len()
    );
}
