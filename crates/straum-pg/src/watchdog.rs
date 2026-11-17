//! The watchdog: two threads that keep a stream's slot from holding more
//! WAL than `--max-retained-wal`. The reader, on a plain connection of its
//! own, reads what the slot holds from `pg_replication_slots` each round.
//! Past the limit it shuts the sockets of the stream's sessions, the
//! load's among them; a walsender sees its socket close and ends, which
//! drops the temporary slot. Each round the slot is still there after
//! that, the reader has PostgreSQL end the session that owns it. Not
//! before: a walsender told to end while its client reads nothing blocks
//! sending the client why, and stays. The clock touches no network: when
//! the reader has gone too long without a reading, whatever it is stuck
//! on, the clock shuts the same sockets, since nothing else bounds the
//! WAL the slot holds. Neither needs anything from the stream's own
//! threads: a writer stuck under its lock, or a load that keeps the
//! slot's connection busy, still leaves a watched slot.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use postgres_protocol::escape::escape_literal;

use crate::dsn::Dsn;
use crate::error::Error;
use straum_app::Lsn;

use crate::wire::{Closer, Connection, ConnectionKind, first_row_value};

/// How often the reader reads what the slot holds.
const ROUND: Duration = Duration::from_secs(1);
/// How soon the reader tries again after a failed reading, so one lost
/// connection costs little of the time the clock allows.
const RETRY: Duration = Duration::from_millis(200);
/// How long either thread sleeps at a time, so both stop soon when told
/// and the clock notices a silent reader soon.
const NAP: Duration = Duration::from_millis(50);
/// How long a query may take before the reader drops its connection.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the reader may go without reading the slot before the clock
/// ends the stream.
pub(crate) const UNWATCHED: Duration = Duration::from_secs(10);

/// How far the watchdog lets a slot go.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Watching {
    /// The most WAL the slot may hold, in bytes: `--max-retained-wal`.
    pub(crate) max_retained_wal: u64,
    /// How long the slot may go unread before the stream ends.
    pub(crate) unwatched: Duration,
}

/// Why the watchdog ended a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The slot held `retained` bytes of WAL, past `limit`.
    Retained { retained: u64, limit: u64 },
    /// The reader couldn't read the slot for `unwatched`.
    Unwatched { unwatched: Duration },
}

/// What keeps the current slot within bounds: the bounds, and the
/// watchdog of the slot the last start made.
pub(crate) struct SlotGuard {
    pub(crate) watching: Watching,
    watchdog: Option<Watchdog>,
}

/// The watchdog's threads. Dropping it stops both and waits for the
/// clock, which only sleeps. It doesn't wait for the reader: a reader
/// stuck in a connect would hold up the next start.
pub(crate) struct Watchdog {
    shared: Arc<Shared>,
    clock: Option<JoinHandle<()>>,
}

/// What the threads and their owner all read and write.
struct Shared {
    stop: AtomicBool,
    verdict: Mutex<Option<Verdict>>,
    /// When the watchdog started; `read_at_ms` counts from here.
    started: Instant,
    /// When the reader last read the slot, in milliseconds from `started`.
    read_at_ms: AtomicU64,
}

/// One slot under watch.
struct Watch {
    dsn: Dsn,
    slot: String,
    /// Where the slot's WAL counts from: WAL the slot keeps from before
    /// this is the server's to recycle at its next checkpoint.
    from: Lsn,
    watching: Watching,
    /// Shut the sockets of the stream's sessions: the one that owns the
    /// slot, and the one the tables load on if that is another.
    closers: Vec<Closer>,
}

/// One reading of the slot from the server.
struct SlotReading {
    /// The session that owns the slot; `None` while nobody does.
    owner: Option<i32>,
    retained: u64,
}

impl SlotGuard {
    /// Watches no slot until [`SlotGuard::watch`].
    pub(crate) fn new(watching: Watching) -> SlotGuard {
        SlotGuard {
            watching,
            watchdog: None,
        }
    }

