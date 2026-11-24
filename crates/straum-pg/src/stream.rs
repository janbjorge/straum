//! The commit stream, read by a thread that owns the replication socket.
//! The thread decodes commits ahead of the writer (`decode.rs` for the
//! logical stream, `physical.rs` for the physical one) and queues them;
//! `Stream::next` takes them off the queue for `source.rs`. Meanwhile the
//! thread answers keepalives and reports the LSN (a WAL position) it has
//! read in standby status updates, so PostgreSQL never times Straum out
//! and may recycle that WAL: the slot is never streamed again. What the
//! writer has applied goes to the lag instead (`report.rs`).
//! While the tables load the thread reads on and queues every commit, up
//! to `--max-load-memory` bytes of them; once the writer takes the first,
//! the queue holds a few commits ahead of it and the thread waits for room.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use postgres_protocol::message::backend::Message as Backend;
use straum_app::{Change, Commit, Lsn};
use straum_domain::Datum;

use crate::decode::Decoder;
use crate::error::Error;
use crate::pgoutput::{self, CopyMessage};
use crate::physical::Replay;
use crate::report::{Progress, Reported, next_report};
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
/// Commits read ahead of the writer once it takes them. While the tables
/// load the bound is `--max-load-memory` instead.
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
    /// The last commit handed out before the writer asked again.
    applied: AtomicU64,
    /// Every commit up to here is applied, as of the thread's last report.
    caught_up: AtomicU64,
    stop: AtomicBool,
    /// Set by the thread before it queues its error, which a full queue
    /// holds back.
    failed: AtomicBool,
    /// Set until the writer asks for its first commit: the tables are
    /// still loading, and the queue is bounded in bytes, not commits.
    loading: AtomicBool,
    /// Items in the queue, the thread's error among them.
    queued_commits: AtomicUsize,
    /// Changes in the queue's commits.
    queued_changes: AtomicUsize,
    /// The queue's commits' footprints, summed.
    queued_bytes: AtomicUsize,
    /// A physical stream's state before its load: how far it is from
    /// where every later commit is whole, the load's snapshot with the WAL
    /// position it was taken at, for the thread to take, and what the
    /// thread made of it.
    consistency: Mutex<Consistency>,
    snapshot: Mutex<Option<(TransactionSnapshot, Lsn)>>,
    sight: Mutex<Option<Sight>>,
}

impl Shared {
    /// Nothing queued, nothing failed, everything before `start` applied,
    /// the tables loading.
    fn new(start: Lsn) -> Shared {
        Shared {
            applied: AtomicU64::new(start.0),
            caught_up: AtomicU64::new(start.0),
            stop: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            loading: AtomicBool::new(true),
            queued_commits: AtomicUsize::new(0),
            queued_changes: AtomicUsize::new(0),
            queued_bytes: AtomicUsize::new(0),
            consistency: Mutex::new(Consistency::Unknown),
            snapshot: Mutex::new(None),
            sight: Mutex::new(None),
        }
    }
}

