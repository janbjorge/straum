//! The physical stream's connection: a temporary physical slot, which keeps
//! WAL from the last checkpoint's redo point, and the WAL it streams, read
//! as records (`wal.rs`) and turned into commits (`wal/pipeline.rs`). The
//! stream carries the whole server's WAL, every database. Also the layouts
//! query: how each spec table's rows lie on its pages, from `pg_class`,
//! `pg_attribute` and `pg_enum`.

use std::collections::HashMap;
use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant};

use postgres_protocol::escape::escape_literal;
use straum_app::{Commit, Lsn};
use straum_domain::Datum;

use crate::catalog::{Checked, VERSION_NUM_PER_MAJOR};
use crate::dsn::Dsn;
use crate::error::Error;
use crate::pgoutput::{Relation, RelationColumn};
use crate::source::{parse_lsn, slot_name};
use crate::stream::POLL;
use crate::unexpected::Unexpected;
use crate::value::{Oid, read, type_of};
use crate::wal::messages::{Layout, Layouts};
use crate::wal::pipeline::Pipeline;
use crate::wal::tuple::{Attribute, Kind};
use crate::wal::{Records, RelationFile};
use crate::wire::{Connection, ConnectionKind, first_row_value};

/// How long a start waits for PostgreSQL to flush the WAL up to the redo
/// point, which a checkpoint in progress sets where WAL is still being
/// written.
const FLUSH_WAIT: Duration = Duration::from_secs(10);

/// How often a start asks how far PostgreSQL has flushed the WAL.
const FLUSH_POLL: Duration = Duration::from_millis(10);

/// A temporary physical slot streaming from its redo point, before the
/// stream's thread takes it over.
pub(crate) struct Physical {
    connection: Connection,
    slot: String,
    records: Records,
    start: u64,
    major: u32,
}

impl Physical {
    /// Creates the slot and starts streaming. Returns the LSN of the first
    /// record, the redo point: every page changed since then comes with a
    /// full image the first time.
    pub(crate) fn start(dsn: &Dsn) -> Result<(Physical, Lsn), Error> {
        let mut connection = Connection::connect(dsn, ConnectionKind::Replication)?;
        let slot = slot_name();
        connection.execute(&format!(
            "CREATE_REPLICATION_SLOT {slot} TEMPORARY PHYSICAL (RESERVE_WAL)"
        ))?;
        let reserved = reserved(&mut connection, &slot)?;
        let records = Records::new(reserved.start.0, reserved.segment_size);
        wait_for_flush(&mut connection, reserved.start)?;
        connection.copy_both(&format!(
            "START_REPLICATION SLOT {slot} PHYSICAL {}",
            Lsn(records.wanted())
        ))?;
        connection.set_read_timeout(POLL)?;
        let physical = Physical {
            connection,
            slot,
            records,
            start: reserved.start.0,
            major: reserved.major,
        };
        Ok((physical, reserved.start))
    }

    /// The next record, or `None` if none came within the read timeout,
    /// for tests that read the WAL without a stream's thread.
    #[cfg(test)]
    pub(crate) fn next_record(&mut self) -> Result<Option<crate::wal::Record>, Error> {
        use postgres_protocol::message::backend::Message as Backend;

        use crate::pgoutput::{self, CopyMessage};
        use crate::stream::status_update;
        use crate::wire::server_error;
        loop {
            if let Some(record) = self.records.next_record()? {
                return Ok(Some(record.clone()));
            }
            match self.connection.try_read()? {
                None => return Ok(None),
                Some(Backend::CopyData(body)) => match pgoutput::copy_message(body.data())? {
                    CopyMessage::XLogData { start, bytes } => self.records.feed(start, bytes)?,
                    CopyMessage::Keepalive { reply, .. } => {
                        if reply {
                            self.connection.send_copy_data(&status_update(self.start))?;
                        }
                    }
                },
                Some(Backend::ErrorResponse(body)) => return Err(server_error(&body)),
                Some(_) => return Err(Error::Protocol(Unexpected::StreamEnded)),
            }
        }
    }

    /// The slot's name.
    pub(crate) fn slot(&self) -> &str {
        &self.slot
    }

    /// The server's major version, which says how some records read.
    pub(crate) fn major(&self) -> u32 {
        self.major
    }

    /// The replication connection, and the rest of the stream for its
    /// thread to read.
    pub(crate) fn into_replay(
        self,
        pipeline: Pipeline,
        catalog: Connection,
        checked: Vec<Checked>,
    ) -> (Connection, Replay) {
        let replay = Replay {
            records: self.records,
            pipeline,
            catalog,
            checked,
            read: self.start,
        };
        (self.connection, replay)
    }
}

