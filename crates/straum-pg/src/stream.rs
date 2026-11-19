//! The commit stream, read by a thread that owns the replication socket.
//! The thread decodes commits ahead of the writer (`decode.rs` for the
//! logical stream, `physical.rs` for the physical one) and queues them;
//! `Stream::next` takes them off the queue for `source.rs`. Meanwhile the
//! thread answers keepalives and reports the applied LSN (a WAL position)
//! in standby status updates, so PostgreSQL never times Straum out.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use postgres_protocol::message::backend::Message as Backend;
use straum_app::{Commit, Lsn};

use crate::decode::Decoder;
use crate::error::Error;
use crate::pgoutput::{self, CopyMessage};
use crate::physical::Replay;
use crate::unexpected::Unexpected;
use crate::value;
use crate::wal::pipeline::Sight;
use crate::wal::transactions::{Consistency, TransactionSnapshot};
use crate::wire::{Closer, Connection, server_error};

/// How long a read waits before the thread checks whether a status update
/// is due or it should stop.
pub(crate) const POLL: Duration = Duration::from_secs(1);
/// How often the thread reports, whether or not the server asks. Short, so
/// a full queue, which stops the thread reading, can't outlast a
/// `wal_sender_timeout` of a few seconds.
pub(crate) const REPORT_INTERVAL: Duration = Duration::from_secs(1);
/// Commits read ahead of the writer.
const QUEUE: usize = 64;
/// Changes read ahead of the writer, beside [`QUEUE`]: 64 commits at the
/// row limit would hold gigabytes. A commit bigger than this still goes
/// in alone.
const QUEUED_CHANGES: usize = 200_000;
/// How long the thread waits for room in a full queue before trying again.
const WAIT: Duration = Duration::from_millis(10);
/// How long a failed stream may take to hand over its error once the
/// queue has room: one wait for room, with time to spare.
const FAILURE_WAIT: Duration = Duration::from_secs(5);

/// What turns the stream's bytes into commits.
pub(crate) enum Reader {
    /// pgoutput messages from a logical slot.
    Logical(Decoder),
    /// WAL from a physical slot.
    Physical(Box<Replay>),
}

impl Reader {
    /// Holds part of a transaction it hasn't handed out.
    fn in_transaction(&self) -> bool {
        match self {
            Reader::Logical(decoder) => decoder.in_transaction(),
            Reader::Physical(_) => false,
        }
    }
}

/// The writer's end of the stream.
pub(crate) struct Stream {
    commits: Receiver<Result<Commit, Error>>,
    shared: Arc<Shared>,
    closer: Closer,
    /// Other connections the thread reads, closed with the stream so a
    /// read blocked on one can't hold the join.
    others: Vec<Closer>,
    thread: Option<JoinHandle<()>>,
    /// The last commit handed out. Asking for the next one means it is
    /// applied.
    applied: Lsn,
}

/// What the writer and the stream's thread both read and write.
struct Shared {
    applied: AtomicU64,
    stop: AtomicBool,
    /// Set by the thread before it queues its error, which a full queue
    /// holds back.
    failed: AtomicBool,
    /// Changes in the queue's commits.
    queued_changes: AtomicUsize,
    /// A physical stream's state before its load: how far it is from
    /// where every later commit is whole, the load's snapshot with the WAL
    /// position it was taken at, for the thread to take, and what the
    /// thread made of it.
    consistency: Mutex<Consistency>,
    snapshot: Mutex<Option<(TransactionSnapshot, Lsn)>>,
    sight: Mutex<Option<Sight>>,
}

impl Shared {
    /// Nothing queued, nothing failed, everything before `start` applied.
    fn new(start: Lsn) -> Shared {
        Shared {
            applied: AtomicU64::new(start.0),
            stop: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            queued_changes: AtomicUsize::new(0),
            consistency: Mutex::new(Consistency::Unknown),
            snapshot: Mutex::new(None),
            sight: Mutex::new(None),
        }
    }
}

impl Stream {
    /// Takes over a connection on which `START_REPLICATION` has begun,
    /// streaming from `start`.
    pub(crate) fn start(
        connection: Connection,
        reader: Reader,
        start: Lsn,
    ) -> Result<Stream, Error> {
        connection.set_read_timeout(POLL)?;
        let closer = connection.closer()?;
        let (commits, receiver) = mpsc::sync_channel(QUEUE);
        let shared = Arc::new(Shared::new(start));
        let thread = StreamThread {
            connection,
            reader,
            commits,
            shared: Arc::clone(&shared),
            sent: start.0,
            idle: start.0,
            reported: start.0,
            last_report: Instant::now(),
        };
        let thread = thread::Builder::new()
            .name("straum-stream".into())
            .spawn(move || thread.run())?;
        Ok(Stream {
            commits: receiver,
            shared,
            closer,
            others: Vec::new(),
            thread: Some(thread),
            applied: start,
        })
    }

