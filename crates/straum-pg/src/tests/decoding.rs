//! The physical stream against logical decoding on one server: both read
//! the same WAL, and the commits the physical stream yields equal a
//! logical slot's byte for byte, for fixed and random workloads.
//! `Pipeline` runs the physical stream's path from records to commits.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use straum_app::{Commit, Lsn, Source, Step};
use straum_domain::Spec;

use super::physical::catch_up;
use super::references::*;
use super::support::*;
use crate::catalog::{self, Checked};
use crate::physical::{self, Physical};
use crate::source::Postgres;
use crate::wal::transactions::TransactionSnapshot;
use crate::wire::{Connection, ConnectionKind};

/// On one server with logical decoding, the physical stream yields the
/// commits a logical slot yields, byte for byte: inserts, `COPY`, HOT and
/// other updates, rows moving page, deletes, `ON CONFLICT`, savepoints
/// kept and rolled back, aborts, two-phase commit, `VACUUM`, a checkpoint
/// between, and changes to tables outside the spec.
#[test]
fn commits_what_logical_decoding_commits() {
    let pg = plain(image(&tag()).with_cmd(&[
        "-c",
        "wal_level=logical",
        "-c",
        "max_prepared_transactions=4",
    ]));
    // A column between those the spec reads, dropped once rows hold it.
    let tables = TABLES.replace("    kind text,\n", "    junk text,\n    kind text,\n");
    let mut db = Db::new(&pg, "twin", &tables);
    db.run("CREATE TABLE $S.other (id int, note text)");
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5), (2, 'Bronx', NULL), (3, 'Manhattan', 12.5)");
    db.run(
        "INSERT INTO $S.trips (id, at, junk, kind, zone_id, fare, rider, tip)
         SELECT i, timestamptz '2024-01-01' + i * interval '7 minutes', repeat('j', 3 + i % 200),
                'cab', 1 + i % 3, i + 0.5, 'ann', i
         FROM generate_series(0, 39) AS i",
    );
    db.run("ALTER TABLE $S.trips DROP COLUMN junk");
    let mut rng = Rng::new(21);
    let (mut stream, _) = Physical::start(&pg.dsn).unwrap();
    let spec = db.spec(SPEC);
    let mut logical = db.pg();
    let start = logical.open(&spec).unwrap();
    load_all(&mut logical, &spec).unwrap();
    let mut pipeline = Pipeline::new(&pg, &spec, &db.name, stream.major());

    write_what_logical_decoding_reads(&mut db, &mut rng);
    // Read before the last commit: other WAL, autovacuum's or a
    // RUNNING_XACTS, may follow it, which the logical slot sends nothing
    // for.
    let before = db.insert_lsn();
    db.run("UPDATE $S.trips SET rider = 'di' WHERE id % 11 = 0");

    let theirs = logical_commits_past(&mut logical, before, "");
    let ours = commits_after(pipeline.through_now(&mut db, &mut stream), start);
    assert!(theirs.len() > 100, "{} commits", theirs.len());
    for (ours, theirs) in ours.iter().zip(&theirs) {
        assert_eq!(ours, theirs);
    }
    assert_eq!(ours.len(), theirs.len());
}

/// The commits the logical slot of `logical` sends until one ends past
/// `before`; `context` starts the message if none comes.
fn logical_commits_past(logical: &mut Postgres, before: Lsn, context: &str) -> Vec<Commit> {
    let mut commits: Vec<Commit> = Vec::new();
    let wait = Wait::new(&format!("{context}a logical commit past {before}"));
    loop {
        if let Some(last) = commits.last()
            && last.lsn > before
        {
            return commits;
        }
        wait.check();
        commits.extend(logical.next_commit(Duration::from_millis(100)).unwrap());
    }
}

/// The commits of `commits` that end past `start`.
fn commits_after(commits: Vec<Commit>, start: Lsn) -> Vec<Commit> {
    let mut after = Vec::new();
    for commit in commits {
        if commit.lsn > start {
            after.push(commit);
        }
    }
    after
}