    /// Watches `slot` in place of any slot watched before, counting its
    /// WAL from `from`; `closers` shut the stream's sessions when the
    /// watchdog ends it.
    pub(crate) fn watch(
        &mut self,
        dsn: &Dsn,
        slot: &str,
        from: Lsn,
        closers: Vec<Closer>,
    ) -> Result<(), Error> {
        self.watchdog = None;
        self.watchdog = Some(Watchdog::start(dsn, slot, from, self.watching, closers)?);
        Ok(())
    }

    /// Stops watching: the slot is gone with its stream.
    pub(crate) fn stop(&mut self) {
        self.watchdog = None;
    }

    /// `error`, or the watchdog's verdict if it ended the stream, which
    /// says why better than the closed connection it left.
    pub(crate) fn blame(&self, error: Error) -> Error {
        let Some(watchdog) = &self.watchdog else {
            return error;
        };
        match watchdog.verdict() {
            Some(verdict) => Error::Watchdog(verdict),
            None => error,
        }
    }
}

impl Watchdog {
    /// Watches `slot`, counting its WAL from `from`; `closers` shut the
    /// stream's sessions.
    pub(crate) fn start(
        dsn: &Dsn,
        slot: &str,
        from: Lsn,
        watching: Watching,
        closers: Vec<Closer>,
    ) -> Result<Watchdog, Error> {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            verdict: Mutex::new(None),
            started: Instant::now(),
            read_at_ms: AtomicU64::new(0),
        });
        let watch = Arc::new(Watch {
            dsn: dsn.clone(),
            slot: slot.to_owned(),
            from,
            watching,
            closers,
        });
        let reader_watch = Arc::clone(&watch);
        let reader_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("straum-watchdog".into())
            .spawn(move || read_rounds(&reader_watch, &reader_shared))?;
        let clock_shared = Arc::clone(&shared);
        let clock = thread::Builder::new()
            .name("straum-watchdog-clock".into())
            .spawn(move || keep_time(&watch, &clock_shared));
        match clock {
            Ok(clock) => Ok(Watchdog {
                shared,
                clock: Some(clock),
            }),
            Err(error) => {
                shared.stop.store(true, Ordering::Release);
                Err(error.into())
            }
        }
    }

    /// Why the watchdog ended the stream, if it did.
    pub(crate) fn verdict(&self) -> Option<Verdict> {
        verdict(&self.shared)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(clock) = self.clock.take() {
            let _ = clock.join();
        }
    }
}

/// Whether a slot holding `retained` bytes of WAL is past
/// `max_retained_wal`, and the verdict if so.
pub(crate) fn past_limit(retained: u64, max_retained_wal: u64) -> Option<Verdict> {
    if retained <= max_retained_wal {
        return None;
    }
    Some(Verdict::Retained {
        retained,
        limit: max_retained_wal,
    })
}

/// The reader: reads the slot each round until told to stop, or, once
/// the stream has a verdict, until the slot is gone.
fn read_rounds(watch: &Watch, shared: &Shared) {
    let mut connection = None;
    while !stopped(shared) {
        let Ok(reading) = read_slot(&mut connection, watch) else {
            connection = None;
            nap(shared, RETRY);
            continue;
        };
        let ended = verdict(shared).is_some();
        match reading {
            None if ended => return,
            None => mark_read(shared),
            // The slot outlived the sockets' shutting: its session is
            // stuck, so PostgreSQL has to end it.
            Some(reading) if ended => end_owner(&mut connection, reading.owner),
            Some(reading) => match past_limit(reading.retained, watch.watching.max_retained_wal) {
                Some(verdict) => {
                    // Recorded first, so the error the stream's end causes
                    // finds it.
                    if record(shared, verdict) {
                        close_all(watch);
                    }
                }
                None => mark_read(shared),
            },
        }
        nap(shared, ROUND);
    }
}

/// The clock: ends the stream once the reader has gone `unwatched`
/// without a reading, or stops once the stream has a verdict.
fn keep_time(watch: &Watch, shared: &Shared) {
    let unwatched = watch.watching.unwatched;
    while !stopped(shared) {
        if verdict(shared).is_some() {
            return;
        }
        if unread_for(shared) >= unwatched {
            // The server sees the socket close and ends the session; on a
            // broken network, wal_sender_timeout does once streaming,
            // since no replies get through either. A reader that reads
            // the slot again from here ends its session too.
            if record(shared, Verdict::Unwatched { unwatched }) {
                close_all(watch);
            }
            return;
        }
        thread::sleep(NAP);
    }
}

