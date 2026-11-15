//! The physical stream: a temporary physical slot, which keeps WAL from
//! the last checkpoint's redo point, and the WAL it streams, read as
//! records. The stream carries the whole server's WAL, every database.

use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

use postgres_protocol::escape::escape_literal;
use straum_app::{Commit, Lsn};
use straum_domain::Datum;

use crate::catalog::Checked;
use crate::dsn::Dsn;
use crate::error::Error;
use crate::pgoutput::{Column, Relation};
use crate::source::{parse_lsn, slot_name};
use crate::stream::POLL;
use crate::value::{read, type_of};
use crate::wal::messages::Table;
use crate::wal::pipeline::Pipeline;
use crate::wal::tuple::{Attribute, Kind};
use crate::wal::{Records, RelationFile};
use crate::wire::Connection;

/// How long a start waits for PostgreSQL to flush the WAL up to the redo
/// point, which a checkpoint in progress sets where WAL is still being
/// written.
const FLUSH_WAIT: Duration = Duration::from_secs(10);

/// A temporary physical slot streaming from its redo point, before the
/// stream's thread takes it over.
pub(crate) struct Physical {
    conn: Connection,
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
        let mut conn = Connection::connect(dsn, true)?;
        let slot = slot_name();
        conn.execute(&format!(
            "CREATE_REPLICATION_SLOT {slot} TEMPORARY PHYSICAL (RESERVE_WAL)"
        ))?;
        // The command's consistent point is a logical slot's; a physical
        // slot reserves from the redo point, which only the view shows.
        let rows = conn.rows(&format!(
            "SELECT restart_lsn, setting, current_setting('server_version_num') \
             FROM pg_replication_slots, pg_settings \
             WHERE slot_name = {} AND name = 'wal_segment_size'",
            escape_literal(&slot)
        ))?;
        let row = rows.first();
        let field = |i: usize| row.and_then(|row| row.get(i)?.as_deref());
        let start = field(0)
            .and_then(parse_lsn)
            .ok_or_else(|| Error::protocol("a physical slot without a restart LSN"))?;
        let segment_size = field(1)
            .and_then(|size| size.parse::<u64>().ok())
            .ok_or_else(|| Error::protocol("no wal_segment_size"))?;
        let major = field(2)
            .and_then(|version| version.parse::<u32>().ok())
            .ok_or_else(|| Error::protocol("no server_version_num"))?
            / 10_000;
        let records = Records::new(start.0, segment_size);
        // The slot keeps WAL from the redo point (`GetRedoRecPtr`), which
        // a checkpoint in progress may have set past what PostgreSQL has
        // flushed, and the server streams nothing it hasn't flushed.
        let deadline = Instant::now() + FLUSH_WAIT;
        loop {
            let rows = conn.rows("SELECT pg_current_wal_flush_lsn()")?;
            let flushed = (rows.first())
                .and_then(|row| row.first()?.as_deref())
                .and_then(parse_lsn)
                .ok_or_else(|| Error::protocol("no WAL flush position"))?;
            if flushed.0 >= start.0 {
                break;
            }
            if Instant::now() >= deadline {
                return Err(Error::protocol(
                    "PostgreSQL never flushed the WAL up to the redo point",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
        conn.copy_both(&format!(
            "START_REPLICATION SLOT {slot} PHYSICAL {}",
            Lsn(records.wanted())
        ))?;
        conn.set_read_timeout(POLL)?;
        let physical = Physical {
            conn,
            slot,
            records,
            start: start.0,
            major,
        };
        Ok((physical, start))
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
                return Ok(Some(record));
            }
            match self.conn.try_read()? {
                None => return Ok(None),
                Some(Backend::CopyData(body)) => match pgoutput::copy_message(body.data())? {
                    CopyMessage::XLogData { start, bytes } => self.records.feed(start, bytes)?,
                    CopyMessage::Keepalive { reply, .. } => {
                        if reply {
                            self.conn.send_copy_data(&status_update(self.start))?;
                        }
                    }
                },
                Some(Backend::ErrorResponse(body)) => return Err(server_error(&body)),
                Some(_) => return Err(Error::protocol("a message that ended the stream")),
            }
        }
    }

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
        (self.conn, replay)
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

impl Replay {
    /// Reads `bytes` of WAL from `start`: the commits they finish.
    pub(crate) fn feed(&mut self, start: u64, bytes: &[u8]) -> Result<Vec<Commit>, Error> {
        self.records.feed(start, bytes)?;
        let mut commits = Vec::new();
        while let Some(record) = self.records.next_record()? {
            let (catalog, checked) = (&mut self.catalog, &self.checked);
            let layouts = || tables(catalog, checked).map(|(tables, _)| tables);
            commits.extend(self.pipeline.record(&record, layouts)?);
            self.read = record.next();
        }
        Ok(commits)
    }

    pub(crate) fn read(&self) -> u64 {
        self.read
    }
}

/// The layout of each of `checked`'s tables, and the labels of the enums
/// their columns have, by `pg_enum` OID, which is what a row stores.
pub(crate) fn tables(
    conn: &mut Connection,
    checked: &[Checked],
) -> Result<(Vec<Table>, HashMap<u32, String>), Error> {
    let names: Vec<String> = checked
        .iter()
        .map(|t| {
            format!(
                "({}, {})",
                escape_literal(t.schema.as_str()),
                escape_literal(t.table.as_str())
            )
        })
        .collect();
    let names = names.join(", ");
    // A dropped column has no type, and its bytes stay in older rows, so
    // the join to pg_type keeps it.
    let rows = conn.rows(&format!(
        "SELECT c.oid, n.nspname, c.relname,
                CASE c.reltablespace WHEN 0 THEN d.dattablespace ELSE c.reltablespace END,
                d.oid, pg_relation_filenode(c.oid), a.attname, a.atttypid, a.atttypmod,
                a.attlen, a.attalign, a.attisdropped, a.attmissingval::text,
                coalesce(t.typtype::text, '')
         FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace
         JOIN pg_database d ON d.datname = current_database()
         JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0
         LEFT JOIN pg_type t ON t.oid = a.atttypid
         WHERE (n.nspname, c.relname) IN ({names})
         ORDER BY c.oid, a.attnum"
    ))?;
    let shape = || Error::protocol("a table layout of an unknown shape");
    let mut tables: Vec<Table> = Vec::new();
    for row in rows {
        let field = |i: usize| row.get(i).and_then(|v| v.as_deref()).ok_or_else(shape);
        let number = |i: usize| field(i)?.parse::<u32>().map_err(|_| shape());
        let oid = number(0)?;
        if tables.last().is_none_or(|t| t.relation.id != oid) {
            tables.push(Table {
                file: RelationFile {
                    tablespace: number(3)?,
                    database: number(4)?,
                    relation: number(5)?,
                },
                relation: Relation {
                    id: oid,
                    schema: field(1)?.to_owned(),
                    name: field(2)?.to_owned(),
                    columns: Vec::new(),
                },
                attributes: Vec::new(),
                columns: Vec::new(),
                missing: Vec::new(),
            });
        }
        let table = tables.last_mut().ok_or_else(shape)?;
        let type_oid = number(7)?;
        let typmod = field(8)?.parse().map_err(|_| shape())?;
        let dropped = field(11)? == "t";
        let align = match field(10)? {
            "c" => 1,
            "s" => 2,
            "i" => 4,
            "d" => 8,
            _ => return Err(shape()),
        };
        // As the decoder reads the column: an enum as its label's text.
        let kind = match type_of(type_oid, typmod) {
            _ if dropped => Kind::Other,
            Some(column_type) => Kind::Read(column_type),
            None if field(13)? == "e" => Kind::Enum,
            None => Kind::Other,
        };
        if !dropped {
            table.columns.push(table.attributes.len());
            table.relation.columns.push(Column {
                name: field(6)?.to_owned(),
                type_oid,
                typmod,
            });
        }
        table.attributes.push(Attribute {
            length: field(9)?.parse().map_err(|_| shape())?,
            align,
            kind,
        });
        let missing = match row.get(12).and_then(|v| v.as_deref()) {
            None => None,
            Some(array) => Some(element(array).ok_or_else(shape)?),
        };
        table.missing.push(match (missing, kind) {
            (Some(text), Kind::Read(column_type)) => read(column_type, &text)?,
            (Some(text), Kind::Enum) => Datum::Text(text.into()),
            _ => Datum::Null,
        });
    }
    let labels = conn.rows(&format!(
        "SELECT DISTINCT e.oid, e.enumlabel
         FROM pg_enum e
         JOIN pg_attribute a ON a.atttypid = e.enumtypid
         JOIN pg_class c ON c.oid = a.attrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE (n.nspname, c.relname) IN ({names})"
    ))?;
    let labels = labels
        .into_iter()
        .map(|row| match &row[..] {
            [Some(oid), Some(label)] => Ok((oid.parse().map_err(|_| shape())?, label.clone())),
            _ => Err(shape()),
        })
        .collect::<Result<_, Error>>()?;
    Ok((tables, labels))
}

/// The one element of an array literal, as `attmissingval::text`
/// prints it: `{7}`, or quoted with backslash escapes when it holds a
/// space, comma, quote, brace or backslash.
fn element(array: &str) -> Option<String> {
    let inner = array.strip_prefix('{')?.strip_suffix('}')?;
    let Some(quoted) = inner.strip_prefix('"') else {
        let plain = !inner.is_empty()
            && !inner.contains([',', '"', '\\', '{', '}'])
            && !inner.eq_ignore_ascii_case("null");
        return plain.then(|| inner.to_owned());
    };
    let mut text = String::new();
    let mut chars = quoted.chars();
    loop {
        match chars.next()? {
            '"' => return chars.next().is_none().then_some(text),
            '\\' => text.push(chars.next()?),
            c => text.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_array_elements() {
        let cases = [
            ("{7}", Some("7")),
            ("{-1.50}", Some("-1.50")),
            (
                "{\"2024-01-01 00:00:00+00\"}",
                Some("2024-01-01 00:00:00+00"),
            ),
            ("{\"a,b\"}", Some("a,b")),
            ("{\"say \\\"hi\\\" \\\\ back\"}", Some("say \"hi\" \\ back")),
            ("{\"\"}", Some("")),
            ("{\"NULL\"}", Some("NULL")),
            ("{NULL}", None),
            ("{}", None),
            ("{a,b}", None),
            ("7", None),
            ("{7", None),
            ("{\"open}", None),
            ("{\"a\"b}", None),
            ("{\"a\\\"}", None),
        ];
        for (array, expected) in cases {
            assert_eq!(element(array).as_deref(), expected, "{array}");
        }
    }
}
