# Glossary

One name per thing, in the guide, the ADRs, the code and its errors.
The code's name follows in parentheses where it differs; "also" lists
spellings still found elsewhere. JOINS.md and POSTGRESQL.md are the
notes under `research/`. The clashes at the end are open.

## Straum

- **Straum**: the product, capitalised in prose. `straum` is every
  identifier: the binary, the crate prefix, the default role and
  publication, the slot prefix, the `application_name`, the chart and
  the command (ADR-0026).
- **Straum SQL**: the subset of PostgreSQL `CREATE VIEW` syntax that
  specs and asks are written in, one spelling per meaning, nothing
  inferred (ADR-0029). Also "the subset", "the notation" (the crate
  name, ADR-0028), "the canon" (ADR-0029).
- **Guide**: the mdBook under `docs/src`. Its words win in prose. Also
  "user guide", "the docs".
- **ADR**: one settled decision in `docs/adr/`, numbered and never
  reused (ADR-0001). ADRs win over the guide and this glossary; one
  written before a rename keeps its old words.
- **Port** and **adapter**: a port is a trait at the edge of
  `straum-app`: `Source` for PostgreSQL, `Spill` for sealed buckets.
  An adapter is the crate that implements one: `straum-pg`,
  `straum-http` (ADR-0003).

## Spec

- **Spec** (`Spec`): the file of `CREATE VIEW` statements Straum reads
  at start, and the parsed whole. Also the `--spec FILE` flag.
- **View** (`Fact`): one `CREATE VIEW` in the spec, and the name asks
  read `FROM`. Never created in PostgreSQL. Errors say "view";
  `straum plan` and the setup check say "fact" and "dim" for a
  table's role.
- **Fact table** (`Fact::table`): the table in `FROM`, whose rows the
  view counts. Needs no primary key, only `REPLICA IDENTITY FULL`.
- **Dim** (`Dim`): a table the view joins, held projected to its key
  and the columns the views read, and followed like a fact. One dim
  per table however many views join it. Also "joined table";
  JOINS.md says "dimension" when quoting others.
- **Dim key** (`Dim::key`): the dim's key columns, which a join's
  columns must equal. Any number of rows may share one; no index is
  required. Not a view key.
- **Spec table** (`Member`): a fact or a dim when the role does not
  matter: what the publication holds, what gets `REPLICA IDENTITY
  FULL`, what a load copies. Also "every table the spec names".
- **Table** (`straum_domain::Table`): a `schema.table`, always written
  whole and with an alias. `catalog::Checked` is a spec table as the
  setup check read it. `Relation` is PostgreSQL's word, in pgoutput
  and the catalog.
- **Alias** (`Ref::alias`): the `AS name` after a table. Asks never
  see one.
- **Name** (`Identifier`): the `AS name` after a `SELECT` item, the
  only way an ask reaches a key or measure. Unique in a view; an
  unquoted lowercase PostgreSQL identifier, at most 63 bytes. Also
  "output name" (ADR-0029), "select name" (JOINS.md).
- **View option**: `WITH (buckets = '...')`, the only one: every
  bucket width the view keeps, narrowest first (ADR-0028).
- **Flag**: a command-line option of the binary (ADR-0025). The chart
  calls the same setting a value (`maxLag`).

## Keys and joins

- **Key**: a selected column that is not a measure: a fact column, a
  dim column, a null-ness key or the bucket. An ask names one by its
  name (`Fact::resolve`).
- **By key** (`ByKey`, `Fact::by_keys`): a key read straight from a
  fact column, or from whether it is null. Code and ADRs say "by key"
  to tell it from a dim column; the guide says key.
- **Null-ness key** (`ByKey::IsNull`): `(f.col IS NULL) AS name` or
  `IS NOT NULL`, a boolean key, and what a `FILTER` with `IS NULL`
  counts through (ADR-0019). Also `Purpose::Nullness` in the setup
  check, which a `count(c)` column shares.
- **Dim column** (`Attribute`, `Ref::reads`, `Dim::attributes`): a
  column of a dim the view selects. A key, read at ask time through
  the cell's join columns, as the dim is now (ADR-0030). Code, the
  ADRs and JOINS.md say "attribute".
- **Join**: how a view reaches a dim: `INNER JOIN`, `LEFT OUTER JOIN`,
  or `EXISTS` and `NOT EXISTS` in `WHERE`. It has no name an ask can
  use.
- **Ref** (`Ref`, `RefKind`): the join as the domain holds it. ADR-0030
  calls a view "a tree of refs rooted at the fact". Errors and the
  guide say join.
- **Join kind** (`RefKind`): left, inner, semi or anti.
- **Join column** (`Via::Fact`): the fact column a join reads its key
  from, `orders.customer_id`. It keys the cells whether selected or
  not (ADR-0030). In code a "via"; also "join id" (ADR-0030).
