# weir plan

weir keeps declared aggregates (count, sum, min, max, avg, count distinct) over
PostgreSQL tables in memory, fed by logical replication, and answers asks
against them. Background and evidence: `research/`.

## How we work

- One milestone is one commit on `main`, subject `Mx: <title>`. No tags.
  Work in progress stays on a branch and is squashed into that one commit
  when the gate passes.
- The milestone commit ticks its boxes here, so `git log --grep '^Mx:'` finds
  the plan, the code and the tests for that milestone together.
- Gate, on the crates the milestone touches:
  - `cargo fmt --check`
  - `cargo clippy -p <crate> --all-targets -- -D warnings`
  - `cargo test -p <crate>`
  - `cargo llvm-cov -p <crate> ... --fail-under-lines 98` over the touched
    crates and the crates whose tests exercise them (`weir-notation` tests
    much of `weir-domain`), with the measured number in the commit body.
    Without rustup, set `LLVM_COV` and `LLVM_PROFDATA` to the tools in
    `$(brew --prefix llvm)/bin`.
- Code goes in with the milestone that first uses it.
- Add a dated line under Decisions when something is settled.

Code:

- rustfmt and Clippy defaults, warnings denied. Names and signatures follow
  the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
- No macros of our own (`macro_rules!`, proc macros). std derives and std
  macros only.
- No `unsafe`: every crate has `#![forbid(unsafe_code)]`.
- Plain structs, enums and functions. Traits only at ports. Named fields over
  tuples, whole words over abbreviations.
- Comments say why, not what.

Tests:

- Every behaviour gets a positive and a negative test.
- State that changes over time gets a property test against a naive
  reference. Planting a bug must make it fail.

## Decisions

- 2026-11-08: Aggregates are declared in weir's own notation, not SQL. No SQL
  query surface for now. (The notation part is replaced by weir SQL below.)
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
- 2026-11-08: Cell state is kept per input column and shared by measures: sum
  and avg share sum and count, min and max share value counts. Filtered
  measures keep none (M1). One enum of cell states, no accumulator trait.
- 2026-11-08: Sum, avg, min and max read integers: integer columns, and
  numeric columns as scaled `i64` (M5 checks the catalog). Sums are `i128`,
  which can't overflow below 2^64 rows.
- 2026-11-08: One port, `Source`. The first load reads the snapshot of the
  slot the stream comes from, on the same connection, so loading and
  streaming are not split. `Source::open` checks the setup.
- 2026-11-08: One writer applies commits in order (`App::step`); after a
  failure weir is not ready and the next step starts over from a new stream.
  How asks read while the writer applies is settled in M6.
- 2026-11-08: A change weir can't apply reads that table again at LSN `at`:
  an update or delete on a fact without the old row, a row that doesn't fit,
  an altered table. Commits up to `at` skip the table, and asks on it answer
  not ready until the stream passes `at`. Other tables keep answering.
- 2026-11-08: Truncate empties the table in place, no reload. A fact applies
  updates and deletes whenever the old row comes (`REPLICA IDENTITY FULL`),
  `append` or not.
- 2026-11-08: An ask names a time zone (e.g. `Europe/Oslo`). The HTTP
  adapter (M6) turns the name into the zone's UTC offsets over the ask's
  range, so the domain holds no zone data. Buckets stay UTC. Hours follow the
  instant, so a repeated hour stays two hours. Days, weeks (from Monday) and
  months follow the wall clock: a repeated midnight counts from the first one,
  a skipped one from the jump. An ask with a time key is rejected when the
  zone's offsets or changes fall inside a bucket.
- 2026-11-08: Specs move from weir's notation to weir SQL, which replaces
  the "own notation, not SQL" decision above. weir SQL is a small subset of
  PostgreSQL `CREATE VIEW` syntax, made only for weir. It is not a SQL engine
  and not a query surface: asks stay HTTP/JSON. Anything outside the subset is
  rejected with an error that names the construct, never ignored. Every doc
  and error that mentions weir SQL says it is a weir-only subset. Each view
  body (the `SELECT`) runs unchanged in PostgreSQL, so it is also the initial
  load query and the reference for tests. The fact's key and `append` are view
  options, `WITH (key = id, append = true)`, which `sqlparser` reads as is.
  PostgreSQL rejects these options, so the `CREATE VIEW` itself is not valid
  PostgreSQL; weir only ever sends the body.
- 2026-11-08: weir SQL is parsed by the `sqlparser` crate with
  `PostgreSqlDialect`, not by a parser of our own. weir's code walks the AST,
  keeps the subset and rejects the rest. `sqlparser` is pure Rust, so the
  static build needs no C toolchain once its default `recursive-protection`
  feature (stacker, which compiles C) is off; its parser depth limit still
  turns deep nesting into an error. It stays inside `weir-notation`, and
  `weir-domain` keeps no dependencies.
