//! What the HTTP tests serve: `Fake`, a scripted source that loads fixed
//! zones and trips and then streams one commit. A test builds a `Writer` over a `Fake` with
//! `writer` and steps it to load the app the server answers from.

use std::collections::VecDeque;
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use straum_app::{App, Change, Commit, Limits, Lsn, Source, SourceError, Wal, Writer};
use straum_domain::{ColumnType, Datum, Identifier, Spec, Store};

/// One view over the trips and their zones, with every kind of measure.
pub(crate) const SPEC: &str = "
CREATE VIEW trips AS
SELECT buckets(t.at, '1 hour', '1 day') AS bucket,
       t.kind AS kind,
       t.zone_id AS zone_id,
       zone.borough AS borough,
       count(*) AS trips,
       sum(t.fare) AS fare,
       avg(t.fare) AS fare_avg,
       max(t.fare) AS fare_max
FROM public.trips AS t
LEFT OUTER JOIN public.zones AS zone ON zone.id = t.zone_id
GROUP BY 1, 2, 3, 4;
";

/// 2024-01-01 00:00 UTC, in Unix seconds.
pub(crate) const JAN_1: i64 = 1_704_067_200;
/// One hour, in seconds.
pub(crate) const HOUR: i64 = 3_600;
/// Where the fake's stream starts.
pub(crate) const START: Lsn = Lsn(0x16_B374_D848);
/// The LSN of the one commit the fake streams.
pub(crate) const COMMIT: Lsn = Lsn(0x17_0000_0000);
const FOUR_GIBIBYTES: u64 = 4 << 30;
/// The limits Straum runs with by default.
pub(crate) const LIMITS: Limits = Limits {
    max_lag: Duration::from_secs(30),
    max_retained_wal: FOUR_GIBIBYTES,
};

/// Loads fixed zones and trips at [`START`], then streams its commits.
pub(crate) struct Fake {
    pub(crate) trips: Vec<Vec<Datum>>,
    pub(crate) commits: VecDeque<Commit>,
    /// How long loading the fact takes.
    pub(crate) pause: Duration,
    pub(crate) wal: Wal,
}

impl Source for Fake {
    fn open(&mut self, _: &Spec) -> Result<Lsn, SourceError> {
        Ok(START)
    }

    /// Zones 1 and 2; a trip in zone 3 finds no row.
    fn load(&mut self, tables: &[Identifier], store: &mut Store) -> Result<(), SourceError> {
        // The spec's one dim and its one view each load alone.
        if tables[0].as_str() == "zones" {
            store.set_types("zones", &[ColumnType::Int, ColumnType::Text])?;
            return Ok(store.add_rows(
                "zones",
                &[int(1), text("Bronx"), int(2), text("Queens \"Q\"")],
            )?);
        }
        thread::sleep(self.pause);
        // at, kind, zone_id, fare.
        store.set_types(
            "trips",
            &[
                ColumnType::Time { zoned: true },
                ColumnType::Int,
                ColumnType::Int,
                ColumnType::Numeric { scale: 2 },
            ],
        )?;
        for row in &self.trips {
            store.add_row("trips", row)?;
        }
        Ok(())
    }

    fn next_commit(&mut self, _: Duration) -> Result<Option<Commit>, SourceError> {
        match self.commits.pop_front() {
            Some(commit) => Ok(Some(commit)),
            None => Err("connection dropped".into()),
        }
    }

    fn wal(&mut self) -> Result<Wal, SourceError> {
        Ok(self.wal)
    }
}

/// An integer cell.
pub(crate) fn int(integer: i64) -> Datum {
    Datum::Int(integer)
}

fn text(text: &str) -> Datum {
    Datum::Text(text.into())
}

/// A trip's row: its hour of 2024-01-01, kind, zone and fare.
pub(crate) fn trip(hour: i64, kind: i64, zone: i64, fare: Datum) -> Vec<Datum> {
    vec![int(JAN_1 + hour * HOUR), int(kind), int(zone), fare]
}

/// Four trips on 2024-01-01 UTC, one in a zone with no row, and a slot
/// that has confirmed all there is.
pub(crate) fn fake() -> Fake {
    Fake {
        trips: vec![
            trip(0, 1, 1, int(1_250)),
            trip(1, 1, 1, int(250)),
            trip(2, 2, 3, Datum::Null),
            trip(23, 2, 2, int(-5)),
        ],
        commits: VecDeque::from([Commit {
            lsn: COMMIT,
            committed: UNIX_EPOCH + Duration::from_micros(1_704_153_600_123_456),
            changes: vec![Change::Insert {
                table: Identifier::new("trips").unwrap(),
                row: trip(3, 1, 1, int(100)),
            }],
        }]),
        pause: Duration::ZERO,
        wal: Wal {
            written: START,
            confirmed: START,
            restart: START,
        },
    }
}

/// A writer over `fake` for an app with `limits`.
pub(crate) fn writer_with(limits: Limits, fake: Fake) -> Writer<Fake> {
    let spec = straum_notation::parse(SPEC).unwrap();
    Writer::new(App::new(spec, limits), fake)
}

/// A writer over the usual fake, nothing loaded yet.
pub(crate) fn writer() -> Writer<Fake> {
    writer_with(LIMITS, fake())
}

/// A writer that has loaded every table.
pub(crate) fn loaded() -> Writer<Fake> {
    let mut writer = writer();
    writer.step().unwrap();
    writer
}
