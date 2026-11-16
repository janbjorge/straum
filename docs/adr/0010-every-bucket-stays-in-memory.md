# ADR-0010: Every bucket stays in memory

Status: Accepted 2026-11-11; revised 2026-11-16

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

## Consequences

- One cell form; an ask, a change and a dim change never wait on disk.
- Steady memory grows with the range the facts hold. A spec that holds
  too much range for its pod has no answer short of a narrower spec.
- The root filesystem stays read-only and Straum keeps nothing on
  disk (ADR-0012).
- Bringing spill back starts from `git log -p` on this record and on
  `crates/straum-domain/src/sealed.rs`.
