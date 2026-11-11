# Glossary

One name per thing. Bold is the term to write; `code` after it is the
code's name where it differs; "also" lists other spellings found and
where. The clashes at the end are still open.

## Straum

- **Straum**: the product, in prose. `straum`: binary, crate prefix,
  role, publication, slot prefix, `application_name`, chart, and the
  command in `straum plan` (ADR-0026).
- **Straum SQL**: the `CREATE VIEW` subset specs and asks are written
  in (ADR-0029). Also "the subset", "the notation" (crate name,
  ADR-0028), "the canon" (ADR-0029).
- **Guide**: the mdBook in `docs/src`. Also "user guide", "the docs".
- **ADR**: one settled decision in `docs/adr/`. Wins over everything.
- **Port**: a trait at the edge of `straum-app` (`Source`, `Spill`).
  **Adapter**: a crate implementing one (`straum-pg`, `straum-http`).

## Spec

- **Spec** (`Spec`): the file of `CREATE VIEW` statements; also the
  `--spec` flag.
- **View** (`Fact`): one `CREATE VIEW`; what asks read `FROM`. Errors
  about a view say "view"; `straum plan` and the setup check say
  "fact" and "dim" for a table's role.
- **Fact table** (`Fact::table`): the table in `FROM`.
- **Dim** (`Dim`): a joined table, held projected to its key and the
  columns the views read. One per table, named after it. Also "joined
  table".
- **Dim key** (`Dim::key`): the dim's key columns a join equals. Not a
  view key.
- **Spec table** (`Member`): a fact or dim table. Also "the spec's
  tables", "every table the spec names".
- **Table**: `schema.table AS alias`, written whole
  (`straum_domain::Table`). `catalog::Checked` is a spec table with its
  columns and types; PostgreSQL's side is `Relation` (pgoutput and
  catalog).
- **Alias** (`Ref::alias`): `AS name` after a table.
- **Name** (`Identifier`): `AS name` after a `SELECT` item; what asks use.
  Also "output name" (ADR-0029), "select name" (JOINS.md).
- **View option**: `WITH (buckets = '...')`, the only one.
- **Flag**: a CLI option (`--max-lag`). The chart says value (`maxLag`).

## Keys and joins

- **Key**: a selected non-measure column: fact column, dim column,
  null-ness key or bucket. Resolves to `Key::By` or `Key::Attr`.
- **By key** (`ByKey`, `Fact::by_keys`): a key from a fact column. Code and
  ADRs say "by key"; the guide says key.
- **Null-ness key** (`ByKey::IsNull`): `(f.col IS NULL) AS name`. Also
  `Role::Nullness`.
- **Dim column** (`Attribute`, `Ref::reads`, `Dim::attributes`): a dim
  column the view selects, read as the dim is now. Code, ADRs and
  JOINS.md say "attribute".
- **Join**: `INNER JOIN`, `LEFT OUTER JOIN`, `EXISTS`, `NOT EXISTS`.
  Has no value and no name asks can use.
- **Ref** (`Ref`, `RefKind`): the join as the domain holds it. Code
  and ADR-0030 only; errors and the guide say join.
- **Join kind** (`RefKind`): left, inner, semi, anti.
- **Join column** (`Via::Fact`): the fact column a join reads; keys the
  cells whether selected or not. Also "via", "join id" (sizing.md,
  ADR-0030).
- **Chain** (`Via::Attribute`): a join keyed from a dim joined before,
  which then carries the column. JOINS.md: "near side", "far dim".
- **Projection** (`Projection`): a dim held as key to attribute codes
  per row. ARCHITECTURE.md: "multimap".

## Time

- **Time column** (`Time::column`): the column inside `date_bin`.
- **Bucket**: one span of time, and the time key's name. `Bucket` in
  `rollup.rs` is the cells of one span.
- **Bucket width** (`Width`): `<n> seconds|minutes|hours|days`.
  ADR-0017 says "grain".
- **Narrowest width** (`Time::width`): the `date_bin` width. Also
  "finest" (ADR-0017, ADR-0028). `Time::wider` holds the wider ones.
- **Level** (`Level`): the cells of one width (ADR-0017,
  how-it-works.md). spec.md says only width.
- **Origin** (`Origin`): `timestamptz 'epoch'` or `timestamp 'epoch'`.
- **Bucket edge**: a multiple of the `date_bin` width; both bounds
  must be on one.
- **Wall clock**: how a `timestamp` reads, no zone applied (ADR-0018).

## Measures and cells

- **Measure** (`Measure`): a named aggregate with optional `FILTER`.
- **Aggregate** (`Aggregate`): the function: count, sum, min, max, avg,
  count distinct. Walker: "aggregate", "an aggregated column".
