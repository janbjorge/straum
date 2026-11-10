# Straum plan

Straum keeps declared aggregates (count, sum, min, max, avg, count distinct) over
PostgreSQL tables in memory, fed by logical replication, and answers asks
against them. Background and evidence: `research/`.

## How we work

- Commits go straight to `main`, one change each, with a short imperative
  subject. No branches, no PRs, no tags.
- A milestone closes with a commit `Mx: <title>` that ticks its last box,
  with the measured coverage in its body, so `git log --grep '^Mx:'` finds
  where the plan, the code and the tests met. The commits that do its
  other boxes tick them. (M0 to M6 were one squashed commit each.)
- Gate, on the crates a commit touches:
  - `cargo fmt --check`
  - `cargo clippy -p <crate> --all-targets -- -D warnings`
  - `cargo test -p <crate>`
  - `cargo llvm-cov -p <crate> ... --fail-under-lines 98` over the touched
    crates and the crates whose tests exercise them (`straum-notation` tests
    much of `straum-domain`). Without rustup, set `LLVM_COV` and
    `LLVM_PROFDATA` to the tools in `$(brew --prefix llvm)/bin`.
- Code goes in with the commit that first uses it.
- Add a dated line under Decisions when something is settled.

Code:

- rustfmt and Clippy defaults, warnings denied. Names and signatures follow
  the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
- No macros of our own (`macro_rules!`, proc macros). std derives and std
  macros only.
- No `unsafe`: every crate has `#![forbid(unsafe_code)]`.
- Plain structs, enums and functions. Traits only at ports. Named fields over
  tuples, whole words over abbreviations.
- No doc comments by default. Names and types carry the what. Write
  `///`, `//!` or `//` only for a why the code can't show: a constraint,
  an invariant, a PostgreSQL quirk, a rejected alternative. Never restate
  the signature or sum up the body. Don't reference callers
  (`// used by X`) or tickets (`// fix for IMP-1234`), and don't leave
  `// TODO`, `// removed` or commented-out code behind.

Tests:

- Every behaviour gets a positive and a negative test.
- State that changes over time gets a property test against a naive
  reference. Planting a bug must make it fail.

## Decisions

- 2026-11-08: Aggregates are declared in Straum's own notation, not SQL. No SQL
  query surface for now. (The notation part is replaced by Straum SQL below.)
- 2026-11-08: A fact reaches other tables only through `ref`, an N:1 pointer to
  a dim key. Rollups are keyed on fact columns, ref ids and the time bucket.
  Dim attributes are mapped after aggregation.
- 2026-11-08: No expressions. Filters are `=` / `in` on rollup-key columns or ref
  attributes. Ask ranges start and end on bucket edges, so asks never read raw
  rows.
- 2026-11-08: Straum reads the spec at start. No build step, no codegen.
- 2026-11-08: Clean architecture as Cargo crates: `straum-domain` (no deps, no
  I/O), `straum-app` (use cases and ports), adapters `straum-notation`, `straum-pg`,
  `straum-http`, and the `straum` binary as composition root. The per-row path
  stays concrete; traits only at the edges.
- 2026-11-08: Simplifications: no raw column store; facts need no row
  store; initial load is one `GROUP BY` in PostgreSQL on the slot snapshot
  (replaced by binary `COPY`, 2026-11-09); one
  time level (bucket) merged up at ask time (replaced by `buckets`,
  2026-11-09); asks over HTTP/JSON, no pgwire;
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
  failure Straum is not ready and the next step starts over from a new stream.
  How asks read while the writer applies is settled in M6.
- 2026-11-08: A change Straum can't apply reads that table again at LSN `at`:
  an update or delete on a fact without the old row, a row that doesn't fit,
  an altered table. Commits up to `at` skip the table, and asks on it answer
  not ready until the stream passes `at`. Other tables keep answering.
- 2026-11-08: Truncate empties the table in place, no reload. A fact applies
  updates and deletes whenever the old row comes (`REPLICA IDENTITY FULL`).
- 2026-11-08: An ask names a time zone (e.g. `Europe/Oslo`). The HTTP
  adapter (M6) turns the name into the zone's UTC offsets over the ask's
  range, so the domain holds no zone data. Buckets stay UTC. Hours follow the
  instant, so a repeated hour stays two hours. Days, weeks (from Monday) and
  months follow the wall clock: a repeated midnight counts from the first one,
  a skipped one from the jump. An ask with a time key is rejected when the
  zone's offsets or changes fall inside a bucket.
