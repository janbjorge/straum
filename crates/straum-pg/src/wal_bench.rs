//! How fast Straum reads physical WAL: PostgreSQL under `replica` writes
//! the WAL of four phases of taxi trip changes, the physical slot streams
//! it, and Straum reads the captured bytes again and again, with no
//! socket in the way. Once as records only, the pages and record headers,
//! and once through the whole pipeline to commits.

use std::io::Write;
use std::path::Path;
use std::process;
use std::thread;
use std::time::{Duration, Instant};

use postgres_protocol::message::backend::Message as Backend;
use straum_app::{Change, Commit, Lsn};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

use crate::catalog::{self, Checked};
use crate::dsn::Dsn;
use crate::pgoutput::{self, CopyMessage};
use crate::physical::tables;
use crate::source::{Replication, parse_lsn, slot_name};
use crate::stream::{POLL, status_update};
use crate::wal::messages::Layouts;
use crate::wal::pipeline::Pipeline;
use crate::wal::transactions::{Consistency, Snapshot};
use crate::wal::{Record, Records};
use crate::wire::{Connection, server_error};

const PASSWORD: &str = "straum";
const READY: &str = "database system is ready to accept connections";
/// Rows each phase changes.
const ROWS: usize = 1_000_000;
/// Rows per transaction, as an application writes them.
const BATCH: usize = 10_000;
/// Rows per `COPY`, as a bulk load writes them.
const COPY_BATCH: usize = 250_000;
const MAX_BYTES: usize = 8 << 30;
const COLUMNS: &str = "vendor_id, pickup_at, pu_location_id, do_location_id, \
                       payment_type, fare_amount, tip_amount, trip_distance";

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

struct Wal {
    start: u64,
    segment_size: u64,
    chunks: Vec<(u64, Vec<u8>)>,
}

