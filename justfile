# Entry point to every tool, locally and in CI. Docs name the recipe;
# just --show RECIPE prints the command it runs.
#
# With no crate named, a recipe takes the crates changed since HEAD,
# untracked files included. Name crates to override: just test straum-pg
# or just check "straum-pg straum-domain". all takes the whole workspace,
# as CI does: just test all.
#
# Every setting below has a default; override it on the command line or
# through its environment variable:
#   just postgres=17 seeds=50 correctness
#   STRAUM_TEST_POSTGRES=17 just test straum-pg
# just --evaluate prints what a run will use.

# A backtick runs only when a recipe reads its variable, so just --list,
# docs and fmt need neither git nor uv
set lazy

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

changed := `scripts/changed-crates.py`
# What all names: every crate of the workspace
all := "all"

[private]
default:
    @just --list

# fmt, clippy, test and the script checks for the changed crates
[group('check')]
check crates=changed: (_named crates) fmt (lint crates) (test crates) scripts

# Format the workspace and this file; just fmt --check only checks, as CI does
[group('check')]
[positional-arguments]
fmt *args:
    cargo fmt --all "$@"
    {{ quote(just_executable()) }} --justfile {{ quote(justfile()) }} --fmt "$@"

# Clippy with warnings as errors
[group('check')]
lint crates=changed: (_named crates)
    cargo clippy {{ if crates == all { '--workspace' } else { prepend('-p ', crates) } }} --all-targets -- -D warnings

# Tests; args go to cargo test: just test straum-pg -- tests::name
[group('check')]
test crates=changed *args: (_named crates)
    cargo test {{ if crates == all { '--workspace' } else { prepend('-p ', crates) } }} {{ args }}

# Line coverage gate; add the crates whose tests exercise these
[group('check')]
cov crates=changed: (_named crates)
    cargo llvm-cov {{ if crates == all { '--workspace' } else { prepend('-p ', crates) } }} --all-targets --fail-under-lines {{ coverage }}

# Layers, no Straum SQL in straum-domain, and the tests of the check and bench scripts
[group('check')]
scripts:
    scripts/check-layers.py
    scripts/check-domain-syntax.py
    scripts/test-checks.py
    bench/test-stack.py

# Physical stream against random workloads, as CI's correctness job
[group('check')]
correctness: (test "straum-pg" "--" "tests::commits_what_logical_decoding_commits_at_random" "tests::follows_random_workloads_from_a_random_start")

# Capture the heap fixtures of the postgres major again, after a WAL layout change
[group('check')]
heap-fixtures: (test "straum-pg" "--lib" "capture_heap_fixture" "--" "--ignored")

# Helm chart checks
[group('check')]
chart:
    scripts/check-chart.py

# Build the image from compose.yaml and run its help, as CI does
[group('check')]
image:
    docker compose build straum
    docker compose run --rm straum help

# Build the book as CI does; just docs serve --open for live reload
[group('docs')]
[positional-arguments]
docs command="build" *flags:
    mdbook "$@" docs

# Taxi trips in a throwaway PostgreSQL: asks, transactions, materialized or shell
[group('taxi')]
taxi mode="asks":
    bench/taxi.py {{ mode }} --wal {{ wal }}

# taxi.sql view against PostgreSQL (loads the taxi month, needs uv)
[group('taxi')]
taxi-month: (test "straum-pg" "--release" "taxi_month" "--" "--ignored" "--nocapture")

# Physical WAL read speed over the taxi month (needs uv)
[group('taxi')]
wal-bench: (test "straum-pg" "--release" "physical_wal_bench" "--" "--ignored" "--nocapture")

# An ask and a zone change on a store the size of the taxi month, against their time budgets
[group('taxi')]
cost-bench: (test "straum-domain" "--release" "--test" "cost" "--" "--ignored" "--nocapture")

# January on the published image: one ask, a delete, a borough move
[group('taxi')]
demo:
    bench/demo.py

# The trigger outbox of ADR-0018 on the taxi trips: its checks, then its cost
[group('taxi')]
outbox:
    bench/outbox.py

# Straum on a copy from straum plan --copy: SPEC --data DIR, then straum shell
[group('try')]
[no-cd]
[positional-arguments]
try *flags:
    {{ quote(justfile_directory() / "bench/try_your_data.py") }} --postgres {{ postgres }} "$@"

# Fails when no crate is named and none changed: cargo with no -p would
# take the whole workspace
[private]
_named crates:
    {{ if trim(crates) == "" { error("no crate changed since HEAD; name one, as in just test straum-pg") } else { "" } }}
