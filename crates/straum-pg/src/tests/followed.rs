//! A spec table that stops being the one Straum loaded, with no change
//! the stream reads to say so: left out of the publication, renamed away
//! with another table made under its name. Each reading of the WAL
//! checks the tables, so Straum starts over, and the setup check of the
//! next start names the problem. The physical stream reads the catalog's
//! own changes and starts over on the DDL itself.

use straum_app::{Change, Source};

use super::references::{SPEC, TABLES};
use super::support::*;

/// The table made again under its name: the same columns as the one
/// renamed away, published.
const MADE_AGAIN: &str = "
ALTER TABLE $S.trips RENAME TO trips_old;
CREATE TABLE $S.trips (LIKE $S.trips_old INCLUDING ALL);
ALTER TABLE $S.trips REPLICA IDENTITY FULL;
ALTER PUBLICATION $S ADD TABLE $S.trips;
";

#[test]
fn a_table_left_out_of_the_publication_stops_the_stream() {
    let pg = postgres();
    let mut db = Db::new(&pg, "unpublished", TABLES);
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    source.open(&spec).unwrap();
    assert!(source.wal().is_ok());
    db.run("ALTER PUBLICATION $S DROP TABLE $S.trips");
    let error = source.wal().unwrap_err().to_string();
    assert_eq!(
        error,
        db.sql(
            "trips: publication $S leaves out $S.trips: ALTER PUBLICATION $S ADD TABLE $S.trips"
        )
    );
}

/// The table renamed away still sends its changes, under its new name:
/// they no longer read as the spec's. The reading of the WAL finds
/// another table under the name.
#[test]
fn a_table_renamed_away_and_made_again_stops_the_stream() {
    let pg = postgres();
    let mut db = Db::new(&pg, "replaced", TABLES);
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    source.open(&spec).unwrap();
    db.run(MADE_AGAIN);
    db.run(&Rng::new(1).trip(1).replace("$S.trips", "$S.trips_old"));
    let commit = source.next_commit(LONG).unwrap().unwrap();
    assert_eq!(commit.changes, [Change::Unfit { table: id("trips") }]);
    let error = source.wal().unwrap_err().to_string();
    assert_eq!(
        error,
        db.sql(
            "trips: $S.trips is another table than the one Straum loaded; Straum loads it again"
        )
    );
}
