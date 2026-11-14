# Straum: research notes

Straum is an aggregate server that sits next to PostgreSQL. The name is
the Nynorsk word for a stream or current: its input is PostgreSQL's
replication stream.

## The idea (jb, 2026-11-08)

- A spec names PostgreSQL tables and columns.
- Straum subscribes to them over logical replication and ingests only
  that subset.
- It keeps the data so that counts, sums, averages, minimums and
  maximums come back about 1000x faster than from PostgreSQL, with
  near-zero drift.
- Written in Rust.

## Where the design went

The research started from SQL: could PostgreSQL hand an aggregate
query to Straum by itself? On managed PostgreSQL every SQL route needed
an extension providers don't allow, a wire proxy in front of all
traffic, or Straum parsing SQL itself. On 2026-11-08 jb dropped SQL.
Aggregates are declared in a spec read at start, and asked for over
HTTP. The spec is Straum SQL, a PostgreSQL `CREATE VIEW` subset made
only for Straum specs. `docs/src/how-it-works.md` describes that
design; `docs/adr/` holds the decisions. The surveys of planner hooks,
Rust crates and competitors that led here are in the git history
before 2026-11-12.

What carried over: the speed comes from pre-aggregation, since
columnar engines are 31x to 103x faster than cached PostgreSQL on
ClickBench and 1000x needs rows that collapse into far fewer groups;
ingest works on managed PostgreSQL with one parameter and a restart;
drift is fixable per ask with a WAL barrier (POSTGRESQL.md §3), which
v1 doesn't build.

## Files

- [POSTGRESQL.md](POSTGRESQL.md): how Straum uses PostgreSQL, with
  the citations; what managed services allow; the AKS target.
- [JOINS.md](JOINS.md): what was built before ask-time joins, the
  sources behind the choice, and the numbers.
- [AGGREGATES.md](AGGREGATES.md): which aggregates merge and which
  take a delete, the sources, what other systems do, and where
  Straum's cell states sit.

## Tags

- **R**: read in the PostgreSQL tree, or in linked fetched source.
- **U**: unverified; from a search snippet, a third party, or memory.
- **I**: inference.

PostgreSQL tree: [upstream PostgreSQL](https://github.com/postgres/postgres/tree/0f3b42ce806),
version `20devel` (`meson.build:11`), master `0f3b42ce806`
(2026-10-19). Line numbers are from master, not the REL_18 or REL_19
branches. Versions are placed by ancestry against the stamp commits
`2652835d3ef` (19devel, the PG18 branch point) and `a281a3e6dbb`
(20devel, the PG19 branch point). PostgreSQL 16 and newer is the
target (jb, 2026-11-14); CI tests 16, 17, 18 and 19rc1.
