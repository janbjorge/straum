//! [`Feeder`], the other end of a [`Feed`](crate::Feed): what a program
//! that holds rows connects with.

use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use straum_domain::{ColumnType, Datum, Identifier, Table};

use crate::error::Error;
use crate::frame::{self, Frame};

/// A connection to the Straum that listens on a feed's socket.
pub struct Feeder {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
    buffer: Vec<u8>,
}

/// What Straum asks of a feeder next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Every row of `table`, each holding `columns` in this order. The
    /// answer is [`Feeder::types`], [`Feeder::rows`] any number of times,
    /// then [`Feeder::end`]; or [`Feeder::fail`].
    Load {
        table: Table,
        columns: Vec<Identifier>,
    },
    /// Straum has every table, and has closed the feed.
    Done,
}

impl Feeder {
    /// Connects and says hello.
    pub fn connect(path: impl AsRef<Path>) -> Result<Feeder, Error> {
        let stream = UnixStream::connect(path)?;
        let mut feeder = Feeder {
            reader: BufReader::new(stream.try_clone()?),
            writer: BufWriter::new(stream),
            buffer: Vec::new(),
        };
        feeder.send(&Frame::Hello)?;
        feeder.writer.flush()?;
        Ok(feeder)
    }

    /// Waits for what Straum asks next. [`Error::Straum`] when Straum
    /// gave up and said why.
    pub fn next_request(&mut self) -> Result<Request, Error> {
        match frame::read(&mut self.reader, &mut self.buffer)? {
            Frame::Load { table, columns } => Ok(Request::Load { table, columns }),
            Frame::Done => Ok(Request::Done),
            Frame::Failed(why) => Err(Error::Straum(why)),
            _ => Err(Error::protocol("a frame Straum never sends")),
        }
    }

    /// The type of each column asked for.
    pub fn types(&mut self, types: &[ColumnType]) -> Result<(), Error> {
        self.send(&Frame::Types(types.to_vec()))
    }

    /// Whole rows of the table asked for, laid end to end.
    pub fn rows(&mut self, rows: &[Datum]) -> Result<(), Error> {
        let sent = frame::write_rows(&mut self.writer, rows, &mut self.buffer);
        self.explained(sent)
    }

    /// The table asked for has no more rows.
    pub fn end(&mut self) -> Result<(), Error> {
        self.send(&Frame::End)?;
        let flushed = self.writer.flush().map_err(Error::from);
        self.explained(flushed)
    }

    /// Tells Straum why the table can't be fed. Straum closes the feed.
    pub fn fail(&mut self, why: &str) -> Result<(), Error> {
        self.send(&Frame::Failed(why.to_owned()))?;
        Ok(self.writer.flush()?)
    }

    fn send(&mut self, frame: &Frame) -> Result<(), Error> {
        let sent = frame::write(&mut self.writer, frame, &mut self.buffer);
        self.explained(sent)
    }

    /// A write fails once Straum has closed the feed. What Straum said
    /// before it closed is the better failure.
    fn explained(&mut self, sent: Result<(), Error>) -> Result<(), Error> {
        let Err(e @ (Error::Io(_) | Error::Closed)) = sent else {
            return sent;
        };
        match frame::read(&mut self.reader, &mut self.buffer) {
            Ok(Frame::Failed(why)) => Err(Error::Straum(why)),
            _ => Err(e),
        }
    }
}
