# Entry point to every tool. The commands in AGENTS.md and
# docs/src/performance.md stay the reference.
#
# With no crate named, a recipe takes the crates changed since HEAD,
# untracked files included. Name crates to override: just test straum-pg
# or just check "straum-pg straum-domain".
#
# Every setting below has a default; override it on the command line or
# through its environment variable:
#   just postgres=17 seeds=50 correctness
#   STRAUM_TEST_POSTGRES=17 just test straum-pg
# just --evaluate prints what a run will use.

# PostgreSQL major of the tests, benchmarks and try
postgres := env("STRAUM_TEST_POSTGRES", "18")
# Random workloads of the physical stream: how many, from which seed
seeds := env("STRAUM_TEST_SEEDS", "6")
seed_from := env("STRAUM_TEST_SEED_FROM", "0")
# Reads of each kind in wal-bench; the fastest counts
runs := env("STRAUM_BENCH_RUNS", "3")
# The WAL Straum reads in taxi: logical or physical
wal := env("STRAUM_TAXI_WAL", "logical")
# Line coverage cov fails under
coverage := env("STRAUM_COVERAGE", "98")

export STRAUM_TEST_POSTGRES := postgres
export STRAUM_TEST_SEEDS := seeds
export STRAUM_TEST_SEED_FROM := seed_from
export STRAUM_BENCH_RUNS := runs

changed := `{ git diff --name-only HEAD -- crates; git ls-files -o --exclude-standard -- crates; } | cut -d/ -f2 | sort -u | paste -sd' ' -`

[private]
default:
    @just --list

# fmt, clippy, test and layers for the changed crates
[group('check')]
check crates=changed: (_named crates) fmt (lint crates) (test crates) layers

# Format the workspace
[group('check')]
fmt:
    cargo fmt --all

# Clippy with warnings as errors
[group('check')]
lint crates=changed: (_named crates)
    cargo clippy {{ prepend('-p ', crates) }} --all-targets -- -D warnings

# Tests; args go to cargo test: just test straum-pg -- tests::name
[group('check')]
test crates=changed *args: (_named crates)
    cargo test {{ prepend('-p ', crates) }} {{ args }}

# Line coverage gate; add the crates whose tests exercise these
[group('check')]
cov crates=changed: (_named crates)
    cargo llvm-cov {{ prepend('-p ', crates) }} --all-targets --fail-under-lines {{ coverage }}

# Crates depend inward only
[group('check')]
layers:
    scripts/check-layers.py

# Physical stream against random workloads, as CI's correctness job
[group('check')]
correctness:
    cargo test -p straum-pg -- tests::commits_what_logical_decoding_commits_at_random tests::follows_random_workloads_from_a_random_start

# Helm chart checks
[group('check')]
chart:
    scripts/check-chart.py

# Compose, ECS and systemd deployment checks (needs uv and Docker)
[group('check')]
deploy:
    scripts/check-deploy.py

# Serve the book with live reload
[group('docs')]
docs:
    mdbook serve docs --open

# Build the book as CI does
[group('docs')]
docs-build:
    mdbook build docs

# Taxi trips in a throwaway PostgreSQL: asks, transactions or shell
[group('taxi')]
taxi mode="asks":
    bench/taxi.py {{ mode }} --wal {{ wal }}

# taxi.sql view against PostgreSQL (loads the taxi month, needs uv)
[group('taxi')]
taxi-month:
    cargo test -p straum-pg --release taxi_month -- --ignored --nocapture

# Physical WAL read speed over the taxi month (needs uv)
[group('taxi')]
wal-bench:
    cargo test -p straum-pg --release physical_wal_bench -- --ignored --nocapture

# January on the published image: one ask, a delete, a borough move
[group('taxi')]
demo:
    bench/demo.py

# Straum on a copy of your tables: --spec and a pg_dump as --data, then straum shell
[group('try')]
[no-cd]
try *flags:
    {{ justfile_directory() }}/bench/try_your_data.py --postgres {{ postgres }} {{ flags }}

[private]
_named crates:
    @[ -n "{{ crates }}" ] || { echo "no crate changed since HEAD; name one, as in just test straum-pg" >&2; exit 1; }
