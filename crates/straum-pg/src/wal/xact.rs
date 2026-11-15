//! How transactions end, and which run, as the WAL says it. Layouts:
//! `src/include/access/xact.h` and `standbydefs.h`, read as
//! `ParseCommitRecord`, `ParseAbortRecord` in
//! `src/backend/access/rmgrdesc/xactdesc.c` and `standby_desc` do.

use super::{RESOURCE_MANAGER_INFO, Record, WalError, u32_at};

const RM_XACT: u8 = 1;
const RM_STANDBY: u8 = 8;

const XACT_OPERATION: u8 = 0x70;
const XACT_HAS_INFO: u8 = 0x80;
const XACT_COMMIT: u8 = 0x00;
const XACT_ABORT: u8 = 0x20;
const XACT_COMMIT_PREPARED: u8 = 0x30;
const XACT_ABORT_PREPARED: u8 = 0x40;
const STANDBY_RUNNING_XACTS: u8 = 0x10;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    Commit(Ending),
    Abort(Ending),
    Running(Running),
}

/// A transaction that ends, with the subtransactions that end with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ending {
    pub(crate) xid: u32,
    pub(crate) subxacts: Vec<u32>,
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
    pub(crate) database: u32,
    pub(crate) relation: u32,
}

/// The transactions running when the record was written, logged at each
/// checkpoint and every 15 seconds the WAL moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Running {
    /// The xid the next transaction gets: none at or past it ran yet.
    pub(crate) next_xid: u32,
    /// Every xid before it has ended, even one a crash cut off, which
    /// leaves no abort record.
    pub(crate) oldest_xid: u32,
    /// The top-level transactions only.
    pub(crate) xids: Vec<u32>,
}

/// The event `record` is, if any. `major` picks the layout.
pub(crate) fn event(record: &Record, major: u32) -> Result<Option<Event>, WalError> {
    let short = WalError::At {
        lsn: record.lsn,
        problem: "a transaction record shorter than its kind",
    };
    match record.resource_manager {
        RM_XACT => {
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
        RM_STANDBY if record.info & RESOURCE_MANAGER_INFO == STANDBY_RUNNING_XACTS => {
            let Some(running) = running(record.main_data()) else {
                return Err(short);
            };
            Ok(Some(Event::Running(running)))
        }
        _ => Ok(None),
    }
}

fn ending(record: &Record, outcome: Outcome, major: u32) -> Option<Ending> {
    let data = record.main_data();
    let time = i64::from_le_bytes(*data.first_chunk()?);
    let mut at = TIME_SIZE;
    let mut info = 0;
    if record.info & XACT_HAS_INFO != 0 {
        info = u32_at(data, at)?;
        at += XINFO_SIZE;
    }
    if info & HAS_DATABASE != 0 {
        at += DATABASE_INFO;
    }
    let mut subxacts = Vec::new();
    if info & HAS_SUBXACTS != 0 {
        let count = read_count(data, &mut at)?;
        for index in 0..count {
            subxacts.push(u32_at(data, at + XID_SIZE * index)?);
        }
        at += XID_SIZE * count;
    }
    if info & HAS_RELATION_FILES != 0 {
        at += RELATION_FILE * read_count(data, &mut at)?;
    }
    if info & HAS_DROPPED_STATS != 0 {
        at += dropped_stat_size(major) * read_count(data, &mut at)?;
    }
    let mut invalidated = Vec::new();
    if outcome == Outcome::Commit && info & HAS_INVALIDATIONS != 0 {
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
    let xid = if info & HAS_TWO_PHASE != 0 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::decode;
    use crate::wal::tests::{Spec, assemble};

    fn record(resource_manager: u8, info: u8, xid: u32, main: Vec<u8>) -> Record {
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

    fn words(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    /// A commit or abort with every part `ParseCommitRecord` reads before
    /// the two-phase xid, and an origin after it.
    fn ending_main(info: u32, outcome: Outcome, stat: usize) -> Vec<u8> {
        let mut main = 12_345i64.to_le_bytes().to_vec();
        main.extend(info.to_le_bytes());
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
                main.extend([INVALIDATE_RELCACHE, 0, 0, 0]);
                main.extend(words(&[5, relation, 0]));
            }
        }
        main.extend(words(&[900]));
        main.extend(b"gid\0");
        main.extend([0; 16]);
        main
    }

    const ALL: u32 = HAS_DATABASE
        | HAS_SUBXACTS
        | HAS_RELATION_FILES
        | HAS_INVALIDATIONS
        | HAS_TWO_PHASE
        | HAS_DROPPED_STATS
        | 1 << 5
        | 1 << 7;

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
                let got = event(&record(RM_XACT, operation | XACT_HAS_INFO, 0, main), major);
                let invalidated = if outcome == Outcome::Commit {
                    [16_384, 0]
                        .map(|relation| Invalidated {
                            database: 5,
                            relation,
                        })
                        .to_vec()
                } else {
                    Vec::new()
                };
                let ending = Ending {
                    xid: 900,
                    subxacts: vec![801, 802],
                    time: 12_345,
                    invalidated,
                };
                let expected = match outcome {
                    Outcome::Commit => Event::Commit(ending),
                    Outcome::Abort => Event::Abort(ending),
                };
                assert_eq!(got, Ok(Some(expected)), "{major} {operation:#x}");
            }
        }
        // No xinfo: the record's own xid, nothing else.
        let got = event(
            &record(RM_XACT, XACT_COMMIT, 77, (-5i64).to_le_bytes().to_vec()),
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
        let mut main = words(&[2, 1]);
        main.extend([0; 4]);
        main.extend(words(&[760, 750, 749, 751, 755, 752]));
        let got = event(&record(RM_STANDBY, STANDBY_RUNNING_XACTS, 0, main), 18);
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
            (RM_XACT, 0x10),
            (RM_XACT, 0x50),
            (RM_XACT, 0x60),
            (RM_STANDBY, 0x00),
            (RM_STANDBY, 0x20),
            (0, 0x00),
            (10, 0x00),
        ] {
            let got = event(&record(resource_manager, info, 1, vec![0; 64]), 18);
            assert_eq!(got, Ok(None), "{resource_manager} {info:#x}");
        }
    }

    #[test]
    fn rejects_records_cut_short() {
        let full = ending_main(ALL, Outcome::Commit, DROPPED_STAT_18);
        // The two-phase xid is the last part read.
        let read = full.len() - 4 - 16;
        for length in 0..read {
            let main = full[..length].to_vec();
            let got = event(&record(RM_XACT, XACT_COMMIT | XACT_HAS_INFO, 1, main), 18);
            assert!(
                matches!(got, Err(WalError::At { problem, .. })
                    if problem == "a transaction record shorter than its kind"),
                "{length}: {got:?}"
            );
        }
        let mut running = words(&[3, 0, 0, 760]);
        running.extend(words(&[0, 0, 751, 752]));
        let got = event(&record(RM_STANDBY, STANDBY_RUNNING_XACTS, 0, running), 18);
        assert!(matches!(got, Err(WalError::At { .. })), "{got:?}");
        let got = event(
            &record(RM_STANDBY, STANDBY_RUNNING_XACTS, 0, words(&[0, 0])),
            18,
        );
        assert!(matches!(got, Err(WalError::At { .. })), "{got:?}");
    }
}
