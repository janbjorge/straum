# ADR-0010: Straum is its own replication client

Status: Accepted (retroactive), 2026-11-10

## Context

Straum reads logical replication. `tokio-postgres` has no replication
mode, `supabase/etl` owns slot creation and the copy, and the
replication crates on crates.io build C for TLS, which the static
build can't take (`research/RUST.md` section 4).

## Decision

`straum-pg` is Straum's own replication client on `postgres-protocol`
for message framing and SCRAM. pgoutput v1 is decoded in `straum-pg`.
The slot is temporary, so it goes when Straum stops, and the first
load reads the snapshot of the slot the stream comes from, on the same
connection, so loading and streaming are one consistent point.

## Consequences

- The build stays pure Rust and static.
- Straum owns the protocol surface it uses and nothing more: simple
  query protocol on the replication connection, binary `COPY`, pgoutput.
- Keepalives and the applied LSN are Straum's to answer (ADR-0008).
- A second plain connection reads the WAL position and resolves dim
  lookups, since the replication connection is busy streaming.