- 2026-11-08: Specs move from Straum's notation to Straum SQL, which replaces
  the "own notation, not SQL" decision above. Straum SQL is a small subset of
  PostgreSQL `CREATE VIEW` syntax, made only for Straum. It is not a SQL engine
  and not a query surface: asks stay HTTP/JSON. Anything outside the subset is
  rejected with an error that names the construct, never ignored. Every doc
  and error that mentions Straum SQL says it is a straum-only subset. Each view
  body (the `SELECT`) runs unchanged in PostgreSQL, so it is also the initial
  load query and the reference for tests. The fact's key is a view option,
  `WITH (key = id)`, which `sqlparser` reads as is (`append` went on
  2026-11-09). PostgreSQL rejects these options, so the `CREATE VIEW` itself is not valid
  PostgreSQL; Straum only ever sends the body. (Since the binary `COPY`
  load, 2026-11-09, Straum sends PostgreSQL neither.)
- 2026-11-08: Straum SQL is parsed by the `sqlparser` crate with
  `PostgreSqlDialect`, not by a parser of our own. Straum's code walks the AST,
  keeps the subset and rejects the rest. `sqlparser` is pure Rust, so the
  static build needs no C toolchain once its default `recursive-protection`
  feature (stacker, which compiles C) is off; its parser depth limit still
  turns deep nesting into an error. It stays inside `straum-notation`, and
  `straum-domain` keeps no dependencies.
- 2026-11-08: The AST walker denies by default. Each `match` lists the
  accepted variants, and a final arm returns an error naming the rejected
  construct. Accepted AST structs are destructured with every field named and
  no `..`, so a field added by a `sqlparser` upgrade fails to compile until it
  is handled. The walker returns `Result` with every error found and never
  panics on input. `sqlparser` is pinned to an exact version. Each rejected
  construct gets a negative test.
- 2026-11-09: Straum stays synchronous; only `straum-http` runs tokio. `App`
  becomes a core that never waits, driven by a loop that does the I/O (M6),
  so readers never wait on PostgreSQL and no dropped future can leave a table
  half loaded. `straum-pg` is a synchronous client with one thread that owns
  the stream: it answers keepalives and reports the applied LSN while the
  writer applies or reloads. Protocol code works on bytes, not sockets, so
  going async later touches only the loop, that thread and the port.
- 2026-11-09: straum-pg is our own replication client on `postgres-protocol`
  (message framing, SCRAM); pgoutput v1 is decoded in straum-pg.
  `tokio-postgres` has no replication, `supabase/etl` owns slot creation and
  the copy, and the replication crates on crates.io build C for TLS
  (`research/RUST.md` §4).
- 2026-11-09: TLS waits for M7 and will be `rustls`; the crypto provider
  (`ring`, which needs a C compiler, or pure-Rust `rustls-graviola`) is chosen
  then. Until then straum-pg refuses an `sslmode` that requires TLS.
- 2026-11-09: TLS is `rustls` 0.23 on `rustls-graviola`, so the build
  stays pure Rust; graviola needs AES, AVX, AVX2, ADX, BMI1/2 and PCLMULQDQ
  on x86_64, or AES, NEON, PMULL and SHA2 on aarch64, and otherwise panics,
  which aborts Straum. `sslmode=disable` is plain TCP. Every other connection
  checks the certificate chain and the host name: `require`, `verify-ca`,
  `verify-full` and no `sslmode` all mean that. `allow` and `prefer`, which
  may fall back to plain TCP, are refused. The roots are Mozilla's, built
  in (`webpki-roots`), or the PEM file `sslrootcert` names, read at each
  connect.
- 2026-11-09: A replication connection takes the simple query protocol only,
  so loads arrive as text. The load is one `GROUP BY` per fact on the rollup
  key plus the min, max and count distinct columns, returning the row count
  and sum and count per sum input (replaced by binary `COPY`, 2026-11-09).
  A reload reads one table on a second replication connection with its own
  temporary slot; its consistent point is the load's LSN.