- **Input column** (`Aggregate::input`, `Input`): the column a measure reads.
  Also `Role::Number`.
- **Cell**: one rollup key and bucket, with row count and states.
  `Cells` is every cell of one bucket.
- **Cell state** (`State`, `StateKind`): sum and count, count per value
  ("extent" in code), distinct counts (ADR-0016).
- **Rollup** (`Rollup`): every cell of one fact, per width. Guide says
  it once (sizing.md).
- **Rollup key** (`Fact::rollup_key`): by keys, join columns, bucket.
  Guide: "cell key", "keys, join columns and bucket".
- **Key part** (`KeyPart`): one part of the rollup key. Code only.
  ADR-0037 says "slot".
- **Codes** (`Codes`): a column's values under small integers.
- **Store** (`Store`): all rollups and projections, behind one
  `RwLock`.
- **Group** (`Groups`, `Partials`): one answer row's key combination.

## Asks and answers

- **Ask** (`Ask`): the `POST /ask` request. `request` in
  `straum-http`.
- **Query**: the ask's `query` field, one `SELECT` over a view.
- **Parameter** (`Literal`): a `$n` value; integer, string or boolean.
- **Literal** (`Literal`): a constant in a spec `FILTER` or `WHERE`.
- **Condition** (`Condition`): `key = value`, `key IN (...)`, `IS [NOT]
  NULL`. **Filter**: a list of them: measure `FILTER`, view `WHERE`
  (`Fact::filter`, JOINS.md "standing filter"), ask `WHERE`. `Match` in
  `store.rs`.
- **Range** (`from`, `to`): `[from, to)` on bucket edges; its two
  **bounds** are `bucket >= $n` and `bucket < $m`. Tests: "window".
- **Limit**: `--max-lag`, `--max-retained-wal`, `--max-transaction-rows`
  (`Limits`, `Transaction::limit`).
- **Cover** (`Rollup::cover`): the buckets an ask reads for its range.
- **Answer** (`Answer`; `Answered` adds `applied` and `as_of`):
  `{"as_of", "rows"}`. Tests: "reply"; `straum-http`: "response".
- **Row** (`AnswerRow`): one answer object. Also a table row.
- **as_of** (`Lag::caught_up`): every commit before it is in the
  answer. Status prints "as of".
- **Straum-LSN**, **Server-Timing**: response headers.
- **Datum** (`Datum`, `DatumError`): a stored column value. **Value**
  (`Value`): a measure result. Guide says "value" for both and for parameters.
- **Column type** (`ColumnType`): what the load learns per column; a
  filter value must fit it (`AskError::WrongType`).
- **Scale**: digits past the point of a numeric.

## Running

- **Start** (`Writer::start`, `Step::Started`): new slot, load every
  table, stream. Status counts "starts". **Load**: one table's `COPY`,
  or all of them.
- **Start over**: after any failure, drop the stream and start again
  (ADR-0015). **Restart** is PostgreSQL or a pod only; ADR-0033 says
  restart for the process.
- **Backoff**: 1 s doubling to 60 s between starts.
- **States**: Loading, Ready, Lagging, Waiting, Stopping (running.md).
  No enum; `following: None` and `why_not`.
- **Ready** (`Status::ready`, `/readyz`): loaded and within
  `--max-lag`.
- **Lag** (`Lag`): time since last caught up with how far PostgreSQL
  has written (`Wal::written`). Also "behind".
- **Caught up** (`Lag::caught_up`): last reading the slot had confirmed
  all of.
- **Snapshot** (`Snapshot`): the database at the slot's consistent
  point, on the replication connection.
- **Consistent point** (`Snapshot::at`): the slot's start LSN. Guide:
  "LSN L".
- **Stream** (`Stream`): commits from the consistent point on. Straum
  follows a table; `Following` is where it stands.
- **Applied** (`Following::applied`, `Status::applied`,
  `Answered::applied`): the last commit in the store. **Committed**
  (`Commit::committed`, `Status::committed`): when it committed.
- **Stream thread** (`StreamThread`, thread `straum-stream`): owns the
  socket, decodes ahead, answers keepalives. **Reader thread**: decodes
  `COPY` batches in a load.
- **Writer** (`Writer`): the one loop that loads and applies.
- **Step** (`Writer::step`): a start or one commit applied. A failed
  step is a **failure** (`Failure`, `Status::failure`).
- **Commit** (`Commit`): one transaction's changes. "Transaction" only
  for `--max-transaction-rows`.
- **Change** (`Change`): insert, update, delete, truncate, or `Unfit`:
  an altered table, a missing old row, a value left out.
