# ADR-0002: Crates depend inward around a synchronous core

Status: Accepted 2026-11-10; revised 2026-11-26

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
enforced by `scripts/check-layers.py` over Cargo's whole resolved graph
and the files a crate pulls in by path: the domain and the use cases
reach no outside crate and name no `std` I/O, and only `straum-http`
reaches an async runtime. The domain holds what a spec and
an ask mean, never Straum SQL's syntax: no keyword, function or
operator of it in a domain string, enforced by
`scripts/check-domain-syntax.py`. An `AskError` is data; straum-notation
words it, and the binary hands those words to straum-http beside the
ask reader (`AskSyntax`). A dependency check alone let ask syntax
collect in the domain's error text: 13 strings at e4e18d1.
Traits exist only at the port (`Source`), and `scripts/check-layers.py`
refuses a trait defined outside `straum-app`; the per-row path is
concrete structs, enums and functions.

Straum is synchronous. `App` is a core that never waits, driven by a
`Writer` loop that does the I/O and applies commits in order.
`straum-pg` is a synchronous client with one thread that owns the
stream, answering keepalives and reporting the applied LSN while the
writer applies or loads. Protocol code works on bytes, not sockets.
Only `straum-http` runs tokio, as hyper 1 with no framework and no
macros.

One store sits behind an `RwLock`. `App` is the store and how far it
has followed the source, cheap to clone. The writer takes the write
lock only to apply commits it has already read, or to swap a table in
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
  hands it a reader and the words for an ask's errors.
- `AskError` has no `Display`: the crate that knows the syntax says it.
- Readers never wait on PostgreSQL, and asks on other tables go on
  during a load.
- A panic aborts the process, since the store may be half written. A
  failed step ends it with exit 1 (ADR-0009).
- A long ask delays the writer, which shows as lag: an ask over every
  15-minute bucket of the taxi month holds the lock 2.4 to 3.0 s.
  After one wait for the lock, the writer applies every commit that
  the stream has queued. Thus a long ask delays a batch, and not each
  commit. A batch stops when the queue is empty, or when a reading of
  the WAL is due, about once a second.
  `/readyz` and `/status` don't wait behind it. They read only the
  progress, which the writer never holds while it waits for the
  store's lock.
