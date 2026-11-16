//! How fast Straum reads physical WAL: PostgreSQL under `replica` writes
//! the WAL of four phases of taxi trip changes, the physical slot streams
//! it, and Straum reads the captured bytes again and again, with no
//! socket in the way. Once as records only, through `wal::Records`, which
//! cuts pages into records, and once through the whole `wal::Pipeline`,
//! from records to commits. Each phase's fastest read counts; the report
//! sets it beside how long PostgreSQL took to write the same WAL.

use std::io::Write;
use std::path::Path;
use std::process;
use std::time::{Duration, Instant};

use straum_app::{Change, Commit, Lsn};
use testcontainers::{Container, GenericImage, ImageExt};

use crate::catalog::{self, Checked};
use crate::physical::{Chunk, next_chunk, read_layouts};
use crate::source::{Replication, parse_lsn, slot_name};
use crate::stream::POLL;
use crate::tests::support::{PostgresContainer, image, load_taxi_month, plain, tag};
use crate::wal::messages::Layouts;
use crate::wal::pipeline::Pipeline;
use crate::wal::transactions::{Consistency, TransactionSnapshot};
use crate::wal::{Record, Records};
use crate::wire::{Connection, ConnectionKind};

/// Rows each phase changes.
const ROWS: usize = 1_000_000;
/// Rows per transaction, as an application writes them.
const BATCH: usize = 10_000;
/// Rows per `COPY`, as a bulk load writes them.
const COPY_BATCH: usize = 250_000;
const MAX_BYTES: usize = 8 << 30;
const COLUMNS: &str = "vendor_id, pickup_at, pu_location_id, do_location_id, \
                       payment_type, fare_amount, tip_amount, trip_distance";
const MEBIBYTE: f64 = 1_048_576.0;
/// The reads of each kind, unless `STRAUM_BENCH_RUNS` says.
const DEFAULT_RUNS: usize = 3;

/// A run of the workload: the WAL from `from` to `to`, and how long
/// PostgreSQL took to write it.
struct Phase {
    name: &'static str,
    from: u64,
    to: u64,
    took: Duration,
    rows: usize,
}

/// What one read of a phase's records counted.
#[derive(Clone, Copy, Default)]
struct Read {
    took: Duration,
    records: usize,
    images: usize,
    rows: usize,
}

/// What one record counted: its full-page images and the rows of the
/// commits it ends.
struct Counted {
    images: usize,
    rows: usize,
}

/// WAL as the slot streamed it, in the pieces it came in.
struct Wal {
    start: u64,
    segment_size: u64,
    chunks: Vec<Chunk>,
}

/// The tables the bench reads, the copy phase's scripts, and where the
/// slot it reads with stands.
struct Setup {
    checked: Vec<Checked>,
    copies: Vec<Vec<u8>>,
    layouts: Layouts,
    snapshot: TransactionSnapshot,
    slot: String,
    start: u64,
    segment_size: u64,
    major: u32,
}

/// The fastest reads of each phase: as records, and through the pipeline,
/// with the most bytes the pipeline held.
struct Fastest {
    parsed: Vec<Read>,
    piped: Vec<Read>,
    peak: usize,
}

/// The ids of one transaction's rows: above `low`, up to `high`.
struct IdRange {
    low: usize,
    high: usize,
}

/// What a phase of the workload does.
#[derive(Clone, Copy)]
enum Work {
    /// `COPY` trips a month later, from scripts made before.
    Copy,
    /// `INSERT ... SELECT` trips two months later.
    Insert,
    /// `UPDATE` every trip's fare.
    Update,
    /// `DELETE` the trips the copy phase wrote.
    Delete,
}

