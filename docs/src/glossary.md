# Glossary

One name per thing, in the guide, the ADRs, the code and its errors.
The code's name follows in parentheses where it differs; "also" lists
spellings still found elsewhere. JOINS.md and POSTGRESQL.md are the
notes under `research/`. The clashes at the end are open.

## Straum

Straum keeps aggregates of PostgreSQL tables in memory and answers
asks about them over HTTP.

- **Straum**: the product, capitalised in prose. `straum` is every
  identifier: the binary, the crate prefix, the default role and
  publication, the slot prefix, the `application_name`, the chart, and
  the command in `straum plan` (ADR-0026).
- **Straum SQL**: the small subset of PostgreSQL `CREATE VIEW` syntax
  that specs and asks are written in. It has one spelling per meaning
  and infers nothing: every schema, alias, column owner and output
  name is written (ADR-0029). The grammar is spec.md and asks.md. Also
  "the subset", "the notation" (the crate name, ADR-0028), "the canon"
  (ADR-0029).
- **Guide**: the mdBook under `docs/src`, published to GitHub Pages.
  It is the user-facing text, and its words win in prose. Also "user
  guide", "the docs".
- **ADR**: one settled decision in `docs/adr/`, numbered and never
  reused (ADR-0001). ADRs win over the guide and this glossary. An ADR
  written before a rename keeps its old words.
- **Port** and **adapter**: a port is a trait at the edge of
  `straum-app` that the domain's use cases talk through: `Source` for
  PostgreSQL, `Spill` for sealed buckets. An adapter is the crate that
  implements one: `straum-pg`, `straum-http`. Crates depend inward only
  (ADR-0003).

## Spec

A spec declares what Straum keeps. It is a file of views; each view
names one fact table, the dims it joins, its keys, its time bucket and
its measures.

- **Spec** (`Spec`): the file of `CREATE VIEW` statements Straum reads
  at start, and the parsed whole: every dim and fact. Also the `--spec
  FILE` flag.
- **View** (`Fact`): one `CREATE VIEW` in the spec, and the name asks
  read `FROM`. Straum never creates it in PostgreSQL; PostgreSQL would
  give the same rows for its body. In code a view is a `Fact`, since
  it is one fact table aggregated. Errors about a view say "view";
  `straum plan` and the setup check say "fact" and "dim" when they mean
  a table's role.
- **Fact table** (`Fact::table`): the table in `FROM`, whose rows the
  view counts. It needs no primary key, only `REPLICA IDENTITY FULL`.
- **Dim** (`Dim`): a table the view joins, for its columns. Straum
  holds a dim projected to its key and the columns the views read, and
  follows it from the stream like a fact. There is one dim per table,
  however many views join it, and it carries the table's name. Also
  "joined table". JOINS.md says "dimension" when quoting others.
- **Dim key** (`Dim::key`): the dim's key columns, one or more, that a
  join's columns must equal. Any number of dim rows may share a key;
  no index is required. A dim key is not a view key.
- **Spec table** (`Member`): a fact or a dim, when the role does not
  matter: what the publication must hold, what gets `REPLICA IDENTITY
  FULL`, what a load copies. Also "the spec's tables", "every table
  the spec names".
- **Table** (`straum_domain::Table`): a `schema.table`, always written
  whole and with an alias. `catalog::Checked` is a spec table as the
  setup check read it from PostgreSQL, with its columns and types.
  `Relation` is PostgreSQL's own word, in pgoutput and the catalog.
- **Alias** (`Ref::alias`): the `AS name` after a table. The fact's
  alias is the near side of every join; a joined table's alias is the
  name of its ref. Asks never see an alias.
- **Name** (`Identifier`): the `AS name` after a `SELECT` item, and
  the only way an ask reaches a key or measure. Names are unique in a
  view. An `Identifier` is an unquoted PostgreSQL identifier,
  lowercase, at most 63 bytes. Also "output name" (ADR-0029), "select
  name" (JOINS.md).
- **View option**: `WITH (buckets = '...')`, the only one. It lists
  every bucket width the view keeps, narrowest first (ADR-0028).
- **Flag**: a command-line option of the binary, such as `--max-lag`
  (ADR-0025). The chart calls the same setting a value (`maxLag`).

## Keys and joins

Every selected column that is not a measure is a key. Keys decide
which cells exist and what asks can group and filter by. Joins reach
keys that live in a dim.

- **Key**: a selected column that is not a measure: a fact column, a
  dim column, a null-ness key or the bucket. Each cell is one distinct
  combination of the keys. An ask names a key by its name, and the
  name resolves to a by key or a dim column (`Fact::resolve`).
