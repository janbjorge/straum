# Straum plan

Straum keeps declared aggregates (count, sum, min, max, avg, count distinct)
over PostgreSQL tables in memory, fed by logical replication, and answers
asks against them. Background and evidence: `research/`.

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
- A settled decision gets a record in `docs/adr/` (ADR-0001).

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
Touches: `bench/`. Run: `bench/run.sh` (needs curl, uv and docker;
`bench/bench.py` replaced it, 2026-11-10).

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
- [x] Gate passes: 99.69% lines over `straum-domain` and `straum-notation`;
      the 3 missed lines are `unreachable!` arms

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
Touches: `crates/straum-notation`, `crates/straum-domain` (tests),
`crates/straum-app` (tests), `research/`, `bench/`.

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
`bench/bench.py` has fetched the data.

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
      `straum-app` and `straum-pg`; 22 planted bugs in `straum-pg` and the
      grouped load all caught

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
to 8.8 ms against 3.8 to 4.0 s in PostgreSQL on the view, and every key
in 2.4 to 3.0 s.

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
- [ ] Ask-time zones and the time keys `hour`, `day`, `week` and `month`
      through a zone parameter and new time keys beside `bucket`; the tz
      database comes back into the binary. Buckets stay UTC. Hours follow
      the instant, so a repeated hour stays two hours; days, weeks (from
      Monday) and months follow the wall clock. An ask whose zone changes
      inside a bucket is rejected. Over a `timestamp` column, which has
      no zone (ADR-0018), a zone parameter is rejected by name: there is
      no instant to move
- [x] The harness behind the Speed table in `docs/src/benchmark.md`,
      committed so it can be rerun: the hourly view, the index on
      `pickup_at` and the `roll` table, the four asks, the `pgbench` runs,
      and the in-process and HTTP timings (`bench/bench.py`, 2026-11-10:
      six asks, timed over HTTP and PostgreSQL's protocol from one
      client; no `roll` table, `pgbench` or in-process timings)

## M9: real schemas

Completed 2026-11-10 (`28730ea` and its preparatory commits).

- [x] Facts no longer declare a `key`: `REPLICA IDENTITY FULL` supplies
      their old rows.
- [x] `timestamp` columns are wall-clock times. Their view uses
      `timestamp 'epoch'`, while `timestamptz` uses `timestamptz 'epoch'`.
- [x] Dims and refs can use several columns. A fact column may be both a
      by key and a ref's via column.
- [x] Null-ness keys make a nullable fact column groupable and filterable.
- [x] `bench/pio/run.py` checks the pio-shaped tasks views against
      PostgreSQL before and after streamed changes.

## M10: start over cheaply

Completed 2026-11-10 (`bc9fb3b` and its preparatory commits).

- [x] `--max-transaction-rows` bounds transaction buffering. A transaction
      above the cap starts from a new snapshot after its commit.
- [x] Infinite times are null times. Other unreadable values name their
      table and column before the restart.

## M11: attributes bound at ingest

Completed 2026-11-10 (`af6c406`).

- [x] A selected dim attribute whose ref id is absent is bound at ingest.
      This trades current dim values for fewer cells. `bench/pio` measures
      the late and early forms.

## M12: joins resolved in PostgreSQL

Goal: Straum holds cells and nothing else. A ref's attributes come with
each fact row, resolved where the dims live, so the join machinery of M9
to M11 goes: no dim rows in memory, no late binding, no two bindings.
Touches: every crate, `docs/`, `bench/pio`. Completed 2026-11-10.

- [x] Spec: a ref reads the dim columns the view selects, which key the
      cells like by columns; a join must select one; a FILTER attribute
      must be selected. `Binding`, `Key::Ref`, `ViaNotKey`, `CompositeRef`,
      `EarlyRef` and `Unbound` are gone, and a ref is no key
- [x] Source: fact rows come whole. The load is one binary `COPY` of the
      fact joined to its dims; each commit's fact rows get their attributes
      from one keyed `SELECT` per ref on the side connection. Dims leave
      the publication, the stream, the decoder and the store
- [x] Dims as of ingest: a dim change is no change to Straum; new rows read
      the new value; a row that came in under the old value and changes
      starts Straum over. Checked in the fake, against PostgreSQL and end
      to end
- [x] `bench/pio` on the copy of pio's tables, and the docs say what the
      store holds now
- [x] Gate passes: 98.97% lines over every crate

Result on the copy of pio's tables (release build, laptop Docker): ready
in 19 s and 84 MiB resident, against 897 MiB and 8 s with the dim rows
held in M11; 311,969 hour cells and 66,107 day cells; every ask equal to
PostgreSQL after the load and after four streamed transactions of 1,000
to 19,024 rows, applied in 74 ms to 3.4 s; a task group recategorised
showed once a task of it changed and Straum started over.

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
- Long asks hold the read lock and delay the writer, and `/readyz` waits
  behind a writer that waits. A coarse ask on the taxi month
  held it 1.5 s; with day and week buckets and the column-at-a-time ask
  it takes 6.6 to 8.8 ms (2026-11-09). An ask over every 15-minute bucket
  still reads every cell, 2.4 to 3.0 s. Cap asks, keep
  the status outside the lock, or both?
- Deferred after M11 (2026-11-10): `AT TIME ZONE` in views, since a
  `timestamp` is wall clock; keeping the old store answering during a
  reload (pio's Redis cache is the stale fallback); naming a poison row by
  `ctid` or key in the failure text; `uuid` and `date` kinds, enums and
  custom types through their label text, `numeric` above precision 18;
  matching relations by OID instead of `(schema, name)`; generated
  columns, which need `publish_generated_columns = stored` and are silent
  today; a `straum estimate` for cell counts, which `bench/pio` counts
  in SQL instead; per-table reload, and a chunked apply of one
  transaction.
- Since M12 a dim lookup reads PostgreSQL as it is when the commit is
  applied, not as it was at the commit, so a dim row changed between the
  two binds the newer value. A snapshot at the commit's LSN would need a
  second slot per commit. Is the window ever wide enough to matter?
- Since M12 a stream of fact changes costs PostgreSQL one keyed `SELECT`
  per ref and transaction. `bench/pio` shows what pio's rate costs; a
  bounded cache of recent keys is the lever if a source writes faster.
- Product limits, not M9 regressions: facts must be plain, unpartitioned
  tables; a fact needs `REPLICA IDENTITY FULL`; column-list publications
  must include every column Straum reads and publications cannot filter a
  tracked table's rows. The readable type set is integer, fixed
  `numeric(p <= 18)`, boolean, text, `timestamp` and `timestamptz`.
  Floats remain rejected because their reversible sums are
  order-dependent. Specs allow one fact and N:1 `LEFT JOIN` refs, not
  arbitrary joins or expressions. Time grains divide a UTC day and use
  an epoch-origin `date_bin`; calendar grains, custom origins and
  sub-second grains are unsupported.