/// The physical stream after the load: WAL read into commits on the
/// stream's thread, with a plain connection to read layouts again.
pub(crate) struct Replay {
    records: Records,
    pub(crate) pipeline: Pipeline,
    catalog: Connection,
    checked: Vec<Checked>,
    /// Every record before here is read.
    read: u64,
}

/// What a new physical slot reserved, on which server.
struct Reserved {
    /// The redo point, where the slot keeps WAL from.
    start: Lsn,
    segment_size: u64,
    major: u32,
}

fn reserved(connection: &mut Connection, slot: &str) -> Result<Reserved, Error> {
    // The command's consistent point is a logical slot's; a physical
    // slot reserves from the redo point, which only the view shows.
    let rows = connection.rows(&format!(
        "SELECT restart_lsn, setting, current_setting('server_version_num') \
         FROM pg_replication_slots, pg_settings \
         WHERE slot_name = {} AND name = 'wal_segment_size'",
        escape_literal(slot)
    ))?;
    let start = (first_row_value(&rows, 0).and_then(parse_lsn))
        .ok_or(Error::Protocol(Unexpected::SlotWithoutRestartLsn))?;
    let segment_size = (first_row_value(&rows, 1).map(str::parse::<u64>))
        .and_then(Result::ok)
        .ok_or(Error::Protocol(Unexpected::NoWalSegmentSize))?;
    let version = (first_row_value(&rows, 2).map(str::parse::<u32>))
        .and_then(Result::ok)
        .ok_or(Error::Protocol(Unexpected::NoServerVersion))?;
    Ok(Reserved {
        start,
        segment_size,
        major: version / VERSION_NUM_PER_MAJOR,
    })
}

/// The slot keeps WAL from the redo point (`GetRedoRecPtr`), which a
/// checkpoint in progress may have set past what PostgreSQL has flushed,
/// and the server streams nothing it hasn't flushed.
fn wait_for_flush(connection: &mut Connection, start: Lsn) -> Result<(), Error> {
    let deadline = Instant::now() + FLUSH_WAIT;
    loop {
        let rows = connection.rows("SELECT pg_current_wal_flush_lsn()")?;
        let flushed = (first_row_value(&rows, 0).and_then(parse_lsn))
            .ok_or(Error::Protocol(Unexpected::NoWalFlushPosition))?;
        if flushed.0 >= start.0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::Protocol(Unexpected::WalNeverFlushed));
        }
        thread::sleep(FLUSH_POLL);
    }
}

impl Replay {
    /// Reads `bytes` of WAL from `start`: the commits they finish.
    pub(crate) fn feed(&mut self, start: u64, bytes: &[u8]) -> Result<Vec<Commit>, Error> {
        self.records.feed(start, bytes)?;
        let mut commits = Vec::new();
        while let Some(record) = self.records.next_record()? {
            commits.extend(
                self.pipeline
                    .record(record, || read_layouts(&mut self.catalog, &self.checked))?,
            );
            self.read = record.next();
        }
        Ok(commits)
    }

    /// The LSN before which every record is read.
    pub(crate) fn read(&self) -> u64 {
        self.read
    }
}

/// The layout of each of `checked`'s tables, and their enums' labels.
pub(crate) fn read_layouts(
    connection: &mut Connection,
    checked: &[Checked],
) -> Result<Layouts, Error> {
    let names = table_names(checked);
    // A dropped column has no type, and its bytes stay in older rows, so
    // the join to pg_type keeps it. A missing value is a one-element
    // array of the column's type.
    let rows = connection.rows(&format!(
        "SELECT c.oid, n.nspname, c.relname,
                CASE c.reltablespace WHEN 0 THEN d.dattablespace ELSE c.reltablespace END,
                d.oid, pg_relation_filenode(c.oid), a.attname, a.atttypid, a.atttypmod,
                a.attlen,
                CASE a.attalign WHEN 'c' THEN 1 WHEN 's' THEN 2 WHEN 'i' THEN 4 ELSE 8 END,
                a.attisdropped, (a.attmissingval::text::text[])[1], t.typtype = 'e'
         FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace
         JOIN pg_database d ON d.datname = current_database()
         JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0
         LEFT JOIN pg_type t ON t.oid = a.atttypid
         WHERE (n.nspname, c.relname) IN ({names})
         ORDER BY c.oid, a.attnum"
    ))?;
    let mut tables: Vec<Layout> = Vec::new();
    for row in &rows {
        let attribute = AttributeRow::parse(row)?;
        let is_next_table = match tables.last() {
            Some(table) => table.relation.id != attribute.table_oid,
            None => true,
        };
        if is_next_table {
            tables.push(attribute.empty_table());
        }
        let table = tables.last_mut().ok_or_else(unknown_shape)?;
        push_attribute(table, &attribute)?;
    }
    let labels = labels(connection, &names)?;
    Ok(Layouts { tables, labels })
}

