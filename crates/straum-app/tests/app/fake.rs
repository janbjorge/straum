//! PostgreSQL in memory. A case writes to the `Database`: each commit
//! changes its tables and logs what pgoutput would send. The `Fake`
//! source reads the database as a slot would: it loads the tables as of
//! the stream's start, then hands out the logged commits one by one.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::mem;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use straum_app::{Change, Commit, Drift, Estimate, LoadCounts, Lsn, Source, SourceError, Wal};
use straum_domain::{ColumnType, Datum, Identifier, RowError, Spec, Store};

/// What the fake says a row of its tables takes.
const ROW_BYTES: u64 = 64;

/// A row as a table holds it.
pub type Row = Vec<Datum>;

/// A table's `REPLICA IDENTITY`: whether its updates and deletes send the
/// whole old row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    Full,
    Default,
}

/// A published table the database starts with, empty.
// Table names are always literals here, so `&'static str` needs no copy.
pub struct Published {
    pub name: &'static str,
    pub identity: Identity,
}

/// A column's type, as the catalog gives it: every table here names its
/// columns alike.
fn type_of(column: &Identifier) -> ColumnType {
    match column.as_str() {
        "at" => ColumnType::Time { zoned: true },
        "borough" | "rider" => ColumnType::Text,
        _ => ColumnType::Int,
    }
}

/// Sets the types a load sets: a fact's columns, or a dim's key and then
/// its attributes.
fn set_types(table: &Identifier, store: &mut Store) {
    let spec = store.spec();
    let types: Vec<ColumnType> = match (spec.fact(table.as_str()), spec.dim(table.as_str())) {
        (Some(fact), _) => fact.columns().iter().map(type_of).collect(),
        (None, Some(dim)) => dim.columns().map(type_of).collect(),
        (None, None) => panic!("{table} is in the spec"),
    };
    store
        .set_types(table.as_str(), &types)
        .expect("one type per column");
}