/// Inserts one at a time and in a batch, HOT and other updates, rows
/// moving page, deletes, DDL logical decoding reads through, savepoints,
/// an abort, two-phase commit, `COPY`, `ON CONFLICT` and `VACUUM`, with a
/// checkpoint between and changes to a table outside the spec.
fn write_what_logical_decoding_reads(db: &mut Db, rng: &mut Rng) {
    for id in 40..140 {
        db.run(&rng.trip(id));
    }
    let mut batch = String::new();
    for id in 140..200 {
        batch.push_str(&rng.trip(id));
    }
    db.run(&format!("BEGIN; {batch} COMMIT"));
    db.run("UPDATE $S.trips SET fare = fare + 1 WHERE id % 3 = 0");
    db.run("CHECKPOINT");
    db.run("UPDATE $S.trips SET tip = tip + 1, kind = kind || '!' WHERE id % 4 = 1");
    db.run("UPDATE $S.trips SET id = id + 1000 WHERE id % 7 = 0");
    db.run("DELETE FROM $S.trips WHERE id % 5 = 0");
    db.run("UPDATE $S.zones SET borough = 'Brooklyn', toll = 2.0 WHERE id = 2");
    // DDL logical decoding reads through: the decoder gets the table
    // again, and older rows lack the new columns.
    db.run("ALTER TABLE $S.trips ADD extra text DEFAULT 'filler', ADD note int");
    db.run("UPDATE $S.trips SET fare = fare + 2 WHERE id % 6 = 0");
    db.run(&rng.trip(300));
    db.run("ALTER TABLE $S.trips DROP COLUMN note");
    db.run("CREATE INDEX ON $S.trips (zone_id)");
    db.run("ANALYZE $S.trips");
    db.run("BEGIN; ALTER TABLE $S.zones ADD region text DEFAULT 'east'; UPDATE $S.zones SET toll = 3.5 WHERE id = 1; COMMIT");
    db.run("UPDATE $S.zones SET toll = 4.5 WHERE id = 3");
    db.run(
        "BEGIN; INSERT INTO $S.logins VALUES (1, now(), 1); SAVEPOINT a;
         INSERT INTO $S.logins VALUES (2, now(), 2); ROLLBACK TO a;
         SAVEPOINT b; UPDATE $S.trips SET tip = 0 WHERE id = 1; RELEASE b;
         INSERT INTO $S.other VALUES (1, 'not read'); INSERT INTO $S.logins VALUES (3, now(), 3);
         COMMIT",
    );
    db.run("BEGIN; DELETE FROM $S.trips; ROLLBACK");
    db.run(
        "BEGIN; INSERT INTO $S.logins VALUES (4, now(), 1); SAVEPOINT a;
         DELETE FROM $S.logins WHERE id = 3; PREPARE TRANSACTION 'two'",
    );
    db.run("INSERT INTO $S.other VALUES (2, 'between')");
    db.run("COMMIT PREPARED 'two'");
    db.run(
        "COPY (SELECT 5000 + i, timestamptz '2024-01-01' + i * interval '1 minute', 'cab', 1 + i % 3,
                      i % 70 + 0.25, NULL, i % 9
               FROM generate_series(1, 150) AS i) TO '/tmp/trips.csv'",
    );
    db.run("COPY $S.trips (id, at, kind, zone_id, fare, rider, tip) FROM '/tmp/trips.csv'");
    db.run(
        "INSERT INTO $S.trips VALUES (5001, now(), 'pool', 2, 1.00, 'ann', 1), (9999, now(), 'van', 3, 2.50, 'bo', 2)
         ON CONFLICT (id) DO UPDATE SET fare = excluded.fare",
    );
    db.run("VACUUM $S.trips");
}