/// What the slot holds, or `None` once it is gone. Connects first if the
/// reader has no connection.
fn read_slot(
    connection: &mut Option<Connection>,
    watch: &Watch,
) -> Result<Option<SlotReading>, Error> {
    let opened = match connection.take() {
        Some(opened) => opened,
        None => open(&watch.dsn)?,
    };
    let connection = connection.insert(opened);
    // GREATEST passes a NULL restart_lsn by, so the CASE keeps it NULL.
    let rows = connection.rows(&format!(
        "SELECT active_pid, pg_wal_lsn_diff(pg_current_wal_lsn(), \
         CASE WHEN restart_lsn IS NOT NULL THEN GREATEST(restart_lsn, {}::pg_lsn) END)::bigint \
         FROM pg_replication_slots WHERE slot_name = {}",
        escape_literal(&watch.from.to_string()),
        escape_literal(&watch.slot)
    ))?;
    if rows.is_empty() {
        return Ok(None);
    }
    let owner = (first_row_value(&rows, 0).map(str::parse::<i32>)).and_then(Result::ok);
    // No restart_lsn yet holds nothing; a negative difference neither.
    let retained = (first_row_value(&rows, 1).map(str::parse::<i64>))
        .and_then(Result::ok)
        .unwrap_or(0);
    Ok(Some(SlotReading {
        owner,
        retained: u64::try_from(retained).unwrap_or(0),
    }))
}

/// A plain connection whose queries can't hang the reader.
fn open(dsn: &Dsn) -> Result<Connection, Error> {
    let connection = Connection::connect(dsn, ConnectionKind::Sql)?;
    connection.set_read_timeout(QUERY_TIMEOUT)?;
    Ok(connection)
}

/// Has PostgreSQL end the session that owns the slot, which drops the
/// temporary slot. A role may end its own sessions. The reader asks again
/// each round the slot is still there: a walsender whose client reads
/// nothing takes the first ask, then blocks for good sending the client
/// its FATAL; only a second ask, coming while it is blocked, ends it
/// without sending.
fn end_owner(connection: &mut Option<Connection>, owner: Option<i32>) {
    let (Some(connection), Some(owner)) = (connection.as_mut(), owner) else {
        return;
    };
    let _ = connection.execute(&format!("SELECT pg_terminate_backend({owner})"));
}

/// Shuts every session of the stream, so a load stops too and lets go of
/// its locks.
fn close_all(watch: &Watch) {
    for closer in &watch.closers {
        closer.close();
    }
}

/// Records `verdict` unless the stream already has one or the watchdog
/// was told to stop; whether it did. Only the thread that records acts,
/// and nothing acts on a slot its owner has let go of.
fn record(shared: &Shared, verdict: Verdict) -> bool {
    let mut recorded = match shared.verdict.lock() {
        Ok(recorded) => recorded,
        Err(poisoned) => poisoned.into_inner(),
    };
    if recorded.is_some() || stopped(shared) {
        return false;
    }
    *recorded = Some(verdict);
    true
}

fn verdict(shared: &Shared) -> Option<Verdict> {
    match shared.verdict.lock() {
        Ok(verdict) => *verdict,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

fn stopped(shared: &Shared) -> bool {
    shared.stop.load(Ordering::Acquire)
}

/// Notes that the reader read the slot just now.
fn mark_read(shared: &Shared) {
    let elapsed_ms = u64::try_from(shared.started.elapsed().as_millis()).unwrap_or(u64::MAX);
    shared.read_at_ms.store(elapsed_ms, Ordering::Release);
}

/// How long since the reader last read the slot, or since the watchdog
/// started if it never has.
fn unread_for(shared: &Shared) -> Duration {
    let read_at = Duration::from_millis(shared.read_at_ms.load(Ordering::Acquire));
    shared.started.elapsed().saturating_sub(read_at)
}

/// Sleeps `round`, waking early when told to stop.
fn nap(shared: &Shared, round: Duration) {
    let until = Instant::now() + round;
    while Instant::now() < until && !stopped(shared) {
        thread::sleep(NAP);
    }
}