- 2026-11-08: The AST walker denies by default. Each `match` lists the
  accepted variants, and a final arm returns an error naming the rejected
  construct. Accepted AST structs are destructured with every field named and
  no `..`, so a field added by a `sqlparser` upgrade fails to compile until it
  is handled. The walker returns `Result` with every error found and never
  panics on input. `sqlparser` is pinned to an exact version. Each rejected
  construct gets a negative test.

## M0: notation and domain model

Goal: the aggregate model and its text notation, validated, no I/O.
Touches: `crates/weir-domain`, `crates/weir-notation`, `research/`.

- [x] Facts, dims, refs, measures, validation (`Spec::new`)
- [x] Asks and `Spec::check`
- [x] Parser, taxi example, tests
- [x] Time zones in asks
- [x] Rewrite `research/ARCHITECTURE.md`, `README.md` and `AKS.md` for the
      notation design
- [x] Gate passes: 99.71% lines over `weir-domain` and `weir-notation`

## M1: taxi proof in SQL

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

## M2: accumulators and rollup

Goal: maintain a fact's rollup in memory and answer asks from it.
Touches: `crates/weir-domain`.

- [x] Cell state per input column: sum and count, value counts for min and
      max, value counts for count distinct
- [x] Rows added and removed; a cell goes with its last row; removing a row
      that was never added is an error
- [x] Rollup keyed on `Fact::rollup_key`; dim rows kept apart and read at ask
      time
- [x] Ask execution: range, filter, group by keys, dim attributes and time
      keys, measure filters
- [x] Property test against a naive reference over random inserts, updates and
      deletes of trips and zones; 9 planted bugs all caught
- [x] Gate passes: 99.69% lines over `weir-domain` and `weir-notation`; the 3
      missed lines are `unreachable!` arms

## M3: application layer

Goal: use cases behind ports, runnable end to end without PostgreSQL.
Touches: `crates/weir-app`, `crates/weir-domain`.

- [x] One port, `Source`: setup check and new stream (`open`), snapshot
      loads (`load_fact`, `load_dim`), and the commit stream (`next_commit`)
- [x] Use cases in `App::step`: load every table, apply a commit, read a
      table again; and `App::ask`, which answers with the LSN it reflects
- [x] In-memory PostgreSQL as a fake source; random writes, dropped
      connections and reloads checked against a fresh load at the same LSN;
      each change that can't apply tested for a reload; 22 planted bugs all
      caught
- [x] Gate passes: 99.75% lines over `weir-domain`, `weir-notation` and
      `weir-app`; the 3 missed lines are `unreachable!` arms

## M4: weir SQL

Goal: specs in weir SQL, a PostgreSQL `CREATE VIEW` subset made only for
weir, parsed by `sqlparser` instead of our own parser.
Touches: `crates/weir-notation`, `crates/weir-domain` (tests), `crates/weir-app`
(tests), `research/`, `bench/`.

- [x] `sqlparser` pinned, without its C-building default feature
- [x] AST walker, deny by default: one view per fact, `WITH (key, append)`,
      `LEFT JOIN` refs, `date_bin` bucket, aggregates with `FILTER`, GROUP BY
      matching the selected keys
- [x] `taxi.sql` replaces `taxi.weir`; the old lexer and parser are gone
- [x] A negative test for each rejected construct the PostgreSQL dialect can
      produce; `Spec::new` errors SQL can't produce are tested in
      `weir-domain`
- [x] On `postgres:18`: the `taxi.sql` body runs as is, and the view options
      are rejected (`unrecognized parameter "key"`)
- [x] Gate passes: 99.29% lines over `weir-domain`, `weir-notation` and
      `weir-app`; the 12 missed lines are `unreachable!` arms and reject arms
      for syntax the PostgreSQL dialect never produces

## M5: PostgreSQL adapter

Goal: feed the use cases from a real PostgreSQL 18.
Touches: `crates/weir-pg`.

- [ ] Catalog check: column types, dim key unique, replica identity
- [ ] Temporary slot, exported snapshot, initial `GROUP BY` load; the domain
      takes grouped cells, since the load reads cells, not rows
- [ ] Confirm that a load at the slot's consistent point holds exactly the
      commits whose end LSN is at or before it (`Commit::lsn`)
- [ ] Reading one table again from a second connection's snapshot
- [ ] pgoutput decoding; Relation messages checked against the spec
- [ ] `weir plan`: print role, grant and publication SQL
- [ ] Taxi end to end: load, replay changes, compare asks with PostgreSQL
- [ ] Gate passes

## M6: ask endpoint and binary

Goal: a runnable `weir` that answers asks over HTTP.
Touches: `crates/weir-http`, `crates/weir`.

- [ ] JSON asks; answers carry LSN and commit time
- [ ] Health, readiness, metrics
- [ ] Binary wiring config, adapters and use cases
- [ ] Gate passes

## M7: deploy

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
- A ref id with no dim row maps to null attributes. The bench and `Store` do
  this; confirm.
- Float columns in sum and avg: order-dependent sums, lossy removal.
- Azure: `wal_level = logical` restart, resync after HA failover.