/// Random workloads, a seed each, over sessions whose transactions
/// interleave: inserts, HOT and other updates, keys changing, deletes,
/// savepoints kept and rolled back, aborts, two-phase commit, columns
/// added and dropped, `VACUUM` and checkpoints between. On one server
/// with logical decoding, the physical stream yields the commits a
/// logical slot yields, byte for byte. `STRAUM_TEST_SEEDS` sets how many.
#[test]
fn commits_what_logical_decoding_commits_at_random() {
    let pg = plain(image(&tag()).with_cmd(&[
        "-c",
        "wal_level=logical",
        "-c",
        "max_prepared_transactions=8",
    ]));
    for seed in seeds() {
        let mut rng = Rng::new(1_000 + seed);
        let mut db = random_db(&pg, &format!("twin{seed}"), &mut rng);
        let (mut stream, _) = Physical::start(&pg.dsn).unwrap();
        let spec = db.spec(SPEC);
        let mut logical = db.pg();
        let start = logical.open(&spec).unwrap();
        load_all(&mut logical, &spec).unwrap();
        let mut pipeline = Pipeline::new(&pg, &spec, &db.name, stream.major());
        let mut workload = Workload::new(&db);
        for _ in 0..STEPS {
            workload.step(&mut rng, &mut db);
        }
        workload.finish(&mut db);
        // A published commit last, for the logical slot to reach, past
        // a position read before it: other WAL, autovacuum's or a
        // RUNNING_XACTS, may follow it, which the slot sends nothing for.
        let before = db.insert_lsn();
        db.run(&rng.trip(u64::MAX >> 8));
        let theirs = logical_commits_past(&mut logical, before, &format!("seed {seed}: "));
        let ours = commits_after(pipeline.through_now(&mut db, &mut stream), start);
        assert!(theirs.len() > 50, "seed {seed}: {} commits", theirs.len());
        for (ours, theirs) in ours.iter().zip(&theirs) {
            assert_eq!(ours, theirs, "seed {seed}");
        }
        assert_eq!(ours.len(), theirs.len(), "seed {seed}");
        let done = workload.done;
        assert!(done.all_ran(), "seed {seed}: {done:?}");
    }
}

/// The same workloads under `wal_level = replica`, where PostgreSQL logs
/// less: an update only the bytes that changed, a delete no old row, an
/// insert with a page image no row. Straum starts at a random step, with
/// transactions open, prepared or committing all around, and its views
/// then equal SQL's.
#[test]
fn follows_random_workloads_from_a_random_start() {
    let pg = plain(image(&tag()).with_cmd(&[
        "-c",
        "wal_level=replica",
        "-c",
        "max_prepared_transactions=8",
    ]));
    for seed in seeds() {
        let mut rng = Rng::new(2_000 + seed);
        let mut db = random_db(&pg, &format!("replica{seed}"), &mut rng);
        let start_at = rng.below(STEPS);
        let (starting, start) = std::sync::mpsc::channel();
        let started = Arc::new(AtomicBool::new(false));
        let streaming = Arc::clone(&started);
        let mut worker = db.another_connection();
        let mut worker_rng = Rng::new(3_000 + seed);
        let workload = thread::spawn(move || {
            let mut workload = Workload::new(&worker);
            for _ in 0..start_at {
                workload.step(&mut worker_rng, &mut worker);
            }
            starting.send(()).unwrap();
            // On through Straum's start, then as many steps again while
            // it streams.
            while !streaming.load(Ordering::Acquire) {
                workload.step(&mut worker_rng, &mut worker);
            }
            for _ in 0..STEPS / 2 {
                workload.step(&mut worker_rng, &mut worker);
            }
            workload.finish(&mut worker);
            workload.done
        });
        start.recv().unwrap();
        let mut writer = db.physical_writer(SPEC);
        let step = writer.step();
        assert!(
            matches!(step, Ok(Step::Started(_))),
            "seed {seed}: {step:?}"
        );
        started.store(true, Ordering::Release);
        let done = workload.join().unwrap();
        assert!(done.all_ran(), "seed {seed}: {done:?}");
        let mut next = FIRST_ID_AFTER_THE_WORKLOAD;
        catch_up(&mut db, &mut writer, &mut rng, &mut next);
    }
}

/// Trip ids from here on are past every id a workload gives.
const FIRST_ID_AFTER_THE_WORKLOAD: u64 = 1 << 40;

/// The seeds the random workloads run: `STRAUM_TEST_SEEDS` of them (a
/// few by default) from `STRAUM_TEST_SEED_FROM` (0), so CI can try new
/// ones each run and a failure names the seed that replays it.
fn seeds() -> std::ops::Range<u64> {
    let from = env_number("STRAUM_TEST_SEED_FROM", 0);
    from..from + env_number("STRAUM_TEST_SEEDS", 6)
}

