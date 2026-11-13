# ADR-0045: Tables can come from a feed in place of PostgreSQL

Status: Accepted, 2026-11-13

## Context

Straum took rows only from PostgreSQL. A benchmark of Straum alone
still started PostgreSQL in Docker, copied 20,332,093 taxi trips into
it and read them back out: a minute before Straum loaded a row. A
flag that made `straum` read files itself would put a file format,
its types and the time zone rules into the binary that follows
PostgreSQL.

## Decision

`straum run --feed SOCKET` takes its tables from a feed in place of
PostgreSQL. The feed is a Unix socket Straum listens on, behind the
`Source` port, in `straum-feed`. A feeder, a second program, connects
and says hello; from there Straum drives. It asks for each spec table
with the columns the spec reads, and the feeder answers with their
types, the rows and an end. After the last table Straum says it is
done and closes. Either end says why before it gives up.

A feed carries loads only. No change follows one, so a fed Straum is
ready, with no lag, until it stops.

`straum-parquet` is the feeder in the repository. It reads Parquet
files as a manifest types them, to the values PostgreSQL would hold.

## Consequences

- `bench/taxi.py --feed` has Straum ready 6.5 s after it starts, with
  no Docker, holding the cells a load from PostgreSQL gives: 1,157,581
  and 713,457 for the benchmark's two views.
- `straum` learns no file format. The Parquet reader, its codecs and
  the zone rules are dependencies of `straum-parquet` alone.
- Another feeder needs only the frames, in any language.
- The socket has no authentication: whoever may connect gives Straum
  every row it holds. Its file is its owner's alone, and it is not
  TCP.
- After a failure Straum waits for a feeder to connect again, and
  nothing starts one. A feed is for benchmarks and try-outs.
- The feed checks only the time column's type against the spec. Any
  other column of the wrong type fails at its first row.
- Frames for inserts, updates and deletes come with the first feeder
  that sends them.