/// Run `cargo test -p straum-pg --release physical_wal_bench -- --ignored
/// --nocapture`. Loads the taxi month with `bench/load_taxi_trips.py`, so
/// it needs uv. `STRAUM_TEST_POSTGRES` picks the major, `STRAUM_BENCH_RUNS`
/// the reads of each kind (3), of which the fastest counts.
#[test]
#[ignore = "a benchmark: it needs uv and the taxi month, and runs for minutes"]
fn physical_wal_bench() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let runs = match std::env::var("STRAUM_BENCH_RUNS") {
        Ok(runs) => runs.parse().expect("STRAUM_BENCH_RUNS is a count"),
        Err(_) => DEFAULT_RUNS,
    };
    let PostgresContainer { container, dsn, .. } = start_postgres();
    let clock = Instant::now();
    load_taxi_month(&root, &container);
    eprintln!("loaded the taxi month in {:.1?}", clock.elapsed());
    let mut sql = Connection::connect(&dsn, ConnectionKind::Sql).unwrap();
    let mut stream = Connection::connect(&dsn, ConnectionKind::Replication).unwrap();
    let setup = set_up(&root, &mut sql, &mut stream);
    let phases = write_phases(&mut sql, &container, &setup.copies, setup.start);
    let flushed = lsn(&mut sql, "SELECT pg_current_wal_flush_lsn()");
    let records = Records::new(setup.start, setup.segment_size);
    stream
        .copy_both(&format!(
            "START_REPLICATION SLOT {} PHYSICAL {}",
            setup.slot,
            Lsn(records.wanted())
        ))
        .unwrap();
    stream.set_read_timeout(POLL).unwrap();
    let wal = Wal {
        start: setup.start,
        segment_size: setup.segment_size,
        chunks: capture(&mut stream, setup.start, flushed),
    };
    let fastest = read_fastest(&wal, &phases, &setup, runs);
    for (index, phase) in phases.iter().enumerate() {
        let read = &fastest.piped[index];
        assert_eq!(read.rows, phase.rows, "rows read in {}", phase.name);
    }
    report(&phases, &fastest, setup.major);
}

/// PostgreSQL in Docker, and how to reach it.
fn start_postgres() -> PostgresContainer {
    // One redo point, the one taken before the slot, and no autovacuum:
    // every run writes the same kinds of records.
    let request = image(&tag()).with_shm_size(1 << 30).with_cmd([
        "-c",
        "wal_level=replica",
        "-c",
        "max_wal_size=16GB",
        "-c",
        "checkpoint_timeout=1h",
        "-c",
        "autovacuum=off",
    ]);
    plain(request)
}

/// Reads the spec's tables, makes the copy phase's scripts, then takes a
/// checkpoint and the slot, so the WAL read starts at a redo point with
/// every page whole.
fn set_up(root: &Path, sql: &mut Connection, stream: &mut Connection) -> Setup {
    let spec_text =
        std::fs::read_to_string(root.join("crates/straum-notation/tests/taxi.sql")).unwrap();
    let spec = straum_notation::parse(&spec_text).unwrap();
    let physical = Replication::Physical {
        max_page_memory: MAX_BYTES,
    };
    let checked = catalog::read(sql, &spec, "straum", physical).unwrap();
    let checked: Vec<Checked> = (checked.facts.into_iter()).chain(checked.dims).collect();
    let copies = copies(sql);
    sql.execute("CHECKPOINT").unwrap();
    let slot = slot_name();
    stream
        .execute(&format!(
            "CREATE_REPLICATION_SLOT {slot} TEMPORARY PHYSICAL (RESERVE_WAL)"
        ))
        .unwrap();
    let start = lsn(
        sql,
        &format!("SELECT restart_lsn FROM pg_replication_slots WHERE slot_name = '{slot}'"),
    );
    let segment_size = number(
        sql,
        "SELECT setting FROM pg_settings WHERE name = 'wal_segment_size'",
    );
    let major = number(sql, "SELECT current_setting('server_version_num')") / 10_000;
    let major = u32::try_from(major).unwrap();
    let layouts = read_layouts(sql, &checked).unwrap();
    sql.execute("SELECT pg_log_standby_snapshot()").unwrap();
    let snapshot = sql.rows("SELECT pg_current_snapshot()").unwrap()[0][0]
        .clone()
        .unwrap();
    let snapshot = TransactionSnapshot::parse(&snapshot).unwrap();
    Setup {
        checked,
        copies,
        layouts,
        snapshot,
        slot,
        start,
        segment_size,
        major,
    }
}

