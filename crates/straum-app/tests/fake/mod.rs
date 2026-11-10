//! PostgreSQL in memory: tables, a log of commits, and a source that loads
//! and streams them the way a temporary slot and pgoutput would.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::mem;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use straum_app::{Change, Commit, Lsn, Old, Source, SourceError, Wal};
use straum_domain::{ColumnType, Datum, Dim, DimTable, Fact, Ident, Rollup, Spec};

pub type Row = Vec<Datum>;

/// A column's type, as the catalog gives it: every table here names its
/// columns alike.
pub fn type_of(column: &Ident) -> ColumnType {
    match column.as_str() {
        "at" => ColumnType::Time { zoned: true },
        "borough" | "rider" => ColumnType::Text,
        _ => ColumnType::Int,
    }
}

/// Sets the types a load sets.
pub fn set_types(rollup: &mut Rollup) {
    let types: Vec<ColumnType> = rollup.columns().iter().map(type_of).collect();
    rollup.set_types(&types).expect("one type per column");
}

/// As [`set_types`], for a dim.
pub fn set_dim_types(dim: &Dim, table: &mut DimTable) {
    let types: Vec<ColumnType> = dim.columns().map(type_of).collect();
    table.set_types(&types).expect("one type per column");
}

#[derive(Debug, Clone)]
struct Table {
    /// `REPLICA IDENTITY FULL`: updates and deletes send the whole old row.
    full: bool,
    /// By primary key. A dim row starts with its key.
    rows: BTreeMap<Datum, Row>,
}

type Tables = BTreeMap<&'static str, Table>;

/// One write in a transaction, by primary key.
#[derive(Debug, Clone)]
pub enum Write {
    Insert {
        table: &'static str,
        key: Datum,
        row: Row,
    },
    Update {
        table: &'static str,
        key: Datum,
        new_key: Datum,
        row: Row,
    },
    Delete {
        table: &'static str,
        key: Datum,
    },
    Truncate {
        table: &'static str,
    },
    /// An `ALTER TABLE`: no rows change, pgoutput sends a new Relation.
    Alter {
        table: &'static str,
    },
}

#[derive(Debug)]
pub struct Database {
    /// The tables after each commit, and before the first.
    history: BTreeMap<Lsn, Tables>,
    log: Vec<Commit>,
    /// The next call from the source fails, as if the connection dropped.
    pub drop_connection: bool,
    /// Asks for a commit that wait the whole wait and find none, before
    /// the next one is handed out.
    pub waits: usize,
    /// What reading the WAL returns instead of a slot that has confirmed
    /// every commit applied.
    pub wal: Option<Wal>,
    /// Every table load, in order.
    pub loads: Vec<String>,
}

/// A source over a shared [`Database`], so a test can write while the app
/// reads.
pub struct Fake {
    db: Rc<RefCell<Database>>,
    stream: Option<Stream>,
    pub on_load: Option<OnLoad>,
}

/// Called with the table's name in the middle of each load.
pub type OnLoad = Box<dyn FnMut(&str)>;

#[derive(Debug)]
struct Stream {
    start: Lsn,
    /// Index of the next commit in the log.
    next: usize,
    /// The last commit handed out, or the start.
    applied: Lsn,
}

/// The fake's clock: a commit at LSN `n` commits `n` seconds after the
/// epoch.
pub fn time(lsn: Lsn) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(lsn.0)
}

pub fn id(s: &str) -> Ident {
    Ident::new(s).unwrap()
}

