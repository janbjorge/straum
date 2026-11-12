# Straum: domain model (2026-11-12)

What Straum is made of and how the pieces depend on each other: the
participants, the use cases, the runtime flows, the model with its
dependencies and cardinalities, the state machines, and the decisions
behind the shape. Read from the code on 2026-11-12; a line reference is
where a rule is enforced. ARCHITECTURE.md says how Straum uses
PostgreSQL, JOINS.md how a join runs, `docs/adr/` holds the decisions
and `docs/src/glossary.md` the words. The glossary's words are used
here; the code's name follows in parentheses where it differs.

## Overview

Straum keeps the aggregates a spec declares as cells in memory, fed by
PostgreSQL's logical replication stream, and answers asks about them
over HTTP. Four participants:

- **PostgreSQL**: the source of truth. Passive: it publishes the
  stream and serves the snapshot loads. Nothing of Straum runs in it.
- **Straum** (`straum run`): active toward PostgreSQL, reactive toward
  clients. One writer thread opens the slot, loads the tables and
  applies every commit; tokio threads answer asks meanwhile.
- **Clients**: the apps that `POST /ask`. They never see a table, a
  join or a row, only the names a view gives its keys and measures.
- **People**: a table owner, who runs the setup SQL `straum plan`
  prints once, and an operator, who runs the binary and reads
  `/readyz`, `/status` and the log. In Kubernetes the chart is the
  operator.

Use cases: set PostgreSQL up, start, follow, answer an ask, start
over, stop.

## Architecture

Six crates, dependencies inward only (`scripts/check-layers.py`,
ADR-0003). An arrow is "depends on".

```text
                 ┌─────────────────────────────────────────────┐
                 │ straum: the binary, composition root        │
                 │ run · plan · status · the writer thread     │
                 └───┬───────────────┬────────────────┬────────┘
                     │               │                │
      ┌──────────────▼──┐   ┌────────▼───────┐   ┌────▼─────────────┐
      │ straum-notation │   │   straum-pg    │   │   straum-http    │
      │ Straum SQL text │   │ Source adapter │   │ /ask /livez      │
      │ into Spec, Ask  │   │ slot, COPY,    │   │ /readyz /status  │
      │                 │   │ pgoutput, WAL  │   │ tokio and hyper  │
      └────────┬────────┘   └────────┬───────┘   └────┬─────────────┘
               │                     │ Source         │ App
               │            ┌────────▼────────────────▼────────┐
               │            │            straum-app            │
               │            │ App, Writer, Status, Lag,        │
               │            │ ports Source and Spill           │
               │            └────────────────┬─────────────────┘
               │                             │
               │            ┌────────────────▼─────────────────┐
               └───────────►│          straum-domain           │
                            │ Spec, Ask, Store, Rollup,        │
                            │ Projection, Sealed, Answer       │
                            └──────────────────────────────────┘
```

- `straum-domain` has no dependencies and no I/O. Everything an ask
  or a change needs is a plain struct, enum or function.
- `straum-app` holds the use cases and the two ports. `Source` is the
  one trait PostgreSQL sits behind; `Spill` is where sealed buckets
  go. No adapter for `Spill` ships; `main.rs:477` passes `None`.
- The adapters never meet: `straum-http` is handed a query reader by
  the binary (`ReadAsk`, `straum-http/src/lib.rs:59`), so HTTP never
  knows Straum SQL.

How data moves at run time, which is not the dependency direction:

```text
PostgreSQL ──snapshot COPY, pgoutput──► straum-pg ──Commit──► Writer
                                                                │ apply
Apps ──POST /ask──► straum-http ──Ask──► App ──read lock──► Store
```

## Use cases

```text
Run Straum ──includes──► Start ──includes──► Load a table
           ──includes──► Follow ──includes──► Apply a commit
           ──includes──► Start over
           ──includes──► Stop
Set up PostgreSQL          (before the first run)
Answer an ask              (while Straum runs)
```

### UC1: Set up PostgreSQL

| Aspect        | Description                                           |
| ------------- | ----------------------------------------------------- |
| Primary actor | Table owner                                           |
| Input         | **Spec**, publication name, role, optional **DSN**    |
| Output        | Setup SQL, run once                                   |

Steps (`straum plan`, `straum-pg/src/plan.rs`):

1. Parse the spec; a spec that doesn't check refuses with every
   problem named.
