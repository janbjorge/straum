//! `/metrics` in the Prometheus text format, version 0.0.4:
//! <https://prometheus.io/docs/instrumenting/exposition_formats/>

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use weir_app::Status;

/// Upper bounds of the ask duration histogram, in seconds.
const BOUNDS: [f64; 9] = [0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0];

/// What became of the asks so far.
#[derive(Debug, Default)]
pub(crate) struct Asks {
    answered: AtomicU64,
    not_ready: AtomicU64,
    invalid: AtomicU64,
    /// weir panicked while answering.
    failed: AtomicU64,
    /// Answered asks per duration bucket, the last past every bound. Not
    /// cumulative; [`write`] adds them up.
    buckets: [AtomicU64; BOUNDS.len() + 1],
    /// Time spent on answered asks.
    micros: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Answered(Duration),
    NotReady,
    Invalid,
    Failed,
}

impl Asks {
    pub(crate) fn record(&self, outcome: Outcome) {
        let counter = match outcome {
            Outcome::Answered(took) => {
                let bucket = (BOUNDS.iter())
                    .position(|&bound| took.as_secs_f64() <= bound)
                    .unwrap_or(BOUNDS.len());
                self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
                let micros = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
                self.micros.fetch_add(micros, Ordering::Relaxed);
                &self.answered
            }
            Outcome::NotReady => &self.not_ready,
            Outcome::Invalid => &self.invalid,
            Outcome::Failed => &self.failed,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn write(status: &Status, asks: &Asks) -> String {
    let mut out = String::new();
    // Each family whole: its help, its type, then its samples.
    let mut family = |name: &str, kind: &str, help: &str, samples: Vec<String>| {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
        for sample in samples {
            out.push_str(&sample);
            out.push('\n');
        }
    };
    family(
        "weir_ready",
        "gauge",
        "1 when every table is loaded and the stream has passed each load.",
        vec![format!("weir_ready {}", u8::from(status.ready))],
    );
    family(
        "weir_lsn",
        "gauge",
        "Every commit up to this LSN is applied; absent while weir starts.",
        (status.lsn.iter())
            .map(|lsn| format!("weir_lsn {}", lsn.0))
            .collect(),
    );
    family(
        "weir_commit_time_seconds",
        "gauge",
        "When the last applied commit committed, by PostgreSQL's clock.",
        (status.time.iter())
            .map(|time| {
                // A clock before 1970 reads as 0.
                let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
                format!("weir_commit_time_seconds {}", since.as_secs_f64())
            })
            .collect(),
    );
    for (name, help, value) in [
        ("weir_starts_total", "Streams started.", status.starts),
        ("weir_commits_total", "Commits applied.", status.commits),
        (
            "weir_reloads_total",
            "Tables read again after a change that didn't fit them.",
            status.reloads,
        ),
        (
            "weir_failures_total",
            "Failures after which weir started over.",
            status.failures,
        ),
    ] {
        family(name, "counter", help, vec![format!("{name} {value}")]);
    }
    family(
        "weir_cells",
        "gauge",
        "Cells in a fact's rollup.",
        (status.facts.iter())
            .map(|f| format!("weir_cells{{fact=\"{}\"}} {}", f.name, f.size))
            .collect(),
    );
    family(
        "weir_rows",
        "gauge",
        "Rows of a dim.",
        (status.dims.iter())
            .map(|d| format!("weir_rows{{dim=\"{}\"}} {}", d.name, d.size))
            .collect(),
    );
    family(
        "weir_table_ready",
        "gauge",
        "1 when asks on the table answer.",
        (status.dims.iter().chain(&status.facts))
            .map(|t| {
                format!(
                    "weir_table_ready{{table=\"{}\"}} {}",
                    t.name,
                    u8::from(t.ready)
                )
            })
            .collect(),
    );
    let count = |c: &AtomicU64| c.load(Ordering::Relaxed);
    family(
        "weir_asks_total",
        "counter",
        "Asks by outcome.",
        [
            ("answered", &asks.answered),
            ("not_ready", &asks.not_ready),
            ("invalid", &asks.invalid),
            ("failed", &asks.failed),
        ]
        .iter()
        .map(|(outcome, c)| format!("weir_asks_total{{outcome=\"{outcome}\"}} {}", count(c)))
        .collect(),
    );
    let mut samples = Vec::new();
    let mut cumulative = 0;
    for (bound, bucket) in BOUNDS.iter().zip(&asks.buckets) {
        cumulative += count(bucket);
        samples.push(format!(
            "weir_ask_seconds_bucket{{le=\"{bound}\"}} {cumulative}"
        ));
    }
    cumulative += count(&asks.buckets[BOUNDS.len()]);
    let seconds = Duration::from_micros(count(&asks.micros)).as_secs_f64();
    samples.extend([
        format!("weir_ask_seconds_bucket{{le=\"+Inf\"}} {cumulative}"),
        format!("weir_ask_seconds_sum {seconds}"),
        format!("weir_ask_seconds_count {cumulative}"),
    ]);
    family(
        "weir_ask_seconds",
        "histogram",
        "Time to answer an ask, for answered asks.",
        samples,
    );
    out
}
