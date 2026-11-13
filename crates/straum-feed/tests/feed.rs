//! A feed end to end: a feeder on a thread, Straum's writer over the
//! socket, and asks answered from what was fed.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use straum_app::{App, Limits, Lsn, Source, Step, Wal, Writer};
use straum_domain::{Ask, AskKey, ColumnType, Datum, Identifier, Spec, Store, Table, Value};
use straum_feed::{Error, Feed, Feeder, Request};

const LIMITS: Limits = Limits {
    max_lag: Duration::from_secs(30),
    max_retained_wal: 4 << 30,
};

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

const TIMESTAMPTZ: ColumnType = ColumnType::Time { zoned: true };
const FARE: ColumnType = ColumnType::Numeric { scale: 2 };

fn spec() -> Spec {
    straum_notation::parse(SPEC).unwrap()
}

fn id(name: &str) -> Identifier {
    Identifier::new(name).unwrap()
}

fn text(value: &str) -> Datum {
    Datum::Text(value.into())
}

/// A directory of the test's own, removed at the end. Its name is short:
/// a socket's path holds about a hundred bytes.
struct Scratch(PathBuf);

static SCRATCH: AtomicUsize = AtomicUsize::new(0);

impl Scratch {
    fn new() -> Scratch {
        let n = SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("sf-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }

    fn socket(&self) -> PathBuf {
        self.0.join("feed")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

/// A table a feeder holds: its columns with their types, and its rows.
struct Held {
    name: &'static str,
    columns: Vec<(&'static str, ColumnType)>,
    rows: Vec<Vec<Datum>>,
}

fn zones() -> Held {
    Held {
        name: "taxi_zones",
        columns: vec![
            ("location_id", ColumnType::Int),
            ("borough", ColumnType::Text),
            ("zone", ColumnType::Text),
        ],
        rows: vec![
            vec![Datum::Int(1), text("EWR"), text("Newark Airport")],
            vec![Datum::Int(4), text("Manhattan"), text("Alphabet City")],
            vec![Datum::Int(7), text("Queens"), text("Astoria")],
        ],
    }
}

/// Four trips in two hours: fares in cents, as a `numeric(12, 2)` reads.
fn trips() -> Held {
    let trip = |at: i64, zone: Datum, payment: i64, fare: Datum| {
        vec![
            Datum::Int(9),
            Datum::Int(at),
            zone,
            Datum::Int(payment),
            fare,
        ]
    };
    Held {
        name: "yellow_trips",
        columns: vec![
            ("vendor_id", ColumnType::Int),
            ("pickup_at", TIMESTAMPTZ),
            ("pu_location_id", ColumnType::Int),
            ("payment_type", ColumnType::Int),
            ("fare_amount", FARE),
        ],
        rows: vec![
            trip(60, Datum::Int(4), 1, Datum::Int(1_250)),
            trip(120, Datum::Int(4), 1, Datum::Int(5)),
            trip(3_700, Datum::Int(7), 2, Datum::Null),
            trip(3_800, Datum::Null, 1, Datum::Int(-225)),
        ],
    }
}

/// Answers every load from `tables` until Straum is done, a batch of
/// `batch` rows at a time. The loads asked, in order.
fn serve(
    feeder: &mut Feeder,
    tables: &[Held],
    batch: usize,
) -> Result<Vec<(Table, Vec<Identifier>)>, Error> {
    let mut asked = Vec::new();
    loop {
        let (table, columns) = match feeder.next_request()? {
            Request::Done => return Ok(asked),
            Request::Load { table, columns } => (table, columns),
        };
        asked.push((table.clone(), columns.clone()));
        let Some(held) = tables.iter().find(|held| held.name == table.name.as_str()) else {
            feeder.fail(&format!("no table {}.{}", table.schema, table.name))?;
            return Ok(asked);
        };
        let at: Vec<usize> = (columns.iter())
            .map(|column| {
                let named = |(name, _): &(&str, ColumnType)| *name == column.as_str();
                held.columns.iter().position(named).unwrap()
            })
            .collect();
        let types: Vec<ColumnType> = at.iter().map(|&at| held.columns[at].1).collect();
        feeder.types(&types)?;
        for rows in held.rows.chunks(batch) {
            let flat: Vec<Datum> = (rows.iter())
                .flat_map(|row| at.iter().map(|&at| row[at].clone()))
                .collect();
            feeder.rows(&flat)?;
        }
        feeder.end()?;
    }
}

type Served = JoinHandle<Result<Vec<(Table, Vec<Identifier>)>, Error>>;

/// A feeder on its own thread, connecting once the socket is there.
fn feeder(socket: &Path, tables: Vec<Held>, batch: usize) -> Served {
    let socket = socket.to_owned();
    thread::spawn(move || serve(&mut Feeder::connect(socket)?, &tables, batch))
}

fn writer(scratch: &Scratch) -> Writer<Feed> {
    let feed = Feed::bind(scratch.socket()).unwrap();
    Writer::new(App::new(spec(), LIMITS, None), feed)
}

fn by_borough(app: &App) -> Vec<(Datum, Vec<Value>)> {
    let ask = Ask {
        view: id("trips"),
        measures: vec![id("trips"), id("fare")],
        keys: vec![AskKey::Key(id("borough"))],
        filter: Vec::new(),
        from: 0,
        to: 7_200,
    };
    let answer = app.ask(&ask).unwrap().answer;
    (answer.rows.into_iter())
        .map(|row| (row.keys[0].clone(), row.values))
        .collect()
}

fn failure(step: Result<Step, straum_app::Error>) -> String {
    match step {
        Err(e) => e.to_string(),
        Ok(step) => panic!("the step went through: {step:?}"),
    }
}

#[test]
fn a_feeder_loads_every_table() {
    let scratch = Scratch::new();
    let mut writer = writer(&scratch);
    assert!(!writer.app().status().ready);
    // A row a batch, so a table comes in several frames.
    let served = feeder(&scratch.socket(), vec![trips(), zones()], 1);
    assert_eq!(writer.step().unwrap(), Step::Started(Lsn(0)));

    let asked = served.join().unwrap().unwrap();
    let names = |names: &[&str]| names.iter().map(|name| id(name)).collect::<Vec<_>>();
    let table = |name: &str| Table {
        schema: id("public"),
        name: id(name),
    };
    // The dim first, then the fact; only the columns the spec reads.
    assert_eq!(asked.len(), 2);
    assert_eq!(
        asked[0],
        (table("taxi_zones"), names(&["location_id", "borough"]))
    );
    assert_eq!(asked[1].0, table("yellow_trips"));
    let mut columns = asked[1].1.clone();
    columns.sort();
    assert_eq!(
        columns,
        names(&["fare_amount", "payment_type", "pickup_at", "pu_location_id"])
    );

    let app = writer.app();
    let status = app.status();
    assert!(status.ready);
    assert_eq!(status.applied, Some(Lsn(0)));
    assert_eq!(status.lag, Some(Duration::ZERO));
    assert_eq!((status.starts, status.failures), (1, 0));
    assert_eq!(
        by_borough(app),
        [
            (Datum::Null, vec![Value::Int(1), Value::Int(-225)]),
            (text("Manhattan"), vec![Value::Int(2), Value::Int(1_255)]),
            (text("Queens"), vec![Value::Int(1), Value::Null]),
        ]
    );
    let held = app.held().unwrap();
    assert_eq!(held.dims[0].rows, 3);
}

#[test]
fn nothing_follows_the_load() {
    let scratch = Scratch::new();
    let mut feed = Feed::bind(scratch.socket()).unwrap();
    assert_eq!(feed.path(), scratch.socket());
    let zero = Wal {
        written: Lsn(0),
        confirmed: Lsn(0),
        restart: Lsn(0),
    };
    assert_eq!(feed.wal().unwrap(), zero);
    let asked = Instant::now();
    assert_eq!(feed.next_commit(Duration::from_millis(30)).unwrap(), None);
    assert!(asked.elapsed() >= Duration::from_millis(30));
}

#[test]
fn a_load_needs_a_feeder() {
    let scratch = Scratch::new();
    let mut feed = Feed::bind(scratch.socket()).unwrap();
    let mut store = Store::new(spec());
    let err = feed.load(&id("trips"), &mut store).unwrap_err();
    assert_eq!(err.to_string(), "no feeder is connected");
}

#[test]
fn a_feeder_that_fails_is_followed_by_the_next() {
    let scratch = Scratch::new();
    let mut writer = writer(&scratch);
    // It holds the zones only, and says so when asked for the trips.
    let served = feeder(&scratch.socket(), vec![zones()], 8);
    assert_eq!(
        failure(writer.step()),
        "the feeder: no table public.yellow_trips"
    );
    assert_eq!(served.join().unwrap().unwrap().len(), 2);
    let status = writer.app().status();
    assert!(!status.ready);
    assert_eq!((status.starts, status.failures), (0, 1));

    let served = feeder(&scratch.socket(), vec![zones(), trips()], 8);
    assert_eq!(writer.step().unwrap(), Step::Started(Lsn(0)));
    served.join().unwrap().unwrap();
    assert!(writer.app().status().ready);
    assert_eq!(by_borough(writer.app()).len(), 3);
}

#[test]
fn a_feeder_that_leaves_closes_the_feed() {
    let scratch = Scratch::new();
    let mut writer = writer(&scratch);
    drop(Feeder::connect(scratch.socket()).unwrap());
    assert_eq!(
        failure(writer.step()),
        "the feed closed before every table was loaded"
    );
    // One that never says hello is gone as soon as it connects.
    drop(UnixStream::connect(scratch.socket()).unwrap());
    assert_eq!(
        failure(writer.step()),
        "the feed closed before every table was loaded"
    );
}

/// What Straum makes of a feeder that sends `sent` raw, and the frame
/// Straum sent back before it closed.
fn sent_raw(sent: &[u8], read_back: bool) -> (String, Vec<u8>) {
    let scratch = Scratch::new();
    let mut writer = writer(&scratch);
    let mut stream = UnixStream::connect(scratch.socket()).unwrap();
    stream.write_all(sent).unwrap();
    let failure = failure(writer.step());
    let mut back = Vec::new();
    if read_back {
        stream.read_to_end(&mut back).unwrap();
    }
    (failure, back)
}

const HELLO: &[u8] = b"\x01\0\0\0\x0dstraum feed 1";

#[test]
fn a_feeder_of_another_protocol_is_refused() {
    let (failure, _) = sent_raw(b"\x01\0\0\0\x0dstraum feed 2", false);
    assert_eq!(failure, "the feed carried a hello of another protocol");
    let (failure, _) = sent_raw(b"\x05\0\0\0\0", false);
    assert_eq!(failure, "the feed carried a first frame that is no hello");
    let (failure, _) = sent_raw(b"GET / HTTP/1.1\r\n\r\n", false);
    assert_eq!(failure, "the feed carried a frame of 1163141167 bytes");
}

#[test]
fn straum_says_why_it_refuses_a_table() {
    // Rows before their types.
    let rows = [HELLO, b"\x04\0\0\0\x01\0"].concat();
    let (failure, back) = sent_raw(&rows, true);
    assert_eq!(failure, "the feed carried rows before their types");
    // The load asked, then why Straum gave up.
    let why = b"the feed carried rows before their types";
    let failed = [&[7, 0, 0, 0, why.len() as u8][..], why].concat();
    assert!(back.ends_with(&failed), "{back:?}");

    // One type for the dim's two columns.
    let (failure, _) = sent_raw(&[HELLO, b"\x03\0\0\0\x01\0"].concat(), true);
    assert_eq!(
        failure,
        "taxi_zones: the feeder gave 1 types for the 2 columns of public.taxi_zones"
    );

    // Types, then a frame that is neither rows nor the end.
    let (failure, _) = sent_raw(&[HELLO, b"\x03\0\0\0\x02\0\x03\x06\0\0\0\0"].concat(), true);
    assert_eq!(
        failure,
        "the feed carried a frame that is no row of the table"
    );

    // Half a row.
    let half = [HELLO, b"\x03\0\0\0\x02\0\x03\x04\0\0\0\x01\0"].concat();
    let (failure, _) = sent_raw(&half, true);
    assert_eq!(failure, "fed row has 1 columns, expected 2");
}

/// The feeder's own failure, when Straum refuses what it feeds.
fn refused(tables: Vec<Held>) -> (String, Error) {
    let scratch = Scratch::new();
    let mut writer = writer(&scratch);
    let served = feeder(&scratch.socket(), tables, 2);
    let failure = failure(writer.step());
    (failure, served.join().unwrap().unwrap_err())
}

#[test]
fn the_time_column_has_the_type_of_its_origin() {
    let mut trips = trips();
    trips.columns[1].1 = ColumnType::Time { zoned: false };
    let (failure, told) = refused(vec![zones(), trips]);
    let why = "trips: column pickup_at of public.yellow_trips is a timestamp; \
               its date_bin origin needs a timestamptz there";
    assert_eq!(failure, why);
    assert!(
        matches!(&told, Error::Straum(told) if told == why),
        "{told}"
    );
}

#[test]
fn a_row_that_does_not_fit_stops_the_load_and_tells_the_feeder() {
    let mut trips = trips();
    // Enough rows that the feeder is still writing when Straum gives up.
    trips.rows = (0..200_000)
        .map(|n| {
            let fare = if n == 0 { text("free") } else { Datum::Int(n) };
            vec![
                Datum::Int(9),
                Datum::Int(n),
                Datum::Int(4),
                Datum::Int(1),
                fare,
            ]
        })
        .collect();
    let (failure, told) = refused(vec![zones(), trips]);
    assert_eq!(failure, "fed column fare_amount is not an integer");
    assert!(
        matches!(&told, Error::Straum(told) if told == "fed column fare_amount is not an integer"),
        "{told}"
    );
}

#[test]
fn a_feeder_refuses_what_straum_never_sends() {
    let scratch = Scratch::new();
    let listener = UnixListener::bind(scratch.socket()).unwrap();
    let mut feeder = Feeder::connect(scratch.socket()).unwrap();
    let (mut straum, _) = listener.accept().unwrap();
    straum.write_all(b"\x05\0\0\0\0").unwrap();
    let err = feeder.next_request().unwrap_err();
    assert_eq!(
        err.to_string(),
        "the feed carried a frame Straum never sends"
    );
    drop(straum);
    assert!(matches!(feeder.next_request(), Err(Error::Closed)));
}

#[test]
fn a_feeder_without_a_straum_fails_to_connect() {
    let scratch = Scratch::new();
    let Err(err) = Feeder::connect(scratch.socket()) else {
        panic!("connected to nothing");
    };
    assert!(matches!(err, Error::Io(_)), "{err}");
}

#[test]
fn the_socket_is_the_owners_alone() {
    let scratch = Scratch::new();
    let _feed = Feed::bind(scratch.socket()).unwrap();
    let mode = fs::metadata(scratch.socket()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn a_socket_left_behind_is_replaced() {
    let scratch = Scratch::new();
    drop(UnixListener::bind(scratch.socket()).unwrap());
    assert!(scratch.socket().exists());
    let mut writer = writer(&scratch);
    let served = feeder(&scratch.socket(), vec![zones(), trips()], 8);
    writer.step().unwrap();
    served.join().unwrap().unwrap();
}

#[test]
fn a_socket_in_use_and_a_file_are_left_alone() {
    let scratch = Scratch::new();
    let first = Feed::bind(scratch.socket()).unwrap();
    let Err(err) = Feed::bind(scratch.socket()) else {
        panic!("two feeds on one socket");
    };
    assert!(matches!(err, Error::Io(_)), "{err}");
    drop(first);

    let file = scratch.0.join("file");
    fs::write(&file, "kept").unwrap();
    assert!(Feed::bind(&file).is_err());
    assert_eq!(fs::read_to_string(&file).unwrap(), "kept");
    assert!(Feed::bind(scratch.0.join("no/such/directory")).is_err());
}