2. Print `wal_level = logical`, `CREATE ROLE ... LOGIN REPLICATION`
   with the DSN's password as a SCRAM verifier, `GRANT USAGE` on each
   schema, `GRANT SELECT` on every spec table, `CREATE PUBLICATION`
   over every spec table, and `REPLICA IDENTITY FULL` on each, dims
   included (ADR-0031).
3. The owner runs it. The setup lives in the catalog and survives
   restarts and failover.

Alternatives: the setup check at every start (`catalog::check`) names
each missing piece and its fix in the log, `/readyz` and
`straum status`.

### UC2: Run Straum

| Aspect        | Description                                           |
| ------------- | ----------------------------------------------------- |
| Primary actor | Operator                                              |
| Input         | **Spec** file, **DSN**, flags (**Limits**)            |
| Output        | A serving Straum                                      |

Steps (`straum/src/main.rs:454`):

1. Read the spec and the DSN, bind the listener.
2. Make an **App** with an empty **Store** and the **Limits**; make a
   **Writer** over a **Postgres** source; spawn the writer thread.
3. Serve HTTP on tokio until SIGTERM or SIGINT.

A panic aborts the process (`main.rs:471`): the store may be half
written.

### UC3: Start

| Aspect        | Description                                           |
| ------------- | ----------------------------------------------------- |
| Primary actor | **Writer**                                            |
| Input         | **Spec**, **Source**                                  |
| Output        | A **Store** at LSN L and a **Following** from L       |

Steps (`Writer::start`, `straum-app/src/app.rs:339`):

1. Drop the old store and empty the spill, so two stores never take
   memory at once.
2. `Source::open`: connect, begin a read-only repeatable-read
   transaction, create a temporary logical slot with `SNAPSHOT 'use'`,
   read L. Read the catalog and check it against the spec.
3. `Source::load` each dim, then each fact: set the column types from
   the catalog, run one binary `COPY` of the spec's columns, add rows
   by the batch (`Store::add_rows`). The snapshot transaction commits
   with the last load.
4. Read the server's WAL once, start the **Lag** from when the slot was
   opened.
5. Take the write lock, swap the store in, set **Following** at L,
   count the start.
6. Run a seal pass.

Alternatives: any error is a **Failure**, and the next step is UC6.
Asks answer not ready until step 5.

### UC4: Follow

| Aspect        | Description                                           |
| ------------- | ----------------------------------------------------- |
| Primary actor | **Writer**                                            |
| Input         | The **Stream**                                        |
| Output        | One **Commit** applied per step                       |

Steps (`Writer::apply_next`, `app.rs:371`):

1. If the WAL was read a second or more ago: read it on the plain
   connection, fail past `--max-retained-wal`, feed the **Lag**, run a
   seal pass.
2. `Source::next_commit`, waiting at most until the next WAL reading
   is due. The first call turns the snapshot connection into the
   stream with `START_REPLICATION` from L.
3. Unseal every sealed bucket a fact row of the commit falls in.
4. Under the write lock, apply the commit: each change to its rollup
   or projection, tables the spec doesn't name skipped, then move
   **Following** to the commit's LSN and time.
5. Forget the sealed buckets of a truncated fact in the spill.

