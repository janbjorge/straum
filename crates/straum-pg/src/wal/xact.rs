//! How transactions end, and which run, as the WAL says it: the commit
//! and abort records of the transaction resource manager, and the
//! standby's `RUNNING_XACTS`, which lists the transactions running when it
//! was written. A record goes in, an `Event` comes out for
//! `transactions.rs`. A commit also names the tables whose relcache, the
//! catalog cache of a table's layout, it invalidates: what DDL leaves.
//! Layouts: `src/include/access/xact.h` and `standbydefs.h`, read as
//! `ParseCommitRecord` and `ParseAbortRecord` in `xactdesc.c` do.

use super::{
    Oid, Problem, RESOURCE_MANAGER_INFO, RESOURCE_MANAGER_STANDBY, RESOURCE_MANAGER_TRANSACTION,
    Record, TransactionId, WalError, u32_at,
};

const XACT_OPERATION: u8 = 0x70;
/// `XLOG_XACT_HAS_INFO`: an `xl_xact_xinfo` says which parts follow.
pub(super) const XACT_HAS_INFO: u8 = 0x80;
/// `XLOG_XACT_COMMIT`.
pub(super) const XACT_COMMIT: u8 = 0x00;
const XACT_ABORT: u8 = 0x20;
const XACT_COMMIT_PREPARED: u8 = 0x30;
const XACT_ABORT_PREPARED: u8 = 0x40;
/// `XLOG_RUNNING_XACTS`, a record of the standby resource manager.
pub(super) const STANDBY_RUNNING_XACTS: u8 = 0x10;

const HAS_DATABASE: u32 = 1 << 0;
const HAS_SUBXACTS: u32 = 1 << 1;
const HAS_RELATION_FILES: u32 = 1 << 2;
const HAS_INVALIDATIONS: u32 = 1 << 3;
const HAS_TWO_PHASE: u32 = 1 << 4;
const HAS_DROPPED_STATS: u32 = 1 << 8;

/// `xl_xact_commit` and `xl_xact_abort` open with `xact_time`, a
/// `TimestampTz`.
const TIME_SIZE: usize = 8;
/// `xl_xact_xinfo`, after the time when the info says it is there.
const XINFO_SIZE: usize = 4;
/// The count before each list: `nsubxacts`, `nrels`, `nitems`, `nmsgs`.
const COUNT_SIZE: usize = 4;
const XID_SIZE: usize = 4;
/// `xl_xact_dbinfo`: database and tablespace.
const DATABASE_INFO: usize = 8;
/// `RelFileLocator`: tablespace, database and relation.
const RELATION_FILE: usize = 12;
/// `SharedInvalidationMessage`: an `int8` kind, then the kind's fields;
/// a relcache one names its database and table (`sinval.h`).
const INVALIDATION: usize = 16;
const INVALIDATE_RELCACHE: u8 = 0xFE;
/// `SharedInvalRelcacheMsg`: `dbId`, then `relId`, after the kind.
const RELCACHE_DATABASE: usize = 4;
const RELCACHE_RELATION: usize = 8;
/// `xl_xact_stats_item`: kind, database and object; the object is 64
/// bits from 18.
const DROPPED_STAT_17: usize = 12;
const DROPPED_STAT_18: usize = 16;
/// `xl_running_xacts`: `xcnt`, `subxcnt`, `subxid_overflow`, `nextXid`,
/// `oldestRunningXid` and `latestCompletedXid`, then the xids.
const RUNNING_COUNT: usize = 0;
const RUNNING_NEXT_XID: usize = 12;
const RUNNING_OLDEST_XID: usize = 16;
const RUNNING_HEADER: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Commit,
    Abort,
}

/// What a transaction or standby record says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    Commit(Ending),
    Abort(Ending),
    Running(Running),
}

/// A transaction that ends, with the subtransactions that end with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ending {
    pub(crate) xid: TransactionId,
    pub(crate) subxacts: Vec<TransactionId>,
    /// Microseconds since 2000-01-01 UTC.
    pub(crate) time: i64,
    /// Tables whose relcache entry the commit invalidates: what DDL, and
    /// some maintenance, on a table leaves.
    pub(crate) invalidated: Vec<Invalidated>,
}

/// A relcache invalidation: one table, or with `relation` 0 every table
/// of the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Invalidated {
    pub(crate) database: Oid,
    pub(crate) relation: Oid,
}

/// The transactions running when the record was written, logged at each
/// checkpoint and every 15 seconds the WAL moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Running {
    /// The xid the next transaction gets: none at or past it ran yet.
    pub(crate) next_xid: TransactionId,
    /// Every xid before it has ended, even one a crash cut off, which
    /// leaves no abort record.
    pub(crate) oldest_xid: TransactionId,
    /// The top-level transactions only.
    pub(crate) xids: Vec<TransactionId>,
}