- 2026-11-09: Tests that need PostgreSQL start it themselves with
  `testcontainers` (`blocking`, no default features, so no C), one container
  per test, removed when the test ends. A dev-dependency only; Docker is the
  one thing the gate needs besides Rust.
- 2026-11-09: straum-pg commits the snapshot transaction right after the last
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
- 2026-11-09: straum-http is hyper 1 on tokio, no framework, no macros. JSON
  asks are walked like Straum SQL, deny by default, over `serde_json` without
  derive: unknown fields, keys given twice and wrong types are errors that
  name the field. The time key is a field of its own (`time`), so it can't
  clash with a column. Zones come from jiff with the tz database built in,
  for the distroless image.
- 2026-11-09: The commit time in answers is pgoutput's Commit timestamp; it
  is null when the LSN is where the stream started. Metrics are Prometheus
  text written by hand. Lag, retained WAL and OTLP move to M7, since they
  need the server's WAL position or a collector.
- 2026-11-09: The binary reads its arguments by hand. The DSN comes from
  `STRAUM_DSN` or `--dsn-file`, never the command line. A failed step starts
  over after 1 s, doubling to 60 s; a panic aborts the process, since the
  store may be half written.
- 2026-11-09: The project is Straum everywhere: crates `straum-*`, the
  binary `straum`, `STRAUM_DSN`, metrics `straum_*`, and `straum` as the
  default role and publication, the slot prefix and the `application_name`.
- 2026-11-09: MVP cuts, each able to come back without breaking a client.
  `/metrics` is gone; telemetry waits for OTLP in M7. Asks take no `zone`,
  and `time` takes `bucket` only, printed in UTC; zones and hour, day, week
  and month can return as an optional field and new values, so the tz
  database leaves the binary. A change that doesn't fit a table (a row that
  doesn't fit, a fact change without the old row, an altered table) starts
  over from a new slot instead of reading that table again on a second
  connection, which replaces the reload in the 2026-11-08 decision. Loads
  read only the snapshot `Source::open` made. Input checks stay as strict.
- 2026-11-09: Lag is a time. About once a second the writer reads
  `pg_current_wal_lsn()` and its slot's `confirmed_flush_lsn` and
  `restart_lsn` from `pg_replication_slots`, on a plain connection, since
  the replication connection is busy streaming. The lag is the time since
  the last reading whose WAL the slot had confirmed all of: up to a second
  longer than the truth, never shorter. WAL written during the load counts
  from the start. `/readyz` is off past `--max-lag` (30 s); asks still
  answer, with their LSN. Past `--max-retained-wal` (4Gi) of WAL held for
  the slot, straum starts over, since Azure fixes `max_slot_wal_keep_size`
  at `-1`. A load that takes longer than the cap allows starts over again
  and again, which the log shows. OTLP gets its own box after the Helm
  chart; until then `/readyz` says why straum isn't ready and the log says
  why it started over.
- 2026-11-09: The image is the static musl binary built in
  `rust:1.99.1-alpine`, copied into `gcr.io/distroless/static-debian13:nonroot`,
  both pinned by digest; it runs as user 65532. The chart,
  `deploy/helm/straum` (Helm 4, chart API v2), follows Helm's best practices
  and the Restricted Pod Security Standard, with a read-only root
  filesystem. It reads the DSN only from an existing Secret and never holds
  it. It has no PDB, HPA, Ingress or NetworkPolicy: each replica is a slot
  and a walsender, so straum doesn't scale out. `scripts/check-chart.py`
  runs `helm lint --strict`, helm-unittest and kubeconform (Kubernetes 1.33
  and 1.36); CI runs it and builds the image. The AKS test cluster is never
  used to try the chart.
- 2026-11-09: No OTLP and no metrics for now; telemetry comes later.
  `straum status` prints what a running straum has done instead, from
  `GET /status`. The image has no shell or curl, so the binary is the
  client: `kubectl exec deployment/straum -- /straum status`. `App` keeps
  starts, failures and the last failure again for it.
- 2026-11-09: No `append` view option. Every fact table needs `REPLICA
  IDENTITY FULL`, which `straum plan` prints and every start checks. FULL adds
  WAL on UPDATE and DELETE only, so insert-only tables pay nothing, and every
  update and delete applies in place instead of reading the fact again. This
  settles the mutable-facts question: no row store in Straum.
