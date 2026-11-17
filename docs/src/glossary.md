# Glossary

One name per thing, in the guide, the ADRs, the code and its errors.
Each term has a line and a link to the page that explains it. The
code's name follows in parentheses where it differs; "also" lists
spellings still found elsewhere, such as in the notes under
`research/`. ADRs win over the guide and this glossary. The clashes at
the end are open.

## Straum

- **Straum**: the product, capitalised in prose; `straum` is every
  identifier: binary, crates, role, publication, slot, chart (ADR-0012).
- **Straum SQL**: the `CREATE VIEW` subset specs and asks are written
  in, one spelling per meaning ([Spec](spec.md)). Also "the subset",
  "the notation", "the canon" (ADR-0004).
- **Guide**: the mdBook under `docs/src`, its pages fixed by ADR-0014.
  Also "user guide", "the docs". `llms.txt` lists the pages.
- **ADR**: the settled decisions on one subject in `docs/adr/`
  (ADR-0001).
- **Port** and **adapter**: a trait at the edge of `straum-app`,
  `Source`, and a crate that implements one, `straum-pg` or
  `straum-http` ([Architecture](design.md#architecture)).

## Spec

- **Spec** (`Spec`): the file of `CREATE VIEW` statements, and the
  parsed whole ([Spec](spec.md)).
- **View** (`Fact`): one `CREATE VIEW`, the name an ask reads `FROM`;
  never created in PostgreSQL ([Spec](spec.md#synopsis)).
- **Fact table** (`Fact::table`): the table in `FROM`, whose rows the
  view counts ([The tables](spec.md#the-tables)).
- **Dim** (`Dim`): a table a view joins, held as a projection and
  followed like a fact ([Dims](how-it-works.md#dims)). Also "joined
  table", "dimension".
- **Dim key** (`Dim::key`): the dim's columns a join equals; not a view
  key ([Joining a dim](spec.md#joining-a-dim)).
- **Spec table** (`Member`): a fact or a dim when the role doesn't
  matter: what a load copies and the publication holds. Also "every
  table the spec names".
- **Table** (`straum_domain::Table`): a `schema.table`, written whole
  with an alias; PostgreSQL's word is relation (`Relation`).
- **Alias** (`Ref::alias`): the `AS name` after a table; asks never see
  one.
- **Name** (`Identifier`): the `AS name` of a key or measure, the only
  way an ask reaches it. Also "output name", "select name".
- **View option**: `WITH (buckets = '...')`, the bucket widths a view
  keeps ([Time](spec.md#time)).
- **Flag**: a command-line option ([Command line](cli.md)); the chart
  calls one a value (`maxLag`).

## Keys and joins

- **Key**: a selected column that is not a measure
  ([Keys](spec.md#keys)).
- **By key** (`ByKey`, `Fact::by_keys`): a key read straight from a
  fact column or its null-ness; the guide says key.
- **Null-ness key** (`ByKey::IsNull`): `(e.col IS NULL) AS name`, a
  boolean key ([Null-ness keys](spec.md#null-ness-keys)).
- **Dim column** (`Attribute`): a dim's column a view selects, read
  through the cell's join column at ask time. Also "attribute".
- **Join**: how a view reaches a dim: `INNER JOIN`, `LEFT OUTER JOIN`,
  `EXISTS`, `NOT EXISTS` ([Joining a dim](spec.md#joining-a-dim)).
- **Ref** (`Ref`, `RefKind`, `JoinError`): the join as the domain holds
  it; its kind is left, inner, semi or anti.
- **Join column** (`Via::Fact`): the fact column a join reads its key
  from; it keys the cells. Also "via", "join id".
- **Chain** (`Via::Attribute`): a join whose key comes from a dim joined
  before it.
- **Join row** (`JoinRow`): one way a cell's joins resolve: per join the
  dim row found, or none.
- **Projection** (`Projection`): a dim as Straum holds it: dim key to
  the coded columns of its rows ([Dims](how-it-works.md#dims)).

## Time

- **Time column** (`Time::column`): the `timestamptz` or `timestamp`
  inside `date_bin` ([Time](spec.md#time)).
- **Bucket**: one span of the time column at one width, and `bucket`,
  the time key's name ([Cells](how-it-works.md#cells)).
- **Bucket width** (`Width`): a span, the `date_bin` one dividing a
  day, each wider one a multiple of the one before. Also "grain".
- **Narrowest width** (`Time::width`): the `date_bin` width; the rest
  are the **wider widths** (`Time::wider`). Also "finest".
- **Level** (`Level`): the cells of one width
  ([The store](design.md#the-store)).
- **Origin** (`Origin`): `date_bin`'s third argument, the `'epoch'` of
  the column's type.
- **Bucket edge**: a multiple of the `date_bin` width; each bound an
  ask writes sits on one ([Parameters](asks.md#parameters)).
- **Wall clock**: how a `timestamp` reads: no zone, printed with a `Z`
  that means nothing ([Time](spec.md#time)).

## Measures and cells

- **Measure** (`Measure`): a named aggregate in a view, with an optional
  `FILTER` ([Measures](spec.md#measures)).
- **Aggregate** (`Aggregate`): the function a measure applies; its
  argument is the walker's "aggregated column".
- **Input column** (`Aggregate::input`, `Input`, `Reads`): the column
  an aggregate reads; measures over one column share its state.
- **Cell**: one combination of rollup key and bucket, with its row count
  and a state per input column ([Cells](how-it-works.md#cells)).
- **Cell state** (`State`, `StateKind`): what a cell keeps for an input
  column ([What a cell keeps](aggregates.md#what-a-cell-keeps)). Also
  "extent" for a count per value.
- **Merge**: adding the states of the cells an ask reads into one answer
  per group ([Merging](aggregates.md#merging)).
- **Cell loop** (`CellLoop`): an ask's pass over its cells, filtering
  and merging them into groups.
- **Distributive**, **algebraic**, **holistic**: what a part keeps to
  merge ([Merging](aggregates.md#merging)).
- **Fraction** (`Fraction`) and **rank**: the `f` of
  `percentile_disc(f)` and the position `ceil(f * n)` it answers
  ([Median and percentiles](aggregates.md#median-and-percentiles)).
- **Sketch**: an approximate summary that merges; Straum has none.
- **Rollup** (`Rollup`): every cell of one fact, per width.
- **Rollup key** (`Fact::rollup_key`): by keys, join columns, bucket.
  The guide says "cell key".
- **Bound cells** (`Bound`): a second rollup keyed by dim columns'
  values, beside the **keyed cells** (`Read::Keyed`)
  ([Dims](how-it-works.md#dims)).
- **Pour** (`Bound::pour`): moving a keyed bucket's cells into the bound
  cells its join rows resolve to now.
- **Overlay** (`Overlay`): a dim key's rows before a change, read so
  bound cells drop what those rows joined.
- **Store** (`Store`): every rollup and projection, behind one `RwLock`
  ([The store](design.md#the-store)).

## Asks and answers

- **Ask** (`Ask`): a query and its parameters sent to `POST /ask`
  ([Asks](asks.md)). `straum-http` calls the parsed form `asked`.
- **Query**: an ask's one `SELECT` over a view
  ([The query](asks.md#the-query)).
- **Parameter** (`Literal`): a `$n` value of an ask
  ([Parameters](asks.md#parameters)).
- **Literal** (`Literal`): a constant in a spec's `FILTER` or `WHERE`.
- **Condition** (`Condition`, `ConditionCheck`, `JoinCheck`): `key =`,
  `IN`, `IS NULL`; a **filter** is conditions joined by `AND`.
- **Comparison** (`MeasureCondition`): `measure op $n` in `HAVING`
  ([HAVING](asks.md#having)).
- **Range** (`Ask::from`, `Ask::to`): the `[from, to)` an ask reads; its
  **bounds** are the two `bucket` conditions. Tests say "window".
- **Cover** (`Rollup::cover`): the buckets an ask reads, widest in the
  middle ([Answering an ask](how-it-works.md#answering-an-ask)).
- **Limit** (`Limits`): what a `--max-*` flag caps
  ([straum run](cli.md#straum-run)).
- **Answer** (`Answer`, `Answered`): `as_of` and rows, as JSON or a
  table ([The answer](asks.md#the-answer)). Also "reply" for the HTTP
  message.
- **Row** (`AnswerRow`): one group of an answer; fact rows and dim rows
  are rows too.
- **as_of** (`Lag::caught_up`): every commit before it is in the answer
  ([The answer](asks.md#the-answer)).
- **Straum-LSN** and **Server-Timing**: response headers
  ([The answer](asks.md#the-answer)).
- **Column type** (`ColumnType`): a column's type as the catalog gave it
  at load; a parameter must fit it.
- **Scale**: the digits past the point of a `numeric(p, s)`
  ([Measures](spec.md#measures)).

## Running

- **Start** (`Writer::start`): a new slot, a **load** of each table in
  its snapshot, then the stream
  ([Load once](how-it-works.md#load-once-follow-every-commit)).
- **Start over**: after a failure, drop the slot, wait, start again
  ([States](deploy.md#states)). A **restart** is PostgreSQL's or a pod's.
- **Backoff**: the wait before a new start, 1 s doubling to 60 s.
- **States**: Loading, Ready, Lagging, Waiting, Stopping
  ([States](deploy.md#states)); no enum.
- **Ready** (`Status::ready`): every table loaded and lag within
  `--max-lag`; else "not ready" and why.
- **Lag** (`Lag`): time since Straum last caught up with PostgreSQL's
  WAL ([States](deploy.md#states)). Also "behind".
- **Snapshot** (`Snapshot`, `TransactionSnapshot`): the database at the
  consistent point, which the load reads.
- **Consistent point** (`Snapshot::at`, `Consistency`): where the
  snapshot is taken and the stream starts. The guide says "LSN L".
- **Stream** (`Stream`): the commits after the consistent point;
  `Following` is where it stands.
- **Applied** and **committed** (`Following::applied`,
  `Commit::committed`): the last commit in the store, and when
  PostgreSQL committed it.
- **Step** (`Writer::step`): one turn of the writer; one that fails is a
  **failure** (`Failure`).
- **Commit** (`Commit`): one transaction's changes, applied whole.
  "Transaction" only where PostgreSQL's word is needed.
- **Change** (`Change`, `HeapChange`): one insert, update, delete or
  truncate of a spec table.
- **Unfit** (`Change::Unfit`): a change the store can't take, which
  starts Straum over ([Following](how-it-works.md#following)).
- **Apply** (`State::apply`): putting a commit into the store under the
  write lock.
- **Source** (`Source`, `Postgres`): the port to PostgreSQL. "Source of
  truth" means PostgreSQL itself.
- **Plain connection** (`Postgres::plain`): the second connection, which
  reads the WAL position each second.
- **WAL retained** (`Wal::retained`): bytes PostgreSQL keeps for the
  slot ([straum status](cli.md#straum-status)).
- **Watchdog** (`SlotGuard`, `watchdog.rs`): the thread that ends the
  stream when the slot holds more than `--max-retained-wal` or it can't
  read the slot ([What Straum costs the server](postgresql.md#what-straum-costs-the-server)).
- **Status** (`Status`): what `GET /status` and `straum status` show;
  not the stream's status report (`StreamThread::report`).
- **Held** (`App::held`): what `GET /views` and `straum views` show
  ([straum views](cli.md#straum-views)).
- **Shell**: `straum shell`, a prompt at a running Straum
  ([straum shell](cli.md#straum-shell)).
- **A view's rows** (`App::rows_ask`): what `GET /views/NAME` and
  `\rows` print ([Endpoints](asks.md#endpoints)).
- **Definition** (`Definition`): a view's `CREATE VIEW` statement as
  the spec wrote it, which `GET /views/NAME/spec` and `\d VIEW` print
  ([Endpoints](asks.md#endpoints)).
- **Health check**: `/livez` and `/readyz`
  ([Endpoints](asks.md#endpoints)).

## PostgreSQL

- **Setup**: the SQL `straum plan` prints
  ([Setting up PostgreSQL](postgresql.md#2-run-the-setup-sql)).
- **Setup check** (`catalog::check`, `Error::Setup`): the catalog
  compared with the spec at every start
  ([What Straum checks](postgresql.md#what-straum-checks-at-start)).
  Also "start check", "catalog check".
- **Problem**: one line of a failed setup check.
- **Role**: the role Straum connects as (`--role`); unrelated to
  `catalog::Purpose`.
- **DSN** (`Dsn`): the connection URL ([Connect](postgresql.md#3-connect)).
- **Publication**: holds every spec table on the logical stream
  (`--publication`).
- **Replication slot**: the temporary slot each start creates; "the
  slot" in the guide.
- **Replication** (`Replication`, `--wal`): logical decoding or the
  physical WAL.
- **Physical stream** (`Physical`, `Replay`): the physical WAL turned
  into pgoutput messages
  ([The physical stream](how-it-works.md#the-physical-stream)).
- **Redo point**: where a checkpoint's recovery starts; the first change
  to a page after it carries a **full-page image**.
- **Page state** (`Heap`, `Page`): the rows Straum keeps of each changed
  page since the last redo point.
- **Layout** (`Layout`): how a table's rows lie on its pages.
- **Pipeline** (`wal::pipeline::Pipeline`): records to commits: page
  state, transactions, publisher, decoder ([Start](design.md#start)).
- **WAL record** (`wal::Record`): one WAL entry, written by one
  **resource manager** (`xl_rmid`). One **passed by** (`Interest::PassBy`)
  names only other relation files' pages: the reader skips it unchecked.
- **Block reference** (`Block`): a page a record changes, by **relation
  file** (`RelationFile`) and **block number** (`BlockNumber`).
- **Slot on a page** (`SlotNumber`): a row's place on a heap page; not a
  replication slot.
- **xid** (`TransactionId`, `Xid`): a transaction id; a
  **subtransaction** is a savepoint's own. `xact` is PostgreSQL's word.
- **Running transactions** (`xact::Running`): the `RUNNING_XACTS`
  record.
- **Relcache invalidation** (`Invalidated`): a commit's note that a
  table's catalog entry changed.
- **Varlena** and **TOAST**: a value with a length header, and the table
  a long one moves to, leaving a pointer.
- **WAL problem** (`wal::Problem`): WAL Straum can't read; it stops the
  stream.
- **Replication connection**: the session that creates the slot, loads
  and streams.
- **LSN** (`Lsn`): a WAL position; a `Wal` reading holds `written`,
  `confirmed` and `restart`.
- **OID** (`Oid`): PostgreSQL's id for a catalog row: table, type, enum
  label.
- **Catalog** (`Catalog`, `Class`, `Column`): the system tables as the
  setup check reads them.
- **Old row** (`Change::Update::old`): the row before an update or
  delete ([Following](how-it-works.md#following)).
- **Label** (`EnumLabel`): an enum value's text
  ([Enum columns](spec.md#enum-columns)).
- **Poison value** (`ValueError::at`): a value Straum can't read, such
  as `NaN`; it stops the load or the stream.

## Testing

- **Reference** (`Reference`): what a test compares an answer with; a
  planted bug must make it fail.
- **Fake** (`Fake`): a scripted PostgreSQL; `PostgresContainer` is a
  real one in Docker.
- **Taxi trips**: the TLC yellow trips of January to June 2024
  ([Performance](performance.md)); the **taxi month** is January. The
  **warehouse copy** is outside the repository.

## Clashes

| Word | Meanings | Verdict |
| --- | --- | --- |
| key | view key; dim key; cell key; rollup key; `Condition::key` | bare key is a view key; qualify the rest |
| attribute / dim column | code, ADRs / guide | dim column in prose; attribute stays in code |
| level / width | design.md, ADR-0004 / spec.md | level is the cells, width the duration |
| state | `app::State`; `source::State`; cell state (`State`); running states | qualify in prose |
| the crate for Straum SQL | `straum-notation` | `straum-sql` needs an `ALLOWED` change in `scripts/check-layers.py`; not done |
| slot | replication slot; a row's slot on a heap page (`SlotNumber`) | qualify: "replication slot", "slot on a page" |