- **By key** (`ByKey`, `Fact::by_keys`): a key read straight from a
  fact column, or from whether that column is null. The code and the
  ADRs say "by key" to tell it from a dim column; the guide says key.
- **Null-ness key** (`ByKey::IsNull`): `(f.col IS NULL) AS name` or
  `IS NOT NULL`, a boolean key over whether a column is null. It keys
  cells on two values instead of the column's cardinality, and a
  `FILTER` with `IS NULL` counts through it (ADR-0019). Also
  `Purpose::Nullness` in the setup check.
- **Dim column** (`Attribute`, `Ref::reads`, `Dim::attributes`): a
  column of a dim the view selects. It is a key like any other, but it
  is read at ask time through the cell's join columns, as the dim is
  now, so a dim change moves every answer at once and moves no cell
  (ADR-0030). Code, the ADRs and JOINS.md say "attribute".
- **Join**: how a view reaches a dim: `INNER JOIN`, `LEFT OUTER JOIN`,
  or `EXISTS` and `NOT EXISTS` in `WHERE`. The join itself has no
  value and no name an ask can use; asks name the dim columns it
  reads.
- **Ref** (`Ref`, `RefKind`): the join as the domain holds it: alias,
  dim, kind, join columns and the dim columns it reads. ADR-0030 calls
  a view "a tree of refs rooted at the fact". The word stays in code
  and that ADR; errors and the guide say join.
- **Join kind** (`RefKind`): left, inner, semi or anti. Left keeps
  every fact row, inner only matched ones, semi and anti keep or drop
  a row and read nothing of the dim. A key held by several dim rows
  repeats the fact row, as PostgreSQL's join does.
- **Join column** (`Via::Fact`): the fact column a join reads its key
  from, `orders.customer_id`. It keys the cells whether the view
  selects it or not, which is why memory follows distinct join ids
  (ADR-0030). In code a "via"; also "join id" (how-it-works.md,
  ADR-0030).
- **Chain** (`Via::Attribute`): a join whose key comes from a dim
  joined before it, `tiers.id = customers.tier_id`. The earlier dim
  then carries that column in its projection.
- **Projection** (`Projection`): the shape Straum holds a dim in: a
  map from dim key to the coded columns of every row under it. An ask
  maps a cell's join columns through it.

## Time

Every view has one time column, bucketed from the epoch. Cells are
kept per bucket, and once more per wider width the view lists, so a
long range reads few cells.

- **Time column** (`Time::column`): the `timestamptz` or `timestamp`
  column inside `date_bin`. A row whose time is null or infinite is
  kept and never answered.
- **Bucket**: one span of the time column, floored to a width from
  the epoch, and `bucket`, the name asks call the time key. In
  `rollup.rs`, `Bucket` is also the cells of one such span, hot or
  sealed.
- **Bucket width** (`Width`): how long a bucket is, `<n> seconds`,
  `minutes`, `hours` or `days`, at most a day and dividing it. Each
  wider width is a whole multiple of the one before. ADR-0017 says
  "grain".
- **Narrowest width** (`Time::width`): the `date_bin` width, first in
  `buckets`. The others are the **wider widths** (`Time::wider`). Also
  "finest" (ADR-0017, ADR-0028).
- **Level** (`Level`): the cells of one width. Each level is an index
  over the same rows: answers don't change, memory grows per width
  (ADR-0017, how-it-works.md). spec.md says only width.
- **Origin** (`Origin`): the third `date_bin` argument, `timestamptz
  'epoch'` or `timestamp 'epoch'`, matching the column's type. Buckets
  start at the epoch, so 7 day buckets start on Thursdays (ADR-0018).
- **Bucket edge**: a time that is a whole multiple of the `date_bin`
  width. Both bounds of an ask must be on one; Straum never answers
  for part of a bucket.
- **Wall clock**: how a `timestamp` column reads: its reading taken as
  UTC, with no zone ever applied. Answers print it with a `Z` that
  means nothing (ADR-0018).

## Measures and cells

A measure says what to aggregate. A cell holds what every measure
needs for one combination of keys and bucket, so an ask merges cells
and never reads a row.

- **Measure** (`Measure`): a named aggregate in the view, `sum(
  orders.amount) AS revenue`, with an optional `FILTER`. Asks name
  measures after the keys.
- **Aggregate** (`Aggregate`): the function a measure applies: count,
  sum, min, max, avg, count distinct. The walker says "aggregate" for
  the function and "an aggregated column" for its argument.