/// Run `cargo test -p straum-pg --release physical_wal_bench -- --ignored
/// --nocapture`. Loads the taxi month with `bench/load_taxi_trips.py`, so
/// it needs uv. `STRAUM_TEST_POSTGRES` picks the major, `STRAUM_BENCH_RUNS`
/// the reads of each kind (3), of which the fastest counts.
#[test]
#[ignore]
fn physical_wal_bench() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let major = std::env::var("STRAUM_TEST_POSTGRES").unwrap_or_else(|_| "18".to_owned());
    let runs: usize = std::env::var("STRAUM_BENCH_RUNS").map_or(3, |runs| {
        runs.parse().expect("STRAUM_BENCH_RUNS is a count")
    });
    // One redo point, the one taken before the slot, and no autovacuum:
    // every run writes the same kinds of records.
    let container = GenericImage::new("postgres", &major)
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stdout(READY))
        .with_wait_for(WaitFor::message_on_stderr(READY))
        .with_shm_size(1 << 30)
        .with_cmd([
            "-c",
            "wal_level=replica",
            "-c",
            "max_wal_size=16GB",
            "-c",
            "checkpoint_timeout=1h",
            "-c",
            "autovacuum=off",
        ])
        .with_env_var("POSTGRES_PASSWORD", PASSWORD)
        .with_env_var("POSTGRES_HOST_AUTH_METHOD", "scram-sha-256")
        .start()
        .expect("Docker runs postgres");
    let port = (0..50)
        .find_map(|_| {
            let port = container.get_host_port_ipv4(5432).ok();
            if port.is_none() {
                thread::sleep(Duration::from_millis(100));
            }
            port
        })
        .expect("Docker publishes port 5432");
    let url = format!("postgres://postgres:{PASSWORD}@127.0.0.1:{port}/postgres?sslmode=disable");
    let dsn = Dsn::parse(&url).unwrap();
    load_month(&root, &container);

    let spec_text =
        std::fs::read_to_string(root.join("crates/straum-notation/tests/taxi.sql")).unwrap();
    let spec = straum_notation::parse(&spec_text).unwrap();
    let mut sql = Connection::connect(&dsn, false).unwrap();
    let physical = Replication::Physical {
        max_page_memory: MAX_BYTES,
    };
    let checked = catalog::read(&mut sql, &spec, "straum", physical).unwrap();
    let checked: Vec<Checked> = (checked.facts.into_iter()).chain(checked.dims).collect();
    let copies = copies(&mut sql);
    sql.execute("CHECKPOINT").unwrap();

    let mut stream = Connection::connect(&dsn, true).unwrap();
    let slot = slot_name();
    stream
        .execute(&format!(
            "CREATE_REPLICATION_SLOT {slot} TEMPORARY PHYSICAL (RESERVE_WAL)"
        ))
        .unwrap();
    let start = lsn(
        &mut sql,
        &format!("SELECT restart_lsn FROM pg_replication_slots WHERE slot_name = '{slot}'"),
    );
    let segment_size = number(
        &mut sql,
        "SELECT setting FROM pg_settings WHERE name = 'wal_segment_size'",
    );
    let major = number(&mut sql, "SELECT current_setting('server_version_num')") / 10_000;
    let major = u32::try_from(major).unwrap();
    let layouts = tables(&mut sql, &checked).unwrap();
    sql.execute("SELECT pg_log_standby_snapshot()").unwrap();
    let snapshot = sql.rows("SELECT pg_current_snapshot()").unwrap()[0][0]
        .clone()
        .unwrap();
    let snapshot = Snapshot::parse(&snapshot).unwrap();

    let mut phases = Vec::new();
    let mut from = start;
    let mut phase = |sql: &mut Connection, name, rows, work: &mut dyn FnMut(&mut Connection)| {
        let clock = Instant::now();
        work(sql);
        let took = clock.elapsed();
        let to = lsn(sql, "SELECT pg_current_wal_insert_lsn()");
        phases.push(Phase {
            name,
            from,
            to,
            took,
            rows,
        });
        from = to;
    };
    phase(&mut sql, "copy", ROWS, &mut |_| {
        for copy in &copies {
            psql(&container, copy);
        }
    });
    phase(&mut sql, "insert", ROWS, &mut |sql| {
        for (low, high) in batches(0) {
            sql.execute(&format!(
                "INSERT INTO yellow_trips ({COLUMNS})
                 SELECT vendor_id, pickup_at + interval '62 days', pu_location_id,
                        do_location_id, payment_type, fare_amount, tip_amount, trip_distance
                   FROM yellow_trips WHERE id > {low} AND id <= {high}"
            ))
            .unwrap();
        }
    });
    phase(&mut sql, "update", ROWS, &mut |sql| {
        for (low, high) in batches(0) {
            sql.execute(&format!(
                "UPDATE yellow_trips SET fare_amount = fare_amount + 1
                  WHERE id > {low} AND id <= {high}"
            ))
            .unwrap();
        }
    });
    phase(&mut sql, "delete", ROWS, &mut |sql| {
        for (low, high) in batches(ROWS) {
            sql.execute(&format!(
                "DELETE FROM yellow_trips WHERE id > {low} AND id <= {high}"
            ))
            .unwrap();
        }
    });
    let flushed = lsn(&mut sql, "SELECT pg_current_wal_flush_lsn()");
    let records = Records::new(start, segment_size);
    stream
        .copy_both(&format!(
            "START_REPLICATION SLOT {slot} PHYSICAL {}",
            Lsn(records.wanted())
        ))
        .unwrap();
    stream.set_read_timeout(POLL).unwrap();
    let wal = Wal {
        start,
        segment_size,
        chunks: capture(&mut stream, start, flushed),
    };
    let ends: Vec<u64> = phases.iter().map(|p| p.to).collect();

    let mut parsed = vec![Read::default(); phases.len()];
    let mut piped = vec![Read::default(); phases.len()];
    let mut peak = 0;
    for _ in 0..runs {
        let read = replay(&wal, &ends, |record| {
            let images = record.blocks.iter().filter(|b| b.image.is_some()).count();
            (images, 0)
        });
        fastest(&mut parsed, &read);
        let mut pipeline = Pipeline::new(
            checked.clone(),
            layouts.clone(),
            major,
            COPY_BATCH,
            MAX_BYTES,
        );
        let mut snapshot = Some(snapshot.clone());
        let mut held = 0;
        let read = replay(&wal, &ends, |record| {
            let mut commits = pipeline.record(record, || relayout(&layouts)).unwrap();
            if pipeline.consistency() == Consistency::Consistent
                && let Some(snapshot) = snapshot.take()
            {
                commits.extend(pipeline.see(snapshot, Lsn(0)).unwrap().1);
            }
            held = held.max(pipeline.bytes());
            (0, rows(&commits))
        });
        peak = held;
        fastest(&mut piped, &read);
    }
    for (phase, read) in phases.iter().zip(&piped) {
        assert_eq!(read.rows, phase.rows, "rows read in {}", phase.name);
    }
    report(&phases, &parsed, &piped, peak, major);
}