/// The `(schema, table)` pairs of `checked`, as an SQL list.
fn table_names(checked: &[Checked]) -> String {
    let mut names = Vec::new();
    for table in checked {
        names.push(format!(
            "({}, {})",
            escape_literal(table.schema.as_str()),
            escape_literal(table.table_name.as_str())
        ));
    }
    names.join(", ")
}

/// A row of the layouts query: one attribute of a table, dropped ones
/// too.
// 'a: borrows the query's text for one row, read once.
struct AttributeRow<'a> {
    table_oid: Oid,
    schema: &'a str,
    table: &'a str,
    file: RelationFile,
    name: &'a str,
    type_oid: Oid,
    typmod: i32,
    length: i16,
    align: usize,
    dropped: bool,
    missing: Option<&'a str>,
    is_enum: bool,
}

impl<'a> AttributeRow<'a> {
    /// Reads the query's columns, in its order.
    fn parse(row: &'a [Option<String>]) -> Result<AttributeRow<'a>, Error> {
        Ok(AttributeRow {
            table_oid: parsed(row, 0)?,
            schema: text(row, 1)?,
            table: text(row, 2)?,
            file: RelationFile {
                tablespace: parsed(row, 3)?,
                database: parsed(row, 4)?,
                relation: parsed(row, 5)?,
            },
            name: text(row, 6)?,
            type_oid: parsed(row, 7)?,
            typmod: parsed(row, 8)?,
            length: parsed(row, 9)?,
            align: parsed(row, 10)?,
            dropped: text(row, 11)? == "t",
            missing: row.get(12).and_then(Option::as_deref),
            is_enum: matches!(row.get(13), Some(Some(value)) if value == "t"),
        })
    }

    /// The attribute's table, with no attributes yet.
    fn empty_table(&self) -> Layout {
        Layout {
            file: self.file,
            relation: Relation {
                id: self.table_oid,
                schema: self.schema.to_owned(),
                name: self.table.to_owned(),
                columns: Vec::new(),
            },
            attributes: Vec::new(),
            columns: Vec::new(),
            missing: Vec::new(),
        }
    }
}

/// Adds `attribute` to `table`, and its column unless it was dropped.
fn push_attribute(table: &mut Layout, attribute: &AttributeRow) -> Result<(), Error> {
    // As the decoder reads the column: an enum as its label's text.
    let kind = match type_of(attribute.type_oid, attribute.typmod) {
        _ if attribute.dropped => Kind::Other,
        Some(column_type) => Kind::Read(column_type),
        None if attribute.is_enum => Kind::Enum,
        None => Kind::Other,
    };
    if !attribute.dropped {
        table.columns.push(table.attributes.len());
        table.relation.columns.push(RelationColumn {
            name: attribute.name.to_owned(),
            type_oid: attribute.type_oid,
            typmod: attribute.typmod,
        });
    }
    table.attributes.push(Attribute {
        length: attribute.length,
        align: attribute.align,
        kind,
    });
    table.missing.push(match (attribute.missing, kind) {
        (Some(text), Kind::Read(column_type)) => read(column_type, text)?,
        (Some(text), Kind::Enum) => Datum::Text(text.into()),
        _ => Datum::Null,
    });
    Ok(())
}

/// Each enum label's text by its `pg_enum` OID.
type LabelsByOid = HashMap<Oid, String>;

/// The labels of the enums the named tables' columns have, by `pg_enum`
/// OID, which is what a row stores.
fn labels(connection: &mut Connection, names: &str) -> Result<LabelsByOid, Error> {
    let rows = connection.rows(&format!(
        "SELECT DISTINCT e.oid, e.enumlabel
         FROM pg_enum e
         JOIN pg_attribute a ON a.atttypid = e.enumtypid
         JOIN pg_class c ON c.oid = a.attrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE (n.nspname, c.relname) IN ({names})"
    ))?;
    let mut labels = HashMap::new();
    for row in rows {
        let [Some(oid), Some(label)] = row.as_slice() else {
            return Err(unknown_shape());
        };
        labels.insert(parsed_text(oid)?, label.clone());
    }
    Ok(labels)
}

fn unknown_shape() -> Error {
    Error::Protocol(Unexpected::LayoutShape)
}

fn text(row: &[Option<String>], column: usize) -> Result<&str, Error> {
    let Some(Some(text)) = row.get(column) else {
        return Err(unknown_shape());
    };
    Ok(text)
}

// T: each column parses to its own number type.
fn parsed<T: FromStr>(row: &[Option<String>], column: usize) -> Result<T, Error> {
    parsed_text(text(row, column)?)
}

fn parsed_text<T: FromStr>(text: &str) -> Result<T, Error> {
    text.parse().map_err(|_| unknown_shape())
}