- 2026-11-09: The client defines every view in the spec and names it in each
  ask. Straum doesn't pick a view for an ask.
- 2026-11-09: A view may declare wider buckets, `WITH (key = id, buckets =
  '1 day, 7 days')`. Each is a wider whole multiple of the width before it
  and starts at the epoch, as `date_bin` does. Straum keeps one level of
  cells per width, updated in the same commit, so there is no `delay` as in
  grain. An ask reads its range from the widest buckets that fit and the
  `date_bin` buckets at its ends; an ask grouped by the bucket reads the
  `date_bin` level. Levels are an index: answers don't change. The load stays
  one `GROUP BY` at the `date_bin` width.
- 2026-11-09: An ask runs a bucket at a time and a column at a time: a
  filter pass and a group pass over the bucket's cell keys, then one merge
  loop per measure over its input's column. Cell states are kept as one
  column per input, still one enum of forms and no trait. Conditions and
  dim keys are worked out once per code an ask meets, and key values are
  numbered without hashing: slot keys by their codes, dim keys by the dim
  column's own codes, time keys in time order. Partials keep four copies
  per group, so adds to one group don't wait on each other. On the taxi
  month in process, the bench asks went from 9 to 70 ns a cell to 1 to 7.
- 2026-11-09: A user guide, built with mdBook from `docs/` and published
  to GitHub Pages by CI on every push to `main`. It covers using and
  running Straum, not `research/` or this plan, and favours no cloud.
  Its examples come from a run of its quick start.
- 2026-11-09: Loads are `COPY (SELECT <columns> FROM <table>) TO STDOUT
  (FORMAT binary)`, one per fact and dim, and Straum groups the rows
  itself. PostgreSQL only scans: no sort or hash that can spill to disk,
  no output functions, and nothing from the spec runs there. A reader
  thread decodes into batches of 8192 rows, four in flight, so a load
  holds about 1.2 MB per column besides the store, however big the
  table.
  `Rollup::add_rows` checks and codes a batch, then fills each bucket
  width on its own thread. The maps a row probes hash with a seeded
  folded multiply, not SipHash, and a min or max cell keeps a single
  value without a tree. Taxi month, 2.96M trips: `GROUP BY` 9.4 s, text
  `COPY` 5.2 s, binary 3.0 s, now 1.9 to 2.0 s, where PostgreSQL alone
  copies the columns to `/dev/null` in 1.4 s. The load still holds
  `AccessShareLock` and pins xmin until the snapshot commits; ctid
  ranges on several connections in the one snapshot, or a standby, if
  that hurts. The stream never starts before the slot's consistent
  point: WAL before it is gone or never held the rows.
- 2026-11-09: Comments, doc comments included, carry only a why the code
  can't show. Most of the existing `///` restate the code, so they go,
  one crate per commit.
- 2026-11-09: A ref id with no dim row groups under null attributes, as
  the view's `LEFT JOIN` does. The store's property test points trips at
  zones that never exist, and `bench/compare.sql` checks the taxi month
  against PostgreSQL.
- 2026-11-09: Straum runs on PostgreSQL 17 and newer, with no upper bound.
  CI runs the tests on 17, 18 and 19rc1 (19 once it ships), picked by
  `STRAUM_TEST_POSTGRES`; locally they run on 18. From 19 a logical slot
  turns logical decoding on under `wal_level = replica` (PostgreSQL
  `67c20979ce7`), so the start check reads `server_version_num` and takes
  `replica` there. Below 19 the slot can't be made under `replica`, and
  PostgreSQL's error names no fix, so the check also runs before the slot.

## M0: notation and domain model

Goal: the aggregate model and its text notation, validated, no I/O.
Touches: `crates/straum-domain`, `crates/straum-notation`, `research/`.

- [x] Facts, dims, refs, measures, validation (`Spec::new`)
- [x] Asks and `Spec::check`
- [x] Parser, taxi example, tests
- [x] Time zones in asks (cut with the MVP cuts, 2026-11-09; M8 plans
      them again)
- [x] Rewrite `research/ARCHITECTURE.md`, `README.md` and `AKS.md` for the
      notation design