/// The event `record` is, if any. `major` picks the layout.
pub(crate) fn event(record: &Record, major: u32) -> Result<Option<Event>, WalError> {
    let short = WalError::At {
        lsn: record.lsn,
        problem: Problem::ShortTransactionRecord,
    };
    match record.resource_manager {
        RESOURCE_MANAGER_TRANSACTION => {
            let outcome = match record.info & XACT_OPERATION {
                XACT_COMMIT | XACT_COMMIT_PREPARED => Outcome::Commit,
                XACT_ABORT | XACT_ABORT_PREPARED => Outcome::Abort,
                _ => return Ok(None),
            };
            let ending = ending(record, outcome, major).ok_or(short)?;
            Ok(Some(match outcome {
                Outcome::Commit => Event::Commit(ending),
                Outcome::Abort => Event::Abort(ending),
            }))
        }
        RESOURCE_MANAGER_STANDBY
            if record.info & RESOURCE_MANAGER_INFO == STANDBY_RUNNING_XACTS =>
        {
            let Some(running) = running(record.main_data()) else {
                return Err(short);
            };
            Ok(Some(Event::Running(running)))
        }
        _ => Ok(None),
    }
}

/// The transaction a commit or abort record ends, with its parts.
fn ending(record: &Record, outcome: Outcome, major: u32) -> Option<Ending> {
    let data = record.main_data();
    let time = i64::from_le_bytes(*data.first_chunk()?);
    let mut at = TIME_SIZE;
    // `xl_xact_xinfo`: which optional parts follow, a flag each.
    let mut parts = 0;
    if record.info & XACT_HAS_INFO != 0 {
        parts = u32_at(data, at)?;
        at += XINFO_SIZE;
    }
    if parts & HAS_DATABASE != 0 {
        at += DATABASE_INFO;
    }
    let mut subxacts = Vec::new();
    if parts & HAS_SUBXACTS != 0 {
        let count = read_count(data, &mut at)?;
        for index in 0..count {
            subxacts.push(u32_at(data, at + XID_SIZE * index)?);
        }
        at += XID_SIZE * count;
    }
    if parts & HAS_RELATION_FILES != 0 {
        at += RELATION_FILE * read_count(data, &mut at)?;
    }
    if parts & HAS_DROPPED_STATS != 0 {
        at += dropped_stat_size(major) * read_count(data, &mut at)?;
    }
    let mut invalidated = Vec::new();
    if outcome == Outcome::Commit && parts & HAS_INVALIDATIONS != 0 {
        let count = read_count(data, &mut at)?;
        for index in 0..count {
            let message = at + INVALIDATION * index;
            if *data.get(message)? == INVALIDATE_RELCACHE {
                invalidated.push(Invalidated {
                    database: u32_at(data, message + RELCACHE_DATABASE)?,
                    relation: u32_at(data, message + RELCACHE_RELATION)?,
                });
            }
        }
        at += INVALIDATION * count;
    }
    let xid = if parts & HAS_TWO_PHASE != 0 {
        u32_at(data, at)?
    } else {
        record.xid
    };
    Some(Ending {
        xid,
        subxacts,
        time,
        invalidated,
    })
}

/// The count at `at`, moving `at` past it.
fn read_count(data: &[u8], at: &mut usize) -> Option<usize> {
    let count = usize::try_from(u32_at(data, *at)?).ok()?;
    *at += COUNT_SIZE;
    Some(count)
}

fn dropped_stat_size(major: u32) -> usize {
    if major >= 18 {
        DROPPED_STAT_18
    } else {
        DROPPED_STAT_17
    }
}

/// The `xl_running_xacts` in `data`.
fn running(data: &[u8]) -> Option<Running> {
    let count = usize::try_from(u32_at(data, RUNNING_COUNT)?).ok()?;
    let next_xid = u32_at(data, RUNNING_NEXT_XID)?;
    let oldest_xid = u32_at(data, RUNNING_OLDEST_XID)?;
    let mut xids = Vec::new();
    for index in 0..count {
        xids.push(u32_at(data, RUNNING_HEADER + XID_SIZE * index)?);
    }
    Some(Running {
        next_xid,
        oldest_xid,
        xids,
    })
}

