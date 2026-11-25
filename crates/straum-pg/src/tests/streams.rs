//! What the tests of a stream share: the physical stream's records up to
//! a switch to the next WAL file, `Pipeline`, its path from records to
//! commits, and a writer applying commits until it has caught up with
//! the database.

use straum_app::{Commit, Lsn, Writer};
use straum_domain::Spec;

use super::references::*;
use super::support::*;
use crate::catalog::{self, Checked};
use crate::physical::{self, Physical};
use crate::source::Postgres;
use crate::wal::Record;
use crate::wal::transactions::TransactionSnapshot;
use crate::wire::{Connection, ConnectionKind};

/// The records a physical stream read up to a switch to the next WAL
/// file, and where WAL stood just before the switch.
pub(super) struct ReadToSwitch {
    pub(super) last: Lsn,
    pub(super) records: Vec<Record>,
}

/// Every record of the WAL written so far: switches to the next WAL file,
/// then reads up to the switch record at or past where WAL stood, that
/// record too.
pub(super) fn records_to_switch(db: &mut Db, physical: &mut Physical) -> ReadToSwitch {
    let last = db.insert_lsn();
    db.run("SELECT pg_switch_wal()");
    let wait = Wait::new(&format!("a switch record past {last}"));
    let mut records = Vec::new();
    loop {
        wait.check();
        let Some(record) = physical.next_record().unwrap() else {
            continue;
        };
        let switched = record.is_switch() && record.lsn >= last.0;
        records.push(record);
        if switched {
            return ReadToSwitch { last, records };
        }
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
            STREAM_MEMORY,
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
    pub(super) fn record(&mut self, record: &Record) -> Vec<Commit> {
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
        let mut commits = Vec::new();
        for record in &records_to_switch(db, stream).records {
            commits.extend(self.record(record));
        }
        assert_eq!(
            self.inner.consistency(),
            crate::wal::transactions::Consistency::Consistent
        );
        commits
    }
}

/// Applies commits until Straum is ready and its views equal SQL's. A
/// trip goes in each round, so there is a commit to wait for.
pub(super) fn catch_up(db: &mut Db, writer: &mut Writer<Postgres>, rng: &mut Rng, next: &mut u64) {
    let wait = Wait::new("Straum to catch up");
    loop {
        wait.check();
        *next += 1;
        let written = db.run_committed(&rng.trip(*next));
        apply_through(writer, written, &wait.then(&format!("{written} applied")));
        if compare(db, writer.app(), &taxi_oracles(2)).is_some() {
            return;
        }
    }
}

/// Applies commits until `writer` has applied every one up to `written`,
/// failing when `wait` ends first.
pub(super) fn apply_through(writer: &mut Writer<Postgres>, written: Lsn, wait: &Wait) {
    while !applied_through(writer, written) {
        wait.check();
        applied(writer);
    }
}

/// Whether `writer` has applied every commit up to `written`.
fn applied_through(writer: &Writer<Postgres>, written: Lsn) -> bool {
    match writer.app().status().applied {
        Some(applied) => applied >= written,
        None => false,
    }
}