/// The taxi month in PostgreSQL, as `taxi_month` loads it.
fn load_month(root: &Path, container: &Container<GenericImage>) {
    let clock = Instant::now();
    let mut loader = process::Command::new(root.join("bench/load_taxi_trips.py"))
        .arg("--data")
        .arg(root.join("bench/data"))
        .stdout(process::Stdio::piped())
        .spawn()
        .expect("bench/load_taxi_trips.py runs with uv");
    let psql = process::Command::new("docker")
        .args(["exec", "-i", container.id(), "psql", "-U", "postgres", "-q"])
        .stdin(loader.stdout.take().unwrap())
        .status()
        .unwrap();
    assert!(psql.success(), "psql: {psql}");
    let loaded = loader.wait().unwrap();
    assert!(loaded.success(), "bench/load_taxi_trips.py: {loaded}");
    eprintln!("loaded the taxi month in {:.1?}", clock.elapsed());
}

/// The scripts of the copy phase, each one `COPY` of trips already there,
/// a month later. Made before the phase, so it times only the copies.
fn copies(sql: &mut Connection) -> Vec<Vec<u8>> {
    (0..ROWS / COPY_BATCH)
        .map(|k| {
            let (low, high) = (k * COPY_BATCH, (k + 1) * COPY_BATCH);
            let mut script = format!(
                "\\set ON_ERROR_STOP on\nCOPY yellow_trips ({COLUMNS}) FROM stdin WITH (FORMAT csv);\n"
            )
            .into_bytes();
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
            script
        })
        .collect()
}

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
fn batches(skip: usize) -> impl Iterator<Item = (usize, usize)> {
    (0..ROWS / BATCH).map(move |k| (skip + k * BATCH, skip + (k + 1) * BATCH))
}

fn number(sql: &mut Connection, query: &str) -> u64 {
    let rows = sql.rows(query).unwrap();
    rows[0][0].as_deref().unwrap().parse().unwrap()
}

fn lsn(sql: &mut Connection, query: &str) -> u64 {
    let rows = sql.rows(query).unwrap();
    parse_lsn(rows[0][0].as_deref().unwrap()).unwrap().0
}

/// The WAL the slot streams, up to `until`, in the pieces it came in.
fn capture(stream: &mut Connection, start: u64, until: u64) -> Vec<(u64, Vec<u8>)> {
    let mut chunks = Vec::new();
    let mut end = 0;
    while end < until {
        match stream.try_read().unwrap() {
            None => {}
            Some(Backend::CopyData(body)) => match pgoutput::copy_message(body.data()).unwrap() {
                CopyMessage::XLogData { start, bytes } => {
                    end = start + bytes.len() as u64;
                    chunks.push((start, bytes.to_vec()));
                }
                CopyMessage::Keepalive { reply, .. } => {
                    if reply {
                        let status = status_update(start);
                        stream.send_copy_data(&status).unwrap();
                    }
                }
            },
            Some(Backend::ErrorResponse(body)) => panic!("{}", server_error(&body)),
            Some(_) => panic!("a message that ended the stream"),
        }
    }
    chunks
}