- **Chain** (`Via::Attribute`): a join whose key comes from a dim
  joined before it, `tiers.id = customers.tier_id`.
- **Projection** (`Projection`): the shape Straum holds a dim in: a
  map from dim key to the coded columns of every row under it.

## Time

- **Time column** (`Time::column`): the `timestamptz` or `timestamp`
  column inside `date_bin`.
- **Bucket**: one span of the time column, floored to a width from
  the epoch, and `bucket`, the name of the time key. In `rollup.rs`,
  `Bucket` is also the cells of one such span, hot or sealed.
- **Bucket width** (`Width`): `<n> seconds`, `minutes`, `hours` or
  `days`, at most a day and dividing it; each wider one a whole
  multiple of the one before. ADR-0017 says "grain".
- **Narrowest width** (`Time::width`): the `date_bin` width, first in
  `buckets`. The others are the **wider widths** (`Time::wider`). Also
  "finest" (ADR-0017, ADR-0028).
- **Level** (`Level`): the cells of one width (ADR-0017, design.md).
  spec.md says only width.
- **Origin** (`Origin`): the third `date_bin` argument, `timestamptz
  'epoch'` or `timestamp 'epoch'`, matching the column's type
  (ADR-0018).
- **Bucket edge**: a whole multiple of the `date_bin` width. Both
  bounds of an ask must be on one.
- **Wall clock**: how a `timestamp` column reads: taken as UTC, no
  zone ever applied, printed with a `Z` that means nothing
  (ADR-0018).

## Measures and cells

- **Measure** (`Measure`): a named aggregate in the view, with an
  optional `FILTER`. Asks name measures after the keys.
- **Aggregate** (`Aggregate`): count, count of a column, sum, min,
  max, avg, count distinct, bool_and, bool_or. The walker says
  "aggregate" for the function and "an aggregated column" for its
  argument.
- **Input column** (`Aggregate::input`, `Input`): the column an
  aggregate reads. Measures over one column share its state
  (ADR-0016). What the aggregate reads of it (`Reads`) decides the
  types the column may have, and the setup check's `Purpose` follows
  it. Also `Purpose::Number`.
- **Cell**: one distinct combination of the rollup key and bucket,
  holding its row count and one state per input column. `Cells` is
  every cell of one bucket, side by side.
- **Cell state** (`State`, `StateKind`): a count of non-null values,
  a sum and a count, a count per value for min and max (the code's
  "extent"), a count per distinct value, or the counts of true and
  false (ADR-0041). Filtered measures keep no state (ADR-0016).
- **Rollup** (`Rollup`): every cell of one fact, per width, with the
  codes of its key columns.
- **Rollup key** (`Fact::rollup_key`): the by keys, then each join
  column once, then the bucket. The guide says "the cell key" or
  "keys, join columns and bucket".
- **Bound cells** (`Bound`): a second rollup of a fact whose joins all
  read fact columns, keyed by the dim columns' values, so an ask reads
  no projection (ADR-0038).
- **Store** (`Store`): the rollups of every fact and the projection of
  every dim, behind one `RwLock` (ADR-0009).

## Asks and answers

- **Ask** (`Ask`): the request to `POST /ask`: a query and its
  parameters. `straum-http` calls the parsed form `request`.
- **Query**: the ask's `query` field, one `SELECT` over a view in
  Straum SQL (ADR-0006).
- **Parameter** (`Literal`): a `$n` value of an ask: integer, string
  or boolean, with its column's type and no conversion. `straum ask`
  reads each one's type from its text (ADR-0039).
- **Literal** (`Literal`): a constant in a spec's `FILTER` or `WHERE`:
  an integer, `'text'`, `true` or `false`.
- **Condition** (`Condition`): `key = value`, `key IN (values)`, or in
  a view `column IS NULL` and `IS NOT NULL`. A **filter** is
  conditions joined by `AND`: a measure's `FILTER`, the view's `WHERE`
  (`Fact::filter`) or an ask's `WHERE`. `store.rs` evaluates one as a
  `Match`.
- **Range** (`Ask::from`, `Ask::to`): the half-open `[from, to)` an
  ask reads, on bucket edges. Its **bounds** are `bucket >= $n` and
  `bucket < $m`, both required. The tests say "window" for a range
  that cuts a day bucket.
- **Cover** (`Rollup::cover`): the buckets an ask reads: the widest
  that fit in the middle, narrower ones at the ends, or `date_bin`
  buckets alone when grouped by bucket.
- **Limit**: what a flag caps: `--max-lag`, `--max-retained-wal`
  (`Limits`) and `--max-transaction-rows` (`Transaction::limit`).
