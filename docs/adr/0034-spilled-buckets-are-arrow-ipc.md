# ADR-0034: Spilled buckets are Arrow IPC

Status: Accepted, 2026-11-11

## Context

ADR-0033 leaves the storage of sealed buckets to an adapter. A sealed
bucket is fixed-width integer columns, read whole by an ask. A format
of our own costs code to keep; Parquet decodes on every read; Vortex
and Lance are young and heavy; a key-value store keeps a row per cell.

## Decision

The adapter `straum-arrow` writes each sealed bucket as one Arrow IPC
record batch under `--spill-dir`, through the `arrow-ipc` and
`arrow-array` crates without compression. Files are read with plain
reads, not mmap.

## Consequences

- An ask reads a bucket's columns without decoding them. The page cache
  keeps hot files warm; reads copy.
- No mmap, so no second `unsafe` (ADR-0027).
- The Arrow crates are Apache-2.0, as Straum is.
- `straum-arrow` depends on `straum-app` and `straum-domain`, and joins
  `scripts/check-layers.py`.
- The chart mounts an `emptyDir` for `--spill-dir`, since the root
  filesystem is read-only (ADR-0022).
