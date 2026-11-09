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
- 2026-11-09: weir stays synchronous; only `weir-http` runs tokio. `App`
  becomes a core that never waits, driven by a loop that does the I/O (M6),
  so readers never wait on PostgreSQL and no dropped future can leave a table
  half loaded. `weir-pg` is a synchronous client with one thread that owns
  the stream: it answers keepalives and reports the applied LSN while the
  writer applies or reloads. Protocol code works on bytes, not sockets, so
  going async later touches only the loop, that thread and the port.
- 2026-11-09: weir-pg is our own replication client on `postgres-protocol`
  (message framing, SCRAM); pgoutput v1 is decoded in weir-pg.
  `tokio-postgres` has no replication, `supabase/etl` owns slot creation and
  the copy, and the replication crates on crates.io build C for TLS
  (`research/RUST.md` §4).
- 2026-11-09: TLS waits for M7 and will be `rustls`; the crypto provider
  (`ring`, which needs a C compiler, or pure-Rust `rustls-graviola`) is chosen
  then. Until then weir-pg refuses an `sslmode` that requires TLS.
- 2026-11-09: A replication connection takes the simple query protocol only,
  so loads arrive as text. The load is one `GROUP BY` per fact on the rollup
  key plus the min, max and count distinct columns, returning the row count
  and sum and count per sum input. A reload reads one table on a second
  replication connection with its own temporary slot; its consistent point is
  the load's LSN.
- 2026-11-09: Tests that need PostgreSQL start it themselves with
  `testcontainers` (`blocking`, no default features, so no C), one container
  per test, removed when the test ends. A dev-dependency only; Docker is the
  one thing the gate needs besides Rust.
- 2026-11-09: weir-pg commits the snapshot transaction right after the last
  load, not at the first stream read. Held open, it pins xmin and blocks DDL
  on the loaded tables; a test's `ALTER TABLE` waited on it.
- 2026-11-09: Asks and the writer share one store behind an `RwLock`.
  `App` is the store and how far it has followed the source, cheap to clone;
  `Writer` is the loop that reads the source. The writer takes the write lock
  only to apply a commit it has read or to swap a table in or out: a load
  takes the table out of the store and fills it outside the lock. So asks
  never wait on PostgreSQL, and asks on the other tables go on during a
  reload. A long ask delays the writer, which shows as lag; copy-on-write
  shards or `left-right` if that matters.
- 2026-11-09: Answers carry the scale of each column (`Answer::scales`). Only
  the source knows column types, so a load sets them (`Rollup::set_scales`,
  `DimTable::set_scales`), and JSON answers print exact decimals.
- 2026-11-09: weir-http is hyper 1 on tokio, no framework, no macros. JSON
  asks are walked like weir SQL, deny by default, over `serde_json` without
  derive: unknown fields, keys given twice and wrong types are errors that
  name the field. The time key is a field of its own (`time`), so it can't
  clash with a column. Zones come from jiff with the tz database built in,
  for the distroless image.
- 2026-11-09: The commit time in answers is pgoutput's Commit timestamp; it
  is null when the LSN is where the stream started. Metrics are Prometheus
  text written by hand. Lag, retained WAL and OTLP move to M7, since they
  need the server's WAL position or a collector.
- 2026-11-09: The binary reads its arguments by hand. The DSN comes from
  `WEIR_DSN` or `--dsn-file`, never the command line. A failed step starts
  over after 1 s, doubling to 60 s; a panic aborts the process, since the
  store may be half written.

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
Touches: `crates/weir-pg`, `crates/weir-domain`, `crates/weir-app` (docs),
`research/`. Tests start `postgres:18` in Docker themselves; the taxi month:
`cargo test -p weir-pg --release taxi_month -- --ignored --nocapture` after
`bench/run.sh` has fetched the data.

- [x] Catalog check: column types, dim key unique, replica identity,
      publication, privileges; each problem says what to fix
- [x] Temporary slot, exported snapshot, initial `GROUP BY` load; the domain
      takes grouped cells (`Rollup::add_group`), since the load reads cells,
      not rows