- **Apply** (`State::apply`): put a commit in the store under the lock.
- **Unfit** (`Change::Unfit`, `Error::Unfit`): a change that no longer
  fits its table. ADR-0015 says "can't apply".
- **Source** (`Source`, `Postgres`): the port to PostgreSQL. "Source of
  truth" means PostgreSQL itself.
- **Plain connection** (`Postgres::plain`, `Connection`): reads the WAL
  position each second (ADR-0010). ARCHITECTURE.md says "side connection".
- **WAL retained** (`Wal::retained`, `--max-retained-wal`,
  `maxRetainedWal`): bytes the server keeps for the slot.
- **Status** (`Status`): `/status` and `straum status`. Not telemetry
  (ADR-0023). The stream thread **reports** the applied LSN to
  PostgreSQL (`StreamThread::report`).
- **Health check**: `/livez`, `/readyz`.

## PostgreSQL

- **Setup**: what a table owner runs once; `straum plan` prints it.
  **Setup check** (`Error::Setup`, `catalog::check`): catalog against
  spec at every start. Also "start checks" (spec.md), "catalog check"
  (ADR-0018, ADR-0037).
- **Problem**: one line of a failed setup check, with its fix.
- **Role**: the PostgreSQL role (`--role`). A column's purpose in the
  setup check is `catalog::Purpose`.
- **DSN** (`Dsn`): from `STRAUM_DSN` or `--dsn-file`, never an
  argument.
- **Publication**: holds every spec table; `straum` by default.
- **Replication slot**: temporary, `straum_<pid>_<nanos>_<n>`, dropped
  with the connection. "The slot".
- **Replication connection**: creates the slot, loads, streams.
- **LSN** (`Lsn`): `16/B374D848`. `Wal`: `server`, `confirmed`,
  `restart`.
- **Catalog** (`Catalog`, `Class`, `Column`): what the setup check
  reads.
- **Old row** (`Change::Update::old`): whole under `REPLICA IDENTITY
  FULL`; facts and dims both need it (ADR-0031).
- **Label** (`EnumLabel`): an enum value's text, read via `::text`.
- **Poison value** (`DatumError::at`): a value Straum can't read; names
  table and column.

## Sealing

- **Seal** (`Store::seal`): hand a `date_bin` bucket older than
  `seal_after` to the spill (ADR-0033, ADR-0035). **Sealed bucket**:
  `Bucket::Sealed`; in memory: **hot** (`Bucket::Hot`). ADR-0033 title
  says "cold".
- **Spill** (`Spill`, `Spilling`): the port sealed buckets go to.
  `--spill-dir`, `--seal-after` and `straum-arrow` (ADR-0033,
  ADR-0034) do not exist yet; `main.rs` passes `None`.
- **Sealed** (`Sealed`, `SealedId`): a bucket's cells as plain data;
  `SealedId::bucket_start` is its first second.
- **Thaw** (`Store::thaw`): check a sealed bucket against the store.
  **Unseal** (`Store::unseal`): put it back. A change **touches** the
  sealed buckets its rows fall in.

## Testing

- **Reference**: what a test compares against: a plain computation over
  raw rows (domain tests), a store loaded at the same LSN (app tests),
  SQL run in PostgreSQL (`Reference`, `straum-pg` tests).
- **Fake** (`Fake`): a scripted PostgreSQL in the app and HTTP tests;
  `PostgresContainer` is a real one in Docker. `MemorySpill`, `Lost`:
  spills.
- **Taxi month**: January 2024 TLC trips; `taxi.sql`, the benchmark,
  the ignored `taxi_month` test. **Warehouse copy**: sizing.md's
  production-shaped tables, not in the repository.

## Clashes

| Word | Meanings | Verdict |
| --- | --- | --- |
| key | view key; dim key; cell key; rollup key; `Condition::key` | bare key is a view key; qualify the rest |
| attribute / dim column | code, ADRs / guide | dim column in prose; attribute stays in code |
| level / width | how-it-works.md, ADR-0017 / spec.md | level is the cells, width the duration |
| state | `app::State`; `source::State`; cell state (`State`); running states | qualify in prose |
| Member | `store::Member`, `catalog::Member`, one meaning in two private enums | left |
| Table | `straum_domain::Table`; `fake::Table` | fine; test-only |
| the crate for Straum SQL | `straum-notation` | `straum-sql` needs an `ALLOWED` change in `scripts/check-layers.py`; not done |
| cold | ADR-0033 title | sealed, hot; ADR text stays |
| spill flags | `--spill-dir`, `--seal-after`, `straum-arrow` in ADR-0033, ADR-0034 | not shipped; `main.rs` passes `None` |
