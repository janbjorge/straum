# Quick start

Straum against PostgreSQL 18 in Docker, with two small tables. You need
Docker, `curl` and a Rust toolchain ([rustup](https://rustup.rs)).

## 1. Install straum

```sh
git clone https://github.com/janbjorge/straum
cd straum
cargo install --locked --path crates/straum
```

`rust-toolchain.toml` picks the Rust version, and rustup fetches it.

## 2. Start PostgreSQL

`wal_level=logical` turns on the logical replication Straum follows:

```sh
docker run -d --name straum-pg -e POSTGRES_PASSWORD=postgres \
  -p 5432:5432 postgres:18 -c wal_level=logical
```

A few seconds later, make the tables:

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

Save as `orders.sql`:

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

PostgreSQL would give the same rows for this view. Straum keeps one cell
per hour, status and customer, and looks up `customer.country` when it
answers.

## 4. Set up PostgreSQL

`straum plan` prints the SQL Straum needs: a role, grants, a publication
and replica identity. Run it as the tables' owner and give the role a
password:

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

This container has no TLS, hence `sslmode=disable`. Otherwise Straum
always connects with TLS.

## 6. Ask

In another terminal:

```sh
curl http://localhost:8080/readyz
curl --json '{
  "query": "SELECT status, customer.country, orders, revenue, avg_amount FROM orders WHERE bucket >= $1 AND bucket < $2 GROUP BY status, customer.country",
  "parameters": ["2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z"]
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

`lsn` is the last commit the answer reflects; `commit_time` is null until
a commit arrives after the load. `avg` comes as sum and count, so you pick
the rounding.

## 7. Change the data

Insert an order, refund another, and ask for paid orders per hour:

```sh
docker exec straum-pg psql -U postgres -c "
  INSERT INTO orders VALUES (5, '2026-01-01 11:00+00', 'paid', 2, 300.00);
  UPDATE orders SET status = 'refunded' WHERE id = 4;"
curl --json '{
  "query": "SELECT bucket, orders, revenue FROM orders WHERE status = $1 AND bucket >= $2 AND bucket < $3 GROUP BY bucket",
  "parameters": ["paid", "2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z"]
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

The refund took the 10:00 order out of the paid rows and the new order
added 11:00, both at the commit the answer names.

## 8. Clean up

`straum status` shows what Straum has done; [Running
Straum](running.md#straum-status) explains it. Stop straum with Ctrl-C;
PostgreSQL drops its slot as the connection closes. Then remove the
container:

```sh
docker rm -f straum-pg
```