- [x] Confirm that a load at the slot's consistent point holds exactly the
      commits whose end LSN is at or before it (`Commit::lsn`): 2,000
      single-row commits racing the slot's creation, three rounds, none lost
      or applied twice
- [x] Reading one table again from a second connection's snapshot
- [x] pgoutput decoding; Relation messages checked against the spec: a
      retyped column reads the table again, a dropped one stops the stream
- [x] A stream thread answers keepalives and reports the applied LSN while
      the writer applies or reloads; a 9 s stall against a 4 s
      `wal_sender_timeout` keeps the stream
- [x] Setup SQL for `weir plan`: role, grant and publication (the command
      comes with the binary in M6)
- [x] Taxi end to end: load, replay changes, compare asks with PostgreSQL
- [x] Gate passes: 99.01% lines over `weir-domain`, `weir-notation`,
      `weir-app` and `weir-pg`; 22 planted bugs in `weir-pg` and the grouped
      load all caught

Result on the taxi month, 2,964,624 trips (release build, laptop Docker):

- The load takes 4.4 s and gives 2,322,783 cells, as M1's rollup did. The
  snapshot transaction, which pins xmin, is open that long.
- The whole `taxi.sql` view, every key and measure, equals PostgreSQL's
  after the load, after 202 streamed commits (10,000 inserts, a zone renamed,
  a zone's key changed) and after an update read the fact again (5.5 s).
- An ask over every key returns 2.3 million groups in 5.3 to 5.7 s; the
  view takes 4.8 s in PostgreSQL. Asks that group coarser are the use case.

## M6: ask endpoint and binary

Goal: a runnable `weir` that answers asks over HTTP.
Touches: `crates/weir-http`, `crates/weir`, `crates/weir-app`,
`crates/weir-domain` (scales), `crates/weir-pg` (commit time, scales),
`research/`. The binary's tests start `postgres:18` in Docker.

- [x] `App` as a core that never waits, driven by a loop that does the I/O
      (`Writer`); asks read while it writes, and go on for the other tables
      while one loads. Checked from another thread in the middle of every
      load of the random run
- [x] JSON asks; answers carry LSN and commit time, numeric values in their
      column's scale
- [x] Health, readiness, metrics
- [x] Binary wiring config, adapters and use cases; `weir plan` prints the
      setup SQL; SIGTERM finishes the asks in flight
- [x] End to end: `weir plan`'s SQL sets up PostgreSQL, `weir run` follows
      it as the role the plan made, asks over HTTP equal the view body in
      PostgreSQL before and after changes, and the slot goes when weir stops
- [x] Gate passes: 98.95% lines over `weir-domain`, `weir-notation`,
      `weir-app`, `weir-pg`, `weir-http` and `weir`; 35 planted bugs all
      caught

Result on the taxi month (release build, laptop Docker): an ask by day and
borough reads every cell and answers 256 groups in 1.4 to 1.5 s, holding the
read lock that long; PostgreSQL takes 3.7 to 4.1 s on the view. The ask over
every key takes 4.1 s.

## M7: deploy

Goal: weir on AKS next to Azure Flexible Server 18.
Touches: `deploy/`.

- [ ] TLS to PostgreSQL with `rustls`; pick the crypto provider
- [ ] Lag and retained WAL from the server's WAL position; readiness off
      past a lag bound; OTLP
- [ ] Helm chart: Deployment, spec ConfigMap, DSN Secret, no PVC
- [ ] Run against a non-production Azure Flexible Server 18

## Open questions

- The name: weir is a working name (`research/README.md`: a low dam that
  measures a river's flow, as weir measures the replication stream). Check
  clashes (crates.io, GitHub, trademarks) before anything is published.
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
- An "as of" time from keepalives, so answers on a quiet database don't
  look stale by their commit time.
- Long asks hold the read lock and delay the writer, and `/readyz` and
  `/metrics` wait behind a writer that waits. A coarse ask on the taxi month
  holds it 1.5 s. Cap asks, keep the status outside the lock, or both?
