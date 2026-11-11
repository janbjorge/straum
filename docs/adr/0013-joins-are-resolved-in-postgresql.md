# ADR-0013: Joins are resolved in PostgreSQL

Status: Superseded by ADR-0030

## Context

A ref is an N:1 pointer from a fact to a dim. Before M12 Straum held
the dim rows and joined in memory, first after aggregation, then at
ingest for refs whose via columns weren't selected. Held dims cost 897
MiB on a copy of a warehouse's tables, and every engine that joins outside the
database holds that mapping (Quass et al. 1996; ClickHouse
dictionaries; Flink and RisingWave lookup joins).

## Decision

The source resolves refs. The load is one binary `COPY` of the fact
`LEFT JOIN`ed to its dims. Each commit's fact rows get their attributes
from one `SELECT ... WHERE (key) IN (...)` per ref, at most 1,000 keys
a statement, on the plain connection. A ref's selected attributes are
rollup keys like `by` columns; a ref adds no slot of its own and asks
name `ref.attr`, never the ref. Dims leave the publication and the
stream; they need `SELECT` and a primary key or unique index on the
key columns, which may be several.

## Consequences

- Straum holds no dim rows (ADR-0002): 84 MiB instead of 897 on the
  warehouse copy.
- Dims are read as of ingest. A dim change rewrites nothing; a fact row
  that came in under an old value and then changes is not found, so
  Straum starts over (ADR-0015).
- A ref id with no dim row groups under null attributes, as the view's
  `LEFT JOIN` does.
- A stream of fact changes costs PostgreSQL one keyed `SELECT` per ref
  and transaction. A bounded key cache is the lever if that hurts.
- A lookup between a commit and its apply can see a newer dim value.
  A snapshot per commit would need a second slot each; not taken.
