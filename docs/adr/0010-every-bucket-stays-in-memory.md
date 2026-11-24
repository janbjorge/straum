# ADR-0010: Every bucket stays in memory

Status: Accepted 2026-11-11; revised 2026-11-24

## Context

Since refs bind at ask time (ADR-0006), the warehouse copy holds 2,378
MiB, and most asks read recent buckets. The narrowest width holds most
cells: 76% of the taxi month's, 50% of the warehouse copy's. Old
buckets could stay, be dropped, or move out of memory.

## Decision

Every bucket of every width stays in memory for as long as Straum
runs, unless the spec keeps the width for a time (ADR-0004). Memory is
sized for the whole range the spec's facts hold, and a restart loads
from PostgreSQL.

A width written `keep('<width>', '<duration>')` keeps its buckets from
a horizon on: PostgreSQL's time less the duration, floored to the
next wider width, never moving back. PostgreSQL's time is the
snapshot's `now()` at the load and each streamed commit's time after,
so a quiet database moves no horizon and no clock of Straum's own
decides what it holds. Buckets before the horizon are dropped and
never made again; the wider widths keep every row, and an ask that
would read a dropped bucket is refused, naming the horizon. The
narrowest width holds most cells, so keeping it for a recent window
bounds memory by time, not by the tables' size, and needs no disk.
A dim change moves bound cells level by level, since the narrowest
level no longer holds every row's time.

Retention is opt-in and drops data rather than moving it: PostgreSQL
stays the source of truth, so an old bucket is one restart away and
nothing need be kept on disk to answer for it. Spill stays out for the
reasons below, and retention answers the same memory with none of them.

Sealing cold `date_bin` buckets out to a spill, behind a `Spill` port
with an Arrow IPC adapter, was built in the domain and the app and
taken out again on 2026-11-16 before an adapter shipped: it added a
port, a second cell form, unseal-on-write and a 500 path for a cost
nobody had yet hit.

`--max-memory`, off unless set, stops Straum with exit 1 (ADR-0009)
once its resident memory passes the limit: a spec too big for its pod
is otherwise killed by the kernel with no line saying why, and on a
shared host the kernel may kill something else. Straum reads the
figure from Linux's `/proc/self/status`, since `std` has no call for
it and the file needs no `unsafe`; where it can't be read, a set limit
fails the start rather than pass for no limit. A cap on cells was
rejected: a cell took from 33 to 177 bytes in the benchmark's runs, so
a count of cells bounds bytes only loosely.

## Consequences

- One cell form; an ask, a change and a dim change never wait on disk.
- Steady memory grows with the range the facts hold. A spec that holds
  too much range for its pod keeps its narrow widths for a time, or
  narrows. With `--max-memory` set, that is a stop with a line naming
  the limit, not a kill by the kernel.
- A width kept for a time answers only from its horizon on; an ask
  further back reads a wider width. The widest width always keeps
  every bucket, so memory still grows with the range, at its rate.
- `GET /views` and `straum views` show each width's cells, so an
  operator sees which width costs the memory.
- Not yet: keeping the widest width for a time, and a sliding window.
- The root filesystem stays read-only and Straum keeps nothing on
  disk (ADR-0012).
- Bringing spill back starts from `git log -p` on this record and on
  `crates/straum-domain/src/sealed.rs`.
