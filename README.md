# Straum

**In-memory PostgreSQL aggregates, kept current through logical replication.**

[![CI](https://github.com/janbjorge/straum/actions/workflows/ci.yml/badge.svg)](https://github.com/janbjorge/straum/actions/workflows/ci.yml)

Straum loads aggregates declared in a SQL spec, then applies PostgreSQL changes
to them in memory. Clients read them through HTTP. PostgreSQL remains the
source of truth; Straum stores no durable copy.

**Not stable.** Use at your own risk.

The [user guide](https://janbjorge.github.io/straum/) covers the spec,
asks, PostgreSQL setup and running Straum on Kubernetes.

## Quick start

Given PostgreSQL source tables, write a spec:

```sql
CREATE VIEW orders WITH (key = id) AS
SELECT date_bin('1 hour', o.placed_at, timestamptz 'epoch') AS hour,
       o.status,
       o.customer_id,
       customer.country,
       count(*) AS orders,
       sum(o.amount) AS revenue,
       avg(o.amount) AS avg_amount
FROM public.orders o
LEFT JOIN public.customers customer ON customer.id = o.customer_id
GROUP BY 1, 2, 3, 4;
```

`CREATE VIEW` here is Straum configuration, not PostgreSQL DDL. `plan`
checks the spec and prints the SQL a table owner must apply:

```sh
cargo run -p straum -- plan --spec orders.sql

export STRAUM_DSN='postgres://straum:password@localhost:5432/app'
cargo run -p straum -- run --spec orders.sql
```

After `/readyz` returns `200`, ask for a declared aggregate:

```sh
curl --json '{
  "fact": "orders",
  "measures": ["orders", "revenue", "avg_amount"],
  "by": ["status", "customer.country"],
  "from": "2026-01-01T00:00:00Z",
  "to": "2026-01-02T00:00:00Z"
}' http://localhost:8080/ask
```

```json
{
  "lsn": "0/17B2788",
  "commit_time": null,
  "columns": ["status", "customer.country", "orders", "revenue", "avg_amount"],
  "rows": [
    ["paid", "NO", 2, 139.99, {"sum": 139.99, "count": 2}],
    ["paid", "SE", 1, 80.50, {"sum": 80.50, "count": 1}],
    ["refunded", "NO", 1, 42.00, {"sum": 42.00, "count": 1}]
  ]
}
```

`lsn` is the last PostgreSQL commit reflected in the answer. `commit_time` is
that commit's timestamp. It is `null` until Straum applies its first streamed
commit. `avg` comes as sum and count, so you pick the rounding. The guide's
[quick start](https://janbjorge.github.io/straum/quick-start.html) makes
these tables in Docker and gives this answer.

## Scope

Straum:

- reads a narrow PostgreSQL `CREATE VIEW` subset;
- maintains `count`, `count distinct`, `sum`, `min`, `max`, and `avg`;
- supports dimensions, N:1 joins, filters, and UTC time buckets, with wider
  buckets for long ranges;
- loads from a snapshot consistent with its replication stream;
- exposes JSON asks, readiness, and liveness.

Straum is not a SQL engine, PostgreSQL replica, cache, or durable store. It
only serves aggregates declared before startup.

## Requirements

Straum needs PostgreSQL 17 or newer and uses its stock logical replication.
On 17 and 18, enable it before applying the SQL emitted by `straum plan`:

```text
wal_level = logical
```

Restart PostgreSQL after changing `wal_level`. From 19, `replica`, the
default, is enough. `run` listens on
`0.0.0.0:8080` by default.

| Endpoint | Purpose |
| --- | --- |
| `POST /ask` | Read a declared aggregate. |
| `GET /livez` | Process liveness. |
| `GET /readyz` | Ready after all tables load and the stream catches up. |
| `GET /status` | What Straum has done, as `straum status` prints it. |

## How it works

![A table owner applies the setup SQL from straum plan. Straum loads from a snapshot at LSN L, then applies every commit after L and answers asks over HTTP.](docs/src/diagrams/overview.svg)

Startup uses an exported snapshot and temporary logical slot. The snapshot and
change stream share an LSN, so no committed change is missed between loading
and replication.

## Development

Docker is required for PostgreSQL integration tests.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
python3 scripts/check-layers.py
cargo test --workspace
cargo llvm-cov --workspace --all-targets --fail-under-lines 98
```

## Documentation

- [User guide](https://janbjorge.github.io/straum/), built from `docs/`
- [Architecture](research/ARCHITECTURE.md)
- [Build plan](PLAN.md)
- [PostgreSQL replication research](research/POSTGRES-REPLICATION.md)
- [Managed PostgreSQL constraints](research/MANAGED-POSTGRES.md)
- [AKS deployment design](research/AKS.md)

## License

[Apache-2.0](LICENSE-APACHE)