/// The number the environment variable `name` holds, or `default`.
fn env_number(name: &str, default: u64) -> u64 {
    let Ok(text) = std::env::var(name) else {
        return default;
    };
    text.parse().unwrap_or(default)
}

/// Steps of each random workload.
const STEPS: u64 = 400;

/// A schema for a random workload: a zone per session, rows before the
/// start, and `tip` dropped and added again with a default, so older rows
/// lack a column the spec reads.
fn random_db(pg: &PostgresContainer, name: &str, rng: &mut Rng) -> Db {
    let mut db = Db::new(pg, name, TABLES);
    db.run("CREATE TABLE $S.other (id int, note text)");
    db.run(&format!(
        "INSERT INTO $S.zones SELECT i, 'z' || i, i FROM generate_series(1, {SESSIONS}) AS i"
    ));
    for id in 0..60 {
        db.run(&rng.trip(id));
    }
    db.run("ALTER TABLE $S.trips DROP COLUMN tip, ADD tip int DEFAULT 7");
    db
}

/// Sessions of [`Workload`]; each changes only its own rows, so one thread
/// drives them all without waiting on a lock.
const SESSIONS: u64 = 4;

/// Several sessions in one thread. Session `s` owns the trips whose id is
/// `s` modulo [`SESSIONS`] and zone `s + 1`.
struct Workload {
    sessions: Vec<Session>,
    schema: String,
    next_id: u64,
    added: Vec<String>,
    columns: u64,
    gids: u64,
    done: Done,
}

/// How often each kind of step ran.
#[derive(Debug, Default, Clone, Copy)]
struct Done {
    insert: u32,
    update: u32,
    key_change: u32,
    delete: u32,
    savepoint: u32,
    commit: u32,
    abort: u32,
    prepare: u32,
    ddl: u32,
    checkpoint: u32,
    vacuum: u32,
}

impl Done {
    /// Whether every kind of step ran at least once.
    fn all_ran(&self) -> bool {
        let counts = [
            self.insert,
            self.update,
            self.key_change,
            self.delete,
            self.savepoint,
            self.commit,
            self.abort,
            self.prepare,
            self.ddl,
            self.checkpoint,
            self.vacuum,
        ];
        for count in counts {
            if count == 0 {
                return false;
            }
        }
        true
    }
}

/// One session of a workload: its connection, whether a transaction is
/// open, how deep its savepoints go.
struct Session {
    connection: Connection,
    open: bool,
    savepoints: u32,
    /// A prepared transaction of this session, holding its rows.
    prepared: Option<String>,
}

impl Workload {
    /// A workload of [`SESSIONS`] sessions over `db`'s schema.
    fn new(db: &Db) -> Workload {
        let mut sessions = Vec::new();
        for _ in 0..SESSIONS {
            sessions.push(Session {
                connection: Connection::connect(&db.dsn, ConnectionKind::Sql).unwrap(),
                open: false,
                savepoints: 0,
                prepared: None,
            });
        }
        Workload {
            sessions,
            schema: db.name.clone(),
            next_id: 1_000,
            added: Vec::new(),
            columns: 0,
            gids: 0,
            done: Done::default(),
        }
    }

    /// A new trip id that `session` owns.
    fn id(&mut self, session: u64) -> u64 {
        self.next_id += 1;
        self.next_id * SESSIONS + session
    }

