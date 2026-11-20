# ADR-0010: Every bucket stays in memory

Status: Accepted 2026-11-11; revised 2026-11-20

## Context

Since refs bind at ask time (ADR-0006), the warehouse copy holds 2,378
MiB, and most asks read recent buckets. The narrowest width holds most
cells: 76% of the taxi month's, 50% of the warehouse copy's. Old
buckets could stay, be dropped, or move out of memory.

## Decision

Every bucket of every width stays in memory for as long as Straum
runs. Memory is sized for the whole range the spec's facts hold, and a
restart loads from PostgreSQL.

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
  too much range for its pod has no answer short of a narrower spec.
  With `--max-memory` set, that is a stop with a line naming the
  limit, not a kill by the kernel.
- The root filesystem stays read-only and Straum keeps nothing on
  disk (ADR-0012).
- Bringing spill back starts from `git log -p` on this record and on
  `crates/straum-domain/src/sealed.rs`.
