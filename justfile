# Shortcuts for the commands in AGENTS.md and docs/src/benchmark.md.
# Optional: those commands stay the reference.

[private]
default:
    @just --list

# fmt, clippy, test, layers for one crate
[group('check')]
check crate:
    cargo fmt --all
    cargo clippy -p {{crate}} --all-targets -- -D warnings
    cargo test -p {{crate}}
    python3 scripts/check-layers.py

# Coverage gate (98% lines) for one crate
[group('check')]
cov crate:
    cargo llvm-cov -p {{crate}} --all-targets --fail-under-lines 98

# Helm chart checks
[group('check')]
chart:
    python3 scripts/check-chart.py

# Serve the book with live reload
[group('docs')]
docs:
    mdbook serve docs --open

# Build the book as CI does
[group('docs')]
docs-build:
    mdbook build docs

# straum shell on the taxi trips in a throwaway PostgreSQL
[group('run')]
try:
    bench/bench.py shell

# Taxi trips benchmark, Straum against PostgreSQL
[group('bench')]
bench *args:
    bench/bench.py {{args}}

# Cost of one large transaction
[group('bench')]
bench-transaction:
    bench/transaction.py

# taxi.sql view against PostgreSQL (needs bench data)
[group('bench')]
taxi-month:
    cargo test -p straum-pg --release taxi_month -- --ignored --nocapture