    /// One random step of one session, or one between transactions.
    fn step(&mut self, rng: &mut Rng, db: &mut Db) {
        if rng.chance(6) {
            return self.between(rng, db);
        }
        let session = rng.below(SESSIONS);
        let index = usize::try_from(session).unwrap();
        if let Some(gid) = self.sessions[index].prepared.take() {
            let end = if rng.chance(70) { "COMMIT" } else { "ROLLBACK" };
            db.run(&format!("{end} PREPARED '{gid}'"));
            return;
        }
        // Picks which of the session's rows a statement changes.
        let residue = rng.below(7);
        let schema = self.schema.clone();
        let statement = match rng.below(12) {
            0..=2 => {
                self.done.insert += 1;
                let mut inserts = String::new();
                for _ in 0..=rng.below(4) {
                    let id = self.id(session);
                    inserts.push_str(&rng.trip(id));
                }
                inserts
            }
            3 | 4 => {
                self.done.update += 1;
                random_update(rng, session, residue)
            }
            5 => {
                self.done.key_change += 1;
                format!(
                    "UPDATE $S.trips SET id = id + {} WHERE id % {SESSIONS} = {session} AND id % 11 = {residue};",
                    SESSIONS * 1_000_000
                )
            }
            6 => {
                self.done.delete += 1;
                format!(
                    "DELETE FROM $S.trips WHERE id % {SESSIONS} = {session} AND id % 13 = {residue};"
                )
            }
            7 => {
                let id = self.id(session);
                format!(
                    "INSERT INTO $S.logins VALUES ({id}, now(), {}); INSERT INTO $S.other VALUES ({id}, 'x');",
                    session + 1
                )
            }
            _ => return self.transaction(rng, db, index),
        };
        let session = &mut self.sessions[index];
        let sql = statement.replace("$S", &schema);
        if !session.open && rng.chance(50) {
            session.connection.execute("BEGIN").unwrap();
            session.open = true;
        }
        execute_or_name(&mut session.connection, &sql);
    }

    /// Savepoints and ends of a session's transaction.
    fn transaction(&mut self, rng: &mut Rng, db: &mut Db, index: usize) {
        let session = &mut self.sessions[index];
        if !session.open {
            session.connection.execute("BEGIN").unwrap();
            session.open = true;
            return;
        }
        let sql = match rng.below(10) {
            0 | 1 => {
                self.done.savepoint += 1;
                session.savepoints += 1;
                format!("SAVEPOINT p{}", session.savepoints)
            }
            2 if session.savepoints > 0 => {
                self.done.savepoint += 1;
                // The savepoint stays, its changes gone.
                format!("ROLLBACK TO p{}", session.savepoints)
            }
            3 if session.savepoints > 0 => {
                self.done.savepoint += 1;
                let sql = format!("RELEASE p{}", session.savepoints);
                session.savepoints -= 1;
                sql
            }
            4 => {
                self.done.abort += 1;
                session.open = false;
                session.savepoints = 0;
                "ROLLBACK".to_owned()
            }
            5 => {
                self.done.prepare += 1;
                self.gids += 1;
                let gid = format!("{}_{}", db.name, self.gids);
                session.open = false;
                session.savepoints = 0;
                session.prepared = Some(gid.clone());
                format!("PREPARE TRANSACTION '{gid}'")
            }
            _ => {
                self.done.commit += 1;
                session.open = false;
                session.savepoints = 0;
                "COMMIT".to_owned()
            }
        };
        execute_or_name(&mut session.connection, &sql);
    }

    /// What runs between transactions: DDL, once every session's
    /// transaction ends, `VACUUM` and checkpoints.
    fn between(&mut self, rng: &mut Rng, db: &mut Db) {
        match rng.below(3) {
            0 => {
                self.done.ddl += 1;
                self.finish(db);
                if self.added.len() < 3 && (self.added.is_empty() || rng.chance(60)) {
                    self.columns += 1;
                    let column = format!("added{}", self.columns);
                    let default = if rng.chance(50) { " DEFAULT 'd'" } else { "" };
                    db.run(&format!("ALTER TABLE $S.trips ADD {column} text{default}"));
                    self.added.push(column);
                } else {
                    let at = rng.index(self.added.len());
                    let column = self.added.remove(at);
                    db.run(&format!("ALTER TABLE $S.trips DROP COLUMN {column}"));
                }
            }
            1 => {
                self.done.checkpoint += 1;
                db.run("CHECKPOINT");
            }
            _ => {
                self.done.vacuum += 1;
                db.run("VACUUM $S.trips");
            }
        }
    }

