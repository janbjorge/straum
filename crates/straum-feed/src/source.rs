//! [`Feed`], the [`Source`] over a Unix socket: a feeder connects, Straum
//! asks it for each table, and nothing changes after.

use std::fs;
use std::io::{self, BufReader};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use straum_app::{Commit, Lsn, Source, SourceError, Wal};
use straum_domain::{ColumnType, Identifier, Spec, Store, Table};

use crate::error::Error;
use crate::frame::{self, Frame};

/// A feed has no WAL: every row it will ever carry is in the load.
const START: Lsn = Lsn(0);

/// Straum's [`Source`] over a feeder.
pub struct Feed {
    listener: UnixListener,
    path: PathBuf,
    /// The feeder of the last [`Source::open`], until it has fed every
    /// table.
    loading: Option<Loading>,
}

struct Loading {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    spec: Spec,
    /// Tables not loaded yet. The feeder is told Straum is done with the
    /// last one.
    unloaded: Vec<Identifier>,
    buffer: Vec<u8>,
}

/// A spec table as a load asks for it.
struct Asked<'a> {
    table: &'a Table,
    columns: Vec<Identifier>,
    /// Where the fact's time column is among `columns`, and the type its
    /// `date_bin` origin needs there.
    time: Option<(usize, ColumnType)>,
}

impl Feed {
    /// Listens on a new socket at `path`, which only the owner may connect
    /// to. A socket nothing listens on any more is replaced; anything else
    /// at `path` is left alone.
    pub fn bind(path: impl AsRef<Path>) -> Result<Feed, Error> {
        let path = path.as_ref().to_owned();
        let listener = match UnixListener::bind(&path) {
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && stale(&path) => {
                fs::remove_file(&path)?;
                UnixListener::bind(&path)?
            }
            listener => listener?,
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(Feed {
            listener,
            path,
            loading: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Whether `path` is a socket a Straum that has stopped left behind.
fn stale(path: &Path) -> bool {
    let socket = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket());
    socket && UnixStream::connect(path).is_err()
}

impl Loading {
    fn send(&mut self, frame: &Frame) -> Result<(), Error> {
        frame::write(&mut self.writer, frame, &mut self.buffer)
    }

    fn receive(&mut self) -> Result<Frame, Error> {
        match frame::read(&mut self.reader, &mut self.buffer)? {
            Frame::Failed(why) => Err(Error::Feeder(why)),
            frame => Ok(frame),
        }
    }

    fn asked(&self, name: &Identifier) -> Asked<'_> {
        let name = name.as_str();
        if let Some(fact) = self.spec.fact(name) {
            let columns = fact.columns();
            let time = columns.iter().position(|c| *c == fact.time.column);
            return Asked {
                table: &fact.table,
                time: time.map(|at| (at, fact.time.origin.column_type())),
                columns,
            };
        }
        let dim = self.spec.dim(name).expect("the writer loads spec tables");
        Asked {
            table: &dim.table,
            columns: dim.columns().cloned().collect(),
            time: None,
        }
    }

    fn load(&mut self, name: &Identifier, store: &mut Store) -> Result<(), Error> {
        let Asked {
            table,
            columns,
            time,
        } = self.asked(name);
        let (table, width) = (table.clone(), columns.len());
        self.send(&Frame::Load {
            table: table.clone(),
            columns: columns.clone(),
        })?;
        let Frame::Types(types) = self.receive()? else {
            return Err(Error::protocol("rows before their types"));
        };
        if types.len() != width {
            return Err(Error::Types(format!(
                "{name}: the feeder gave {} types for the {width} columns of {}.{}",
                types.len(),
                table.schema,
                table.name
            )));
        }
        if let Some((at, needed)) = time
            && types[at] != needed
        {
            return Err(Error::Types(format!(
                "{name}: column {} of {}.{} is {}; its date_bin origin needs {needed} there",
                columns[at], table.schema, table.name, types[at]
            )));
        }
        store.set_types(name.as_str(), &types).map_err(Error::Row)?;
        loop {
            match self.receive()? {
                Frame::Rows(rows) => store.add_rows(name.as_str(), &rows).map_err(Error::Row)?,
                Frame::End => return Ok(()),
                _ => return Err(Error::protocol("a frame that is no row of the table")),
            }
        }
    }
}

impl Source for Feed {
    /// Waits for a feeder to connect, however long that takes.
    fn open(&mut self, spec: &Spec) -> Result<Lsn, SourceError> {
        // The old feeder goes first: it reads the close and stops.
        self.loading = None;
        let (stream, _) = self.listener.accept().map_err(Error::from)?;
        let mut loading = Loading {
            reader: BufReader::new(stream.try_clone().map_err(Error::from)?),
            writer: stream,
            spec: spec.clone(),
            unloaded: (spec.dims().iter().map(|dim| &dim.name))
                .chain(spec.facts().iter().map(|fact| &fact.name))
                .cloned()
                .collect(),
            buffer: Vec::new(),
        };
        let Frame::Hello = loading.receive()? else {
            return Err(Error::protocol("a first frame that is no hello").into());
        };
        self.loading = Some(loading);
        Ok(START)
    }

    fn load(&mut self, table: &Identifier, store: &mut Store) -> Result<(), SourceError> {
        let loading = self.loading.as_mut().ok_or(Error::NotLoading)?;
        if let Err(e) = loading.load(table, store) {
            // The feeder may be gone, and has its own failure then.
            let _ = loading.send(&Frame::Failed(e.to_string()));
            self.loading = None;
            return Err(e.into());
        }
        loading.unloaded.retain(|t| t != table);
        if loading.unloaded.is_empty() {
            let done = loading.send(&Frame::Done);
            self.loading = None;
            done?;
        }
        Ok(())
    }

    /// Never a commit: a feed carries loads only.
    fn next_commit(&mut self, wait: Duration) -> Result<Option<Commit>, SourceError> {
        thread::sleep(wait);
        Ok(None)
    }

    fn wal(&mut self) -> Result<Wal, SourceError> {
        Ok(Wal {
            written: START,
            confirmed: START,
            restart: START,
        })
    }
}