/// Test builders `pipeline.rs`'s tests share.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::wal::test_support::{Spec, assemble, decode};
    use crate::wal::{RESOURCE_MANAGER_HEAP, RESOURCE_MANAGER_XLOG};

    /// `XACT_XINFO_HAS_ORIGIN`: an `xl_xact_origin` comes last.
    const HAS_ORIGIN: u32 = 1 << 5;
    /// `XACT_XINFO_HAS_GID`: a prepared transaction's name follows its xid.
    const HAS_GID: u32 = 1 << 7;
    /// `xl_xact_origin`: `origin_lsn` and `origin_timestamp`, 8 bytes each.
    const ORIGIN_SIZE: usize = 16;
    /// A prepared transaction's name, `twophase_gid`, with its zero byte.
    const GID: &[u8] = b"gid\0";
    /// `XLOG_XACT_PREPARE`, `XLOG_XACT_ASSIGNMENT`, `XLOG_XACT_INVALIDATIONS`:
    /// transaction records that end nothing.
    const XACT_PREPARE: u8 = 0x10;
    const XACT_ASSIGNMENT: u8 = 0x50;
    const XACT_INVALIDATIONS: u8 = 0x60;
    /// `XLOG_STANDBY_LOCK` and `XLOG_INVALIDATIONS`: standby records other
    /// than `RUNNING_XACTS`.
    const STANDBY_LOCK: u8 = 0x00;
    const STANDBY_INVALIDATIONS: u8 = 0x20;
    /// `xl_running_xacts.latestCompletedXid`.
    const RUNNING_LATEST_COMPLETED_XID: usize = 20;
    /// `xl_running_xacts.subxcnt`.
    const RUNNING_SUBXACT_COUNT: usize = 4;

    fn record(resource_manager: u8, info: u8, xid: TransactionId, main: Vec<u8>) -> Record {
        let spec = Spec {
            xid,
            resource_manager,
            info,
            toplevel_xid: None,
            origin: false,
            blocks: Vec::new(),
            main,
        };
        let bytes = assemble(&spec);
        decode(0x100_0028, 0x100_0028 + bytes.len() as u64, bytes).unwrap()
    }

    /// `values` as `uint32`s, one after another.
    fn words(values: &[u32]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in values {
            bytes.extend(value.to_le_bytes());
        }
        bytes
    }

    /// A relcache `SharedInvalidationMessage` for `relation` of `database`.
    pub(crate) fn relcache_invalidation(database: u32, relation: u32) -> [u8; INVALIDATION] {
        let mut message = [0; INVALIDATION];
        message[0] = INVALIDATE_RELCACHE;
        message[RELCACHE_DATABASE..RELCACHE_DATABASE + XID_SIZE]
            .copy_from_slice(&database.to_le_bytes());
        message[RELCACHE_RELATION..RELCACHE_RELATION + XID_SIZE]
            .copy_from_slice(&relation.to_le_bytes());
        message
    }

    /// The main data of a commit at `time` whose only part is one relcache
    /// invalidation of `relation` in `database`.
    pub(crate) fn invalidating_commit(time: i64, database: u32, relation: u32) -> Vec<u8> {
        let mut main = time.to_le_bytes().to_vec();
        main.extend(HAS_INVALIDATIONS.to_le_bytes());
        main.extend(1u32.to_le_bytes());
        main.extend(relcache_invalidation(database, relation));
        main
    }

    /// An `xl_running_xacts` naming `xids`, with no subtransactions.
    pub(crate) fn running_main(count: u32, next: u32, oldest: u32, xids: &[u32]) -> Vec<u8> {
        let mut main = vec![0; RUNNING_HEADER];
        let latest = next.wrapping_sub(1);
        for (at, value) in [
            (RUNNING_COUNT, count),
            (RUNNING_NEXT_XID, next),
            (RUNNING_OLDEST_XID, oldest),
            (RUNNING_LATEST_COMPLETED_XID, latest),
        ] {
            main[at..at + XID_SIZE].copy_from_slice(&value.to_le_bytes());
        }
        main.extend(words(xids));
        main
    }

    /// A commit or abort with every part `ParseCommitRecord` reads before
    /// the two-phase xid, and a gid and an origin after it.
    fn ending_main(parts: u32, outcome: Outcome, stat: usize) -> Vec<u8> {
        let mut main = 12_345i64.to_le_bytes().to_vec();
        main.extend(parts.to_le_bytes());
        main.extend(words(&[5, 1663]));
        main.extend(words(&[2, 801, 802]));
        main.extend(words(&[1]));
        main.extend([7; RELATION_FILE]);
        main.extend(words(&[2]));
        main.extend(vec![8; 2 * stat]);
        if outcome == Outcome::Commit {
            main.extend(words(&[3]));
            // A catcache message, then the relcache of one table and of
            // all of database 5.
            main.extend([9; INVALIDATION]);
            for relation in [16_384, 0] {
                main.extend(relcache_invalidation(5, relation));
            }
        }
        main.extend(words(&[900]));
        main.extend(GID);
        main.extend([0; ORIGIN_SIZE]);
        main
    }

    const ALL: u32 = HAS_DATABASE
        | HAS_SUBXACTS
        | HAS_RELATION_FILES
        | HAS_INVALIDATIONS
        | HAS_TWO_PHASE
        | HAS_DROPPED_STATS
        | HAS_ORIGIN
        | HAS_GID;

    /// What `ending_main` says, as an event.
    fn expected_ending(outcome: Outcome) -> Event {
        let mut invalidated = Vec::new();
        if outcome == Outcome::Commit {
            for relation in [16_384, 0] {
                invalidated.push(Invalidated {
                    database: 5,
                    relation,
                });
            }
        }
        let ending = Ending {
            xid: 900,
            subxacts: vec![801, 802],
            time: 12_345,
            invalidated,
        };
        match outcome {
            Outcome::Commit => Event::Commit(ending),
            Outcome::Abort => Event::Abort(ending),
        }
    }

    #[test]
    fn reads_endings() {
        for (major, stat) in [(17, DROPPED_STAT_17), (18, DROPPED_STAT_18)] {
            for (operation, outcome) in [
                (XACT_COMMIT, Outcome::Commit),
                (XACT_COMMIT_PREPARED, Outcome::Commit),
                (XACT_ABORT, Outcome::Abort),
                (XACT_ABORT_PREPARED, Outcome::Abort),
            ] {
                let main = ending_main(ALL, outcome, stat);
                let info = operation | XACT_HAS_INFO;
                let got = event(&record(RESOURCE_MANAGER_TRANSACTION, info, 0, main), major);
                let expected = expected_ending(outcome);
                assert_eq!(got, Ok(Some(expected)), "{major} {operation:#x}");
            }
        }
        // No xinfo: the record's own xid, nothing else.
        let main = (-5i64).to_le_bytes().to_vec();
        let got = event(
            &record(RESOURCE_MANAGER_TRANSACTION, XACT_COMMIT, 77, main),
            18,
        );
        let ending = Ending {
            xid: 77,
            subxacts: Vec::new(),
            time: -5,
            invalidated: Vec::new(),
        };
        assert_eq!(got, Ok(Some(Event::Commit(ending))));
    }

    #[test]
    fn reads_running_transactions() {
        // Two running, then one subtransaction, which Straum passes over.
        let mut main = running_main(2, 760, 750, &[751, 755, 752]);
        main[RUNNING_SUBXACT_COUNT..RUNNING_SUBXACT_COUNT + XID_SIZE]
            .copy_from_slice(&1u32.to_le_bytes());
        let got = event(
            &record(RESOURCE_MANAGER_STANDBY, STANDBY_RUNNING_XACTS, 0, main),
            18,
        );
        let running = Running {
            next_xid: 760,
            oldest_xid: 750,
            xids: vec![751, 755],
        };
        assert_eq!(got, Ok(Some(Event::Running(running))));
    }

    #[test]
    fn passes_over_other_records() {
        for (resource_manager, info) in [
            (RESOURCE_MANAGER_TRANSACTION, XACT_PREPARE),
            (RESOURCE_MANAGER_TRANSACTION, XACT_ASSIGNMENT),
            (RESOURCE_MANAGER_TRANSACTION, XACT_INVALIDATIONS),
            (RESOURCE_MANAGER_STANDBY, STANDBY_LOCK),
            (RESOURCE_MANAGER_STANDBY, STANDBY_INVALIDATIONS),
            (RESOURCE_MANAGER_XLOG, 0x00),
            (RESOURCE_MANAGER_HEAP, 0x00),
        ] {
            let got = event(&record(resource_manager, info, 1, vec![0; 64]), 18);
            assert_eq!(got, Ok(None), "{resource_manager} {info:#x}");
        }
    }

    #[test]
    fn rejects_records_cut_short() {
        let full = ending_main(ALL, Outcome::Commit, DROPPED_STAT_18);
        // The two-phase xid is the last part read.
        let read = full.len() - GID.len() - ORIGIN_SIZE;
        for length in 0..read {
            let main = full[..length].to_vec();
            let info = XACT_COMMIT | XACT_HAS_INFO;
            let got = event(&record(RESOURCE_MANAGER_TRANSACTION, info, 1, main), 18);
            assert_eq!(
                got,
                Err(WalError::At {
                    lsn: 0x100_0028,
                    problem: Problem::ShortTransactionRecord
                }),
                "{length}"
            );
        }
        // Three running named, two there; then a header cut short.
        for main in [running_main(3, 760, 0, &[751, 752]), words(&[0, 0])] {
            let got = event(
                &record(RESOURCE_MANAGER_STANDBY, STANDBY_RUNNING_XACTS, 0, main),
                18,
            );
            assert!(matches!(got, Err(WalError::At { .. })), "{got:?}");
        }
    }
}