impl Stream {
    /// Takes over a connection on which `START_REPLICATION` has begun,
    /// streaming from `start`. Past `max_load_memory` bytes of commits
    /// queued while the tables load, the stream fails.
    pub(crate) fn start(
        connection: Connection,
        reader: Reader,
        start: Lsn,
        max_load_memory: usize,
    ) -> Result<Stream, Error> {
        connection.set_read_timeout(POLL)?;
        let closer = connection.closer()?;
        let (commits, receiver) = mpsc::channel();
        let shared = Arc::new(Shared::new(start));
        let thread = StreamThread {
            connection,
            reader,
            commits,
            shared: Arc::clone(&shared),
            sent: start,
            idle: start,
            reported: Reported::at(start),
            last_report: Instant::now(),
            max_load_memory,
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

    /// Takes `commit` off the counts of what is queued.
    fn count_out(&self, commit: &Commit) {
        let size = Size::of(commit);
        let shared = &self.shared;
        shared.queued_commits.fetch_sub(1, Ordering::AcqRel);
        shared
            .queued_changes
            .fetch_sub(size.changes, Ordering::AcqRel);
        shared.queued_bytes.fetch_sub(size.bytes, Ordering::AcqRel);
    }

    /// The next commit, waiting at most `wait` for one. The first call
    /// ends the load, and the queue's bounds hold from then on.
    pub(crate) fn next(&mut self, wait: Duration) -> Result<Option<Commit>, Error> {
        self.shared.loading.store(false, Ordering::Release);
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

    /// Every commit up to here is applied, as of the thread's last report:
    /// the last one applied, or the last keepalive once nothing waits.
    pub(crate) fn caught_up(&self) -> Lsn {
        Lsn(self.shared.caught_up.load(Ordering::Acquire))
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
    commits: Sender<Result<Commit, Error>>,
    shared: Arc<Shared>,
    /// The bytes of commits the queue may hold while the tables load.
    max_load_memory: usize,
    /// The LSN of the last commit queued.
    sent: Lsn,
    /// The server's position at the last keepalive between transactions.
    /// Every commit up to it has been queued.
    idle: Lsn,
    /// What the last report told the server, and what it knew applied.
    reported: Reported,
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
        self.deliver_commits(commits)
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
                return self.deliver_commits(commits);
            }
            CopyMessage::Keepalive { end, reply } => {
                if !self.reader.in_transaction() {
                    self.idle = self.idle.max(Lsn(end));
                }
                if reply {
                    self.report()?;
                }
            }
        }
        Ok(true)
    }

    /// Queues each commit in order. `false` once the writer is gone or the
    /// stream is stopping.
    fn deliver_commits(&mut self, commits: Vec<Commit>) -> Result<bool, Error> {
        for commit in commits {
            self.sent = commit.lsn;
            if !self.deliver(Ok(commit))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Queues the item, answering the server while the queue is full, in
    /// commits or in changes. `false` once the writer is gone or the
    /// stream is stopping.
    fn deliver(&mut self, item: Result<Commit, Error>) -> Result<bool, Error> {
        let size = match &item {
            Ok(commit) => Size::of(commit),
            Err(_) => Size::NOTHING,
        };
        while self.room(size)? == Room::Full {
            if self.shared.stop.load(Ordering::Acquire) {
                return Ok(false);
            }
            if self.last_report.elapsed() >= REPORT_INTERVAL {
                self.report()?;
            }
            thread::sleep(WAIT);
        }
        // Counted before it is sent: the writer may take it and count it
        // out at once.
        let shared = &self.shared;
        shared.queued_commits.fetch_add(1, Ordering::AcqRel);
        shared
            .queued_changes
            .fetch_add(size.changes, Ordering::AcqRel);
        shared.queued_bytes.fetch_add(size.bytes, Ordering::AcqRel);
        Ok(self.commits.send(item).is_ok())
    }

    /// Whether an item of `size` may be queued now. While the tables load
    /// it may, unless it takes the queue past `--max-load-memory`; after,
    /// only within the queue's bounds in commits and changes.
    fn room(&self, size: Size) -> Result<Room, Error> {
        let shared = &self.shared;
        if shared.loading.load(Ordering::Acquire) {
            let queued = shared.queued_bytes.load(Ordering::Acquire);
            let held = queued.saturating_add(size.bytes);
            if held > self.max_load_memory {
                let limit = self.max_load_memory;
                return Err(Error::LoadMemory { held, limit });
            }
            return Ok(Room::Fits);
        }
        let commits = shared.queued_commits.load(Ordering::Acquire);
        let changes = shared.queued_changes.load(Ordering::Acquire);
        if commits < QUEUE && has_room(changes, size.changes) {
            return Ok(Room::Fits);
        }
        Ok(Room::Full)
    }

    /// Tells the server what is read, so it may recycle the WAL before
    /// it, and keeps what is applied for the lag.
    fn report(&mut self) -> Result<(), Error> {
        // A physical keepalive may name WAL not read yet; the replay
        // knows what it has read.
        let idle = match &self.reader {
            Reader::Logical(_) => self.idle,
            Reader::Physical(replay) => Lsn(replay.read()),
        };
        let progress = Progress {
            applied: Lsn(self.shared.applied.load(Ordering::Acquire)),
            sent: self.sent,
            idle,
            in_transaction: self.reader.in_transaction(),
            loading: self.shared.loading.load(Ordering::Acquire),
        };
        self.reported = next_report(self.reported, progress);
        self.shared
            .caught_up
            .store(self.reported.applied.0, Ordering::Release);
        self.connection
            .send_copy_data(&status_update(self.reported.position.0))?;
        self.last_report = Instant::now();
        Ok(())
    }
}

/// Whether an item fits in the queue now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Room {
    Fits,
    Full,
}

/// What one item adds to the queue's counts.
#[derive(Debug, Clone, Copy)]
struct Size {
    changes: usize,
    bytes: usize,
}

impl Size {
    /// An error: nothing counted but its place in the queue.
    const NOTHING: Size = Size {
        changes: 0,
        bytes: 0,
    };

    fn of(commit: &Commit) -> Size {
        Size {
            changes: commit.changes.len(),
            bytes: footprint(commit),
        }
    }
}

/// The bytes `commit` holds, near enough to bound memory by: its changes
/// and their rows' values, text by its length.
// Leaves out the tables' names, shared with the spec, and `Vec` slack.
fn footprint(commit: &Commit) -> usize {
    let mut bytes = size_of::<Commit>();
    for change in &commit.changes {
        bytes += size_of::<Change>();
        bytes += match change {
            Change::Insert { row, .. } => row_footprint(row),
            Change::Update { old, new, .. } => row_footprint(old) + row_footprint(new),
            Change::Delete { old, .. } => row_footprint(old),
            Change::Truncate { .. } | Change::Unfit { .. } => 0,
        };
    }
    bytes
}

/// The bytes of one row's values, text by its length.
fn row_footprint(row: &[Datum]) -> usize {
    let mut bytes = size_of_val(row);
    for datum in row {
        if let Datum::Text(text) = datum {
            bytes += text.len();
        }
    }
    bytes
}

/// Whether a commit of `changes` fits beside the `queued` changes: always
/// into an empty queue.
fn has_room(queued: usize, changes: usize) -> bool {
    queued == 0 || queued.saturating_add(changes) <= QUEUED_CHANGES
}

/// A standby status update saying the server may recycle the WAL before
/// `done`.
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

    use crate::tests::support::{LOAD_MEMORY, now, scripted, signed_in};
    use crate::wire::ConnectionKind;
    use straum_domain::Identifier;

    /// Long enough that only a thread that stopped reading passes it.
    const STALL: Duration = Duration::from_secs(10);

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

    /// A commit of one insert of a row holding `text` and a number.
    fn commit_with_text(text: &str) -> Commit {
        let table = Identifier::new("trips").unwrap();
        let row = vec![Datum::Text(text.into()), Datum::Int(1)];
        Commit {
            lsn: Lsn(0x20),
            committed: SystemTime::now(),
            changes: vec![Change::Insert { table, row }],
        }
    }

    /// The writer's end and the thread's end of a stream from a fake
    /// server, the thread's end not yet running, the tables loading.
    fn ends(max_load_memory: usize) -> (Stream, StreamThread) {
        let server = scripted(vec![now(signed_in())]);
        let connection = Connection::connect(&server.dsn, ConnectionKind::Sql).unwrap();
        let closer = connection.closer().unwrap();
        let (commits, receiver) = mpsc::channel();
        let shared = Arc::new(Shared::new(Lsn(0)));
        let thread = StreamThread {
            connection,
            reader: Reader::Logical(Decoder::new(Vec::new(), 1)),
            commits,
            shared: Arc::clone(&shared),
            max_load_memory,
            sent: Lsn(0),
            idle: Lsn(0),
            reported: Reported::at(Lsn(0)),
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
            let (mut stream, mut thread) = ends(LOAD_MEMORY);
            // Loaded, so the queue's bounds hold the error back.
            stream.shared.loading.store(false, Ordering::Release);
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

    /// While the tables load the thread queues every commit it reads,
    /// with nobody taking them, past the bound that holds after.
    #[test]
    fn while_loading_the_queue_holds_more_than_its_bound() {
        let (stream, mut thread) = ends(LOAD_MEMORY);
        let shared = Arc::clone(&stream.shared);
        let (done, delivered) = mpsc::channel();
        let handle = thread::spawn(move || {
            for _ in 0..QUEUE * 4 {
                let Ok(true) = thread.deliver(Ok(commit_of(1))) else {
                    return;
                };
            }
            done.send(()).unwrap();
        });
        let finished = delivered.recv_timeout(STALL);
        // Lets a thread waiting for room go.
        shared.stop.store(true, Ordering::Release);
        handle.join().unwrap();
        let queued = shared.queued_commits.load(Ordering::Acquire);
        assert!(finished.is_ok(), "the thread stopped at {queued} commits");
        assert_eq!(queued, QUEUE * 4);
    }

    /// Past `--max-load-memory` the stream fails, and the writer hears it
    /// after the commits queued, with what was held and the limit. Up to
    /// the limit it does not.
    #[test]
    fn past_the_load_memory_the_stream_fails() {
        let bytes = footprint(&commit_of(1));
        let (mut stream, mut thread) = ends(bytes * 3);
        for _ in 0..3 {
            assert!(thread.deliver(Ok(commit_of(1))).unwrap());
        }
        let error = thread.deliver(Ok(commit_of(1))).unwrap_err();
        thread.fail(error);
        let error = stream.check().unwrap_err().to_string();
        let held = bytes * 4;
        let limit = bytes * 3;
        assert_eq!(
            error,
            format!(
                "the stream holds {held} bytes of commits while the tables load, past the \
                 --max-load-memory limit of {limit}"
            )
        );
    }

    /// Once the writer takes its first commit the queue's bounds hold
    /// again: the thread may queue more of a backlog from the load only
    /// once the writer has taken it below them, in commits and in changes.
    #[test]
    fn after_the_load_the_queue_is_bounded_again() {
        let one = Size::of(&commit_of(1));
        let (mut stream, mut thread) = ends(LOAD_MEMORY);
        let backlog = QUEUE * 2;
        // Room is checked first: a stream that bounds the queue while the
        // tables load would block in `deliver`, with nobody to drain it.
        for queued in 0..backlog {
            assert_eq!(thread.room(one).unwrap(), Room::Fits, "{queued} queued");
            assert!(thread.deliver(Ok(commit_of(1))).unwrap());
        }
        assert_eq!(thread.room(one).unwrap(), Room::Fits);
        let mut taken = 0;
        while stream.next(Duration::ZERO).unwrap().is_some() {
            taken += 1;
            let left = backlog - taken;
            let room = if left < QUEUE { Room::Fits } else { Room::Full };
            assert_eq!(thread.room(one).unwrap(), room, "{left} queued");
        }
        assert_eq!(taken, backlog);

        let (mut stream, mut thread) = ends(LOAD_MEMORY);
        for queued in 0..3 {
            let half = commit_of(QUEUED_CHANGES / 2);
            let size = Size::of(&half);
            assert_eq!(thread.room(size).unwrap(), Room::Fits, "{queued} queued");
            assert!(thread.deliver(Ok(half)).unwrap());
        }
        stream.next(Duration::ZERO).unwrap().unwrap();
        assert_eq!(thread.room(one).unwrap(), Room::Full);
        stream.next(Duration::ZERO).unwrap().unwrap();
        assert_eq!(thread.room(one).unwrap(), Room::Fits);
    }

    /// The count of bytes queued goes back to zero once the writer has
    /// taken everything queued, through `next` and through `check`.
    #[test]
    fn the_bytes_queued_return_to_zero() {
        let (mut stream, mut thread) = ends(LOAD_MEMORY);
        let commits = [commit_of(1), commit_of(5), commit_with_text("trip")];
        let mut total = 0;
        for commit in commits {
            total += footprint(&commit);
            assert!(thread.deliver(Ok(commit)).unwrap());
        }
        let shared = Arc::clone(&stream.shared);
        assert_eq!(shared.queued_bytes.load(Ordering::Acquire), total);
        stream.next(Duration::ZERO).unwrap().unwrap();
        assert!(shared.queued_bytes.load(Ordering::Acquire) < total);
        thread.fail(Error::Protocol(Unexpected::StreamEnded));
        stream.check().unwrap_err();
        assert_eq!(shared.queued_bytes.load(Ordering::Acquire), 0);
        assert_eq!(shared.queued_changes.load(Ordering::Acquire), 0);
    }

    /// A commit's footprint counts its text by length, and nothing else
    /// of a row varies.
    #[test]
    fn a_commit_counts_its_text() {
        let empty = footprint(&commit_with_text(""));
        assert_eq!(
            footprint(&commit_with_text(&"x".repeat(1000))),
            empty + 1000
        );
        assert_eq!(footprint(&commit_with_text("")), empty);
        assert!(empty > footprint(&commit_of(1)));
    }

    /// A status update names one position three times, in the order and
    /// size the protocol gives, and asks for no reply.
    #[test]
    fn a_status_update_names_the_position() {
        let message = status_update(0x16_B374_D848);
        assert_eq!(message.len(), STATUS_UPDATE_SIZE);
        assert_eq!(message[0], b'r');
        for position in 0..STATUS_UPDATE_POSITIONS {
            let at = 1 + 8 * position;
            let bytes: [u8; 8] = message[at..at + 8].try_into().unwrap();
            assert_eq!(u64::from_be_bytes(bytes), 0x16_B374_D848);
        }
        assert_eq!(message[STATUS_UPDATE_SIZE - 1], 0);
        assert_ne!(status_update(1), status_update(2));
    }
}
