//! How transactions end, and which run, as the WAL says it. Layouts:
//! `src/include/access/xact.h` and `standbydefs.h`, read as
//! `ParseCommitRecord`, `ParseAbortRecord` in
//! `src/backend/access/rmgrdesc/xactdesc.c` and `standby_desc` do.

use super::{Record, WalError};

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

const DATABASE_INFO: usize = 8;
const RELATION_FILE: usize = 12;
/// `SharedInvalidationMessage`: an `int8` kind, then the kind's fields;
/// a relcache one names its database and table (`sinval.h`).
const INVALIDATION: usize = 16;
const INVALIDATE_RELCACHE: u8 = 0xFE;
/// `xl_xact_stats_item`: kind, database and object; the object is 64
/// bits from 18.
const DROPPED_STAT_16: usize = 12;
const DROPPED_STAT_18: usize = 16;
/// `xl_running_xacts` up to its xids.
const RUNNING_HEADER: usize = 24;

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
    let short = WalError::Heap {
        lsn: record.lsn,
        problem: "a transaction record shorter than its kind",
    };
    match record.resource_manager {
        RM_XACT => {
            let operation = record.info & XACT_OPERATION;
            let commit = match operation {
                XACT_COMMIT | XACT_COMMIT_PREPARED => true,
                XACT_ABORT | XACT_ABORT_PREPARED => false,
                _ => return Ok(None),
            };
            let has_info = record.info & XACT_HAS_INFO != 0;
            let ending = ending(record, has_info, commit, major >= 18).ok_or(short)?;
            Ok(Some(if commit {
                Event::Commit(ending)
            } else {
                Event::Abort(ending)
            }))
        }
        RM_STANDBY if record.info & 0xF0 == STANDBY_RUNNING_XACTS => running(record.main_data())
            .map(|r| Some(Event::Running(r)))
            .ok_or(short),
        _ => Ok(None),
    }
}

fn ending(record: &Record, has_info: bool, commit: bool, wide_stats: bool) -> Option<Ending> {
    let data = record.main_data();
    let time = i64::from_le_bytes(data.get(..8)?.try_into().ok()?);
    let mut at = 8;
    let info = if has_info {
        at += 4;
        word(data, 8)?
    } else {
        0
    };
    let count = |at: &mut usize| -> Option<usize> {
        let n = usize::try_from(word(data, *at)?).ok()?;
        *at += 4;
        Some(n)
    };
    if info & HAS_DATABASE != 0 {
        at += DATABASE_INFO;
    }
    let mut subxacts = Vec::new();
    if info & HAS_SUBXACTS != 0 {
        let n = count(&mut at)?;
        for i in 0..n {
            subxacts.push(word(data, at + 4 * i)?);
        }
        at += 4 * n;
    }
    if info & HAS_RELATION_FILES != 0 {
        at += RELATION_FILE * count(&mut at)?;
    }
    if info & HAS_DROPPED_STATS != 0 {
        let item = if wide_stats {
            DROPPED_STAT_18
        } else {
            DROPPED_STAT_16
        };
        at += item * count(&mut at)?;
    }
    let mut invalidated = Vec::new();
    if commit && info & HAS_INVALIDATIONS != 0 {
        let n = count(&mut at)?;
        for i in 0..n {
            let message = at + INVALIDATION * i;
            if *data.get(message)? == INVALIDATE_RELCACHE {
                invalidated.push(Invalidated {
                    database: word(data, message + 4)?,
                    relation: word(data, message + 8)?,
                });
            }
        }
        at += INVALIDATION * n;
    }
    let xid = if info & HAS_TWO_PHASE != 0 {
        word(data, at)?
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

fn running(data: &[u8]) -> Option<Running> {
    let count = usize::try_from(word(data, 0)?).ok()?;
    let next_xid = word(data, 12)?;
    let oldest_xid = word(data, 16)?;
    let xids = (0..count)
        .map(|i| word(data, RUNNING_HEADER + 4 * i))
        .collect::<Option<_>>()?;
    Some(Running {
        next_xid,
        oldest_xid,
        xids,
    })
}

fn word(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
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
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// A commit or abort with every part `ParseCommitRecord` reads before
    /// the two-phase xid, and an origin after it.
    fn ending_main(info: u32, commit: bool, stat: usize) -> Vec<u8> {
        let mut main = 12_345i64.to_le_bytes().to_vec();
        main.extend(info.to_le_bytes());
        main.extend(words(&[5, 1663]));
        main.extend(words(&[2, 801, 802]));
        main.extend(words(&[1]));
        main.extend([7; RELATION_FILE]);
        main.extend(words(&[2]));
        main.extend(vec![8; 2 * stat]);
        if commit {
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
        for (major, stat) in [
            (16, DROPPED_STAT_16),
            (17, DROPPED_STAT_16),
            (18, DROPPED_STAT_18),
        ] {
            for (operation, commit) in [
                (XACT_COMMIT, true),
                (XACT_COMMIT_PREPARED, true),
                (XACT_ABORT, false),
                (XACT_ABORT_PREPARED, false),
            ] {
                let main = ending_main(ALL, commit, stat);
                let got = event(&record(RM_XACT, operation | XACT_HAS_INFO, 0, main), major);
                let invalidated = if commit {
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
                let expected = if commit {
                    Event::Commit(ending)
                } else {
                    Event::Abort(ending)
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
        let full = ending_main(ALL, true, DROPPED_STAT_18);
        // The two-phase xid is the last part read.
        let read = full.len() - 4 - 16;
        for length in 0..read {
            let main = full[..length].to_vec();
            let got = event(&record(RM_XACT, XACT_COMMIT | XACT_HAS_INFO, 1, main), 18);
            assert!(
                matches!(got, Err(WalError::Heap { problem, .. })
                    if problem == "a transaction record shorter than its kind"),
                "{length}: {got:?}"
            );
        }
        let mut running = words(&[3, 0, 0, 760]);
        running.extend(words(&[0, 0, 751, 752]));
        let got = event(&record(RM_STANDBY, STANDBY_RUNNING_XACTS, 0, running), 18);
        assert!(matches!(got, Err(WalError::Heap { .. })), "{got:?}");
        let got = event(
            &record(RM_STANDBY, STANDBY_RUNNING_XACTS, 0, words(&[0, 0])),
            18,
        );
        assert!(matches!(got, Err(WalError::Heap { .. })), "{got:?}");
    }
}
