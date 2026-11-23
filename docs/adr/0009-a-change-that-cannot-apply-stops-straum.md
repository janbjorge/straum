# ADR-0009: A change that can't apply stops Straum

Status: Accepted 2026-11-10; revised 2026-11-23

## Context

Some changes can't be applied to cells: a row that doesn't fit, an
altered table, a transaction too big to buffer, a slot holding too
much WAL. The first design read one table again at the
failing LSN while the others kept answering. Readiness needs a
measure of how far behind Straum is; the applied LSN means nothing
without the server's, and a commit time stops on a quiet database.

## Decision

Straum fails hard. Any failure of the writer, at a start or while
following, logs the error and ends the process with exit 1. Whatever
runs Straum starts it again, and the new process loads from a new slot.
Its snapshot is after the failing commit, so those rows arrive through
the bulk load (ADR-0007) instead of being held and applied under the
write lock. Starting over inside the process, after a backoff of 1 s
doubling to 60 s, was dropped on 2026-11-17: each failure left state to
reset by hand, and every supervisor already restarts with a backoff of
its own. A writer whose step failed takes no more steps, and `/status`
dropped the starts, failures and last failure it kept across starts.
`--max-transaction-rows` (100000) caps the rows of the spec's tables
a transaction may change, a default measured by `bench/taxi.py
transactions`. A row counts once, however many views and dims read its
table; counting a change per view stopped a spec with two views over
one table at half the rows the flag names. Past
`--max-retained-wal` (4Gi) Straum stops too, since Azure fixes
`max_slot_wal_keep_size` at `-1`, and past `--max-memory`, off unless
set (ADR-0010). Past `--max-load-memory` (1Gi) Straum stops as well:
while the tables load, the stream thread reads on and holds every
commit it reads, where it used to stop reading once 64 commits or
200,000 changes waited, and the flag bounds those commits in bytes, as
ADR-0010 bounds memory in bytes rather than a count. The thread still
reports nothing applied until the load ends, so the server keeps the
load's WAL, and the WAL cap still ends a load too long for it. A value
Straum can't read (`NaN`,
an overflowing numeric, non-UTF-8 text) fails the load naming table
and column.

The writer's check needs the writer moving, and a stuck writer still
has its stream thread answering keepalives, which keeps
`wal_sender_timeout` from ever ending the session. So a watchdog thread
on its own connection reads `pg_current_wal_lsn() - restart_lsn` for
the slot every second, from the slot's making on, loads included. Past
the limit it shuts the stream's sockets, the load's too; a walsender
sees its socket close and ends, which drops the temporary slot. Each
second the slot is still there, it runs `pg_terminate_backend` on the
slot's session. A role may end its own sessions, so it needs no grant.
One `pg_terminate_backend` first was rejected: a walsender whose client
reads nothing takes it, then blocks sending the client its FATAL for
as long as the socket stays full, and keeps the slot (PostgreSQL 17,
18 and 19rc2; still there after 100 seconds). A second one, coming
while it is blocked, ends it without sending. Ten seconds without a
reading end the stream the same way: an unwatched slot has no
bound. A second watchdog thread, which touches no network, keeps that
time, so a reader stuck in DNS, a connect or a silent server can't
stretch it. Every connection also gives a server 10 seconds of silence
while it starts before giving up. Checking from the stream thread was
rejected: it stops reading the socket while its queue is full, which
a stuck writer causes, so it never learns the server's position then.

About once a second the writer reads `pg_current_wal_insert_lsn()` and
its slot's `confirmed_flush_lsn` and `restart_lsn`. The insert position,
not the write position: under `synchronous_commit = off` a commit is
acknowledged before its WAL is written, and an `as_of` read from the
write position led it. At a page's start the insert position points
past the page header, where no record ends, so Straum reads it as the
page's start. Lag is the time since
the last reading whose WAL the slot had wholly confirmed; WAL written
during a load counts from its start. `/readyz` is off past `--max-lag`
(30 s), while asks still answer. The same reading gives `as_of`
(ADR-0005) and the retained WAL.

## Consequences

- One recovery path, the process's own start, and no retry inside it
  to test. A setup that can never work fails each start, and the
  supervisor's restart count shows it.
- No thread of Straum's has to move for the WAL limit to hold; the
  watchdog costs one more connection and one query a second. A server
  the watchdog can't reach for 10 seconds stops Straum.
- A network cut between Straum and the server is beyond the watchdog:
  it can't reach the server, and shut sockets send the server nothing.
  A load's session, blocked writing `COPY` rows, then lasts until the
  server's TCP gives up, unless the server sets `tcp_user_timeout`.
- Asks answer not ready while a load runs, and get no answer between a
  failure and the next start, as the port closes; the failed run's last
  log lines say why it stopped.
- A load slower than the WAL cap allows fails each start, which the
  supervisor's restarts show. So does a load whose stream holds more
  commits than `--max-load-memory`, and a spec whose cells pass
  `--max-memory`.
- Lag is up to a second longer than the truth, never shorter. The
  readings need no grant beyond the tables'.
- Per-table reload and a chunked apply are open, not planned.