- **Input column** (`Aggregate::input`, `Input`): the column an
  aggregate reads. Measures over one column share its state: sum and
  avg share a sum and a count (ADR-0016). Also `Purpose::Number`.
- **Cell**: one distinct combination of the rollup key and bucket,
  holding its row count and one state per input column. A cell goes
  with its last row. `Cells` is every cell of one bucket, side by
  side, so an ask reads each state as one flat run.
- **Cell state** (`State`, `StateKind`): what a cell keeps per input
  column: a sum and a count, a count per value for min and max (the
  code's "extent"), or a count per distinct value. Filtered measures
  keep no state of their own (ADR-0016).
- **Rollup** (`Rollup`): every cell of one fact, per width, with the
  codes of its key columns. The guide says it once, in sizing.md.
- **Rollup key** (`Fact::rollup_key`): what a cell is keyed by: the by
  keys, then each join column once, then the bucket. The guide calls
  it "the cell key" or "keys, join columns and bucket".
- **Store** (`Store`): the rollups of every fact and the projection of
  every dim, behind one `RwLock`. A load fills a new store outside the
  lock and swaps it in (ADR-0009).

## Asks and answers

An ask is a `SELECT` over a view with every value a parameter. The
answer is the groups it asked for, with how current they are.

- **Ask** (`Ask`): the request to `POST /ask`: a query and its
  parameters. `straum-http` calls the parsed form `request`.
- **Query**: the ask's `query` field, one `SELECT` over a view in
  Straum SQL: keys then measures, `=` and `IN` joined by `AND`, and
  both bucket bounds (ADR-0006).
- **Parameter** (`Literal`): a `$n` value of an ask, an integer,
  string or boolean. Every value in an ask is a parameter; a value
  written into the query is refused, so values never become query
  text. A parameter must have its column's type, with no conversion.
- **Literal** (`Literal`): a constant written in a spec's `FILTER` or
  `WHERE`: an integer, `'text'`, `true` or `false`. The same type
  carries parameters.
- **Condition** (`Condition`): `key = value`, `key IN (values)`, or in
  a view `column IS NULL` and `IS NOT NULL`. A **filter** is a list of
  conditions joined by `AND`: a measure's `FILTER`, the view's `WHERE`
  (`Fact::filter`), or an ask's `WHERE`.
  `store.rs` evaluates one as a `Match`.
- **Range** (`Ask::from`, `Ask::to`): the half-open `[from, to)` an
  ask reads, in Unix seconds on bucket edges. Its two **bounds** are
  `bucket >= $n` and `bucket < $m`, both required. The tests say
  "window" for a range that cuts a day bucket.
- **Cover** (`Rollup::cover`): the buckets an ask reads for its range:
  the widest that fit in the middle, narrower ones at the ends, or
  `date_bin` buckets alone when grouped by bucket (how-it-works.md).
- **Limit**: what a flag caps: `--max-lag`, `--max-retained-wal`
  (`Limits`) and `--max-transaction-rows` (`Transaction::limit`).
- **Answer**: the JSON `{"as_of", "rows"}`. `Answer` is its columns
  and rows; `Answered` adds `as_of` and the applied LSN for the
  `Straum-LSN` header. The HTTP tests say "reply"; `straum-http` says
  "response" for the HTTP message around it.
- **Row** (`AnswerRow`): one object of an answer's `rows`, keyed by
  the names in `SELECT`, sorted by key. A fact row and a dim row are
  rows too; the context says which.
- **as_of**: when the slot was last known to have confirmed all the
  WAL there was, by Straum's clock. Every commit made before it is in
  the answer, and it keeps moving on a quiet database (ADR-0007,
  `Lag::caught_up`). `straum status` prints "as of".
- **Straum-LSN** and **Server-Timing**: response headers. The first
  names the last commit the answer reflects, for "Seeing your own
  write" in asks.md. The second says where the time went.
- **Column type** (`ColumnType`): integer, numeric with a scale,
  boolean, text, or time with or without a zone. Only the load learns
  it, from the catalog. A filter value must fit it
  (`AskError::WrongType`).
- **Scale**: the digits past the point of a `numeric(p, s)`. Sums and
  extremes print in it; averages at four places past it (ADR-0007).

## Running

One writer loads the tables and applies the stream. Asks read the
store meanwhile. Any failure makes Straum start over from a new slot.

- **Start** (`Writer::start`, `Step::Started`): opening a new slot,
  loading every table in its snapshot and streaming from its
  consistent point. `straum status` counts starts. A **load** is one
  table's binary `COPY` into the store; "the load" is all of them.
- **Start over**: what Straum does after any failure: close the
  stream, which drops the slot, wait, then start again (ADR-0015). A
  **restart** is PostgreSQL's, for `wal_level`, or a pod's. ADR-0033
  says restart for the process.
- **Backoff**: the wait before a new start, 1 s doubling to 60 s,
  reset by a load that finishes.
- **States**: Loading, Ready, Lagging, Waiting, Stopping (running.md).
  There is no enum; `following` is `None` while loading or waiting,
  and `why_not` says which.
- **Ready** (`Status::ready`, `/readyz`): every table loaded and lag
  within `--max-lag`. Otherwise "not ready" and why.
- **Lag** (`Lag`): the time since Straum last caught up with a WAL
  position PostgreSQL had reached, read every second. It can overstate
  the delay by up to a second, never understate it (ADR-0020). Also
  "behind", as in "45 s behind PostgreSQL".
- **Caught up** (`Lag::caught_up`): the last WAL reading the slot had
  confirmed all of. It is the source of `as_of`.
- **Snapshot** (`Snapshot`): the database as of a new slot's
  consistent point, seen by a read-only repeatable-read transaction on
  the replication connection. It ends with the last load (ADR-0010).
- **Consistent point** (`Snapshot::at`): the LSN a new slot reports,
  where the snapshot is taken and the stream starts. The guide calls
  it "LSN L".
- **Stream** (`Stream`): the commits pgoutput sends from the
  consistent point on. Straum follows a table by applying its stream;
  `Following` is where the stream stands.
- **Applied** (`Following::applied`, `Status::applied`,
  `Answered::applied`): the last commit in the store. **Committed**
  (`Commit::committed`, `Status::committed`): when that commit
  committed, by PostgreSQL's clock.
- **Step** (`Writer::step`): one turn of the writer: a start, or one
  commit applied. A step that fails is a **failure** (`Failure`,
  `Status::failure`, `last_failure`).
- **Commit** (`Commit`): one transaction's changes, delivered whole
  after it commits and applied whole, so an answer never sees half of
  one. "Transaction" is used only where PostgreSQL's word is needed,
  as in `--max-transaction-rows`.
- **Change** (`Change`): one insert, update, delete or truncate of a
  spec table, or `Unfit`.
- **Unfit** (`Change::Unfit`, `Error::Unfit`): a change the store
  can't take, which starts Straum over: an altered table, an update or
  delete without its old row, a value left out, a row never added.
  The error says "no longer fits"; ADR-0015 says "can't apply".
- **Apply** (`State::apply`): putting a commit into the store under
  the write lock. `straum status` prints "applied".
- **Source** (`Source`, `Postgres`): the port to PostgreSQL: open a
  stream, load a table, next commit, WAL reading. "Source of truth" in
  the guide means PostgreSQL itself, not the port.
- **Plain connection** (`Postgres::plain`, `Connection`): the second,
  non-replication connection that reads the WAL position each second,
  since the replication connection is busy streaming (ADR-0010).
- **WAL retained** (`Wal::retained`, `--max-retained-wal`,
  `maxRetainedWal`): the bytes PostgreSQL keeps for Straum's slot, its
  written position less the slot's `restart_lsn`. Past the limit
  Straum starts over, so a slow Straum can't fill the disk.
- **Status** (`Status`): what `GET /status` and `straum status` show,
  read at one moment: ready or why not, applied, as of, lag, WAL
  retained, starts, failures, last failure, sealed. Not telemetry
  (ADR-0023). The stream thread reports the applied LSN to PostgreSQL
  (`StreamThread::report`), a different message.
- **Health check**: `/livez`, up while the process serves, and
  `/readyz`, up while ready. Lag never fails `/livez`.

## PostgreSQL

Straum needs stock PostgreSQL 17 or newer, logical replication, one
role and one publication. The setup is done once by a table owner.

- **Setup**: the SQL a table owner runs once: logical decoding, the
  role, its grants, the publication, `REPLICA IDENTITY FULL` on every
  spec table. `straum plan` prints it. The **setup check**
  (`Error::Setup`, `catalog::check`) compares the catalog with the spec
  at every start and reports every problem with its fix. Also "start
  check" (ADR-0014, ADR-0024, ADR-0031), "catalog check" (ADR-0018,
  ADR-0037).