/// Loads a table's rows into the store, types first, as a load does.
fn fill(table: &Identifier, rows: &[Row], store: &mut Store) -> Result<(), RowError> {
    set_types(table, store);
    for row in rows {
        store.add_row(table.as_str(), row)?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct Table {
    identity: Identity,
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

/// The tables and the log of commits a case writes.
#[derive(Debug)]
pub struct Database {
    /// The tables after each commit, and before the first.
    history: BTreeMap<Lsn, Tables>,
    /// What pgoutput would send for each commit: every table's changes.
    log: Vec<Commit>,
    /// The next call from the source fails, as if the connection dropped.
    pub drop_connection: bool,
    /// Asks for a commit that wait the whole wait and find none, before
    /// the next one is handed out.
    pub waits: usize,
    /// What reading the WAL returns instead of one where Straum has applied
    /// every commit handed out.
    pub wal: Option<Wal>,
    /// Every load, dims and facts, in order.
    pub loads: Vec<String>,
    /// The estimates give no rows, as of a table never analysed.
    pub unanalysed: bool,
}

/// A source over a shared [`Database`], so a test can write while the app
/// reads.
pub struct Fake {
    database: Rc<RefCell<Database>>,
    stream: Option<Stream>,
    pub on_load: Option<OnLoad>,
}

/// Called with the fact's name in the middle of each load, half its rows
/// counted.
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

/// A name the tests know is valid.
pub fn id(name: &str) -> Identifier {
    Identifier::new(name).unwrap()
}

impl Database {
    /// The published tables, empty.
    pub fn new(tables: &[Published]) -> Database {
        let mut empty = Tables::new();
        for table in tables {
            let rows = BTreeMap::new();
            let identity = table.identity;
            empty.insert(table.name, Table { identity, rows });
        }
        Database {
            history: BTreeMap::from([(Lsn(100), empty)]),
            log: Vec::new(),
            drop_connection: false,
            waits: 0,
            wal: None,
            loads: Vec::new(),
            unanalysed: false,
        }
    }

    /// The database, to share between a case and its fake source.
    pub fn shared(self) -> Rc<RefCell<Database>> {
        Rc::new(RefCell::new(self))
    }

    /// The LSN of the last commit.
    pub fn lsn(&self) -> Lsn {
        *self.history.keys().next_back().unwrap()
    }

    fn tables_at(&self, lsn: Lsn) -> &Tables {
        let (_, tables) = self.history.range(..=lsn).next_back().unwrap();
        tables
    }

    /// A table's rows after every commit up to `lsn`: what a load at
    /// `lsn` reads. A dim row starts with its key, as [`Dim::columns`].
    fn rows_at(&self, lsn: Lsn, table: &str) -> Vec<Row> {
        let tables = self.tables_at(lsn);
        tables[table].rows.values().cloned().collect()
    }

    /// A store loaded from the tables as they were at `lsn`, dims first,
    /// as the writer loads them.
    pub fn store_at(&self, spec: &Spec, lsn: Lsn) -> Store {
        let mut store = Store::new(spec.clone());
        for dim in spec.dims() {
            self.load_at(lsn, &dim.name, &mut store);
        }
        for fact in spec.facts() {
            self.load_at(lsn, &fact.name, &mut store);
        }
        store
    }

    fn load_at(&self, lsn: Lsn, table: &Identifier, store: &mut Store) {
        let rows = self.rows_at(lsn, table.as_str());
        fill(table, &rows, store).expect("a row the store takes");
    }

    /// The primary keys a table holds now.
    pub fn keys(&self, table: &str) -> Vec<Datum> {
        let tables = self.history.values().next_back().unwrap();
        tables[table].rows.keys().cloned().collect()
    }

    /// Commits the writes and logs what pgoutput would send for them.
    pub fn commit(&mut self, writes: Vec<Write>) -> Lsn {
        let mut tables = self.history.values().next_back().unwrap().clone();
        let mut changes = Vec::new();
        for write in writes {
            changes.push(apply_write(&mut tables, write));
        }
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
            committed: time(lsn),
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

fn apply_write(tables: &mut Tables, write: Write) -> Change {
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
            let held = tables.get_mut(table).unwrap();
            let before = held.rows.remove(&key).expect("updated row exists");
            held.rows.insert(new_key, row.clone());
            if held.identity == Identity::Full {
                Change::Update {
                    table: id(table),
                    old: before,
                    new: row,
                }
            } else {
                Change::Unfit {
                    table: id(table),
                    drift: Drift::Altered,
                }
            }
        }
        Write::Delete { table, key } => {
            let held = tables.get_mut(table).unwrap();
            let before = held.rows.remove(&key).expect("deleted row exists");
            if held.identity == Identity::Full {
                Change::Delete {
                    table: id(table),
                    old: before,
                }
            } else {
                Change::Unfit {
                    table: id(table),
                    drift: Drift::Altered,
                }
            }
        }
        Write::Truncate { table } => {
            tables.get_mut(table).unwrap().rows.clear();
            Change::Truncate { table: id(table) }
        }
        Write::Alter { table } => Change::Unfit {
            table: id(table),
            drift: Drift::Altered,
        },
    }
}

impl Fake {
    /// A source over the database, with no stream open yet.
    pub fn new(database: &Rc<RefCell<Database>>) -> Fake {
        Fake {
            database: Rc::clone(database),
            stream: None,
            on_load: None,
        }
    }

    /// A table's rows in the snapshot the stream starts from, counted
    /// into `counts` half before the load's middle and half after.
    fn read(&mut self, table: &Identifier, counts: &LoadCounts) -> Result<Vec<Row>, SourceError> {
        let mut database = self.database.borrow_mut();
        database.connect()?;
        database.loads.push(table.to_string());
        let start = self.stream.as_ref().expect("open first").start;
        let rows = database.rows_at(start, table.as_str());
        drop(database);
        let half = rows.len() as u64 / 2;
        counts.rows.fetch_add(half, Ordering::Relaxed);
        if let Some(on_load) = &mut self.on_load {
            on_load(table.as_str());
        }
        counts
            .rows
            .fetch_add(rows.len() as u64 - half, Ordering::Relaxed);
        Ok(rows)
    }
}

impl Source for Fake {
    fn open(&mut self, _spec: &Spec) -> Result<Lsn, SourceError> {
        let mut database = self.database.borrow_mut();
        database.connect()?;
        let start = database.lsn();
        self.stream = Some(Stream {
            start,
            next: database.log.len(),
            applied: start,
        });
        Ok(start)
    }

    /// The table's rows now, 64 bytes each.
    fn estimate(&mut self, tables: &[Identifier]) -> Result<Estimate, SourceError> {
        let mut database = self.database.borrow_mut();
        database.connect()?;
        let start = self.stream.as_ref().expect("open first").start;
        let rows = database.rows_at(start, tables[0].as_str()).len() as u64;
        let estimated = if database.unanalysed {
            None
        } else {
            Some(rows)
        };
        Ok(Estimate {
            rows: estimated,
            bytes: rows * ROW_BYTES,
        })
    }

    /// Views of one table read it once, as one copy does.
    fn load(
        &mut self,
        tables: &[Identifier],
        store: &mut Store,
        counts: &LoadCounts,
    ) -> Result<(), SourceError> {
        let uncounted = LoadCounts::default();
        for (index, table) in tables.iter().enumerate() {
            let counting = if index == 0 { counts } else { &uncounted };
            let rows = self.read(table, counting)?;
            fill(table, &rows, store)?;
        }
        Ok(())
    }

    fn next_commit(&mut self, wait: Duration) -> Result<Option<Commit>, SourceError> {
        let mut database = self.database.borrow_mut();
        database.connect()?;
        if database.waits > 0 {
            database.waits -= 1;
            thread::sleep(wait);
            return Ok(None);
        }
        let stream = self.stream.as_mut().expect("open first");
        // Every published table's changes, as pgoutput sends them; the
        // writer skips the ones outside the spec.
        let commit = database
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
        let mut database = self.database.borrow_mut();
        database.connect()?;
        let applied = self.stream.as_ref().expect("open first").applied;
        Ok(database.wal.unwrap_or(Wal {
            written: database.lsn(),
            applied,
            restart: applied,
        }))
    }
}
