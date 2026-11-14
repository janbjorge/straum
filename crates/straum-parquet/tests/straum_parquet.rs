//! The `straum-parquet` binary end to end: it feeds Parquet files shaped
//! as the TLC's to Straum's writer over a real feed, and asks answer what
//! the files hold.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use parquet::data_type::{ByteArray, ByteArrayType, DataType, DoubleType, Int32Type, Int64Type};
use parquet::file::writer::{SerializedFileWriter, SerializedRowGroupWriter};
use parquet::schema::parser::parse_message_type;
use straum_app::{App, Limits, Lsn, Step, Writer};
use straum_domain::{Ask, AskKey, Datum, Identifier, Value};
use straum_feed::Feed;

const FEEDER: &str = env!("CARGO_BIN_EXE_straum-parquet");

const LIMITS: Limits = Limits {
    max_lag: Duration::from_secs(30),
    max_retained_wal: 4 << 30,
};

/// The quick start's view, by the hour.
const SPEC: &str = "
CREATE VIEW trips WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', trips.pickup_at, timestamptz 'epoch') AS bucket,
       trips.payment_type AS payment_type,
       pickup.borough AS borough,
       count(*) AS trips,
       sum(trips.fare_amount) AS fare
FROM public.yellow_trips AS trips
LEFT OUTER JOIN public.taxi_zones AS pickup ON pickup.location_id = trips.pu_location_id
GROUP BY 1, 2, 3;
";

const MANIFEST: &str = "\
# The taxi trips, as the TLC publishes them.
table public.taxi_zones
file taxi_zones.parquet
column location_id int from LocationID
column borough text from Borough
column zone text from Zone

table public.yellow_trips
file yellow_tripdata_2024-01.parquet
file yellow_tripdata_2024-03.parquet
column vendor_id int from VendorID
column pickup_at timestamptz from tpep_pickup_datetime in America/New_York
column pu_location_id int from PULocationID
column payment_type int from payment_type
column fare_amount numeric(12,2) from fare_amount
";

const ZONES: &str = "message schema {
    OPTIONAL INT64 LocationID;
    OPTIONAL BYTE_ARRAY Borough (STRING);
    OPTIONAL BYTE_ARRAY Zone (STRING);
}";

const TRIPS: &str = "message schema {
    OPTIONAL INT32 VendorID;
    OPTIONAL INT64 tpep_pickup_datetime (TIMESTAMP(MICROS,false));
    OPTIONAL INT32 PULocationID;
    OPTIONAL INT64 payment_type;
    OPTIONAL DOUBLE fare_amount;
}";

fn id(name: &str) -> Identifier {
    Identifier::new(name).unwrap()
}

/// A directory of the test's own, removed at the end. Its name is short:
/// a socket's path holds about a hundred bytes.
struct Scratch(PathBuf);

static SCRATCH: AtomicUsize = AtomicUsize::new(0);

static SOCKET: AtomicUsize = AtomicUsize::new(0);