    /// Ends every transaction, committing.
    fn finish(&mut self, db: &mut Db) {
        for session in &mut self.sessions {
            if session.open {
                session.connection.execute("COMMIT").unwrap();
                session.open = false;
                session.savepoints = 0;
            }
            if let Some(gid) = session.prepared.take() {
                db.run(&format!("COMMIT PREPARED '{gid}'"));
            }
        }
    }
}

/// Runs `sql`; panics naming it on an error.
fn execute_or_name(connection: &mut Connection, sql: &str) {
    if let Err(error) = connection.execute(sql) {
        panic!("{error}: {sql}");
    }
}

/// An update to the rows of `session` that `residue` picks, or to its
/// zone.
fn random_update(rng: &mut Rng, session: u64, residue: u64) -> String {
    match rng.below(4) {
        0 => format!(
            "UPDATE $S.trips SET fare = fare + 1 WHERE id % {SESSIONS} = {session} AND id % 7 = {residue};"
        ),
        1 => format!(
            "UPDATE $S.trips SET tip = coalesce(tip, 0) + 1, kind = left(coalesce(kind, '') || 'x', 30) WHERE id % {SESSIONS} = {session} AND id % 5 = {};",
            residue % 5
        ),
        2 => format!(
            "UPDATE $S.trips SET rider = NULL, zone_id = {} WHERE id % {SESSIONS} = {session} AND id % 9 = {residue};",
            session + 1
        ),
        _ => format!(
            "UPDATE $S.zones SET toll = toll + 0.5, borough = borough || '.' WHERE id = {};",
            session + 1
        ),
    }
}

/// The physical stream's path from records to commits, with a snapshot
/// that sees nothing, so every whole commit comes, and layouts read again
/// on a plain connection.
pub(super) struct Pipeline {
    pub(super) connection: Connection,
    pub(super) checked: Vec<Checked>,
    pub(super) inner: crate::wal::pipeline::Pipeline,
    /// How many times the layouts were read again.
    pub(super) layouts_read: usize,
}

impl Pipeline {
    /// The pipeline for `spec`'s tables in `publication`'s schema, read
    /// from the WAL of a server at `major`.
    pub(super) fn new(
        pg: &PostgresContainer,
        spec: &Spec,
        publication: &str,
        major: u32,
    ) -> Pipeline {
        let mut connection = Connection::connect(&pg.dsn, ConnectionKind::Sql).unwrap();
        let tables = catalog::read(&mut connection, spec, publication, PHYSICAL).unwrap();
        let checked: Vec<Checked> = tables.facts.into_iter().chain(tables.dims).collect();
        let layouts = physical::read_layouts(&mut connection, &checked).unwrap();
        let mut inner = crate::wal::pipeline::Pipeline::new(
            checked.clone(),
            layouts,
            major,
            ROW_LIMIT,
            PAGE_MEMORY,
        );
        let nothing = TransactionSnapshot::parse("1:1:").unwrap();
        assert_eq!(inner.see(nothing, Lsn(0)).unwrap().1, []);
        Pipeline {
            connection,
            checked,
            inner,
            layouts_read: 0,
        }
    }

    /// The commits `record` completes.
    pub(super) fn record(&mut self, record: &crate::wal::Record) -> Vec<Commit> {
        let connection = &mut self.connection;
        let checked = &self.checked;
        let layouts_read = &mut self.layouts_read;
        (self.inner)
            .record(record, |_| {
                *layouts_read += 1;
                physical::read_layouts(connection, checked)
            })
            .unwrap()
    }

    /// The commits of all WAL written so far.
    pub(super) fn through_now(&mut self, db: &mut Db, stream: &mut Physical) -> Vec<Commit> {
        let last = db.insert_lsn();
        db.run("SELECT pg_switch_wal()");
        let mut commits = Vec::new();
        let deadline = Instant::now() + LONG;
        loop {
            assert!(Instant::now() < deadline, "no switch record past {last}");
            let Some(record) = stream.next_record().unwrap() else {
                continue;
            };
            commits.extend(self.record(&record));
            if record.is_switch() && record.lsn >= last.0 {
                assert_eq!(
                    self.inner.consistency(),
                    crate::wal::transactions::Consistency::Consistent
                );
                return commits;
            }
        }
    }
}
