# ADR-0008: Only straum-http runs tokio

Status: Accepted (retroactive), 2026-11-10

## Context

Async everywhere is the Rust default for a server. It would put awaits
on the per-row path, let a dropped future leave a table half loaded,
and make readers wait on PostgreSQL whenever the writer does.

## Decision

Straum is synchronous. `App` is a core that never waits, driven by a
`Writer` loop that does the I/O and applies commits in order.
`straum-pg` is a synchronous client with one thread that owns the
stream, answering keepalives and reporting the applied LSN while the
writer applies or loads. Protocol code works on bytes, not sockets.
Only `straum-http` runs tokio, for hyper.

## Consequences

- Readers never wait on PostgreSQL, and no dropped future can leave the
  store half written.
- A panic aborts the process, since the store may be half written; a
  failed step starts over after 1 s, doubling to 60 s.
- Going async later touches only the loop, the stream thread and the
  port.
- `straum-http` is hyper 1 on tokio, no framework, no macros.
