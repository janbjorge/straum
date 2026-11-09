# Straum

> Live, pre-aggregated answers from PostgreSQL replication.

[![CI](https://github.com/janbjorge/straum/actions/workflows/ci.yml/badge.svg)](https://github.com/janbjorge/straum/actions/workflows/ci.yml)

Straum keeps declared PostgreSQL aggregates current in memory from logical
replication. Clients ask the HTTP API instead of repeatedly grouping source
tables. PostgreSQL remains the source of truth; Straum keeps no durable copy
and rebuilds on start.

> **Experimental.** M0–M6 are implemented and tested. Deployment hardening
> (TLS, lag bounds, Helm, and Azure validation) is planned next. Do not use
> this in production yet.

## Why

Analytical endpoints often run the same `GROUP BY` over a large operational
table. Cache misses and growing data make those queries slower over time.
Straum collapses rows into the groups declared in a spec, then updates those
groups as PostgreSQL changes.

```text
startup
-------
  Straum SQL spec
        |
        v
  parse and validate
        |
        v
+------------------+    temporary logical slot + exported snapshot    +------------+
| Straum           | ------------------------------------------------> | PostgreSQL |
|                  | <------------------------------------------------ |            |
| fact rollups     |  generated fact aggregates + dimension rows      | source     |
| dimension tables |                                                    +------------+
+------------------+

continuous operation
--------------------
+------------+       pgoutput changes       +------------------+       HTTP       +---------+
| PostgreSQL | ---------------------------> | Straum           | -------------> | clients |
| source     |  insert, update, delete,     | apply to rollups |  /ask, /readyz |         |
+------------+  truncate, commit            | LSN + commit time|                +---------+
                                           +------------------+
```

Every answer includes the LSN and commit time it reflects. Callers can see
exactly how current the result is.

## What it does

- Reads a small PostgreSQL `CREATE VIEW` subset as a specification.
- Maintains `count`, `count distinct`, `sum`, `min`, `max`, and `avg`.
- Supports dimensions, N:1 joins, filters, time buckets, and time zones.
- Loads from a snapshot consistent with its replication stream.
- Serves JSON asks, readiness, liveness, and Prometheus metrics over HTTP.
- Uses stock PostgreSQL logical replication; no extension or query proxy.

Straum is not a general SQL engine, a PostgreSQL replica, or a durable data
store. It only serves aggregates declared before it starts.

## A spec

Straum SQL borrows the shape of a PostgreSQL view, but it is a deliberately
narrow configuration format. PostgreSQL cannot create the statement because
`key` and `append` are Straum-only options. Straum parses the `SELECT` body,
generates an aggregate load query for each fact and a plain load for each
dimension, then keeps those in-memory structures current from `pgoutput`.

```sql
CREATE VIEW trips WITH (key = id, append = true) AS
SELECT date_bin('15 minutes', t.pickup_at, timestamptz 'epoch') AS bucket,
       t.payment_type,
       z.borough,
       count(*) AS trips,
       sum(t.fare_amount) AS fare,
       avg(t.trip_distance) AS distance
FROM public.trips t
LEFT JOIN public.zones z ON z.location_id = t.pickup_location_id
GROUP BY 1, 2, 3;
```

This creates a rollup keyed by the bucket, payment type, and zone reference.
The source rows do not live in Straum.

## Run locally

```sh
# Verify a spec and print the SQL required on PostgreSQL.
cargo run -p straum -- plan --spec path/to/trips.sql

# A table owner applies that SQL, then starts Straum.
export STRAUM_DSN='postgres://straum:password@localhost:5432/app'
cargo run -p straum -- run --spec path/to/trips.sql
```

`run` listens on `0.0.0.0:8080` by default. Its endpoints are:

| Endpoint | Purpose |
| --- | --- |
| `POST /ask` | Ask for a declared aggregate. |
| `GET /livez` | Process liveness. |
| `GET /readyz` | Ready only after every table is loaded and caught up. |
| `GET /metrics` | Prometheus metrics. |

The server needs PostgreSQL logical replication enabled. On PostgreSQL 18,
set `wal_level = logical` and restart the server before applying the SQL from
`straum plan`.

## Architecture

The workspace keeps policy separate from I/O:

```text
straum (composition root)
├── straum-notation ──> straum-domain
├── straum-pg ────────> straum-app ──> straum-domain
└── straum-http ──────> straum-app ──> straum-domain
```

`straum-domain` has no dependencies or I/O. `straum-app` holds use cases and the
`Source` port. PostgreSQL, HTTP, and parsing are adapters. CI checks this
production dependency graph.

## Deployment direction

The first product target is AKS beside Azure Database for PostgreSQL Flexible
Server. The planned Helm chart uses one Deployment replica, a ConfigMap for
the spec, a Secret for the replication DSN, and no persistent volume.

Before that release, Straum needs TLS to PostgreSQL, retained-WAL and lag
handling, OTLP, and a non-production Azure validation run.

## Development

Docker is required for the PostgreSQL integration tests.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
python3 scripts/check-layers.py
cargo test --workspace
cargo llvm-cov --workspace --all-targets --fail-under-lines 98
```

## Documentation

- [Architecture](research/ARCHITECTURE.md)
- [Build plan](PLAN.md)
- [PostgreSQL replication research](research/POSTGRES-REPLICATION.md)
- [Managed PostgreSQL constraints](research/MANAGED-POSTGRES.md)
- [AKS deployment design](research/AKS.md)

## License

Straum is licensed under [Apache-2.0](LICENSE-APACHE).
