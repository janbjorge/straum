# ADR-0009: A change that can't apply starts Straum over

Status: Accepted 2026-11-10; revised 2026-11-16

## Context

Some changes can't be applied to cells: a row that doesn't fit, an
altered table, a transaction too big to buffer, a slot holding too
much WAL. The first design read one table again at the
failing LSN while the others kept answering. Readiness needs a
measure of how far behind Straum is; the applied LSN means nothing
without the server's, and a commit time stops on a quiet database.

## Decision

Straum starts over from a new slot. Its snapshot is after the failing
commit, so those rows arrive through the bulk load (ADR-0007) instead
of being held and applied under the write lock.
`--max-transaction-rows` (100000) caps the changes a transaction may
hold, a default measured by `bench/taxi.py transactions`. Past
`--max-retained-wal` (4Gi) Straum starts over too, since Azure fixes
`max_slot_wal_keep_size` at `-1`. A value Straum can't read (`NaN`,
an overflowing numeric, non-UTF-8 text) fails the load naming table
and column.

The writer's check needs the writer moving, and a stuck writer still
has its stream thread answering keepalives, which keeps
`wal_sender_timeout` from ever ending the session. So a watchdog thread
on its own connection reads `pg_current_wal_lsn() - restart_lsn` for
the slot every second, from the slot's making on, loads included. Past
the limit it runs `pg_terminate_backend` on the slot's session, which
drops the temporary slot, and shuts the stream's sockets, the load's
too. A role may end its own sessions, so it needs no grant. Ten seconds
without a reading end the stream the same way: an unwatched slot has no
bound. A second watchdog thread, which touches no network, keeps that
time, so a reader stuck in DNS, a connect or a silent server can't
stretch it. Every connection also gives a server 10 seconds of silence
while it starts before giving up. Checking from the stream thread was
rejected: it stops reading the socket while its queue is full, which
every long load causes, so it never learns the server's position then.

About once a second the writer reads `pg_current_wal_lsn()` and its
slot's `confirmed_flush_lsn` and `restart_lsn`. Lag is the time since
the last reading whose WAL the slot had wholly confirmed; WAL written
during a load counts from its start. `/readyz` is off past `--max-lag`
(30 s), while asks still answer. The same reading gives `as_of`
(ADR-0005) and the retained WAL.

## Consequences

- One recovery path to test, not per-table reload plus restart.
- No thread of Straum's has to move for the WAL limit to hold; the
  watchdog costs one more connection and one query a second. A server
  the watchdog can't reach for 10 seconds starts Straum over.
- A network cut between Straum and the server is beyond the watchdog:
  it can't reach the server, and shut sockets send the server nothing.
  A load's session, blocked writing `COPY` rows, then lasts until the
  server's TCP gives up, unless the server sets `tcp_user_timeout`.
- Asks answer not ready while a load runs; `/readyz` says why and the
  log says why Straum started over.
- A load slower than the WAL cap allows starts over again and again,
  which the log shows.
- Lag is up to a second longer than the truth, never shorter. The
  readings need no grant beyond the tables'.
- Per-table reload and a chunked apply are open, not planned.