impl Database {
    /// Empty tables, each with whether it has `REPLICA IDENTITY FULL`.
    pub fn new(tables: &[(&'static str, bool)]) -> Database {
        let empty = tables
            .iter()
            .map(|&(name, full)| {
                let rows = BTreeMap::new();
                (name, Table { full, rows })
            })
            .collect();
        Database {
            history: BTreeMap::from([(Lsn(100), empty)]),
            log: Vec::new(),
            drop_connection: false,
            waits: 0,
            wal: None,
            loads: Vec::new(),
        }
    }

    pub fn shared(self) -> Rc<RefCell<Database>> {
        Rc::new(RefCell::new(self))
    }

    pub fn lsn(&self) -> Lsn {
        *self.history.keys().next_back().unwrap()
    }

    /// The table's rows after every commit up to `lsn`.
    pub fn rows_at(&self, lsn: Lsn, table: &str) -> Vec<Row> {
        let (_, tables) = self.history.range(..=lsn).next_back().unwrap();
        tables[table].rows.values().cloned().collect()
    }

    pub fn keys(&self, table: &str) -> Vec<Datum> {
        let tables = self.history.values().next_back().unwrap();
        tables[table].rows.keys().cloned().collect()
    }

    /// Commits the writes and logs what pgoutput would send for them.
    pub fn commit(&mut self, writes: Vec<Write>) -> Lsn {
        let mut tables = self.history.values().next_back().unwrap().clone();
        let changes = writes.into_iter().map(|w| write(&mut tables, w)).collect();
        self.push(changes, tables)
    }

    /// Logs a commit with these changes and leaves the tables alone, as a
    /// faulty source might.
    pub fn send(&mut self, changes: Vec<Change>) -> Lsn {
        let tables = self.history.values().next_back().unwrap().clone();
        self.push(changes, tables)
    }

    fn push(&mut self, changes: Vec<Change>, tables: Tables) -> Lsn {
        let lsn = Lsn(self.lsn().0 + 10);
        self.log.push(Commit {
            lsn,
            time: time(lsn),
            changes,
        });
        self.history.insert(lsn, tables);
        lsn
    }

    fn connect(&mut self) -> Result<(), SourceError> {
        if mem::take(&mut self.drop_connection) {
            return Err("connection dropped".into());
        }
        Ok(())
    }
}

fn write(tables: &mut Tables, write: Write) -> Change {
    match write {
        Write::Insert { table, key, row } => {
            tables.get_mut(table).unwrap().rows.insert(key, row.clone());
            Change::Insert {
                table: id(table),
                row,
            }
        }
        Write::Update {
            table,
            key,
            new_key,
            row,
        } => {
            let t = tables.get_mut(table).unwrap();
            let before = t.rows.remove(&key).expect("updated row exists");
            t.rows.insert(new_key.clone(), row.clone());
            let old = if t.full {
                Old::Row(before)
            } else if new_key != key {
                Old::Key(vec![key])
            } else {
                Old::SameKey
            };
            Change::Update {
                table: id(table),
                old,
                new: row,
            }
        }
        Write::Delete { table, key } => {
            let t = tables.get_mut(table).unwrap();
            let before = t.rows.remove(&key).expect("deleted row exists");
            let old = if t.full {
                Old::Row(before)
            } else {
                Old::Key(vec![key])
            };
            Change::Delete {
                table: id(table),
                old,
            }
        }
        Write::Truncate { table } => {
            tables.get_mut(table).unwrap().rows.clear();
            Change::Truncate { table: id(table) }
        }
        Write::Alter { table } => Change::Altered { table: id(table) },
    }
}

impl Fake {
    pub fn new(db: &Rc<RefCell<Database>>) -> Fake {
        Fake {
            db: Rc::clone(db),
            stream: None,
            on_load: None,
        }
    }

    /// The table's rows in the snapshot the stream starts from.
    fn read(&mut self, table: &Ident) -> Result<Vec<Row>, SourceError> {
        let mut db = self.db.borrow_mut();
        db.connect()?;
        db.loads.push(table.to_string());
        let start = self.stream.as_ref().expect("open first").start;
        let rows = db.rows_at(start, table.as_str());
        drop(db);
        if let Some(on_load) = &mut self.on_load {
            on_load(table.as_str());
        }
        Ok(rows)
    }
}

impl Source for Fake {
    fn open(&mut self, _spec: &Spec) -> Result<Lsn, SourceError> {
        let mut db = self.db.borrow_mut();
        db.connect()?;
        let start = db.lsn();
        self.stream = Some(Stream {
            start,
            next: db.log.len(),
            applied: start,
        });
        Ok(start)
    }

    fn load_fact(&mut self, fact: &Fact, rollup: &mut Rollup) -> Result<(), SourceError> {
        set_types(rollup);
        for row in self.read(&fact.name)? {
            rollup.add(&row)?;
        }
        Ok(())
    }

    fn load_dim(&mut self, dim: &Dim, table: &mut DimTable) -> Result<(), SourceError> {
        set_dim_types(dim, table);
        for row in self.read(&dim.name)? {
            table.upsert(&row)?;
        }
        Ok(())
    }

    fn next_commit(&mut self, wait: Duration) -> Result<Option<Commit>, SourceError> {
        let mut db = self.db.borrow_mut();
        db.connect()?;
        if db.waits > 0 {
            db.waits -= 1;
            thread::sleep(wait);
            return Ok(None);
        }
        let stream = self.stream.as_mut().expect("open first");
        let commit = db
            .log
            .get(stream.next)
            .expect("a commit is waiting")
            .clone();
        stream.next += 1;
        // The writer applies it before it reads the WAL again.
        stream.applied = commit.lsn;
        Ok(Some(commit))
    }

    fn wal(&mut self) -> Result<Wal, SourceError> {
        let mut db = self.db.borrow_mut();
        db.connect()?;
        let applied = self.stream.as_ref().expect("open first").applied;
        Ok(db.wal.unwrap_or(Wal {
            server: db.lsn(),
            confirmed: applied,
            restart: applied,
        }))
    }
}