/// The four phases of changes, from `start`, each timed.
fn write_phases(
    sql: &mut Connection,
    container: &Container<GenericImage>,
    copies: &[Vec<u8>],
    start: u64,
) -> Vec<Phase> {
    let mut phases = Vec::new();
    let mut from = start;
    for (name, work) in [
        ("copy", Work::Copy),
        ("insert", Work::Insert),
        ("update", Work::Update),
        ("delete", Work::Delete),
    ] {
        let clock = Instant::now();
        write_phase(work, sql, container, copies);
        let took = clock.elapsed();
        let to = lsn(sql, "SELECT pg_current_wal_insert_lsn()");
        phases.push(Phase {
            name,
            from,
            to,
            took,
            rows: ROWS,
        });
        from = to;
    }
    phases
}

/// Writes one phase's changes.
fn write_phase(
    work: Work,
    sql: &mut Connection,
    container: &Container<GenericImage>,
    copies: &[Vec<u8>],
) {
    match work {
        Work::Copy => {
            for copy in copies {
                psql(container, copy);
            }
        }
        Work::Insert => {
            for IdRange { low, high } in batches(0) {
                sql.execute(&format!(
                    "INSERT INTO yellow_trips ({COLUMNS})
                     SELECT vendor_id, pickup_at + interval '62 days', pu_location_id,
                            do_location_id, payment_type, fare_amount, tip_amount, trip_distance
                       FROM yellow_trips WHERE id > {low} AND id <= {high}"
                ))
                .unwrap();
            }
        }
        Work::Update => {
            for IdRange { low, high } in batches(0) {
                sql.execute(&format!(
                    "UPDATE yellow_trips SET fare_amount = fare_amount + 1
                      WHERE id > {low} AND id <= {high}"
                ))
                .unwrap();
            }
        }
        Work::Delete => {
            for IdRange { low, high } in batches(ROWS) {
                sql.execute(&format!(
                    "DELETE FROM yellow_trips WHERE id > {low} AND id <= {high}"
                ))
                .unwrap();
            }
        }
    }
}

/// How a replay reads each record: as a record only, or through the
/// pipeline to commits.
#[expect(
    clippy::large_enum_variant,
    reason = "one reader a replay, made once: its size costs nothing"
)]
enum Reader {
    Records,
    Pipeline(PipelineRead),
}

/// A pipeline as a replay drives it: it takes the load's snapshot once
/// consistent, and keeps the most bytes it held.
struct PipelineRead {
    pipeline: Pipeline,
    layouts: Layouts,
    snapshot: Option<TransactionSnapshot>,
    peak: usize,
}

impl Reader {
    /// What `record` counts.
    fn count(&mut self, record: &Record) -> Counted {
        match self {
            Reader::Records => Counted {
                images: count_images(record),
                rows: 0,
            },
            Reader::Pipeline(read) => Counted {
                images: 0,
                rows: read.commit_rows(record),
            },
        }
    }
}

impl PipelineRead {
    fn new(setup: &Setup) -> PipelineRead {
        PipelineRead {
            pipeline: Pipeline::new(
                setup.checked.clone(),
                setup.layouts.clone(),
                setup.major,
                COPY_BATCH,
                MAX_BYTES,
            ),
            layouts: setup.layouts.clone(),
            snapshot: Some(setup.snapshot.clone()),
            peak: 0,
        }
    }

    /// Reads `record` through the pipeline: the rows of the commits it
    /// ends.
    fn commit_rows(&mut self, record: &Record) -> usize {
        // The workload changes no table: a commit that invalidates one is
        // a statistic or a hint, and the layout stays.
        let layouts = &self.layouts;
        let mut commits = (self.pipeline.record(record, || Ok(layouts.clone()))).unwrap();
        if self.pipeline.consistency() == Consistency::Consistent
            && let Some(snapshot) = self.snapshot.take()
        {
            commits.extend(self.pipeline.see(snapshot, Lsn(0)).unwrap().1);
        }
        self.peak = self.peak.max(self.pipeline.bytes());
        rows(&commits)
    }
}