impl Scratch {
    fn new() -> Scratch {
        let n = SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("sp-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).to_str().unwrap().to_owned()
    }

    /// A path no feed has bound. A feed leaves its socket behind, and a
    /// child another test is spawning holds the closed listener until it
    /// execs, so the same path can still read as in use.
    fn socket(&self) -> String {
        self.path(&format!("feed-{}", SOCKET.fetch_add(1, Ordering::Relaxed)))
    }

    fn file(&self, name: &str, text: &str) -> String {
        fs::write(self.0.join(name), text).unwrap();
        self.path(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn put<T: DataType>(rows: &mut SerializedRowGroupWriter<'_, File>, cells: &[Option<T::T>])
where
    T::T: Clone,
{
    let levels: Vec<i16> = cells.iter().map(|cell| cell.is_some().into()).collect();
    let stored: Vec<T::T> = cells.iter().flatten().cloned().collect();
    let mut column = rows.next_column().unwrap().unwrap();
    let typed = column.typed::<T>();
    typed.write_batch(&stored, Some(&levels), None).unwrap();
    column.close().unwrap();
}

/// Writes one row group of `schema`, its columns through `columns`.
fn write(path: &Path, schema: &str, columns: impl FnOnce(&mut SerializedRowGroupWriter<'_, File>)) {
    let schema = Arc::new(parse_message_type(schema).unwrap());
    let file = File::create(path).unwrap();
    let mut writer = SerializedFileWriter::new(file, schema, Default::default()).unwrap();
    let mut rows = writer.next_row_group().unwrap();
    columns(&mut rows);
    rows.close().unwrap();
    writer.close().unwrap();
}

fn zones(scratch: &Scratch) {
    let text = |text: &str| Some(ByteArray::from(text.as_bytes().to_vec()));
    write(&scratch.0.join("taxi_zones.parquet"), ZONES, |rows| {
        put::<Int64Type>(rows, &[Some(1), Some(4), Some(7)]);
        put::<ByteArrayType>(rows, &[text("EWR"), text("Manhattan"), text("Queens")]);
        put::<ByteArrayType>(rows, &[text("Newark Airport"), text("Alphabet City"), None]);
    });
}

/// A trip: New York's clock in seconds since 1970 as if it were UTC, the
/// pickup zone, the payment type and the fare.
type Trip = (i64, Option<i32>, i64, Option<f64>);

fn trips(scratch: &Scratch, name: &str, trips: &[Trip]) {
    write(&scratch.0.join(name), TRIPS, |rows| {
        let column = |cell: fn(&Trip) -> Option<i64>| trips.iter().map(cell).collect::<Vec<_>>();
        put::<Int32Type>(rows, &vec![Some(2); trips.len()]);
        put::<Int64Type>(rows, &column(|trip| Some(trip.0 * 1_000_000)));
        put::<Int32Type>(rows, &trips.iter().map(|trip| trip.1).collect::<Vec<_>>());
        put::<Int64Type>(rows, &column(|trip| Some(trip.2)));
        put::<DoubleType>(rows, &trips.iter().map(|trip| trip.3).collect::<Vec<_>>());
    });
}

/// 2024-01-15 00:00 UTC and 2024-03-10 00:00 UTC, in Unix seconds.
const JANUARY_15: i64 = 1_705_276_800;
const MARCH_10: i64 = 1_710_028_800;
const HOUR: i64 = 3_600;

/// Two months of trips. January is five hours behind UTC all through; on
/// March 10 New York's clock goes from 02:00 to 03:00.
fn taxi(scratch: &Scratch) -> String {
    zones(scratch);
    let january = [
        (JANUARY_15 + 10 * HOUR + 60, Some(4), 1, Some(12.5)),
        (JANUARY_15 + 10 * HOUR + 120, Some(4), 1, Some(1.005)),
        (JANUARY_15 + 11 * HOUR, Some(7), 2, None),
        (JANUARY_15 + 11 * HOUR + 1, None, 1, Some(-2.25)),
    ];
    let march = [
        (MARCH_10 + HOUR + 1_800, Some(4), 1, Some(10.0)),
        (MARCH_10 + 2 * HOUR + 1_800, Some(4), 1, Some(20.0)),
        (MARCH_10 + 3 * HOUR + 1_800, Some(4), 1, Some(40.0)),
    ];
    trips(scratch, "yellow_tripdata_2024-01.parquet", &january);
    trips(scratch, "yellow_tripdata_2024-03.parquet", &march);
    scratch.file("taxi.manifest", MANIFEST)
}

fn feeder(args: &[&str]) -> Command {
    let mut command = Command::new(FEEDER);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// The exit code, stdout and stderr.
fn finished(output: Output) -> (Option<i32>, String, String) {
    let text = |bytes| String::from_utf8(bytes).unwrap();
    (
        output.status.code(),
        text(output.stdout),
        text(output.stderr),
    )
}

fn output(args: &[&str]) -> (Option<i32>, String, String) {
    finished(feeder(args).output().unwrap())
}

/// Straum's writer over a feed at `socket`.
fn writer(socket: &str) -> Writer<Feed> {
    let feed = Feed::bind(socket).unwrap();
    let spec = straum_notation::parse(SPEC).unwrap();
    Writer::new(App::new(spec, LIMITS, None), feed)
}

fn feed(socket: &str, manifest: &str) -> Child {
    feeder(&["--manifest", manifest, "--feed", socket])
        .spawn()
        .unwrap()
}

/// What Straum's step and the feeder each make of one start.
fn start(
    scratch: &Scratch,
    manifest: &str,
) -> (Result<Step, String>, (Option<i32>, String, String)) {
    let socket = scratch.socket();
    let mut writer = writer(&socket);
    let feeder = feed(&socket, manifest);
    let step = writer.step().map_err(|e| e.to_string());
    (step, finished(feeder.wait_with_output().unwrap()))
}

/// Both ends' failure, which must be the same but for who says it.
fn failed(scratch: &Scratch, manifest: &str) -> (String, String) {
    let (step, (code, stdout, stderr)) = start(scratch, manifest);
    assert_eq!(code, Some(1), "{stderr}");
    assert_eq!(stdout, "");
    let stderr = stderr.strip_prefix("straum-parquet: ").unwrap().trim_end();
    // Tables fed before the failure are logged before it.
    let last = stderr
        .lines()
        .last()
        .unwrap()
        .trim_start_matches("straum-parquet: ");
    (step.unwrap_err(), last.to_owned())
}

/// Trips and fares by `key` over `from..to`.
fn by(app: &App, key: &str, from: i64, to: i64) -> Vec<(Datum, Value, Value)> {
    let ask = Ask {
        view: id("trips"),
        measures: vec![id("trips"), id("fare")],
        keys: vec![match key {
            "bucket" => AskKey::Time(straum_domain::TimeKey::Bucket),
            key => AskKey::Key(id(key)),
        }],
        filter: Vec::new(),
        from,
        to,
    };
    let rows = app.ask(&ask).unwrap().answer.rows;
    (rows.into_iter())
        .map(|row| {
            (
                row.keys[0].clone(),
                row.values[0].clone(),
                row.values[1].clone(),
            )
        })
        .collect()
}

#[test]
fn feeds_the_taxi_trips_from_parquet() {
    let scratch = Scratch::new();
    let manifest = taxi(&scratch);
    let (step, (code, stdout, stderr)) = start(&scratch, &manifest);
    assert_eq!(step, Ok(Step::Started(Lsn(0))));
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "straum-parquet: fed 3 rows of public.taxi_zones\n\
         straum-parquet: fed 7 rows of public.yellow_trips\n\
         straum-parquet: Straum has every table\n"
    );
}

#[test]
fn asks_answer_what_the_files_hold() {
    let scratch = Scratch::new();
    let manifest = taxi(&scratch);
    let socket = scratch.socket();
    let mut writer = writer(&socket);
    let feeder = feed(&socket, &manifest);
    writer.step().unwrap();
    assert!(feeder.wait_with_output().unwrap().status.success());
    let app = writer.app();
    assert!(app.status().ready);

    let text = |text: &str| Datum::Text(text.into());
    let (int, fare) = (Value::Int, Value::Int);
    // January 15 in UTC: 1.005 is 1.01 in a numeric(12,2), and a null
    // fare and a null zone are kept as nulls.
    assert_eq!(
        by(app, "borough", JANUARY_15, JANUARY_15 + 24 * HOUR),
        [
            (Datum::Null, int(1), fare(-225)),
            (text("Manhattan"), int(2), fare(1_351)),
            (text("Queens"), int(1), Value::Null),
        ]
    );
    // 10:00 on New York's clock in January is 15:00 UTC.
    assert_eq!(
        by(app, "bucket", JANUARY_15, JANUARY_15 + 24 * HOUR),
        [
            (Datum::Int(JANUARY_15 + 15 * HOUR), int(2), fare(1_351)),
            (Datum::Int(JANUARY_15 + 16 * HOUR), int(2), fare(-225)),
        ]
    );
    // March 10: 01:30 is 06:30 UTC. 02:30 was never on the clock and
    // reads as if it had not been set forward, 07:30 UTC, the hour 03:30
    // is in too.
    assert_eq!(
        by(app, "bucket", MARCH_10, MARCH_10 + 24 * HOUR),
        [
            (Datum::Int(MARCH_10 + 6 * HOUR), int(1), fare(1_000)),
            (Datum::Int(MARCH_10 + 7 * HOUR), int(2), fare(6_000)),
        ]
    );
    let held = app.held().unwrap();
    assert_eq!((held.views[0].cells, held.dims[0].rows), (5, 3));
}

#[test]
fn a_table_the_manifest_lacks_stops_both_ends() {
    let scratch = Scratch::new();
    taxi(&scratch);
    let zones = MANIFEST
        .split("\ntable public.yellow_trips")
        .next()
        .unwrap();
    let manifest = scratch.file("zones.manifest", zones);
    let (straum, feeder) = failed(&scratch, &manifest);
    assert_eq!(feeder, "the manifest has no table public.yellow_trips");
    assert_eq!(straum, format!("the feeder: {feeder}"));

    let no_fare = MANIFEST.replace("column fare_amount numeric(12,2) from fare_amount\n", "");
    let manifest = scratch.file("no_fare.manifest", &no_fare);
    let (straum, feeder) = failed(&scratch, &manifest);
    assert_eq!(
        feeder,
        "the manifest has no column fare_amount for table public.yellow_trips"
    );
    assert_eq!(straum, format!("the feeder: {feeder}"));
}

#[test]
fn a_file_that_does_not_read_stops_both_ends() {
    let scratch = Scratch::new();
    let manifest = taxi(&scratch);
    let march = scratch.path("yellow_tripdata_2024-03.parquet");

    // A fare that is no number, in the second file's first row.
    trips(
        &scratch,
        "yellow_tripdata_2024-03.parquet",
        &[(MARCH_10, Some(4), 1, Some(f64::NAN))],
    );
    let (straum, feeder) = failed(&scratch, &manifest);
    assert_eq!(
        feeder,
        format!("{march}: row 1: can't read NaN as a numeric of scale 2 in column fare_amount")
    );
    assert_eq!(straum, format!("the feeder: {feeder}"));

    fs::remove_file(&march).unwrap();
    let (straum, feeder) = failed(&scratch, &manifest);
    assert!(
        feeder.starts_with(&format!("{march}: No such file")),
        "{feeder}"
    );
    assert_eq!(straum, format!("the feeder: {feeder}"));

    // The file's own name for the column, misspelt.
    let misspelt = MANIFEST.replace("from PULocationID", "from PULocationId");
    let manifest = scratch.file("misspelt.manifest", &misspelt);
    let january = scratch.path("yellow_tripdata_2024-01.parquet");
    let (straum, feeder) = failed(&scratch, &manifest);
    assert_eq!(
        feeder,
        format!("{january}: has no column PULocationId, which column pu_location_id is read from")
    );
    assert_eq!(straum, format!("the feeder: {feeder}"));
}

#[test]
fn straum_refusing_a_table_stops_the_feeder() {
    let scratch = Scratch::new();
    taxi(&scratch);
    // The view bins a timestamptz; a timestamp is another clock.
    let wall = MANIFEST.replace(
        "timestamptz from tpep_pickup_datetime in America/New_York",
        "timestamp from tpep_pickup_datetime",
    );
    let manifest = scratch.file("wall.manifest", &wall);
    let (straum, feeder) = failed(&scratch, &manifest);
    assert_eq!(
        straum,
        "trips: column pickup_at of public.yellow_trips is a timestamp; \
         its date_bin origin needs a timestamptz there"
    );
    assert_eq!(feeder, format!("Straum: {straum}"));
}

#[test]
fn a_straum_that_leaves_stops_the_feeder() {
    let scratch = Scratch::new();
    let manifest = taxi(&scratch);
    let socket = scratch.socket();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let feeder = feed(&socket, &manifest);
    // Gone once the feeder's hello is in, so the feeder has connected.
    let (mut straum, _) = listener.accept().unwrap();
    let mut hello = [0; 64];
    assert!(std::io::Read::read(&mut straum, &mut hello).unwrap() > 0);
    drop(straum);
    let (code, _, stderr) = finished(feeder.wait_with_output().unwrap());
    assert_eq!(code, Some(1));
    assert_eq!(
        stderr,
        "straum-parquet: the feed closed before every table was loaded\n"
    );
}

#[test]
fn a_straum_that_leaves_mid_table_stops_the_feeder() {
    let scratch = Scratch::new();
    let manifest = taxi(&scratch);
    let socket = scratch.socket();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let feeder = feed(&socket, &manifest);
    let (mut straum, _) = listener.accept().unwrap();
    // A load of the trips' fares, as Straum asks for one, and then gone.
    let text = |text: &str| [&(text.len() as u32).to_be_bytes()[..], text.as_bytes()].concat();
    let payload = [text("public"), text("yellow_trips"), text("fare_amount")].concat();
    let load = [&[2][..], &(payload.len() as u32).to_be_bytes(), &payload].concat();
    std::io::Write::write_all(&mut straum, &load).unwrap();
    drop(straum);
    let (code, _, stderr) = finished(feeder.wait_with_output().unwrap());
    assert_eq!(code, Some(1));
    assert_eq!(
        stderr,
        "straum-parquet: the feed closed before every table was loaded\n"
    );
}

#[test]
fn what_it_cannot_start_with_is_said_before_it_connects() {
    let scratch = Scratch::new();
    let manifest = taxi(&scratch);
    let socket = scratch.path("feed");

    let (code, _, stderr) = output(&["--manifest", &manifest, "--feed", &socket]);
    assert_eq!(code, Some(1));
    let prefix = format!("straum-parquet: connecting to the feed at {socket}: ");
    assert!(stderr.starts_with(&prefix), "{stderr}");

    let missing = scratch.path("missing.manifest");
    let (code, _, stderr) = output(&["--manifest", &missing, "--feed", &socket]);
    assert_eq!(code, Some(1));
    assert!(
        stderr.starts_with(&format!("straum-parquet: reading {missing}: ")),
        "{stderr}"
    );

    let broken = scratch.file(
        "broken.manifest",
        "table public.t\nfile t.parquet\ncolumn a\n",
    );
    let (code, _, stderr) = output(&["--manifest", &broken, "--feed", &socket]);
    assert_eq!(code, Some(1));
    assert_eq!(
        stderr,
        format!(
            "straum-parquet: {broken}: line 3: column takes NAME TYPE from FILE_COLUMN, \
             then in ZONE if need be, not \"a\"\n"
        )
    );
}

#[test]
fn help_version_and_usage() {
    let (code, stdout, stderr) = output(&["--help"]);
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    assert!(stdout.starts_with("usage: straum-parquet --manifest FILE --feed SOCKET\n"));
    assert!(stdout.ends_with("Guide: https://janbjorge.github.io/straum/feed.html\n"));

    let (code, stdout, stderr) = output(&["--version"]);
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    assert_eq!(
        stdout,
        format!("straum-parquet {}\n", env!("CARGO_PKG_VERSION"))
    );

    let (code, stdout, stderr) = output(&[]);
    assert_eq!((code, stdout.as_str()), (Some(2), ""));
    assert_eq!(
        stderr,
        "straum-parquet: straum-parquet needs --manifest\ntry: straum-parquet --help\n"
    );
    let (code, _, stderr) = output(&["--manifest", "m", "--socket", "s"]);
    assert_eq!(code, Some(2));
    assert_eq!(
        stderr,
        "straum-parquet: straum-parquet takes no \"--socket\"\ntry: straum-parquet --help\n"
    );
}
