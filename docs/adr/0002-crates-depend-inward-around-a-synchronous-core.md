# ADR-0002: Crates depend inward around a synchronous core

Status: Accepted 2026-11-10; revised 2026-11-17

## Context

One crate would let the per-row path reach the socket and the JSON
reader in one step; traits everywhere would put dynamic dispatch on the
hot path for abstractions nothing uses twice. Async everywhere, the
Rust default for a server, would put awaits on the per-row path, let a
dropped future leave a table half loaded, and make readers wait on
PostgreSQL whenever the writer does. Asks read while the writer applies
commits and loads tables.

## Decision

Straum is clean architecture as Cargo crates: `straum-domain`, with no
dependencies and no I/O; `straum-app`, with the use cases and the
ports; the adapters `straum-notation`, `straum-pg` and `straum-http`;
and the `straum` binary as composition root. Crates depend inward only,
enforced by `scripts/check-layers.py`. Traits exist only at the port
(`Source`); the per-row path is concrete structs, enums and
functions.

Straum is synchronous. `App` is a core that never waits, driven by a
`Writer` loop that does the I/O and applies commits in order.
`straum-pg` is a synchronous client with one thread that owns the
stream, answering keepalives and reporting the applied LSN while the
writer applies or loads. Protocol code works on bytes, not sockets.
Only `straum-http` runs tokio, as hyper 1 with no framework and no
macros.

One store sits behind an `RwLock`. `App` is the store and how far it
has followed the source, cheap to clone. The writer takes the write
lock only to apply a commit it has already read, or to swap a table in
or out: a load takes the table out and fills it outside the lock. How
far it has followed, its lag and its WAL readings, sit behind a mutex
of their own, so the status and the readings never wait on an ask. A
lock per table, copy-on-write shards and `left-right` were passed over;
the last two are the next step if the lock ever matters.

## Consequences

- The domain is property-tested against a naive reference with no
  PostgreSQL, the app against an in-memory fake source.
- A new adapter touches only its crate and the composition root.
- Test helpers can't cross crates, so small ones (`Rng`, `id()`) are
  copied per crate. `straum-http` knows no Straum SQL; the binary
  hands it a reader.
- Readers never wait on PostgreSQL, and asks on other tables go on
  during a load.
- A panic aborts the process, since the store may be half written. A
  failed step starts over after 1 s, doubling to 60 s.
- A long ask delays the writer, which shows as lag, and `/readyz` waits
  behind it: an ask over every 15-minute bucket of the taxi month holds
  the lock 2.4 to 3.0 s.