/// Reads the WAL `runs` times each way, keeping each phase's fastest.
fn read_fastest(wal: &Wal, phases: &[Phase], setup: &Setup, runs: usize) -> Fastest {
    let mut ends = Vec::new();
    for phase in phases {
        ends.push(phase.to);
    }
    let mut fastest = Fastest {
        parsed: vec![Read::default(); phases.len()],
        piped: vec![Read::default(); phases.len()],
        peak: 0,
    };
    for _ in 0..runs {
        let read = replay(wal, &ends, &mut Reader::Records);
        keep_fastest(&mut fastest.parsed, &read);
        let mut reader = Reader::Pipeline(PipelineRead::new(setup));
        let read = replay(wal, &ends, &mut reader);
        if let Reader::Pipeline(piped) = reader {
            fastest.peak = piped.peak;
        }
        keep_fastest(&mut fastest.piped, &read);
    }
    fastest
}

/// The full-page images `record` carries.
fn count_images(record: &Record) -> usize {
    let mut images = 0;
    for block in &record.blocks {
        if block.image.is_some() {
            images += 1;
        }
    }
    images
}

/// The scripts of the copy phase, each one `COPY` of trips already there,
/// a month later. Made before the phase, so it times only the copies.
fn copies(sql: &mut Connection) -> Vec<Vec<u8>> {
    let mut scripts = Vec::new();
    for index in 0..ROWS / COPY_BATCH {
        let (low, high) = (index * COPY_BATCH, (index + 1) * COPY_BATCH);
        let mut script = format!(
            "\\set ON_ERROR_STOP on\nCOPY yellow_trips ({COLUMNS}) FROM stdin WITH (FORMAT csv);\n"
        )
        .into_bytes();
        // `copy_out` hands each piece of the copy to the closure.
        sql.copy_out(
            &format!(
                "COPY (SELECT vendor_id, pickup_at + interval '31 days', pu_location_id,
                              do_location_id, payment_type, fare_amount, tip_amount,
                              trip_distance
                         FROM yellow_trips WHERE id > {low} AND id <= {high})
                 TO STDOUT WITH (FORMAT csv)"
            ),
            |data| {
                script.extend_from_slice(data);
                Ok(())
            },
        )
        .unwrap();
        script.extend_from_slice(b"\\.\n");
        scripts.push(script);
    }
    scripts
}

/// Runs `script` in `psql` inside the container.
fn psql(container: &Container<GenericImage>, script: &[u8]) {
    let mut psql = process::Command::new("docker")
        .args(["exec", "-i", container.id(), "psql", "-U", "postgres", "-q"])
        .stdin(process::Stdio::piped())
        .spawn()
        .unwrap();
    psql.stdin.take().unwrap().write_all(script).unwrap();
    let status = psql.wait().unwrap();
    assert!(status.success(), "psql: {status}");
}

/// The id ranges of one phase's transactions, past `skip`.
fn batches(skip: usize) -> Vec<IdRange> {
    let mut ranges = Vec::new();
    for index in 0..ROWS / BATCH {
        ranges.push(IdRange {
            low: skip + index * BATCH,
            high: skip + (index + 1) * BATCH,
        });
    }
    ranges
}

/// The number `query` returns.
fn number(sql: &mut Connection, query: &str) -> u64 {
    let rows = sql.rows(query).unwrap();
    rows[0][0].as_deref().unwrap().parse().unwrap()
}

/// The LSN `query` returns.
fn lsn(sql: &mut Connection, query: &str) -> u64 {
    let rows = sql.rows(query).unwrap();
    parse_lsn(rows[0][0].as_deref().unwrap()).unwrap().0
}

/// The WAL the slot streams, up to `until`, in the pieces it came in.
fn capture(stream: &mut Connection, start: u64, until: u64) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut end = 0;
    while end < until {
        if let Some(chunk) = next_chunk(stream, start).unwrap() {
            end = chunk.start + chunk.bytes.len() as u64;
            chunks.push(chunk);
        }
    }
    chunks
}

