//! The physical stream's connection: a temporary physical slot, which keeps
//! WAL from the last checkpoint's redo point, and the WAL it streams, read
//! as records (`wal.rs`) and turned into commits (`wal/pipeline.rs`). The
//! stream carries the whole server's WAL, every database. Also the layouts
//! query: how each spec table's rows lie on its pages, from `pg_class` and
//! `pg_attribute`, with the enum labels `catalog/query.rs` reads. After DDL the
//! query waits for a snapshot that sees the DDL's commit.

use std::collections::HashMap;
use std::io;
use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant};

use postgres_protocol::escape::escape_literal;
use straum_app::{Commit, Lsn};
use straum_domain::Datum;

use crate::catalog::{self, Checked, VERSION_NUM_PER_MAJOR};
use crate::dsn::Dsn;
use crate::error::Error;
use crate::pgoutput::{Relation, RelationColumn};
use crate::problem::WaitProblem;
use crate::source::{parse_lsn, slot_name, waited_for};
use crate::stream::POLL;
use crate::unexpected::Unexpected;
use crate::value::{Oid, read, type_of};
use crate::wal::messages::{Layout, Layouts};
use crate::wal::pipeline::Pipeline;
use crate::wal::transactions::TransactionSnapshot;
use crate::wal::tuple::{Attribute, Kind, Label};
use crate::wal::{Records, RelationFile, TransactionId};
use crate::wire::{Closer, Connection, ConnectionKind, first_row_value};

/// How long a start waits for PostgreSQL to flush the WAL up to the redo
/// point, which a checkpoint in progress sets where WAL is still being
/// written.
const FLUSH_WAIT: Duration = Duration::from_secs(10);

/// How often a start asks how far PostgreSQL has flushed the WAL.
const FLUSH_POLL: Duration = Duration::from_millis(10);

/// How long the layouts read after DDL wait for a snapshot that sees it.
/// Past it the stream ends and Straum stops; the next start waits again.
pub(crate) const DDL_WAIT: Duration = Duration::from_secs(10);

/// How often they take a snapshot again meanwhile.
const DDL_POLL: Duration = Duration::from_millis(10);

