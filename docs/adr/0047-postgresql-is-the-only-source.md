# ADR-0047: PostgreSQL is the only source

Status: Accepted, 2026-11-14

## Context

ADR-0045 let Straum take its tables from a feed: a Unix socket, a
frame protocol and a second binary, `straum-parquet`, that read
Parquet files through a manifest. It saved the benchmark a minute of
PostgreSQL setup. It cost two crates, a second binary, a socket with
no authentication, a manifest format and a guide page, for a mode
that follows nothing. Every way to keep it smaller still needed a
format of its own or a tool beside Straum.

## Decision

Straum takes its tables from PostgreSQL only. `--feed`, `straum-feed`
and `straum-parquet` are gone. `straum-pg` is the one adapter behind
`Source`, and `straum` is the one binary. Demos and benchmarks run on
a throwaway PostgreSQL holding the taxi trips, through `bench/taxi.py`.

## Consequences

- One binary and one way in. The guide has no feed page, manifest or
  frame table.
- `parquet` and `jiff` leave the workspace. Parquet is read by
  pyarrow in `bench/taxi.py` alone, on the way into PostgreSQL.
- A benchmark of Straum alone again starts PostgreSQL in Docker and
  loads the trips into it first: about a minute before Straum loads a
  row.
- `Source` stays a port. Another source comes with a new record.