- [x] Gate passes: 99.71% lines over `straum-domain` and `straum-notation`

## M1: taxi proof in SQL

Goal: show that rollup-then-map equals the raw join before writing the engine.
Touches: `bench/`. Run: `bench/run.sh` (needs curl, uv and docker).

- [x] Fetch one month of TLC yellow trips and `taxi_zone_lookup.csv` into
      `bench/data/` (gitignored)
- [x] Load into a local `postgres:18` container
- [x] Build the rollup with `GROUP BY` on the rollup key of `taxi.straum`
- [x] For each ask shape, compare raw join and rollup-then-map with `EXCEPT`
      both ways
- [x] Record the collapse ratio (raw rows to rollup rows)

Result on 2024-01, 2,964,624 trips:

- 8 ask shapes, no row differs either way (`EXCEPT ALL`). They cover avg from
  sum and count, filters on by columns and ref attributes, two refs to one dim,
  and ranges on 15 minute edges inside an hour.
- Filtered measures need no state of their own. Their conditions name
  rollup-key parts only, so asks apply them to rollup rows.
- Collapse is 1.3x for the taxi.straum key at 15m (2,322,783 rollup rows):
  pickup and dropoff zone together are nearly unique per trip. 1h gives 1.8x,
  1d 5.7x, and 15m without the dropoff ref 5.2x.

## M2: accumulators and rollup

Goal: maintain a fact's rollup in memory and answer asks from it.
Touches: `crates/straum-domain`.

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
- [x] Gate passes: 99.69% lines over `straum-domain` and `straum-notation`; the 3
      missed lines are `unreachable!` arms

## M3: application layer

Goal: use cases behind ports, runnable end to end without PostgreSQL.
Touches: `crates/straum-app`, `crates/straum-domain`.

- [x] One port, `Source`: setup check and new stream (`open`), snapshot
      loads (`load_fact`, `load_dim`), and the commit stream (`next_commit`)
- [x] Use cases in `App::step`: load every table, apply a commit, read a
      table again; and `App::ask`, which answers with the LSN it reflects
      (since the MVP cuts, 2026-11-09, a change that can't apply starts
      over from a new slot instead of reading the table again)
- [x] In-memory PostgreSQL as a fake source; random writes, dropped
      connections and reloads checked against a fresh load at the same LSN;
      each change that can't apply tested for a reload; 22 planted bugs all
      caught
- [x] Gate passes: 99.75% lines over `straum-domain`, `straum-notation` and
      `straum-app`; the 3 missed lines are `unreachable!` arms

## M4: Straum SQL

Goal: specs in Straum SQL, a PostgreSQL `CREATE VIEW` subset made only for
Straum, parsed by `sqlparser` instead of our own parser.
Touches: `crates/straum-notation`, `crates/straum-domain` (tests), `crates/straum-app`
(tests), `research/`, `bench/`.

- [x] `sqlparser` pinned, without its C-building default feature
- [x] AST walker, deny by default: one view per fact, `WITH (key, append)`,
      `LEFT JOIN` refs, `date_bin` bucket, aggregates with `FILTER`, GROUP BY
      matching the selected keys (`append` went and `buckets` came,
      2026-11-09)
- [x] `taxi.sql` replaces `taxi.straum`; the old lexer and parser are gone
- [x] A negative test for each rejected construct the PostgreSQL dialect can
      produce; `Spec::new` errors SQL can't produce are tested in
      `straum-domain`
- [x] On `postgres:18`: the `taxi.sql` body runs as is, and the view options
      are rejected (`unrecognized parameter "key"`)
- [x] Gate passes: 99.29% lines over `straum-domain`, `straum-notation` and
      `straum-app`; the 12 missed lines are `unreachable!` arms and reject arms
      for syntax the PostgreSQL dialect never produces

## M5: PostgreSQL adapter

Goal: feed the use cases from a real PostgreSQL 18.
Touches: `crates/straum-pg`, `crates/straum-domain`, `crates/straum-app` (docs),
`research/`. Tests start `postgres:18` in Docker themselves; the taxi month:
`cargo test -p straum-pg --release taxi_month -- --ignored --nocapture` after
`bench/run.sh` has fetched the data.

- [x] Catalog check: column types, dim key unique, replica identity,
      publication, privileges; each problem says what to fix