Alternatives: a change that no longer fits (`Change::Unfit`, a row
never added, a row that doesn't fit) is `Error::Unfit`; a transaction
past `--max-transaction-rows` is `Error::Transaction`; either is UC6.

### UC5: Answer an ask

| Aspect        | Description                                           |
| ------------- | ----------------------------------------------------- |
| Primary actor | Client                                                |
| Input         | **Ask**: a query and its parameters                   |
| Output        | **Answer** with `as_of`, `Straum-LSN`, `Server-Timing`|

Steps (`straum-http/src/lib.rs:124`, `App::ask`, `app.rs:163`):

1. Read at most 1 MiB of body; parse the query with the parameters
   bound to `$n`; refuse a range jiff can't print.
2. On a blocking thread, take the read lock.
3. Check the ask against the spec and name the sealed buckets it
   reads (`Store::sealed_read`); an invalid ask is 400 even while
   loading.
4. Without a **Following**, 503 not ready.
5. Get each sealed bucket from the spill and thaw it against the
   store; a bucket that can't be read is 500.
6. `Store::ask_with`: cover the range, group the cells, merge each
   measure, sort rows by key.
7. Answer with `as_of` from the **Lag** and the applied LSN in the
   header, `Cache-Control: no-store`.

### UC6: Start over

| Aspect        | Description                                           |
| ------------- | ----------------------------------------------------- |
| Primary actor | **Writer**                                            |
| Input         | A **Failure**                                         |
| Output        | A new **Start** after a backoff                       |

Steps (`Writer::step`, `app.rs:317`; `run_writer`, `main.rs:524`):

1. Set **Following** to none, record the failure, count it. Asks
   answer 503 from here.
2. Sleep 1 s, doubling after each failure in a row up to 60 s. A start
   that finishes resets it.
3. Step again, which is UC3. Opening the new stream drops the old one
   first; PostgreSQL drops its temporary slot as the connection
   closes.

### UC7: Stop

SIGTERM or SIGINT: stop accepting, finish the asks in flight, exit
(`main.rs:500`). The connections close and PostgreSQL drops the slots.

## Runtime flow

### Start

```text
Operator      Writer              straum-pg                 PostgreSQL
  │             │                     │                         │
  │ straum run  │                     │                         │
  │────────────►│ open(spec)          │                         │
  │             │────────────────────►│ BEGIN READ ONLY RR      │
  │             │                     │────────────────────────►│
  │             │                     │ CREATE_REPLICATION_SLOT │
  │             │                     │   TEMPORARY (SNAPSHOT)  │
  │             │                     │────────────────────────►│
  │             │                     │◄──────── L ─────────────│
  │             │                     │ catalog, check          │
  │             │◄──── L ─────────────│                         │
  │             │ load(dim), load(fact)                         │
  │             │────────────────────►│ COPY (...) TO STDOUT    │
  │             │                     │────────────────────────►│
  │             │◄──── batches ───────│◄──── binary rows ───────│
  │             │ add_rows            │ COMMIT after last load  │
  │             │ wal()               │────────────────────────►│
  │             │ swap store in; Following at L                 │
  │             │ next_commit         │ START_REPLICATION at L  │
  │             │────────────────────►│────────────────────────►│
```

### Follow and answer, at once

```text
PostgreSQL      stream thread      Writer            App          Client
  │ pgoutput       │                 │                │              │
  │───────────────►│ decode until    │                │              │
  │                │ Commit          │                │   POST /ask  │
  │                │────Commit──────►│                │◄─────────────│
  │◄──status r────│                 │ unseal touched │ read lock    │
  │                │                 │ write lock     │ ask_with     │
  │                │                 │ apply          │─────answer──►│
  │                │                 │ unlock         │              │
  │                │◄──applied LSN───│                │              │
  │   every ~1 s   │                 │ wal() on the plain connection │
  │◄───────────────────────────────── │ lag.read, seal pass          │
```

The stream thread reads ahead of the writer, up to 64 commits, and
answers keepalives while the writer is busy (`stream.rs:29`).

### Start over

```text
Writer                         straum-pg                   PostgreSQL
  │ step fails                     │                           │
  │ Following = None, failure      │                           │
  │ sleep backoff                  │                           │
  │ open(spec)                     │ drop old Stream           │
  │───────────────────────────────►│ close connection ────────►│ slot dropped
  │                                │ new slot, new snapshot    │
  │ ... UC3 from step 2            │                           │
```

## Model

Arrows denote dependency, not communication flow. A cardinality is
how many of the pointed-at thing one of the pointing thing holds.

### The spec: what Straum keeps

Immutable once `Spec::new` has checked it. Every node is a value
object compared by value.

```text
┌────────┐ 0..* ┌─────────┐ 1  ┌───────┐
│  Spec  │─────►│   Dim   │───►│ Table │ schema, name
│        │      │ key 1..*│    └───────┘
│        │      │ attrs   │        ▲ 1
│        │      │ 0..*    │        │
│        │      └─────────┘        │
│        │           ▲ 1 (by name) │
│        │ 1..* ┌────┴────┐ 0..* ┌─┴───────┐ 1..* ┌─────┐
│        │─────►│  Fact   │─────►│   Ref   │─────►│ Via │ Fact | Attribute
└────────┘      │ (view)  │      │ kind:   │ 0..* ┌───────────┐
                │         │      │ RefKind │─────►│ Attribute │ name, column
                │         │      └─────────┘      └───────────┘
                │         │ 1    ┌──────┐ 1   ┌───────┐
                │         │─────►│ Time │────►│ Width │ seconds
                │         │      │origin│ 0..*└───────┘ wider
                │         │      └──────┘
                │         │ 0..* ┌───────┐
                │         │─────►│ ByKey │ Column | IsNull
                │         │      └───────┘
                │         │ 1..* ┌─────────┐ 1   ┌───────────┐
                │         │─────►│ Measure │────►│ Aggregate │
                │         │      │         │     └───────────┘
                │         │      │         │ 0..* (FILTER)
                │         │      └────┬────┘
                │         │ 0..* (WHERE)│
                │         │───────────┐│
                └─────────┘           ▼▼
                                 ┌───────────┐ 1..* ┌─────────┐
                                 │ Condition │─────►│ Literal │
                                 └───────────┘      └─────────┘
```

An **Aggregate** is count, sum, min, max, avg or count distinct; all
but count name one input column (`aggregate.rs:11`).

- A **Spec** holds dims and facts in one namespace
  (`spec.rs:344`). `Spec::tables` lists each table once, dims first.
- A **Dim** is a table: one per table however many views join it,
  carrying the union of the columns any view reads (`Dim::attributes`).
  Its key is one column or more, and any number of rows may share a
  key.
- A **Fact** is one view: a table, one **Time**, by keys, refs in join
  order, measures, and the view's `WHERE` as a filter. A ref joined
  through another dim comes after it (`Via::Attribute::through` is an
  index into `Fact::refs` before it, `spec.rs:189`).
- A **Ref** points at a dim by name, with as many **Via** columns as
  the dim's key has (`SpecError::KeyLength`). Left and inner refs read
  at least one **Attribute**; semi and anti read none
  (`spec.rs:536`).
- Keys, attributes and measures share one namespace per view, and
  nothing may be named `bucket` (`spec.rs:585`). An ask reaches any of
  them by that name alone (`Fact::resolve`).
- **Width** divides a day; each wider width is a whole multiple of
  the one before (`spec.rs:438`).

Derived from a fact, not stored:

| Derived        | What                                      | Where         |
| -------------- | ----------------------------------------- | ------------- |
| Rollup key     | by parts, each fact-side via once, bucket | `spec.rs:249` |
| Row layout     | time, by columns, vias, inputs, each once | `spec.rs:298` |
| Dim row layout | key columns, then attributes              | `spec.rs:37`  |
| Direct         | every via is a fact column, no chain      | `spec.rs:272` |

### The store: what Straum holds at run time

Mutable, one per process, owned by **App**. Identity is the fact or
dim name, the width and the bucket start.

```text
┌───────┐ 1          ┌──────┐
│ Store │───────────►│ Spec │
│       │            └──────┘
│       │ 1 per fact ┌────────┐ 1 per width ┌───────┐ 0..* ┌────────┐
│       │───────────►│ Rollup │────────────►│ Level │─────►│ Bucket │
│       │ 0..1 per   │ keyed  │             └───────┘      │ Hot |  │
│       │ fact       │        │ 1 per key   ┌───────┐      │ Sealed │
│       │───────────►│ Rollup │ column      │ Codes │      └───┬────┘
│       │            │ bound  │────────────►│       │          │ 1 if hot
│       │            └────────┘             └───────┘      ┌───▼────┐
│       │ 1 per dim  ┌────────────┐ 1 per attribute ▲      │ Cells  │
│       │───────────►│ Projection │─────────────────┘      └───┬────┘
└───────┘            │ key -> rows│                            │ 1 per input
                     └────────────┘                        ┌───▼────────┐
                                                           │ State      │
                                                           │ Sum |      │
                                                           │ Extent |   │
                                                           │ Distinct   │
                                                           └────────────┘
```

- A **Rollup** (`rollup.rs:23`) is every cell of one fact: one
  **Level** per width, the `date_bin` width first, and one **Codes**
  per rollup-key column. The bucket is not coded; it keys the level's
  map. Rows without a time go under `None`, which no ask reads
  (`rollup.rs:96`).
- A **Bucket** is **Hot**, holding **Cells**, or **Sealed**, holding
  only its cell count (`rollup.rs:100`). Only `date_bin` buckets are
  sealed (ADR-0035, `rollup.rs:561`).
- **Cells** are columnar: keys, counts and one **State** per input
  column, side by side, with an index from key codes to position
  (`rollup.rs:171`). A cell goes with its last row (`rollup.rs:491`).
- A **State** is shared by the measures that read its column: sum and
  avg one `Sum`, min and max one `Extent`, count distinct a
  `Distinct` (`aggregate.rs:29`). Filtered measures keep no state;
  their conditions name keys only.
- A **Projection** (`projection.rs:14`) is a dim as a map from key
  values to the attribute codes of every row under it. A row with a
  null in its key is never held (`projection.rs:100`). `Null` is code
  0 in every attribute, so no row and a null read alike.
- **Bound** cells (`store.rs:41`, ADR-0038) are a second rollup of a
  fact whose refs are all direct: keyed by the by keys and the
  attributes' values, so an ask reads no projection. A fact row goes
  into both rollups; into the bound one once per way its refs resolve
  now (`Bound::rows`, `store.rs:101`). A change to a dim the fact
  reads drops them while the fact holds rows (`Store::stale`,
  `store.rs:298`); the next start makes them again.

### The ask and the answer

```text
┌───────┐ 1 (by name) ┌──────┐          ┌────────┐ 1..* ┌──────────────┐
│  Ask  │────────────►│ Fact │          │ Answer │─────►│ AnswerColumn │
│       │ 1..*  ┌─────────┐  │          │        │ 0..* ┌───────────┐  │
│       │──────►│ Measure │ (by name)   │        │─────►│ AnswerRow │  │
│       │ 0..*  ┌────────┐               └────────┘      │ keys 0..*│  │
│       │──────►│ AskKey │ Key(name) | Time(bucket)      │ values   │  │
│       │ 0..*  ┌───────────┐                            │ 1..*     │  │
│       │──────►│ Condition │                            └──────────┘  │
│       │ from, to: bucket edges, from < to              Datum, Value  │
└───────┘
```

- `Spec::check_ask` (`ask.rs:42`) resolves names, refuses two time
  keys, a name asked twice, an empty range and a bound off a bucket
  edge. The store adds the type check once a load has set the types
  (`store.rs:491`).
- **Answered** (`app.rs:90`) wraps an answer with the applied LSN and
  `as_of`. The domain never knows time or LSNs beyond the bucket.

### The running system

```text
┌────────┐ 1   ┌─────┐ 1 (RwLock) ┌───────┐ 1    ┌───────┐
│ Writer │────►│ App │───────────►│ State │─────►│ Store │
│        │     └─────┘            │       │ 0..1 ┌───────────┐ 1   ┌─────┐
│        │ 1     ▲ clones share   │       │─────►│ Following │────►│ Lag │
│        │────► Source (port)     │       │      │ applied   │ 1   ┌─────┐
│        │                        │       │      │ committed │────►│ Wal │
└────────┘                        │       │ 0..1 ┌──────────┐      └─────┘
                                  │       │─────►│ Spilling │──► Spill (port)
                                  │       │ 1    ┌────────┐
                                  │       │─────►│ Limits │
                                  │       │ 0..1 ┌─────────┐
                                  │       │─────►│ Failure │
                                  └───────┘      └─────────┘

Source ──► Commit 0..* ──► Change 1..* ──► Datum 0..*
                                           Insert | Update | Delete |
                                           Truncate | Unfit
```

- **App** is an `Arc<RwLock<State>>`; clones share it (ADR-0009).
  Asks take the read lock, the writer the write lock only to apply a
  commit it has read or to swap a store in (`app.rs:357`, `:387`).
- **Following** exists only while every table is loaded; a failure
  removes it (`app.rs:326`). Its absence is "not ready".
- **Lag** (`lag.rs:14`) keeps the WAL readings the slot hasn't
  confirmed, at most an hour of them, and the instant it last had;
  `as_of` and readiness derive from it.
- A **Commit** is one transaction, whole, with the end of its commit
  record as LSN. The stream delivers nothing before the commit, and
  the store sees it as one.

### Components

| Component        | Type         | Description                            |
| ---------------- | ------------ | -------------------------------------- |
| Writer           | Service      | The one loop that loads and applies    |
| App              | Service      | The store and how far it follows       |
| Store            | Entity       | Rollups, projections, bound cells      |
| Rollup           | Entity       | Every cell of one fact, per width      |
| Level            | Entity       | The buckets of one width               |
| Bucket           | Entity       | The cells of one span, hot or sealed   |
| Cells            | Entity       | One bucket's cells, columnar           |
| State            | Entity       | One input column's per-cell state      |
| Projection       | Entity       | A dim as key to attribute codes        |
| Codes            | Entity       | A column's values under small integers |
| Lag              | Entity       | Unconfirmed WAL readings, caught up at |
| Source           | Port         | Open, load, next commit, read WAL      |
| Spill            | Port         | Put, get, remove, clear sealed buckets |
| Postgres         | Adapter      | `Source` over a slot, `COPY`, pgoutput |
| HTTP server      | Adapter      | `/ask`, `/livez`, `/readyz`, `/status` |
| Straum SQL       | Adapter      | Text into **Spec** and **Ask**         |
| Spec             | Value Object | Every dim and fact, checked            |
| Fact, Dim        | Value Object | One view; one joined table             |
| Ref, Via         | Value Object | A join and where its key comes from    |
| Time, Width      | Value Object | The time column, origin and widths     |
| ByKey            | Value Object | A fact column, or whether it is null   |
| Measure          | Value Object | Name, aggregate, filter                |
| Condition        | Value Object | `key = value` or `key IN (values)`     |
| Literal          | Value Object | An integer, text or boolean            |
| Identifier       | Value Object | Unquoted lowercase, at most 63 bytes   |
| Ask              | Value Object | View, measures, keys, filter, range    |
| Answer           | Value Object | Columns and rows                       |
| Answered         | Value Object | Answer, applied LSN, `as_of`           |
| Datum, Value     | Value Object | One stored value; one measure's result |
| ColumnType       | Value Object | Int, numeric, bool, text, time         |
| Commit, Change   | Value Object | One transaction; one row change        |
| Lsn, Wal         | Value Object | A position; written/confirmed/restart  |
| Following        | Value Object | Applied LSN, commit time, lag, WAL     |
| Limits           | Value Object | `--max-lag`, `--max-retained-wal`      |
| Status, Failure  | Value Object | What `/status` shows; one failed step  |
| Sealed, SealedId | Value Object | A bucket as plain data; its name       |
| Thawed           | Value Object | A sealed bucket checked by the store   |

## Invariants

What the code holds true, and where. Planting a bug in any of these
makes a property test fail.

1. **A commit applies whole.** The decoder holds a transaction until
   its Commit message (`decode.rs:28`), and the store takes it under
   one write lock (`app.rs:387`). An answer never sees half of one.
2. **No commit is missed or applied twice.** The snapshot is at the
   slot's consistent point L and the stream starts at L
   (`source.rs:89`, `:210`); `the_snapshot_and_the_stream_split_exactly`
   races 2,000 commits against it.
3. **Only the writer writes.** Every `Spill` method takes `&self`; the
   store is reached through `App::read` by asks and `App::write` by
   the writer alone.
4. **A row removed was added.** `RowError::Missing` from a cell, a
   value count or a projection entry ends the step
   (`rollup.rs:486`, `projection.rs:141`), and Straum starts over.
5. **Types come from the catalog only.** The load sets them
   (`load.rs:37`); a filter value must fit its column
   (`value.rs:35`); a Relation message that changes them is `Unfit`
   (`decode.rs:132`).
6. **Null joins nothing.** A dim row with a null key is never held and
   a lookup with a null finds nothing (`projection.rs:100`, `:164`).
7. **A row with no time is kept, never answered** (`rollup.rs:96`).
8. **Every width divides a day and nests in the next**
   (`spec.rs:438`); every ask bound is on a `date_bin` edge
   (`ask.rs:96`); an ask never reads part of a bucket.
9. **Levels agree.** Every row goes into one cell per width in the
   same add (`rollup.rs:404`); `cover` reads each instant of a range
   from exactly one level (`rollup.rs:538`).
10. **A sealed bucket takes no change.** A keyed rollup refuses a row
    into one (`rollup.rs:744`); the writer unseals before it applies
    (`app.rs:385`). A bound rollup, which is derived, lets the row
    pass and keeps nothing (`rollup.rs:136`).
11. **The spill holds what the store has sealed and no more.** Put
    before seal (`app.rs:410`), removed on unseal and truncate
    (`app.rs:437`, `:388`), cleared at every start (`app.rs:343`).
12. **Bound cells agree with the dims they were bound under.** They
    exist only for a direct fact (`store.rs:60`) and go the moment a
    dim they read changes under held rows (`store.rs:298`); an ask
    falls back to the keyed cells, also when a covered bucket is
    sealed (`store.rs:600`).
13. **Lag never understates.** Readings merge only toward older
    (`lag.rs:33`); `matches_every_reading_kept` checks it against a
    reference that keeps every reading.
14. **Two stores never take memory at once.** The old store goes
    before the new load (`app.rs:341`).
15. **One name, one thing, per view.** Keys, attributes and measures
    share a namespace, `bucket` is reserved, and an answer's columns
    are its names (`spec.rs:423`, `ask.rs:74`).

## State machines

### The writer, and what `/readyz` says

There is no enum. The state is derived: **Following** is `None` while
loading or waiting, `Failure` says which, and lag against `--max-lag`
tells Ready from Lagging (`App::status`, `app.rs:191`; `why_not`,
`status.rs:12`).

```text
   straum run
       │
       ▼
  ┌─────────┐  every table loaded   ┌─────────┐
  │ Loading │──────────────────────►│  Ready  │◄──────────────┐
  └─────────┘                       └────┬────┘               │
       ▲                                 │ lag > --max-lag    │ catches up
       │ backoff over                    ▼                    │
  ┌────┴────┐  any failure          ┌─────────┐               │
  │ Waiting │◄───────────────────── │ Lagging │───────────────┘
  └─────────┘◄──────────────────────┴─────────┘
       ▲         any failure from Loading too
       │
  any step that fails: lost connection, setup problem, Unfit,
  Transaction, Retained, Spill, Sealed

  SIGTERM or SIGINT from any state ──► Stopping ──► exit
```

| State    | `/readyz`                               | Asks     |
| -------- | --------------------------------------- | -------- |
| Loading  | 503, `loading` or the last failure      | 503      |
| Ready    | 200                                     | answered |
| Lagging  | 503, `N s behind PostgreSQL`            | answered |
| Waiting  | 503, the failure                        | 503      |
| Stopping | connection refused once the listener closes | drained |

### The source

`Postgres` moves `Closed` to `Loading` on `open`, `Loading` to
`Streaming` on the first `next_commit`, and back to `Closed` on the
next `open` (`straum-pg/src/source.rs:41`). A `load` outside
`Loading` and a `wal` while `Closed` are `Error::Stopped`.

```text
  Closed ──open──► Loading ──next_commit──► Streaming
    ▲                 │                         │
    └───── open ──────┴──────── open ───────────┘
```

### A bucket

```text
  (new row)        seal pass, ended --seal-after ago
     │                        │
     ▼                        ▼
   Hot ──────────────────────────────► Sealed
    ▲                                    │
    └──── a change touches it (unseal) ──┘
    └──── last row removed ──► gone
```

Only `date_bin` buckets seal. A truncate drops every bucket of the
fact, sealed ones from the spill too.

### Bound cells of a fact

```text
  Store::new, fact direct with refs
     │
     ▼
   Present ──── a dim it reads changes while the fact holds rows ───► Dropped
     ▲                                                                   │
     └──────────────────── next start (new Store) ───────────────────────┘
```

A dim change while the fact holds no rows keeps them
(`a_dim_change_under_no_rows_keeps_the_bound_cells`); a dim update
that reads the same changes nothing (`store.rs:255`).

## Key design decisions

Each is settled in an ADR; this is the shape they give the model.

1. **Cells and nothing else** (ADR-0002, amended by ADR-0030). No raw
   rows, no column store. Memory follows distinct keys times buckets,
   plus each dim projected to its key and the columns read.
   Tradeoff: anything a cell can't be updated from in place must come
   from PostgreSQL again, which is why updates and deletes need the
   old row and why a start is a full load.
2. **Crates inward, traits only at ports** (ADR-0003). The domain is
   property-tested against a naive reference with no PostgreSQL; the
   app against a scripted fake. Tradeoff: small test helpers are
   copied per crate.
3. **A spec is a `CREATE VIEW` subset that reads as PostgreSQL would**
   (ADR-0004, ADR-0005, ADR-0029). The view body is the test oracle.
   One spelling per meaning, nothing inferred. Tradeoff: verbose
   specs, and a walker that must list every accepted construct.
4. **Asks are Straum SQL with every value a parameter** (ADR-0006).
   Values never become query text. Tradeoff: a client learns a small
   grammar instead of a JSON shape.
5. **Answers carry `as_of`, the LSN goes in a header** (ADR-0007,
   ADR-0020). A client reasons about time; `as_of` keeps moving on a
   quiet database and errs up to a second early, never late.
6. **One store behind one `RwLock`** (ADR-0009). A load fills a new
   store outside the lock. Tradeoff: a long ask delays the writer,
   which shows as lag.
7. **Straum is its own replication client on a temporary slot**
   (ADR-0010, ADR-0012, ADR-0022). The load reads the snapshot of the
   slot the stream comes from, on the same connection, so one port
   covers both. A crash leaves no slot and no retained WAL. Tradeoff:
   no slot failover; every start loads everything.
8. **Every spec table has `REPLICA IDENTITY FULL`** (ADR-0014,
   ADR-0031). Facts need the old row to leave a cell, dims to find the
   projection entry. Tradeoff: WAL grows on update and delete, never
   on insert.
9. **A change that can't apply starts Straum over** (ADR-0015). One
   recovery path. The new snapshot is after the failing commit, so its
   rows come by `COPY` instead of under the write lock.
   `--max-transaction-rows` and `--max-retained-wal` use the same
   path.
10. **Refs are bound at ask time, with bound cells beside**
    (ADR-0030, ADR-0038). Cells are keyed by join columns and read
    the dim as it is now, so a dim change moves every answer and
    starts nothing over. Bound cells give an unjoined ask's cost
    while they hold. Tradeoff: cells follow distinct join ids.
11. **One level of cells per width the view lists** (ADR-0017,
    ADR-0028). Levels are an index: answers don't change, memory grows
    per width, a long range reads few cells.
12. **Cold `date_bin` buckets seal through a port** (ADR-0033,
    ADR-0035). The domain hands a bucket out as plain data and keeps
    its place; the writer unseals before a change reaches it.
    Tradeoff: an ask grouped by bucket over old time reads the spill
    for every bucket.

## Where the code is

| Concept                      | File                                     |
| ---------------------------- | ---------------------------------------- |
| Spec, Fact, Dim, Ref, checks | `crates/straum-domain/src/spec.rs`       |
| Ask, `check_ask`             | `crates/straum-domain/src/ask.rs`        |
| Aggregate, Partials          | `crates/straum-domain/src/aggregate.rs`  |
| Rollup, Level, Bucket, Cells, State | `crates/straum-domain/src/rollup.rs` |
| Projection                   | `crates/straum-domain/src/projection.rs` |
| Store, Bound, ask loops      | `crates/straum-domain/src/store.rs`      |
| Sealed, SealedId, Thawed     | `crates/straum-domain/src/sealed.rs`     |
| Datum, Value, ColumnType     | `crates/straum-domain/src/value.rs`      |
| App, State, Writer, Status   | `crates/straum-app/src/app.rs`           |
| Source port, Commit, Change  | `crates/straum-app/src/source.rs`        |
| Spill port, Spilling         | `crates/straum-app/src/spill.rs`         |
| Lag                          | `crates/straum-app/src/lag.rs`           |
| Straum SQL walker            | `crates/straum-notation/src/sql/`        |
| Postgres source, Snapshot    | `crates/straum-pg/src/source.rs`         |
| Setup check                  | `crates/straum-pg/src/catalog.rs`        |
| Load                         | `crates/straum-pg/src/load.rs`           |
| Stream thread                | `crates/straum-pg/src/stream.rs`         |
| Decoder, Transaction         | `crates/straum-pg/src/decode.rs`         |
| HTTP                         | `crates/straum-http/src/lib.rs`          |
| Commands, writer thread      | `crates/straum/src/main.rs`              |

## Deviations and open points

1. **ADR-0038 describes a rebind pass the code doesn't have.** The ADR
   says a dim change moves bound cells from the keyed ones in one pass
   over them. The code drops the bound rollup instead
   (`Store::stale`, `store.rs:298`) and reads the keyed cells until
   the next start, which is what `docs/src/how-it-works.md` and the
   tests say. Either the ADR gets an amendment or the pass gets
   built.
2. **No spill ships.** `--spill-dir` and `--seal-after` in ADR-0033
   don't exist; `main.rs:477` passes `None`, so nothing seals outside
   the tests.
3. **A chained ref keeps no bound cells** (`store.rs:60`, ADR-0038).
4. **The running states have no enum.** `Status` derives them each
   time; a reader of the code finds them in `status.rs:12`.
5. **Per-ask freshness barrier** stays open (ARCHITECTURE.md §4c).