    /// Closes `other` too when the stream stops: a connection its thread
    /// reads besides the stream's own.
    pub(crate) fn close_too(&mut self, other: Closer) {
        self.others.push(other);
    }

    /// The stream's error once its thread has failed, the commits queued
    /// before it dropped: a load checks between batches, so a stream that
    /// fails while the tables load stops the load then, not at its end.
    pub(crate) fn check(&mut self) -> Result<(), Error> {
        if !self.shared.failed.load(Ordering::Acquire) {
            return Ok(());
        }
        loop {
            match self.commits.recv_timeout(FAILURE_WAIT) {
                // Counted out, so the error the thread holds back finds room.
                Ok(Ok(commit)) => self.count_out(&commit),
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(Error::Stopped),
            }
        }
    }

    /// Takes `commit`'s changes off the count of those queued.
    fn count_out(&self, commit: &Commit) {
        self.shared
            .queued_changes
            .fetch_sub(commit.changes.len(), Ordering::AcqRel);
    }

    /// The next commit, waiting at most `wait` for one.
    pub(crate) fn next(&mut self, wait: Duration) -> Result<Option<Commit>, Error> {
        self.shared.applied.store(self.applied.0, Ordering::Release);
        match self.commits.recv_timeout(wait) {
            Ok(Ok(commit)) => {
                self.applied = commit.lsn;
                self.count_out(&commit);
                Ok(Some(commit))
            }
            Ok(Err(error)) => Err(error),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(Error::Stopped),
        }
    }
}

impl Stream {
    /// For a physical stream: waits at most `wait` until every
    /// transaction that commits from now on is whole in the WAL read, and
    /// says how far it got. No commit comes before the load's snapshot, so
    /// the queue holds at most the thread's error.
    pub(crate) fn wait_consistent(&mut self, wait: Duration) -> Result<Consistency, Error> {
        let deadline = Instant::now() + wait;
        loop {
            let consistency = self
                .shared
                .consistency
                .lock()
                .map_err(|_| Error::Stopped)?
                .clone();
            let left = deadline.saturating_duration_since(Instant::now());
            if consistency == Consistency::Consistent || left.is_zero() {
                return Ok(consistency);
            }
            match self.commits.recv_timeout(left.min(WAIT)) {
                Ok(Err(error)) => return Err(error),
                Ok(Ok(_)) => return Err(Error::Protocol(Unexpected::CommitBeforeSnapshot)),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(Error::Stopped),
            }
        }
    }

    /// For a physical stream: hands the thread the load's snapshot, taken
    /// with the server's WAL at `at`, and says what it made of it.
    pub(crate) fn see(&mut self, snapshot: TransactionSnapshot, at: Lsn) -> Result<Sight, Error> {
        *self.shared.snapshot.lock().map_err(|_| Error::Stopped)? = Some((snapshot, at));
        loop {
            if let Some(sight) = self.shared.sight.lock().map_err(|_| Error::Stopped)?.take() {
                return Ok(sight);
            }
            // A thread that stopped left its error in the queue.
            if self.thread.as_ref().is_none_or(JoinHandle::is_finished) {
                return Err(match self.commits.try_recv() {
                    Ok(Err(error)) => error,
                    _ => Error::Stopped,
                });
            }
            thread::sleep(WAIT);
        }
    }
}

impl Drop for Stream {
    /// Stops the thread and closes the connection, which drops the slot.
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.closer.close();
        for other in &self.others {
            other.close();
        }
        if let Some(thread) = self.thread.take() {
            // A panic in the thread already reached the writer as a closed
            // queue.
            let _ = thread.join();
        }
    }
}

/// The thread's end: the socket, the reader and what it has reported.
struct StreamThread {
    connection: Connection,
    reader: Reader,
    commits: SyncSender<Result<Commit, Error>>,
    shared: Arc<Shared>,
    /// The LSN of the last commit queued.
    sent: u64,
    /// The server's position at the last keepalive between transactions.
    /// Every commit up to it has been queued.
    idle: u64,
    /// The last position reported. A physical slot moves its restart
    /// point to whatever is reported, back too, so no report goes below.
    reported: u64,
    last_report: Instant,
}

impl StreamThread {
    fn run(mut self) {
        if let Err(error) = self.serve() {
            self.fail(error);
        }
    }

