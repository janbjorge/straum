# Run `just` to list the recipes.
#
# lint, test, cov and check take crate names, or all for the workspace.
# With no crate named, they take the crates that changed since HEAD:
#
#   just test
#   just test straum-pg -- NAME
#   just check "straum-pg straum-http"
#   just lint all
#
# A recipe with *args gives them to its tool: just taxi --help.
# postgres sets the PostgreSQL major: just postgres=17 taxi.

# A backtick runs only when a recipe reads its variable, so fmt, docs and
# image need no uv
set lazy
# "$@" in a recipe holds its arguments
set positional-arguments

# The tests and the bench scripts read the major from STRAUM_TEST_POSTGRES,
# with the same default
postgres := env("STRAUM_TEST_POSTGRES", "18")
export STRAUM_TEST_POSTGRES := postgres

# The crates that changed since HEAD, untracked files included
changed := `scripts/changed-crates.py`

[private]
default:
    @just --list --unsorted

# Stop when no crate is named and none changed: cargo with no -p takes the
# whole workspace
[private]
_named crates:
    {{ if trim(crates) == "" { error("no crate changed since HEAD; name one, as in just test straum-pg") } else { "" } }}

# Run fmt, lint, test and scripts
[group('check')]
check crates=changed: (_named crates) fmt (lint crates) (test crates) scripts

# Format the Rust code and this file; just fmt --check only checks
[group('check')]
fmt *args:
    cargo fmt --all "$@"
    just --fmt "$@"

# Run clippy; a warning fails
[group('check')]
lint crates=changed: (_named crates)
    cargo clippy {{ if crates == "all" { "--workspace" } else { prepend("-p ", crates) } }} --all-targets -- -D warnings

# Run the tests; args go to cargo test: just test straum-pg -- NAME
[group('check')]
test crates=changed *args: (_named crates)
    cargo test {{ if crates == "all" { "--workspace" } else { prepend("-p ", crates) } }} {{ args }}

# Fail under 98% line coverage; name each crate whose tests run the code
[group('check')]
cov crates=changed: (_named crates)
    cargo llvm-cov {{ if crates == "all" { "--workspace" } else { prepend("-p ", crates) } }} --all-targets --fail-under-lines 98

# Run the checks in scripts/ and the tests of scripts/ and bench/
[group('check')]
scripts:
    scripts/check-layers.py
    scripts/check-domain-syntax.py
    scripts/check-test-filters.py
    scripts/check-openapi.py
    scripts/test-checks.py
    bench/test-stack.py

# Test both streams on random workloads; STRAUM_TEST_SEEDS=50 runs 50
[group('check')]
correctness: (test "straum-pg" "--" "tests::decoding::commits_what_logical_decoding_commits_at_random" "tests::decoding::follows_random_workloads_from_a_random_start" "tests::decoding::follows_random_workloads_from_a_random_logical_start")

# Write the heap fixtures again after a WAL layout change, for each postgres major
[group('check')]
heap-fixtures: (test "straum-pg" "--lib" "capture_heap_fixture" "--" "--ignored")

# Check Cargo.lock against the RustSec advisories; needs cargo-audit
[group('check')]
audit:
    cargo audit

# Check the Helm chart
[group('check')]
chart:
    scripts/check-chart.py

# Build the image from compose.yaml and run its help
[group('check')]
image:
    docker compose build straum
    docker compose run --rm straum help

# Build the guide; just docs serve --open shows it while you edit
[group('check')]
docs command="build" *args:
    mdbook "$@" docs

# Compare Straum with PostgreSQL on the taxi trips; --help lists the modes
[group('bench')]
taxi *args:
    bench/taxi.py "$@"

# Compare the taxi.sql view with PostgreSQL through the taxi month
[group('bench')]
taxi-month: (test "straum-pg" "--release" "taxi_month" "--" "--ignored" "--nocapture")

# Time the physical WAL reads; STRAUM_BENCH_RUNS=6 reads six times
[group('bench')]
wal-bench: (test "straum-pg" "--release" "physical_wal_bench" "--" "--ignored" "--nocapture")

# Time an ask and a zone change on a store the size of the taxi month
[group('bench')]
cost-bench: (test "straum-domain" "--release" "--test" "cost" "--" "--ignored" "--nocapture")

# Check the trigger outbox of ADR-0018 on the taxi trips, then time it
[group('bench')]
outbox:
    bench/outbox.py

# Run Straum on a copy from straum plan --copy: just try SPEC --data DIR
[group('try')]
[no-cd]
try *args:
    {{ quote(justfile_directory() / "bench/try_your_data.py") }} "$@"

# Make the usage events from the Azure Functions trace and run Straum on them; --days 1 is quicker
[group('try')]
usage-events *args:
    bench/load_usage_events.py --out bench/data/usage-events "$@"
    cargo run --quiet --package straum -- plan bench/usage.sql --copy bench/data/usage-events
    bench/try_your_data.py bench/usage.sql --data bench/data/usage-events

# Run the published image on January's trips; --tag X.Y.Z picks a release
[group('try')]
demo *args:
    bench/demo.py "$@"
