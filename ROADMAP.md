# Roadmap

What Straum does not do yet, in the order we mean to take it. Decisions
live in [docs/adr/](docs/adr/); an item here names its record and goes
when it ships. An item under Parked says what would bring it back.

## Now: usage metering

A billing service, for example Stripe Meters, Lago or OpenMeter,
charges each account for its usage. Many apps write a row for each use
into their own tables. Straum can keep the sums of these rows and give
them to the billing service. The binary needs no change for this. An
ask with the bounds of a month reads the day buckets of the view.

1. Write a guide that shows the steps on the `usage` view. Edit
   ADR-0014 first, because the guide changes the page set.
2. Write a Python script, run with uv. The script asks Straum for the
   sum of each account in the last hour and sends it to Stripe Meters.
3. Find out if users want this. Show it in a Show HN post and in a
   Railway template.

Still to decide:

- The home of the guide: a new page, or a section of `asks.md`.
- The data that the script uses. The `usage` tables have no data, and
  the taxi trips have no accounts.

## Parked

### The outbox stream

Triggers write each change to a table, and Straum reads that table
(ADR-0018). This stream is for a server that gives neither logical
decoding nor the replication attribute. `bench/outbox.py` measures the
design. None of it is in the binary.

On 2026-11-25 we found little demand for this stream. Heroku Classic
plans give no replication slot. Heroku Advanced gives logical slots
since 2026-10-06. We found no user from 2025 or 2026 who asked for a
stream without a slot. The triggers also add a cost to each write of
the app.

These steps remain:

1. Prove these behaviours in `bench/outbox.py`: a start while writers
   run, an apply of only whole transactions, and the cost of a wide
   column that no spec reads.
2. Remove the write-ahead log (WAL) words from the source port, so
   that a source without a log sequence number (LSN) fits it
   (ADR-0016).
3. Build the adapter in `straum-pg`: the start, the poll and the sign.
   `straum plan` prints the table, the sequences, the trigger and the
   SQL that removes them.
4. Write the property test against a reference that ADR-0018 asks for.
5. Make the chart stop the old Straum before the new one starts. Show
   the size, backlog and skipped writes of the outbox in the status.

These questions remain open:

- The flag that selects the stream: `--wal logical|physical` today,
  or `--stream logical|physical|outbox`.
- The value that the outbox stream shows in place of an LSN, in the
  `Straum-LSN` header and the status line.
- A spec table with a deferred unique key. A new row can get to the
  outbox before the old row goes out.
- A table with a column that PostgreSQL cannot send in binary. The
  printed SQL can refuse it, or the writes to the table fail.

Back when: a user names a server that gives neither logical decoding
nor the replication attribute.

### A third-party PostgreSQL client

Straum's client is its own, about 1,400 lines on `postgres-protocol`
and `rustls` (ADR-0007). Surveyed on 2026-11-20:

- No released, established pure-Rust client reads the physical WAL.
  `tokio-postgres` has no replication; Materialize's fork has, from
  git only.
- `tokio-postgres` would do for the outbox stream alone. It needs
  tokio inside `straum-pg`, behind functions that block, which
  `scripts/check-layers.py` refuses today, and it leaves two clients.
- libpq does everything and needs a C compiler and OpenSSL, against
  ADR-0007 and ADR-0012.

Back when: a server asks for a sign-in Straum lacks and the fix below
is not enough; `tokio-postgres` ships replication; or a bug in the
client costs real time.

### Sign-in beyond SCRAM-SHA-256

Straum signs in with the Salted Challenge Response Authentication
Mechanism (SCRAM) or not at all. Managed servers also ask for a
password in the clear over TLS, which carries their identity tokens,
for SCRAM with channel binding, and old ones for MD5.
`postgres-protocol` has the functions; Straum doesn't call them.

Back when: a server Straum should follow asks for one.

### The TLS crypto

`rustls-graviola` keeps the build free of a C compiler (ADR-0012). It
calls itself very new, and aborts on a CPU without the instructions it
needs. `ring` and `aws-lc-rs` need a C compiler on Linux.

Back when: one of them builds without one, or graviola stops a start
on a server someone runs.

### An ask that pins each part of the rollup key

An ask can give each part of the rollup key with `=`. Then it reads at
most one cell in each bucket. The cell loop reads every cell of each
bucket that the ask covers, but each bucket has an index from cell key
to cell (`Cells::find`). The cell loop could find that one cell in the
index.

Back when: the cost benchmark (`just cost-bench`) shows that such an
ask spends its time on the cells of a bucket. Measure it first, then
find the cell in the index.

## Open in the records

- A reload of one table, and applying a large commit in pieces
  (ADR-0009).
- Percentile asks miss the 1 ms target; ADR-0005 lists the steps.
- Keeping the widest width for a time, and a sliding window
  (ADR-0010).