/// Reads the WAL once, timing each phase; `each` counts a record's full
/// page images and the rows it ends with.
fn replay(wal: &Wal, ends: &[u64], mut each: impl FnMut(&Record) -> (usize, usize)) -> Vec<Read> {
    let mut reads = vec![Read::default(); ends.len()];
    let mut records = Records::new(wal.start, wal.segment_size);
    let mut phase = 0;
    let mut clock = Instant::now();
    for (start, bytes) in &wal.chunks {
        records.feed(*start, bytes).unwrap();
        while let Some(record) = records.next_record().unwrap() {
            // A record past the last phase is a keepalive's, or the
            // standby snapshot after it: the last phase takes it.
            while phase + 1 < ends.len() && record.lsn >= ends[phase] {
                reads[phase].took += clock.elapsed();
                clock = Instant::now();
                phase += 1;
            }
            let (images, rows) = each(record);
            let read = &mut reads[phase];
            read.records += 1;
            read.images += images;
            read.rows += rows;
        }
    }
    reads[phase].took += clock.elapsed();
    reads
}

fn fastest(best: &mut [Read], read: &[Read]) {
    for (best, read) in best.iter_mut().zip(read) {
        if best.took.is_zero() || read.took < best.took {
            *best = Read {
                images: read.images.max(best.images),
                rows: read.rows.max(best.rows),
                ..*read
            };
        }
    }
}

/// The workload changes no table: a commit that invalidates one is a
/// statistic or a hint, and the layout stays.
fn relayout(layouts: &Layouts) -> Result<Layouts, crate::error::Error> {
    Ok(layouts.clone())
}

fn rows(commits: &[Commit]) -> usize {
    let changes = commits.iter().flat_map(|c| &c.changes);
    changes
        .inspect(|change| assert!(!matches!(change, Change::Unfit { .. }), "{change:?}"))
        .count()
}

fn report(phases: &[Phase], parsed: &[Read], piped: &[Read], peak: usize, major: u32) {
    let mib = |bytes: u64| bytes as f64 / f64::from(1 << 20);
    let per_second = |n: f64, took: Duration| n / took.as_secs_f64();
    eprintln!("\nPostgreSQL {major}, wal_level = replica; the fastest read of each kind:\n");
    eprintln!(
        "| Phase | WAL | Records | Images | Rows | PostgreSQL wrote | Records read | Pipeline read | Rows/s | Against PostgreSQL |"
    );
    eprintln!("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    let mut total = (0, Duration::ZERO, Duration::ZERO, Duration::ZERO, 0);
    for ((phase, parsed), piped) in phases.iter().zip(parsed).zip(piped) {
        let bytes = phase.to - phase.from;
        total.0 += bytes;
        total.1 += phase.took;
        total.2 += parsed.took;
        total.3 += piped.took;
        total.4 += piped.rows;
        let faster = phase.took.as_secs_f64() / piped.took.as_secs_f64();
        eprintln!(
            "| {} | {:.1} MiB | {} | {} | {} | {:.1?} | {:.0} MiB/s | {:.0} MiB/s | {:.0} | {faster:.1}x |",
            phase.name,
            mib(bytes),
            parsed.records,
            parsed.images,
            piped.rows,
            phase.took,
            per_second(mib(bytes), parsed.took),
            per_second(mib(bytes), piped.took),
            per_second(piped.rows as f64, piped.took),
        );
    }
    let (bytes, wrote, parsed, piped, rows) = total;
    eprintln!(
        "| all | {:.1} MiB | | | {rows} | {wrote:.1?} | {:.0} MiB/s | {:.0} MiB/s | {:.0} | {:.1}x |",
        mib(bytes),
        per_second(mib(bytes), parsed),
        per_second(mib(bytes), piped),
        per_second(rows as f64, piped),
        wrote.as_secs_f64() / piped.as_secs_f64(),
    );
    eprintln!(
        "\nThe pipeline held at most {:.1} MiB of rows.",
        mib(peak as u64)
    );
}