- **Answer**: the JSON `{"as_of", "rows"}`, or the same as a table in
  plain text for `Accept: text/plain` and `straum ask` (ADR-0039).
  `Answer` is its columns and rows; `Answered` adds `as_of` and the
  applied LSN. The HTTP
  tests say "reply"; `straum-http` says "response" for the HTTP
  message around it.
- **Row** (`AnswerRow`): one object of an answer's `rows`, sorted by
  key. A fact row and a dim row are rows too.
- **as_of**: when the slot was last known to have confirmed all the
  WAL there was, by Straum's clock (ADR-0007, `Lag::caught_up`).
  `straum status` prints "as of".
- **Straum-LSN** and **Server-Timing**: response headers: the last
  commit the answer reflects, and where the time went.
- **Column type** (`ColumnType`): integer, numeric with a scale,
  boolean, text, or time with or without a zone. Learned from the
  catalog at load; a filter value must fit it (`AskError::WrongType`).
- **Scale**: the digits past the point of a `numeric(p, s)`. Sums and
  extremes print in it; averages at four places past it (ADR-0007).

## Running

- **Start** (`Writer::start`, `Step::Started`): opening a new slot,
  loading every table in its snapshot and streaming from its
  consistent point. A **load** is one table's binary `COPY`; "the
  load" is all of them.
- **Start over**: after any failure: close the stream, which drops
  the slot, wait, then start again (ADR-0015). A **restart** is
  PostgreSQL's or a pod's; ADR-0033 says restart for the process.
- **Backoff**: the wait before a new start, 1 s doubling to 60 s,
  reset by a load that finishes.
- **States**: Loading, Ready, Lagging, Waiting, Stopping (running.md).
  No enum; `following` is `None` while loading or waiting, and
  `why_not` says which.
- **Ready** (`Status::ready`, `/readyz`): every table loaded and lag
  within `--max-lag`. Otherwise "not ready" and why.
- **Lag** (`Lag`): the time since Straum last caught up with a WAL
  position PostgreSQL had reached, read every second; never
  understated (ADR-0020). Also "behind".
- **Caught up** (`Lag::caught_up`): the last WAL reading the slot had
  confirmed all of; the source of `as_of`.
- **Snapshot** (`Snapshot`): the database as of a new slot's
  consistent point, seen by a read-only repeatable-read transaction on
  the replication connection. It ends with the last load (ADR-0010).
- **Consistent point** (`Snapshot::at`): the LSN a new slot reports,
  where the snapshot is taken and the stream starts. The guide says
  "LSN L".
- **Stream** (`Stream`): the commits pgoutput sends from the
  consistent point on. `Following` is where the stream stands.
- **Applied** (`Following::applied`, `Status::applied`,
  `Answered::applied`): the last commit in the store. **Committed**
  (`Commit::committed`, `Status::committed`): when it committed, by
  PostgreSQL's clock.
- **Step** (`Writer::step`): one turn of the writer: a start, or one
  commit applied. A step that fails is a **failure** (`Failure`,
  `Status::failure`, `last_failure`).
- **Commit** (`Commit`): one transaction's changes, delivered and
  applied whole. "Transaction" only where PostgreSQL's word is needed,
  as in `--max-transaction-rows`.
- **Change** (`Change`): one insert, update, delete or truncate of a
  spec table, or `Unfit`.
- **Unfit** (`Change::Unfit`, `Error::Unfit`): a change the store
  can't take, which starts Straum over. The error says "no longer
  fits"; ADR-0015 says "can't apply".
- **Apply** (`State::apply`): putting a commit into the store under
  the write lock.
- **Source** (`Source`, `Postgres`): the port to PostgreSQL: open a
  stream, load a table, next commit, WAL reading. "Source of truth"
  means PostgreSQL itself.
- **Plain connection** (`Postgres::plain`, `Connection`): the second,
  non-replication connection that reads the WAL position each second
  (ADR-0010).
- **WAL retained** (`Wal::retained`, `--max-retained-wal`,
  `maxRetainedWal`): the bytes PostgreSQL keeps for Straum's slot, its
  written position less the slot's `restart_lsn`.
- **Status** (`Status`): what `GET /status` and `straum status` show,
  read at one moment. Not telemetry (ADR-0023). The stream thread's
  report of the applied LSN to PostgreSQL (`StreamThread::report`) is
  a different message.
- **Held** (`Held`, `App::held`): what `GET /views` and `straum views`
  show, read at one moment: per view its widths, cells, range of
  buckets, keys with their types and measures; per dim its rows
  (ADR-0039). None until every table is loaded. As JSON for a program
  that asks for it (ADR-0040).
