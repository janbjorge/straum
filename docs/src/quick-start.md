# Quick start

This runs Straum against PostgreSQL 18 in Docker, with two small tables of
orders and customers. You need Docker, `curl` and a Rust toolchain
([rustup](https://rustup.rs)).

## 1. Install straum

```sh
git clone https://github.com/janbjorge/straum
cd straum
cargo install --locked --path crates/straum
```

`rust-toolchain.toml` picks the Rust version, and rustup fetches it the
first time. `straum help` lists the commands.

## 2. Start PostgreSQL

Straum reads changes through logical replication, which needs
`wal_level = logical`:

```sh
docker run -d --name straum-pg -e POSTGRES_PASSWORD=postgres \
  -p 5432:5432 postgres:18 -c wal_level=logical
```

Give it a few seconds to start, then make the tables and a few rows:

```sh
docker exec -i straum-pg psql -U postgres <<'SQL'
CREATE TABLE customers (id int PRIMARY KEY, country text);
CREATE TABLE orders (
  id bigint PRIMARY KEY,
  placed_at timestamptz NOT NULL,
  status text NOT NULL,
  customer_id int REFERENCES customers,
  amount numeric(10, 2) NOT NULL
);
INSERT INTO customers VALUES (1, 'NO'), (2, 'SE'), (3, 'NO');
INSERT INTO orders VALUES
  (1, '2026-01-01 09:12+00', 'paid', 1, 120.00),
  (2, '2026-01-01 09:40+00', 'paid', 2, 80.50),
  (3, '2026-01-01 10:05+00', 'refunded', 3, 42.00),
  (4, '2026-01-01 10:30+00', 'paid', 3, 19.99);
SQL
```

## 3. Write a spec

Save this as `orders.sql`:

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

It reads as SQL, and PostgreSQL would give the same rows for it, but it is
Straum configuration: Straum keeps one cell per hour, status and customer,
and looks up `customer.country` when it answers. [Writing a
spec](spec.md) covers what a spec can say.

## 4. Set up PostgreSQL

`straum plan` checks the spec and prints the SQL that lets Straum read the
tables: a role, its grants, a publication, and the replica identity that
gives Straum the old row of every update and delete. Run it as the tables'
owner, then give the role a password:

```sh
straum plan --spec orders.sql | docker exec -i straum-pg psql -U postgres
docker exec straum-pg psql -U postgres -c "ALTER ROLE straum PASSWORD 'straum'"
```

## 5. Run straum

```sh
export STRAUM_DSN='postgres://straum:straum@localhost:5432/postgres?sslmode=disable'
straum run --spec orders.sql
```

```text
straum: listening on 0.0.0.0:8080
straum: loaded every table; streaming from 0/17B2788
```

Straum connects with TLS unless the DSN says `sslmode=disable`. The
PostgreSQL in this container has no TLS, so this DSN turns it off; don't
do that on a network you don't trust.

## 6. Ask

In another terminal, check that `/readyz` says `ready`, then ask for
orders and revenue by status and country on 1 January. Answers come on
one line; they're spread out here to read.

```sh
curl http://localhost:8080/readyz
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

`lsn` is the last commit the answer reflects. `commit_time` is null
because no commit has come since the load. `avg` comes back as its sum and
count, so you pick the rounding.

## 7. Change the data

Insert an order and refund another, then ask again, this time per hour
for paid orders:

```sh
docker exec straum-pg psql -U postgres -c "
  INSERT INTO orders VALUES (5, '2026-01-01 11:00+00', 'paid', 2, 300.00);
  UPDATE orders SET status = 'refunded' WHERE id = 4;"
curl --json '{
  "fact": "orders",
  "measures": ["orders", "revenue"],
  "time": "bucket",
  "filter": {"status": "paid"},
  "from": "2026-01-01T00:00:00Z",
  "to": "2026-01-02T00:00:00Z"
}' http://localhost:8080/ask
```

```json
{
  "lsn": "0/17B2958",
  "commit_time": "2026-11-09T17:12:57.346371Z",
  "columns": ["bucket", "orders", "revenue"],
  "rows": [
    ["2026-01-01T09:00:00Z", 2, 200.50],
    ["2026-01-01T11:00:00Z", 1, 300.00]
  ]
}
```

The refunded order left the 10:00 bucket, and the new one is in the 11:00
bucket. The answer now names the commit that made both changes.

## 8. Look at it

`straum status` asks the running straum what it has done:

```sh
straum status
```

```text
ready:        yes
applied:      0/17B2958, committed 2026-11-09T17:12:57.346371Z
lag:          0.0 s, at most 30 s
WAL held:     0.0 MiB, at most 4096.0 MiB; the server is at 0/17B2958
starts:       1
failures:     0
last failure: none
```

## 9. Clean up

Stop straum with Ctrl-C. PostgreSQL drops its replication slot when the
connection closes. Then remove the container:

```sh
docker rm -f straum-pg
```

## Next

- [Writing a spec](spec.md): what you can declare.
- [Asking](asks.md): every field of an ask and an answer.
- [Setting up PostgreSQL](postgresql.md): a real server, managed or not.