/// A temporary physical slot streaming from its redo point, before the
/// stream's thread takes it over.
pub(crate) struct Physical {
    connection: Connection,
    slot: String,
    records: Records,
    major: u32,
    /// The server's WAL position when the slot was made.
    made: Lsn,
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
            major: reserved.major,
            made: reserved.made,
        };
        Ok((physical, reserved.start))
    }

    /// The next record, or `None` if none came within the read timeout,
    /// for tests that read the WAL without a stream's thread.
    #[cfg(test)]
    pub(crate) fn next_record(&mut self) -> Result<Option<crate::wal::Record>, Error> {
        loop {
            if let Some(record) = self.records.next_record()? {
                return Ok(Some(record.clone()));
            }
            let Some(chunk) = next_chunk(&mut self.connection, self.records.passed())? else {
                return Ok(None);
            };
            self.records.feed(chunk.start, &chunk.bytes)?;
        }
    }

    /// Shuts the replication connection, which owns the slot.
    pub(crate) fn closer(&self) -> io::Result<Closer> {
        self.connection.closer()
    }

    /// The slot's name.
    pub(crate) fn slot(&self) -> &str {
        &self.slot
    }

    /// The server's major version, which says how some records read.
    pub(crate) fn major(&self) -> u32 {
        self.major
    }

    /// The server's WAL position when the slot was made. The WAL between
    /// the redo point and here the server keeps anyway until its next
    /// checkpoint, so the watchdog counts only what comes after.
    pub(crate) fn made(&self) -> Lsn {
        self.made
    }

    /// The replication connection, and the rest of the stream for its
    /// thread to read.
    pub(crate) fn into_replay(
        self,
        pipeline: Pipeline,
        catalog: Connection,
        checked: Vec<Checked>,
    ) -> (Connection, Replay) {
        let mut records = self.records;
        records.pass_by_other_files(pipeline.files());
        let replay = Replay {
            records,
            pipeline,
            catalog,
            checked,
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
}

/// What a new physical slot reserved, on which server.
struct Reserved {
    /// The redo point, where the slot keeps WAL from.
    start: Lsn,
    segment_size: u64,
    major: u32,
    /// The server's WAL position just after the slot was made.
    made: Lsn,
}

fn reserved(connection: &mut Connection, slot: &str) -> Result<Reserved, Error> {
    // The command's consistent point is a logical slot's; a physical
    // slot reserves from the redo point, which only the view shows.
    let rows = connection.rows(&format!(
        "SELECT restart_lsn, setting, current_setting('server_version_num'), \
         pg_current_wal_lsn() FROM pg_replication_slots, pg_settings \
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
    let made = (first_row_value(&rows, 3).and_then(parse_lsn))
        .ok_or(Error::Protocol(Unexpected::NoWalPosition))?;
    Ok(Reserved {
        start,
        segment_size,
        major: version / VERSION_NUM_PER_MAJOR,
        made,
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
            commits.extend(self.pipeline.record(record, |xid| {
                read_layouts_seeing(&mut self.catalog, &self.checked, xid, DDL_WAIT)
            })?);
        }
        Ok(commits)
    }

    /// The LSN before which every record is read or passed by.
    pub(crate) fn read(&self) -> u64 {
        self.records.passed()
    }
}

/// One piece of streamed WAL: the bytes from `start`.
#[cfg(test)]
pub(crate) struct Chunk {
    pub(crate) start: u64,
    pub(crate) bytes: Vec<u8>,
}

/// The next piece of WAL a physical stream sends, or `None` if none came
/// within the read timeout. A keepalive that asks for a reply gets one
/// saying `start` is applied.
#[cfg(test)]
pub(crate) fn next_chunk(connection: &mut Connection, start: u64) -> Result<Option<Chunk>, Error> {
    use postgres_protocol::message::backend::Message as Backend;

    use crate::pgoutput::{self, CopyMessage};
    use crate::stream::status_update;
    use crate::wire::server_error;
    loop {
        let message = match connection.try_read()? {
            None => return Ok(None),
            Some(Backend::CopyData(body)) => body,
            Some(Backend::ErrorResponse(body)) => return Err(server_error(&body)),
            Some(_) => return Err(Error::Protocol(Unexpected::StreamEnded)),
        };
        match pgoutput::copy_message(message.data())? {
            CopyMessage::XLogData { start, bytes } => {
                let bytes = bytes.to_vec();
                return Ok(Some(Chunk { start, bytes }));
            }
            CopyMessage::Keepalive { reply: true, .. } => {
                connection.send_copy_data(&status_update(start))?;
            }
            CopyMessage::Keepalive { reply: false, .. } => {}
        }
    }
}

/// The layouts of `checked`'s tables as of a snapshot that sees `xid`, a
/// transaction that changed them, waiting at most `wait` for one.
/// PostgreSQL logs a commit before snapshots see it, and waits for
/// synchronous standbys in between, so the stream may read DDL's commit
/// while the catalog shows the columns from before it, and the rows
/// after it have more.
pub(crate) fn read_layouts_seeing(
    connection: &mut Connection,
    checked: &[Checked],
    xid: TransactionId,
    wait: Duration,
) -> Result<Layouts, Error> {
    let deadline = Instant::now() + wait;
    loop {
        connection.execute("BEGIN READ ONLY ISOLATION LEVEL REPEATABLE READ")?;
        let taken = connection.rows("SELECT pg_current_snapshot()")?;
        let snapshot = (first_row_value(&taken, 0).and_then(TransactionSnapshot::parse))
            .ok_or(Error::Protocol(Unexpected::SnapshotShape))?;
        if snapshot.sees(xid) {
            let layouts = read_layouts(connection, checked)?;
            connection.execute("COMMIT")?;
            return Ok(layouts);
        }
        connection.execute("ROLLBACK")?;
        if Instant::now() >= deadline {
            let problem = WaitProblem::InvisibleCommits(vec![xid]);
            return Err(waited_for(wait.as_secs(), problem));
        }
        thread::sleep(DDL_POLL);
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
        push_attribute(table, &attribute, spec_use(checked, &attribute))?;
    }
    let mut labels = HashMap::new();
    for label in catalog::labels(connection, &names)? {
        let read = Label {
            enum_type: label.type_oid,
            text: label.label,
        };
        labels.insert(label.oid, read);
    }
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

/// Whether the spec reads a column of a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpecUse {
    Reads,
    Skips,
}

/// Whether one of `checked`'s tables reads `attribute`.
fn spec_use(checked: &[Checked], attribute: &AttributeRow) -> SpecUse {
    for table in checked {
        if table.schema.as_str() != attribute.schema || table.table_name.as_str() != attribute.table
        {
            continue;
        }
        for column in &table.columns {
            if column.as_str() == attribute.name {
                return SpecUse::Reads;
            }
        }
    }
    SpecUse::Skips
}

/// Adds `attribute` to `table`, and its column unless it was dropped. A
/// column the spec skips is only stepped over: on a busy table, values no
/// view reads were most of what the stream decoded.
fn push_attribute(
    table: &mut Layout,
    attribute: &AttributeRow,
    spec_use: SpecUse,
) -> Result<(), Error> {
    // As the decoder reads the column: an enum as its label's text.
    let kind = match type_of(attribute.type_oid, attribute.typmod) {
        _ if attribute.dropped || spec_use == SpecUse::Skips => Kind::Other,
        Some(column_type) => Kind::Read(column_type),
        None if attribute.is_enum => Kind::Enum(attribute.type_oid),
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
        (Some(text), Kind::Enum(_)) => Datum::Text(text.into()),
        _ => Datum::Null,
    });
    Ok(())
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

#[cfg(test)]
mod tests {
    use straum_domain::{ColumnType, Identifier};

    use super::*;
    use crate::value::{INT4, TEXT};

    fn id(name: &str) -> Identifier {
        Identifier::new(name).unwrap()
    }

    /// `public.t`, of which the spec reads `kept`.
    fn checked() -> Checked {
        Checked {
            name: id("t"),
            oid: 0,
            schema: id("public"),
            table_name: id("t"),
            columns: vec![id("kept")],
            types: vec![ColumnType::Text],
            enums: Vec::new(),
        }
    }

    /// A text column of `public.t` named `name`, added with a default.
    fn text_column(name: &str) -> AttributeRow<'_> {
        AttributeRow {
            table_oid: 77,
            schema: "public",
            table: "t",
            file: RelationFile {
                tablespace: 1663,
                database: 5,
                relation: 77,
            },
            name,
            type_oid: TEXT,
            typmod: -1,
            length: -1,
            align: 4,
            dropped: false,
            missing: Some("late"),
            is_enum: false,
        }
    }

    fn layout_of(columns: &[AttributeRow]) -> Layout {
        let mut table = columns[0].empty_table();
        for column in columns {
            push_attribute(&mut table, column, spec_use(&[checked()], column)).unwrap();
        }
        table
    }

    #[test]
    fn reads_only_the_columns_the_spec_reads() {
        let table = layout_of(&[text_column("kept"), text_column("skipped")]);
        assert_eq!(table.attributes[0].kind, Kind::Read(ColumnType::Text));
        assert_eq!(table.missing[0], Datum::Text("late".into()));
        // Stepped over, yet still a column, as pgoutput sends it.
        assert_eq!(table.attributes[1].kind, Kind::Other);
        assert_eq!(table.missing[1], Datum::Null);
        assert_eq!(table.relation.columns.len(), 2);
    }

    #[test]
    fn a_column_of_the_same_name_in_another_table_is_skipped() {
        let mut other = text_column("kept");
        other.table = "u";
        assert_eq!(spec_use(&[checked()], &other), SpecUse::Skips);
        let mut int = text_column("kept");
        int.type_oid = INT4;
        assert_eq!(spec_use(&[checked()], &int), SpecUse::Reads);
    }
}
