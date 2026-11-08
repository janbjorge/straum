# weir plan

weir keeps declared aggregates (count, sum, min, max, avg, count distinct) over
PostgreSQL tables in memory, fed by logical replication, and answers asks
against them. Background and evidence: `research/`.

## How we work

- One milestone is one commit on `master`, subject `Mx: <title>`, tagged
  `v0.x.0`. Work in progress stays on a branch and is squashed into that one
  commit when the gate passes.
- The milestone commit ticks its boxes here, so `git show v0.x.0` shows the
  plan, the code and the tests for that milestone together.
- Gate, on the crates the milestone touches:
  - `cargo fmt --check`
  - `cargo clippy -p <crate> --all-targets -- -D warnings`
  - `cargo test -p <crate>`
  - `cargo llvm-cov -p <crate> --fail-under-lines 98`, with the measured
    number in the commit body
- Code goes in with the milestone that first uses it.
- Add a dated line under Decisions when something is settled.

## Decisions

- 2026-11-08: Aggregates are declared in weir's own notation, not SQL. No SQL
  query surface for now.
- 2026-11-08: A fact reaches other tables only through `ref`, an N:1 pointer to
  a dim key. Rollups are keyed on fact columns, ref ids and the time bucket.
  Dim attributes are mapped after aggregation.
- 2026-11-08: No expressions. Filters are `=` / `in` on rollup-key columns or ref
  attributes. Ask ranges start and end on bucket edges, so asks never read raw
  rows.
- 2026-11-08: weir reads the spec at start. No build step, no codegen.
- 2026-11-08: Clean architecture as Cargo crates: `weir-domain` (no deps, no
  I/O), `weir-app` (use cases and ports), adapters `weir-notation`, `weir-pg`,
  `weir-http`, and the `weir` binary as composition root. The per-row path
  stays concrete; traits only at the edges.
- 2026-11-08: Simplifications: no raw column store; `append` facts need no row
  store; initial load is one `GROUP BY` in PostgreSQL on the slot snapshot; one
  time level (bucket) merged up at ask time; asks over HTTP/JSON, no pgwire;
  no WAL barrier in v1, answers carry the LSN they reflect.

## M0: notation and domain model (v0.1.0)

Goal: the aggregate model and its text notation, validated, no I/O.
Touches: `crates/weir-domain`, `crates/weir-notation`, `research/`.

- [x] Facts, dims, refs, measures, validation (`Spec::new`)
- [x] Asks and `Spec::check`
- [x] Parser, taxi example, tests
- [ ] Time zones in asks
- [ ] Rewrite `research/ARCHITECTURE.md`, `README.md` and `AKS.md` for the
      notation design (they still describe SQL over views)
- [ ] Gate passes

## M1: taxi proof in SQL (v0.2.0)

Goal: show that rollup-then-map equals the raw join before writing the engine.
Touches: `bench/`. Run: `bench/run.sh` (needs curl, uv and docker).

- [x] Fetch one month of TLC yellow trips and `taxi_zone_lookup.csv` into
      `bench/data/` (gitignored)
- [x] Load into a local `postgres:18` container
- [x] Build the rollup with `GROUP BY` on the rollup key of `taxi.weir`
- [x] For each ask shape, compare raw join and rollup-then-map with `EXCEPT`
      both ways
- [x] Record the collapse ratio (raw rows to rollup rows)

Result on 2024-01, 2,964,624 trips:

- 7 ask shapes, no row differs either way (`EXCEPT ALL`). They cover avg from
  sum and count, filters on by columns and ref attributes, two refs to one dim,
  and ranges on 15 minute edges inside an hour.
- Filtered measures need no state of their own. Their conditions name
  rollup-key parts only, so asks apply them to rollup rows.
- Collapse is 1.3x for the taxi.weir key at 15m (2,322,783 rollup rows):
  pickup and dropoff zone together are nearly unique per trip. 1h gives 1.8x,
  1d 5.7x, and 15m without the dropoff ref 5.2x.

## M2: accumulators and rollup (v0.3.0)

Goal: maintain a fact's rollup in memory and answer asks from it.
Touches: `crates/weir-domain`.

- [ ] Accumulator trait: add, remove, merge, finalize
- [ ] count, sum (i128, promote on overflow), avg, min/max, count distinct
- [ ] Rollup store keyed on `Fact::rollup_key`
- [ ] Ask execution: filter, merge by ref id, map to attributes, merge again
- [ ] Property tests against a naive reference, including updates and deletes
- [ ] Gate passes

## M3: application layer (v0.4.0)

Goal: use cases behind ports, runnable end to end without PostgreSQL.
Touches: `crates/weir-app`.

- [ ] Ports: change source, initial loader, catalog
- [ ] Use cases: load fact, apply commit, answer ask, reload fact
- [ ] In-memory fakes; full flow tested with them
- [ ] Gate passes

## M4: PostgreSQL adapter (v0.5.0)

Goal: feed the use cases from a real PostgreSQL 18.
Touches: `crates/weir-pg`.

- [ ] Catalog check: column types, dim key unique, replica identity
- [ ] Temporary slot, exported snapshot, initial `GROUP BY` load
- [ ] pgoutput decoding; Relation messages checked against the spec
- [ ] `weir plan`: print role, grant and publication SQL
- [ ] Taxi end to end: load, replay changes, compare asks with PostgreSQL
- [ ] Gate passes

## M5: ask endpoint and binary (v0.6.0)

Goal: a runnable `weir` that answers asks over HTTP.
Touches: `crates/weir-http`, `crates/weir`.

- [ ] JSON asks; answers carry LSN and commit time
- [ ] Health, readiness, metrics
- [ ] Binary wiring config, adapters and use cases
- [ ] Gate passes

## M6: deploy (v0.7.0)

Goal: weir on AKS next to Azure Flexible Server 18.
Touches: `deploy/`.

- [ ] Helm chart: Deployment, spec ConfigMap, DSN Secret, no PVC
- [ ] Run against a non-production Azure Flexible Server 18

## Open questions

- Mutable facts: `REPLICA IDENTITY FULL`, or a PK row store in weir?
- Who may ask what: access control per fact in the spec?
- Fallback when weir is not ready: weir renders SQL per ask and sends it to
  PostgreSQL?
- Memory for count distinct on high-cardinality columns.
- Low collapse: a key with two refs can be nearly as wide as the fact (taxi:
  1.3x). Should `weir plan` measure collapse with `count(distinct ...)` and
  warn?
- A ref id with no dim row maps to null attributes (the bench assumes this).
- Azure: `wal_level = logical` restart, resync after HA failover.