- [x] Temporary slot, exported snapshot, initial `GROUP BY` load; the domain
      takes grouped cells (`Rollup::add_group`), since the load reads cells,
      not rows (now a binary `COPY` of rows, 2026-11-09)
- [x] Confirm that a load at the slot's consistent point holds exactly the
      commits whose end LSN is at or before it (`Commit::lsn`): 2,000
      single-row commits racing the slot's creation, three rounds, none lost
      or applied twice
- [x] Reading one table again from a second connection's snapshot (gone
      with the MVP cuts, 2026-11-09: straum starts over instead)
- [x] pgoutput decoding; Relation messages checked against the spec: a
      retyped column reads the table again, a dropped one stops the stream
      (a retyped one starts over since the MVP cuts)
- [x] A stream thread answers keepalives and reports the applied LSN while
      the writer applies or reloads; a 9 s stall against a 4 s
      `wal_sender_timeout` keeps the stream
- [x] Setup SQL for `straum plan`: role, grant and publication (the command
      comes with the binary in M6)
- [x] Taxi end to end: load, replay changes, compare asks with PostgreSQL
- [x] Gate passes: 99.01% lines over `straum-domain`, `straum-notation`,
      `straum-app` and `straum-pg`; 22 planted bugs in `straum-pg` and the grouped
      load all caught

Result on the taxi month, 2,964,624 trips (release build, laptop Docker):

- The load takes 2.0 s (rerun 2026-11-09; 4.4 s at M5) and gives
  2,322,783 cells, as M1's rollup did. The snapshot transaction, which
  pins xmin, is open that long.