- **Problem**: one line of a failed setup check, naming what is wrong
  and what to run. `/readyz` and `straum status` show them.
- **Role**: the PostgreSQL role Straum connects as, the DSN's user or
  `straum` (`--role`). Unrelated: `catalog::Purpose` is what a column
  is for in the setup check.
- **DSN** (`Dsn`): the connection URL, from `STRAUM_DSN` or
  `--dsn-file`, never from the command line (ADR-0021). Its `Display`
  leaves the password out.
- **Publication**: the PostgreSQL publication holding every spec
  table, `straum` by default (`--publication`).
- **Replication slot**: the temporary logical slot each start creates,
  `straum_<pid>_<nanos>_<n>`. PostgreSQL drops it when the connection
  closes, so a crash leaves no WAL held. "The slot" in the guide.
- **Replication connection**: the connection that creates the slot,
  loads in its snapshot and then streams, all on one session
  (ADR-0010).
- **LSN** (`Lsn`): a WAL position, printed as PostgreSQL does,
  `16/B374D848`. A `Wal` reading holds three: `written`, how far
  PostgreSQL has written; `confirmed`, the slot's
  `confirmed_flush_lsn`; `restart`, its `restart_lsn`.
- **Catalog** (`Catalog`, `Class`, `Column`): PostgreSQL's system
  tables as the setup check reads them: `server_version`, `wal_level`,
  the publication, `pg_class` rows, columns, enum labels.
