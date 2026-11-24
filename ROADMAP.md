# Roadmap

What Straum does not do yet, in the order we mean to take it. Decisions
live in [docs/adr/](docs/adr/); an item here names its record and goes
when it ships. An item under Parked says what would bring it back.

## Now: the outbox stream

A third way to follow PostgreSQL, for servers that give neither
logical decoding nor the replication attribute: triggers write changes
to a table Straum polls (ADR-0018). Designed and measured in
`bench/outbox.py`; none of it is in the binary.

1. Prove what is still only reasoned, in `bench/outbox.py`: a start
   while writers run, applying only whole transactions, and what a
   wide column no spec reads costs.
2. Take the write-ahead log (WAL) words out of the source port, so a
   source that has no log sequence number (LSN) fits it: positions and
   a backlog (ADR-0016).
3. Build the adapter in `straum-pg` on the connection methods it has
   today, so the client can change under it: the start, the poll, the
   sign. `straum plan` prints the table, the sequences, the trigger
   and the SQL that removes them.
4. The property test against a reference that ADR-0018 asks for.
5. What ships with it: the chart stops the old Straum before the new
   one starts on this stream; the status shows the outbox's size,
   backlog and skipped writes; the guide and the help text say one
   Straum an outbox.

Still to decide:

- The flag that picks the stream. `--wal logical|physical` today;
  `--stream logical|physical|outbox` is proposed.
- What the outbox stream shows where the others show an LSN: the
  `Straum-LSN` header and the status line.
- A spec table whose unique key is deferred: a new row can reach the
  outbox before the old one leaves.
- Whether the printed SQL refuses a table with a column PostgreSQL
  can't send in binary, which would fail the table's writes.

## Parked

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

## Open in the records

- A reload of one table, and applying a large commit in pieces
  (ADR-0009).
- Percentile asks miss the 1 ms target; ADR-0005 lists the steps.
- Keeping the widest width for a time, and a sliding window
  (ADR-0010).
