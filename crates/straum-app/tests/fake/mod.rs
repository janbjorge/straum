//! PostgreSQL in memory: tables, a log of commits, and a source that loads
//! and streams facts the way a temporary slot and pgoutput would, each ref
//! resolved against the dims as they are at the commit.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::mem;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use straum_app::{Change, Commit, Lsn, Source, SourceError, Wal};
use straum_domain::{ColumnType, Datum, Fact, Ident, Spec, Store};

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

/// Sets the types a load sets: the fact's columns, then its attributes.
pub fn set_types(fact: &Fact, store: &mut Store) {
    let types: Vec<ColumnType> = (fact.columns().iter())
        .chain(fact.attrs().map(|(_, attr)| attr))
        .map(type_of)
        .collect();
    store
        .set_fact_types(fact.name.as_str(), &types)
        .expect("one type per column");
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
    /// What pgoutput would send for each commit: every table's changes,
    /// the fact rows unresolved.
    log: Vec<Commit>,
    /// The next call from the source fails, as if the connection dropped.
    pub drop_connection: bool,
    /// Asks for a commit that wait the whole wait and find none, before
    /// the next one is handed out.
    pub waits: usize,
    /// What reading the WAL returns instead of a slot that has confirmed
    /// every commit applied.
    pub wal: Option<Wal>,
    /// Every fact load, in order.
    pub loads: Vec<String>,
}

/// A source over a shared [`Database`], so a test can write while the app
/// reads.
pub struct Fake {
    db: Rc<RefCell<Database>>,
    spec: Option<Spec>,
    stream: Option<Stream>,
    pub on_load: Option<OnLoad>,
}

/// Called with the fact's name in the middle of each load.
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

    fn tables_at(&self, lsn: Lsn) -> &Tables {
        let (_, tables) = self.history.range(..=lsn).next_back().unwrap();
        tables
    }

    /// The fact's rows after every commit up to `lsn`, each with its
    /// attributes as the dims were then: what a load at `lsn` reads.
    pub fn rows_at(&self, spec: &Spec, lsn: Lsn, fact: &Fact) -> Vec<Row> {
        let tables = self.tables_at(lsn);
        let mut rows: Vec<Row> = tables[fact.name.as_str()].rows.values().cloned().collect();
        for row in &mut rows {
            resolve(tables, spec, fact, row);
        }
        rows
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

    /// A commit as straum's source hands it over: the facts' rows with
    /// their attributes as the dims are after the commit, and no change to
    /// a dim, which straum never follows.
    fn delivered(&self, spec: &Spec, commit: &Commit) -> Commit {
        let tables = self.tables_at(commit.lsn);
        let changes = (commit.changes.iter())
            .filter(|change| spec.dim(table_of(change).as_str()).is_none())
            .cloned()
            .map(|mut change| {
                if let Some(fact) = spec.fact(table_of(&change).as_str()) {
                    match &mut change {
                        Change::Insert { row, .. } | Change::Delete { old: row, .. } => {
                            resolve(tables, spec, fact, row);
                        }
                        Change::Update { old, new, .. } => {
                            resolve(tables, spec, fact, old);
                            resolve(tables, spec, fact, new);
                        }
                        Change::Truncate { .. } | Change::Altered { .. } => {}
                    }
                }
                change
            })
            .collect();
        Commit {
            lsn: commit.lsn,
            time: commit.time,
            changes,
        }
    }
}

fn table_of(change: &Change) -> &Ident {
    match change {
        Change::Insert { table, .. }
        | Change::Update { table, .. }
        | Change::Delete { table, .. }
        | Change::Truncate { table }
        | Change::Altered { table } => table,
    }
}

/// Appends the attributes of each ref to a row of `fact`, from the dim
/// row its via column names, or nulls when there is none. A row of the
/// wrong width is left for the store to refuse.
fn resolve(tables: &Tables, spec: &Spec, fact: &Fact, row: &mut Row) {
    let columns = fact.columns();
    if row.len() != columns.len() {
        return;
    }
    for r in &fact.refs {
        let dim = spec.dim(r.dim.as_str()).expect("the spec names the dim");
        let via = columns.iter().position(|c| *c == r.via[0]).unwrap();
        let found = match &row[via] {
            Datum::Null => None,
            key => tables[dim.table.name.as_str()].rows.get(key),
        };
        for attr in &r.attrs {
            let i = dim.columns().position(|c| c == attr).unwrap();
            row.push(found.map_or(Datum::Null, |d| d[i].clone()));
        }
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
            t.rows.insert(new_key, row.clone());
            if t.full {
                Change::Update {
                    table: id(table),
                    old: before,
                    new: row,
                }
            } else {
                Change::Altered { table: id(table) }
            }
        }
        Write::Delete { table, key } => {
            let t = tables.get_mut(table).unwrap();
            let before = t.rows.remove(&key).expect("deleted row exists");
            if t.full {
                Change::Delete {
                    table: id(table),
                    old: before,
                }
            } else {
                Change::Altered { table: id(table) }
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
            spec: None,
            stream: None,
            on_load: None,
        }
    }

    /// The fact's rows in the snapshot the stream starts from.
    fn read(&mut self, fact: &Fact) -> Result<Vec<Row>, SourceError> {
        let mut db = self.db.borrow_mut();
        db.connect()?;
        db.loads.push(fact.name.to_string());
        let start = self.stream.as_ref().expect("open first").start;
        let rows = db.rows_at(self.spec.as_ref().expect("open first"), start, fact);
        drop(db);
        if let Some(on_load) = &mut self.on_load {
            on_load(fact.name.as_str());
        }
        Ok(rows)
    }
}

impl Source for Fake {
    fn open(&mut self, spec: &Spec) -> Result<Lsn, SourceError> {
        let mut db = self.db.borrow_mut();
        db.connect()?;
        let start = db.lsn();
        self.spec = Some(spec.clone());
        self.stream = Some(Stream {
            start,
            next: db.log.len(),
            applied: start,
        });
        Ok(start)
    }

    fn load_fact(&mut self, fact: &Fact, store: &mut Store) -> Result<(), SourceError> {
        set_types(fact, store);
        for row in self.read(fact)? {
            store.add_fact_row(fact.name.as_str(), &row)?;
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
        let commit = db.log.get(stream.next).expect("a commit is waiting");
        let commit = db.delivered(self.spec.as_ref().expect("open first"), commit);
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