- **Old row** (`Change::Update::old`): the row before an update or
  delete, whole under `REPLICA IDENTITY FULL`. A fact needs it to take
  the row out of its cell, a dim to find the projection entry
  (ADR-0031).
- **Label** (`EnumLabel`): an enum value's text. A spec reads an enum
  column as its label by writing `::text` (ADR-0037).
- **Poison value** (`ValueError::at`): a value Straum can't read, such
  as a `NaN`. It stops the load or the stream, naming its table and
  column so the row can be fixed.

## Sealing

Old `date_bin` buckets can leave memory through a port. The domain
hands them out as plain data and keeps their place; the writer brings
one back before a change reaches it (ADR-0033, ADR-0035).

- **Seal** (`Store::seal`): hand a `date_bin` bucket that ended
  `seal_after` ago to the spill and drop its cells, keeping its place
  in the level. A **sealed bucket** is `Bucket::Sealed`; one still in
  memory is **hot** (`Bucket::Hot`). ADR-0033's title says "cold".
- **Spill** (`Spill`, `Spilling`): the port sealed buckets go to: put,
  get and remove by `SealedId`, and clear. Every start empties it. The
  flags `--spill-dir` and `--seal-after` and the crate `straum-arrow` that
  ADR-0033 and ADR-0034 name do not exist yet; `main.rs` passes `None`.
- **Sealed** (`Sealed`, `SealedId`): a bucket's cells as plain data:
  keys as codes, counts, one `SealedColumn` per input. `SealedId` is
  the fact, the width and `bucket_start`, the bucket's first second.

## Testing

- **Reference**: what a test compares Straum's answer with. In the
  domain tests it is a plain computation over raw rows; in the app
  tests a store loaded from the fake at the same LSN; in the
  `straum-pg` tests SQL run in PostgreSQL (`Reference`). Planting a
  bug must make the comparison fail.
- **Fake** (`Fake`): a scripted PostgreSQL in the app and HTTP tests.
  `PostgresContainer` is a real one in Docker. `MemorySpill` and `Lost`
  are spills.
- **Taxi month**: the January 2024 TLC yellow trips, `bench/data`,
  `taxi.sql`, the benchmark and the ignored `taxi_month` test. The
  **warehouse copy** is sizing.md's production-shaped tables, not in
  the repository.

## Clashes

| Word | Meanings | Verdict |
| --- | --- | --- |
| key | view key; dim key; cell key; rollup key; `Condition::key` | bare key is a view key; qualify the rest |
| attribute / dim column | code, ADRs / guide | dim column in prose; attribute stays in code |
| level / width | how-it-works.md, ADR-0017 / spec.md | level is the cells, width the duration |
| state | `app::State`; `source::State`; cell state (`State`); running states | qualify in prose |
| the crate for Straum SQL | `straum-notation` | `straum-sql` needs an `ALLOWED` change in `scripts/check-layers.py`; not done |
| cold | ADR-0033 title | sealed, hot; ADR text stays |
| spill flags | `--spill-dir`, `--seal-after`, `straum-arrow` in ADR-0033, ADR-0034 | not shipped; `main.rs` passes `None` |