- The whole `taxi.sql` view, every key and measure, equals PostgreSQL's
  after the load, after 202 streamed commits (10,000 inserts, a zone renamed,
  a zone's key changed, 4.9 s) and after an update read the fact again.
- An ask over every key returns 2.3 million groups in 2.4 to 3.0 s; the
  view takes 6.2 to 6.4 s in PostgreSQL (rerun 2026-11-09; 5.3 to 5.7 s
  against 4.8 s at M5). Asks that group coarser are the use case.

## M6: ask endpoint and binary

Goal: a runnable `straum` that answers asks over HTTP.
Touches: `crates/straum-http`, `crates/straum`, `crates/straum-app`,
`crates/straum-domain` (scales), `crates/straum-pg` (commit time, scales),
`research/`. The binary's tests start `postgres:18` in Docker.

- [x] `App` as a core that never waits, driven by a loop that does the I/O
      (`Writer`); asks read while it writes, and go on for the other tables
      while one loads. Checked from another thread in the middle of every
      load of the random run
- [x] JSON asks; answers carry LSN and commit time, numeric values in their
      column's scale
- [x] Health, readiness, metrics (`/metrics` went with the MVP cuts,
      2026-11-09)
- [x] Binary wiring config, adapters and use cases; `straum plan` prints the
      setup SQL; SIGTERM finishes the asks in flight
- [x] End to end: `straum plan`'s SQL sets up PostgreSQL, `straum run` follows
      it as the role the plan made, asks over HTTP equal the view body in
      PostgreSQL before and after changes, and the slot goes when Straum stops
- [x] Gate passes: 98.95% lines over `straum-domain`, `straum-notation`,
      `straum-app`, `straum-pg`, `straum-http` and `straum`; 35 planted bugs all
      caught

Result on the taxi month (release build, laptop Docker): an ask by day and
borough reads every cell and answers 256 groups in 1.4 to 1.5 s, holding the
read lock that long; PostgreSQL takes 3.7 to 4.1 s on the view. The ask over
every key takes 4.1 s. Rerun 2026-11-09, after wider buckets and the
column-at-a-time ask: `taxi_month` asks by borough alone, 8 groups, in 6.6
to 8.8 ms against 3.8 to 4.0 s in PostgreSQL, and every key in 2.4 to
3.0 s.

## M7: deploy

Goal: Straum on AKS next to Azure Flexible Server 18.
Touches: `crates/straum-pg` (TLS, WAL readings), `crates/straum-app`
(lag), `crates/straum-http`, `crates/straum`, `deploy/`, `Dockerfile`.

- [x] TLS to PostgreSQL with `rustls` on `rustls-graviola`. Tests make a CA
      in the `postgres:18` container: replication, a reload and keepalives
      over TLS; an unknown CA, a wrong host name and a server with ssl off
      fail the connect
- [x] Lag and retained WAL from the server's WAL position; readiness off
      past `--max-lag`; start over past `--max-retained-wal`. Tests: the
      lag against every reading kept, over random readings; the cap at the
      start and while streaming; `/readyz` saying how far behind; readings
      from `postgres:18`, as the role `straum plan` makes
- [x] Helm chart: Deployment, spec ConfigMap, DSN Secret, no PVC; plus a
      ClusterIP Service, a ServiceAccount and the `Dockerfile`. Tests: 34
      helm-unittest cases over good values and bad, which ten planted bugs
      each fail; kubeconform; the image run read-only as user 65532 with no
      capabilities against `postgres:18`, ready and answering
- [x] `straum status`: what a running straum has done, from its plain-text
      `GET /status`: ready or why not, lag, WAL held against the cap, LSN
      and commit time, starts, failures and the last failure. Tests: the
      text for each state, the client against a stand-in server's good
      and bad replies, and end to end against `postgres:18`
- [ ] Run against a non-production Azure Flexible Server 18, with
      `straum run` from a machine that reaches the server, not the chart.
      Check that the server's certificate chain verifies against the
      built-in Mozilla roots, or needs `sslrootcert`, which the chart
      can't mount yet; that the 4Gi `--max-retained-wal` default suits a
      server whose `max_slot_wal_keep_size` is fixed at `-1`; and settle
      the Azure open question and `research/AKS.md` open points 2 and 4

## M8: telemetry, zones and the speed harness

Goal: bring back what the MVP cuts deferred, and speed numbers anyone can
rerun.
Touches: `crates/straum-http`, `crates/straum-app`, `crates/straum`,
`deploy/`, `bench/`, `docs/`.

- [ ] Telemetry: what `GET /status` shows (ready or why not, lag, WAL held
      against the cap, LSN and commit time, starts, failures) as OTLP to a
      collector, the plan before the cuts, or as metrics to scrape; the
      chart wires it
- [ ] Ask-time zones and the time keys `hour`, `day`, `week` and `month`,
      as the 2026-11-08 zone decision describes, through an optional
      `zone` field and new `time` values; the tz database comes back into
      the binary
- [ ] The harness behind the Speed table in `docs/src/benchmark.md`,
      committed so it can be rerun: the hourly view, the index on
      `pickup_at` and the `roll` table, the four asks, the `pgbench` runs,
      and the in-process and HTTP timings

## Open questions

- The name: Straum, Nynorsk for a stream (`research/README.md`). Its crate
  names were free on crates.io on 2026-11-09; trademarks are unchecked.
  Check again before anything is published.
- Releases: the crates are 0.0.0 with `publish = false`, the chart is
  0.1.0 with appVersion 0.0.0, and the guide has users build the image
  and `cargo install --path`. When do versions and a published image and
  chart come, and the crates, once the name is cleared?
- Who may ask what: access control per fact in the spec?
- Fallback when Straum is not ready: Straum renders SQL per ask and sends it to
  PostgreSQL?
- Memory for count distinct on high-cardinality columns.
- Low collapse: a key with two refs can be nearly as wide as the fact (taxi:
  1.3x). Should `straum plan` measure collapse with `count(distinct ...)` and
  warn?
- Float columns in sum and avg: order-dependent sums, lossy removal.
- Azure: `wal_level = logical` restart, resync after HA failover.
- A CA bundle in the chart: a value naming a ConfigMap or Secret, mounted
  read-only, for the DSN's `sslrootcert`. Until then a server whose
  certificate needs `sslrootcert` can't be reached through the chart.
- An "as of" time from keepalives, so answers on a quiet database don't
  look stale by their commit time.
- Long asks hold the read lock and delay the writer, and `/readyz` waits
  behind a writer that waits. A coarse ask on the taxi month
  held it 1.5 s; with day and week buckets and the column-at-a-time ask
  it takes 6.6 to 8.8 ms (2026-11-09). An ask over every 15-minute bucket
  still reads every cell, 2.4 to 3.0 s. Cap asks, keep
  the status outside the lock, or both?
