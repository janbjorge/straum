//! The commit stream, read by a thread that owns the replication socket.
//! The thread decodes commits ahead of the writer, answers keepalives and
//! reports the applied LSN while the writer applies, so PostgreSQL never
//! times Straum out for being busy.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use postgres_protocol::message::backend::Message as Backend;
use straum_app::{Commit, Lsn};

use crate::decode::Decoder;
use crate::error::Error;
use crate::pgoutput::{self, CopyMessage};
use crate::value;
use crate::wire::{Closer, Conn, server_error};

/// How long a read waits before the thread checks whether a status update
/// is due or it should stop.
const POLL: Duration = Duration::from_secs(1);
/// How often the thread reports, whether or not the server asks. Short, so
/// a full queue, which stops the thread reading, can't outlast a
/// `wal_sender_timeout` of a few seconds.
const STATUS_INTERVAL: Duration = Duration::from_secs(1);
/// Commits read ahead of the writer.
const QUEUE: usize = 64;
/// How long the thread waits for room in a full queue before trying again.
const WAIT: Duration = Duration::from_millis(10);

/// The writer's end of the stream.
pub(crate) struct Stream {
    commits: Receiver<Result<Commit, Error>>,
    shared: Arc<Shared>,
    closer: Closer,
    thread: Option<JoinHandle<()>>,
    /// The last commit handed out. Asking for the next one means it is
    /// applied.
    last: Lsn,
}

struct Shared {
    applied: AtomicU64,
    stop: AtomicBool,
}

impl Stream {
    /// Takes over a connection on which `START_REPLICATION` has begun,
    /// streaming from `start`.
    pub(crate) fn start(conn: Conn, decoder: Decoder, start: Lsn) -> Result<Stream, Error> {
        conn.set_read_timeout(POLL)?;
        let closer = conn.closer()?;
        let (commits, receiver) = mpsc::sync_channel(QUEUE);
        let shared = Arc::new(Shared {
            applied: AtomicU64::new(start.0),
            stop: AtomicBool::new(false),
        });
        let thread = StreamThread {
            conn,
            decoder,
            commits,
            shared: Arc::clone(&shared),
            sent: start.0,
            idle: start.0,
            last_status: Instant::now(),
        };
        let thread = thread::Builder::new()
            .name("straum-stream".into())
            .spawn(move || thread.run())?;
        Ok(Stream {
            commits: receiver,
            shared,
            closer,
            thread: Some(thread),
            last: start,
        })
    }

    /// The next commit, waiting at most `wait` for one.
    pub(crate) fn next(&mut self, wait: Duration) -> Result<Option<Commit>, Error> {
        self.shared.applied.store(self.last.0, Ordering::Release);
        match self.commits.recv_timeout(wait) {
            Ok(Ok(commit)) => {
                self.last = commit.lsn;
                Ok(Some(commit))
            }
            Ok(Err(e)) => Err(e),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(Error::Stopped),
        }
    }
}

impl Drop for Stream {
    /// Stops the thread and closes the connection, which drops the slot.
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.closer.close();
        if let Some(thread) = self.thread.take() {
            // A panic in the thread already reached the writer as a closed
            // queue.
            let _ = thread.join();
        }
    }
}

/// The thread's end: the socket, the decoder and what it has reported.
struct StreamThread {
    conn: Conn,
    decoder: Decoder,
    commits: SyncSender<Result<Commit, Error>>,
    shared: Arc<Shared>,
    /// The LSN of the last commit queued.
    sent: u64,
    /// The server's position at the last keepalive between transactions.
    /// Every commit up to it has been queued.
    idle: u64,
    last_status: Instant,
}

impl StreamThread {
    fn run(mut self) {
        if let Err(e) = self.serve() {
            // The writer may be gone, or the connection with it.
            let _ = self.deliver(Err(e));
        }
    }

    fn serve(&mut self) -> Result<(), Error> {
        while !self.shared.stop.load(Ordering::Acquire) {
            match self.conn.try_read()? {
                None => {}
                Some(Backend::CopyData(body)) => {
                    if !self.copy_data(body.data())? {
                        return Ok(());
                    }
                }
                Some(Backend::ErrorResponse(body)) => return Err(server_error(&body)),
                Some(_) => return Err(Error::protocol("a message that ended the stream")),
            }
            if self.last_status.elapsed() >= STATUS_INTERVAL {
                self.status()?;
            }
        }
        Ok(())
    }

    /// `false` once the writer is gone.
    fn copy_data(&mut self, data: &[u8]) -> Result<bool, Error> {
        let message = pgoutput::copy_message(data)
            .map_err(|_| Error::protocol("a copy message Straum doesn't know"))?;
        match message {
            CopyMessage::XLogData(bytes) => {
                if let Some(commit) = self.decoder.feed(pgoutput::decode(bytes)?)? {
                    self.sent = commit.lsn.0;
                    return self.deliver(Ok(commit));
                }
            }
            CopyMessage::Keepalive { end, reply } => {
                if !self.decoder.in_transaction() {
                    self.idle = self.idle.max(end);
                }
                if reply {
                    self.status()?;
                }
            }
        }
        Ok(true)
    }

    /// Queues the item, answering the server while the queue is full.
    /// `false` once the writer is gone or the stream is stopping.
    fn deliver(&mut self, mut item: Result<Commit, Error>) -> Result<bool, Error> {
        loop {
            match self.commits.try_send(item) {
                Ok(()) => return Ok(true),
                Err(TrySendError::Disconnected(_)) => return Ok(false),
                Err(TrySendError::Full(back)) => {
                    item = back;
                    if self.shared.stop.load(Ordering::Acquire) {
                        return Ok(false);
                    }
                    if self.last_status.elapsed() >= STATUS_INTERVAL {
                        self.status()?;
                    }
                    thread::sleep(WAIT);
                }
            }
        }
    }

    /// Tells the server what is applied, so it may recycle the WAL before
    /// it. With every queued commit applied and no transaction open, that is
    /// everything up to the last keepalive.
    fn status(&mut self) -> Result<(), Error> {
        let applied = self.shared.applied.load(Ordering::Acquire);
        let done = if applied >= self.sent && !self.decoder.in_transaction() {
            applied.max(self.idle)
        } else {
            applied
        };
        let now = value::timestamp_micros(SystemTime::now());
        let mut message = Vec::with_capacity(34);
        message.push(b'r');
        // Written, flushed and applied are one point for straum.
        for _ in 0..3 {
            message.extend(done.to_be_bytes());
        }
        message.extend(now.to_be_bytes());
        message.push(0);
        self.conn.send_copy_data(&message)?;
        self.last_status = Instant::now();
        Ok(())
    }
}