/// Reads the WAL once, timing each phase; `reader` counts each record's
/// full-page images and the rows it ends with.
fn replay(wal: &Wal, ends: &[u64], reader: &mut Reader) -> Vec<Read> {
    let mut reads = vec![Read::default(); ends.len()];
    let mut records = Records::new(wal.start, wal.segment_size);
    let mut phase = 0;
    let mut clock = Instant::now();
    for chunk in &wal.chunks {
        records.feed(chunk.start, &chunk.bytes).unwrap();
        while let Some(record) = records.next_record().unwrap() {
            // A record past the last phase is a keepalive's, or the
            // standby snapshot after it: the last phase takes it.
            while phase + 1 < ends.len() && record.lsn >= ends[phase] {
                reads[phase].took += clock.elapsed();
                clock = Instant::now();
                phase += 1;
            }
            let counted = reader.count(record);
            let read = &mut reads[phase];
            read.records += 1;
            read.images += counted.images;
            read.rows += counted.rows;
        }
    }
    reads[phase].took += clock.elapsed();
    reads
}

/// Keeps in `best` each phase's faster read of the two.
fn keep_fastest(best: &mut [Read], read: &[Read]) {
    for (index, best) in best.iter_mut().enumerate() {
        let read = read[index];
        if best.took.is_zero() || read.took < best.took {
            *best = Read {
                images: read.images.max(best.images),
                rows: read.rows.max(best.rows),
                ..read
            };
        }
    }
}

/// The rows `commits` change. None may be unfit: the bench changes no
/// table's layout.
fn rows(commits: &[Commit]) -> usize {
    let mut rows = 0;
    for commit in commits {
        for change in &commit.changes {
            assert!(!matches!(change, Change::Unfit { .. }), "{change:?}");
            rows += 1;
        }
    }
    rows
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a report's figure: f64 is exact far past the bytes a bench writes"
)]
fn mebibytes(bytes: u64) -> f64 {
    bytes as f64 / MEBIBYTE
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a report's figure: f64 is exact far past the rows a bench writes"
)]
fn per_second(count: usize, took: Duration) -> f64 {
    count as f64 / took.as_secs_f64()
}

fn mebibytes_per_second(bytes: u64, took: Duration) -> f64 {
    mebibytes(bytes) / took.as_secs_f64()
}

/// Every phase's figures, summed.
#[derive(Default)]
struct Totals {
    bytes: u64,
    wrote: Duration,
    parsed: Duration,
    piped: Duration,
    rows: usize,
}

/// Prints a table of each phase's fastest reads against PostgreSQL's
/// writing, in markdown.
fn report(phases: &[Phase], fastest: &Fastest, major: u32) {
    eprintln!("\nPostgreSQL {major}, wal_level = replica; the fastest read of each kind:\n");
    eprintln!(
        "| Phase | WAL | Records | Images | Rows | PostgreSQL wrote | Records read | Pipeline read | Rows/s | Against PostgreSQL |"
    );
    eprintln!("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    let mut total = Totals::default();
    for (index, phase) in phases.iter().enumerate() {
        let parsed = &fastest.parsed[index];
        let piped = &fastest.piped[index];
        let bytes = phase.to - phase.from;
        total.bytes += bytes;
        total.wrote += phase.took;
        total.parsed += parsed.took;
        total.piped += piped.took;
        total.rows += piped.rows;
        let faster = phase.took.as_secs_f64() / piped.took.as_secs_f64();
        eprintln!(
            "| {} | {:.1} MiB | {} | {} | {} | {:.1?} | {:.0} MiB/s | {:.0} MiB/s | {:.0} | {faster:.1}x |",
            phase.name,
            mebibytes(bytes),
            parsed.records,
            parsed.images,
            piped.rows,
            phase.took,
            mebibytes_per_second(bytes, parsed.took),
            mebibytes_per_second(bytes, piped.took),
            per_second(piped.rows, piped.took),
        );
    }
    eprintln!(
        "| all | {:.1} MiB | | | {} | {:.1?} | {:.0} MiB/s | {:.0} MiB/s | {:.0} | {:.1}x |",
        mebibytes(total.bytes),
        total.rows,
        total.wrote,
        mebibytes_per_second(total.bytes, total.parsed),
        mebibytes_per_second(total.bytes, total.piped),
        per_second(total.rows, total.piped),
        total.wrote.as_secs_f64() / total.piped.as_secs_f64(),
    );
    eprintln!(
        "\nThe pipeline held at most {:.1} MiB of rows.",
        mebibytes(fastest.peak as u64)
    );
}