    /// Says the stream failed and queues `error` for the writer.
    fn fail(&mut self, error: Error) {
        self.shared.failed.store(true, Ordering::Release);
        // The writer may be gone, or the connection with it.
        let _ = self.deliver(Err(error));
    }

    fn serve(&mut self) -> Result<(), Error> {
        while !self.shared.stop.load(Ordering::Acquire) {
            if !self.before_load()? {
                return Ok(());
            }
            match self.connection.try_read()? {
                // A walsender may warn or change a setting at any time, as
                // any backend may; neither says anything of the stream.
                None | Some(Backend::NoticeResponse(_) | Backend::ParameterStatus(_)) => {}
                Some(Backend::CopyData(body)) => {
                    if !self.copy_data(body.data())? {
                        return Ok(());
                    }
                }
                Some(Backend::ErrorResponse(body)) => return Err(server_error(&body)),
                Some(_) => return Err(Error::Protocol(Unexpected::StreamEnded)),
            }
            if self.last_report.elapsed() >= REPORT_INTERVAL {
                self.report()?;
            }
        }
        Ok(())
    }

    /// A physical stream says when it is consistent, and takes the load's
    /// snapshot: the commits held that it didn't see go out. `false` once
    /// the writer is gone.
    fn before_load(&mut self) -> Result<bool, Error> {
        let Reader::Physical(replay) = &mut self.reader else {
            return Ok(true);
        };
        let mut consistency = self.shared.consistency.lock().map_err(|_| Error::Stopped)?;
        if *consistency != Consistency::Consistent {
            *consistency = replay.pipeline.consistency();
        }
        drop(consistency);
        let taken = self
            .shared
            .snapshot
            .lock()
            .map_err(|_| Error::Stopped)?
            .take();
        let Some((snapshot, at)) = taken else {
            return Ok(true);
        };
        let (sight, commits) = replay.pipeline.see(snapshot, at)?;
        *self.shared.sight.lock().map_err(|_| Error::Stopped)? = Some(sight);
        for commit in commits {
            self.sent = commit.lsn.0;
            if !self.deliver(Ok(commit))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `false` once the writer is gone.
    fn copy_data(&mut self, data: &[u8]) -> Result<bool, Error> {
        match pgoutput::copy_message(data)? {
            CopyMessage::XLogData { start, bytes } => {
                let commits = match &mut self.reader {
                    Reader::Logical(decoder) => decoder
                        .feed(&pgoutput::decode(bytes)?)?
                        .into_iter()
                        .collect(),
                    Reader::Physical(replay) => replay.feed(start, bytes)?,
                };
                for commit in commits {
                    self.sent = commit.lsn.0;
                    if !self.deliver(Ok(commit))? {
                        return Ok(false);
                    }
                }
            }
            CopyMessage::Keepalive { end, reply } => {
                if !self.reader.in_transaction() {
                    self.idle = self.idle.max(end);
                }
                if reply {
                    self.report()?;
                }
            }
        }
        Ok(true)
    }

    /// Queues the item, answering the server while the queue is full, in
    /// commits or in changes. `false` once the writer is gone or the
    /// stream is stopping.
    fn deliver(&mut self, mut item: Result<Commit, Error>) -> Result<bool, Error> {
        let changes = match &item {
            Ok(commit) => commit.changes.len(),
            Err(_) => 0,
        };
        loop {
            let queued = self.shared.queued_changes.load(Ordering::Acquire);
            if has_room(queued, changes) {
                // Counted before it is sent: the writer may take it and
                // count it out at once.
                self.shared
                    .queued_changes
                    .fetch_add(changes, Ordering::AcqRel);
                match self.commits.try_send(item) {
                    Ok(()) => return Ok(true),
                    Err(TrySendError::Disconnected(_)) => return Ok(false),
                    Err(TrySendError::Full(back)) => {
                        self.shared
                            .queued_changes
                            .fetch_sub(changes, Ordering::AcqRel);
                        item = back;
                    }
                }
            }
            if self.shared.stop.load(Ordering::Acquire) {
                return Ok(false);
            }
            if self.last_report.elapsed() >= REPORT_INTERVAL {
                self.report()?;
            }
            thread::sleep(WAIT);
        }
    }

    /// Tells the server what is applied, so it may recycle the WAL before
    /// it. With every queued commit applied and no transaction open, that is
    /// everything up to the last keepalive.
    fn report(&mut self) -> Result<(), Error> {
        let applied = self.shared.applied.load(Ordering::Acquire);
        // A physical keepalive may name WAL not read yet; the replay
        // knows what it has read.
        let idle = match &self.reader {
            Reader::Logical(_) => self.idle,
            Reader::Physical(replay) => replay.read(),
        };
        let done = if applied >= self.sent && !self.reader.in_transaction() {
            applied.max(idle)
        } else {
            applied
        };
        self.reported = self.reported.max(done);
        self.connection
            .send_copy_data(&status_update(self.reported))?;
        self.last_report = Instant::now();
        Ok(())
    }
}

/// Whether a commit of `changes` fits beside the `queued` changes: always
/// into an empty queue.
fn has_room(queued: usize, changes: usize) -> bool {
    queued == 0 || queued.saturating_add(changes) <= QUEUED_CHANGES
}

/// A standby status update saying everything before `done` is applied.
pub(crate) fn status_update(done: u64) -> Vec<u8> {
    let now = value::timestamp_micros(SystemTime::now());
    let mut message = Vec::with_capacity(STATUS_UPDATE_SIZE);
    message.push(b'r');
    // Written, flushed and applied are one point for straum.
    for _ in 0..STATUS_UPDATE_POSITIONS {
        message.extend(done.to_be_bytes());
    }
    message.extend(now.to_be_bytes());
    // Asks for no reply.
    message.push(0);
    message
}

/// The positions a standby status update reports: written, flushed and
/// applied.
const STATUS_UPDATE_POSITIONS: usize = 3;
/// The tag, eight bytes for each position and for the clock, and the
/// reply flag.
const STATUS_UPDATE_SIZE: usize = 1 + 8 * STATUS_UPDATE_POSITIONS + 8 + 1;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::{now, scripted, signed_in};
    use crate::wire::ConnectionKind;
    use straum_app::Change;
    use straum_domain::Identifier;

    /// A commit of `count` inserts into one table.
    fn commit_of(count: usize) -> Commit {
        let table = Identifier::new("trips").unwrap();
        let mut changes = Vec::new();
        for _ in 0..count {
            let row = Vec::new();
            let table = table.clone();
            changes.push(Change::Insert { table, row });
        }
        Commit {
            lsn: Lsn(0x20),
            committed: SystemTime::now(),
            changes,
        }
    }

    /// The writer's end and the thread's end of a stream from a fake
    /// server, the thread's end not yet running.
    fn ends() -> (Stream, StreamThread) {
        let server = scripted(vec![now(signed_in())]);
        let connection = Connection::connect(&server.dsn, ConnectionKind::Sql).unwrap();
        let closer = connection.closer().unwrap();
        let (commits, receiver) = mpsc::sync_channel(QUEUE);
        let shared = Arc::new(Shared::new(Lsn(0)));
        let thread = StreamThread {
            connection,
            reader: Reader::Logical(Decoder::new(Vec::new(), 1)),
            commits,
            shared: Arc::clone(&shared),
            sent: 0,
            idle: 0,
            reported: 0,
            last_report: Instant::now(),
        };
        let stream = Stream {
            commits: receiver,
            shared,
            closer,
            others: Vec::new(),
            thread: None,
            applied: Lsn(0),
        };
        (stream, thread)
    }

    /// Once the load's check takes the commits queued before a failure,
    /// the error after them fits and the check gives it, not a stream that
    /// merely stopped: after a commit past the bound, and after a small one.
    #[test]
    fn a_check_gives_the_error_after_the_commits_queued() {
        for count in [QUEUED_CHANGES + 1, 1] {
            let (mut stream, mut thread) = ends();
            let handle = thread::spawn(move || {
                assert!(thread.deliver(Ok(commit_of(count))).unwrap());
                thread.fail(Error::Protocol(Unexpected::StreamEnded));
            });
            stream.thread = Some(handle);
            while !stream.shared.failed.load(Ordering::Acquire) {
                thread::sleep(WAIT);
            }
            let error = stream.check().unwrap_err().to_string();
            assert_eq!(
                error, "PostgreSQL sent a message that ended the stream",
                "{count}"
            );
        }
    }

    /// The queue takes commits until their changes reach the bound, and
    /// a commit past the bound on its own into an empty queue, so a big
    /// transaction still streams.
    #[test]
    fn the_queue_is_bounded_in_changes() {
        assert!(has_room(0, 1));
        assert!(has_room(0, QUEUED_CHANGES * 3));
        assert!(has_room(QUEUED_CHANGES - 10, 10));
        assert!(!has_room(QUEUED_CHANGES - 10, 11));
        assert!(!has_room(1, QUEUED_CHANGES));
        assert!(!has_room(usize::MAX, 1));
    }
}