- **Shell** (`straum shell`): a prompt at a running Straum: `\d` for
  the views, `\rows VIEW` for a view's rows, an ask as `QUERY;
  PARAMETER...`, with Tab completion at a terminal (ADR-0040).
- **A view's rows** (`App::rows_ask`, `GET /views/NAME`, `\rows`): the
  rows PostgreSQL would give for the view, which is the answer to an
  ask for every measure by the bucket and every key over the buckets
  held.
- **Health check**: `/livez`, up while the process serves, and
  `/readyz`, up while ready.

## PostgreSQL

- **Setup**: the SQL a table owner runs once, which `straum plan`
  prints. The **setup check** (`Error::Setup`, `catalog::check`)
  compares the catalog with the spec at every start. Also "start
  check" (ADR-0014, ADR-0024, ADR-0031), "catalog check" (ADR-0018,
  ADR-0037).
- **Problem**: one line of a failed setup check, naming what is wrong
  and what to run.
- **Role**: the PostgreSQL role Straum connects as, the DSN's user or
  `straum` (`--role`). Unrelated: `catalog::Purpose` is what a column
  is for in the setup check.
- **DSN** (`Dsn`): the connection URL, from `STRAUM_DSN` or
  `--dsn-file`, never the command line (ADR-0021). Its `Display`
  leaves the password out.
- **Publication**: the publication holding every spec table, `straum`
  by default (`--publication`).
- **Replication slot**: the temporary logical slot each start creates,
  `straum_<pid>_<nanos>_<n>`, dropped when the connection closes.
  "The slot" in the guide.
- **Replication connection**: the connection that creates the slot,
  loads in its snapshot and then streams, all on one session
  (ADR-0010).
- **LSN** (`Lsn`): a WAL position, printed as PostgreSQL does. A
  `Wal` reading holds three: `written`, `confirmed` (the slot's
  `confirmed_flush_lsn`) and `restart` (its `restart_lsn`).
- **Catalog** (`Catalog`, `Class`, `Column`): PostgreSQL's system
  tables as the setup check reads them.
- **Old row** (`Change::Update::old`): the row before an update or
  delete, whole under `REPLICA IDENTITY FULL` (ADR-0031).
- **Label** (`EnumLabel`): an enum value's text, read by writing
  `::text` (ADR-0037).
- **Poison value** (`ValueError::at`): a value Straum can't read, such
  as a `NaN`. It stops the load or the stream, naming table and
  column.

## Sealing

Old `date_bin` buckets can leave memory through a port (ADR-0033,
ADR-0035).

- **Seal** (`Store::seal`): hand a `date_bin` bucket that ended
  `seal_after` ago to the spill and drop its cells, keeping its place
  in the level. A **sealed bucket** is `Bucket::Sealed`; one in
  memory is **hot** (`Bucket::Hot`). ADR-0033's title says "cold".
- **Spill** (`Spill`, `Spilling`): the port sealed buckets go to.
  Every start empties it. The flags `--spill-dir` and `--seal-after`
  and the crate `straum-arrow` that ADR-0033 and ADR-0034 name do not
  exist yet; `main.rs` passes `None`.
- **Sealed** (`Sealed`, `SealedId`): a bucket's cells as plain data.
  `SealedId` is the fact, the width and `bucket_start`.

## Testing

- **Reference**: what a test compares Straum's answer with: a plain
  computation over raw rows in the domain tests, a store loaded from
  the fake at the same LSN in the app tests, SQL run in PostgreSQL in
  the `straum-pg` tests (`Reference`). Planting a bug must make the
  comparison fail.
- **Fake** (`Fake`): a scripted PostgreSQL in the app and HTTP tests.
  `PostgresContainer` is a real one in Docker. `MemorySpill` and
  `Lost` are spills.
- **Taxi month**: the January 2024 TLC yellow trips, `bench/data`,
  `taxi.sql`, the benchmark and the ignored `taxi_month` test. The
  **warehouse copy** is the benchmark's production-shaped tables, not
  in the repository.

## Clashes

| Word | Meanings | Verdict |
| --- | --- | --- |
| key | view key; dim key; cell key; rollup key; `Condition::key` | bare key is a view key; qualify the rest |
| attribute / dim column | code, ADRs / guide | dim column in prose; attribute stays in code |
| level / width | design.md, ADR-0017 / spec.md | level is the cells, width the duration |
| state | `app::State`; `source::State`; cell state (`State`); running states | qualify in prose |
| the crate for Straum SQL | `straum-notation` | `straum-sql` needs an `ALLOWED` change in `scripts/check-layers.py`; not done |
| cold | ADR-0033 title | sealed, hot; ADR text stays |
| spill flags | `--spill-dir`, `--seal-after`, `straum-arrow` in ADR-0033, ADR-0034 | not shipped; `main.rs` passes `None` |
