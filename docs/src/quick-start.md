# Quick start

Straum against PostgreSQL 18, both in Docker, with two small tables.
You need Docker and `curl`. Every other page uses these tables and this
spec.

## 1. Pull the image

```sh
docker pull ghcr.io/janbjorge/straum
```

`latest` is the newest stable release.

## 2. Start PostgreSQL

```sh
docker network create straum
docker run -d --name straum-pg --network straum \
  -e POSTGRES_PASSWORD=postgres postgres:18 -c wal_level=logical
```

A few seconds later, make the tables:

```sh
docker exec -i straum-pg psql -U postgres <<'SQL'
CREATE TABLE customers (id int PRIMARY KEY, country text);
CREATE TABLE orders (
  id bigint PRIMARY KEY,
  placed_at timestamptz NOT NULL,
  shipped_at timestamptz,
  status text NOT NULL,
  customer_id int REFERENCES customers,
  amount numeric(10, 2) NOT NULL
);
INSERT INTO customers VALUES (1, 'NO'), (2, 'SE'), (3, 'NO');
INSERT INTO orders VALUES
  (1, '2026-01-01 09:12+00', '2026-01-02 08:00+00', 'paid', 1, 120.00),
  (2, '2026-01-01 09:40+00', NULL, 'paid', 2, 80.50),
  (3, '2026-01-01 10:05+00', NULL, 'refunded', 3, 42.00),
  (4, '2026-01-01 10:30+00', NULL, 'paid', 3, 19.99);
SQL
```

## 3. Write a spec

Save as `orders.sql`:

```sql
CREATE VIEW orders WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', orders.placed_at, timestamptz 'epoch') AS bucket,
       orders.status AS status,
       orders.customer_id AS customer_id,
       customers.country AS country,
       count(*) AS orders,
       sum(orders.amount) AS revenue,
       avg(orders.amount) AS avg_amount
FROM public.orders AS orders
LEFT OUTER JOIN public.customers AS customers ON customers.id = orders.customer_id
GROUP BY 1, 2, 3, 4;
```

PostgreSQL would give the same rows for this view. Straum never creates
it; it keeps one cell per hour, status, customer and country in memory.
`WITH (buckets = '1 hour')` lists every bucket width it keeps, here the
hour of the `date_bin` alone. [Writing a spec](spec.md) explains each
line.

## 4. Set up PostgreSQL

Straum connects as the user in `STRAUM_DSN`. `straum plan` reads it too,
and prints the SQL that makes that role with that password, lets it read
the tables and publishes `orders`. Run the SQL as the tables' owner:

```sh
export STRAUM_DSN='postgres://straum:straum@straum-pg:5432/postgres?sslmode=disable'
docker run --rm -e STRAUM_DSN -v "$PWD:/spec" ghcr.io/janbjorge/straum \
  plan --spec /spec/orders.sql | docker exec -i straum-pg psql -U postgres
```

This container has no TLS, hence `sslmode=disable`. Otherwise Straum
always connects with TLS.

## 5. Run straum

```sh
docker run --rm --name straum --network straum -p 8080:8080 \
  -e STRAUM_DSN -v "$PWD:/spec" ghcr.io/janbjorge/straum \
  run --spec /spec/orders.sql
```

```text
straum: listening on 0.0.0.0:8080
straum: loaded every table; streaming from 0/17B2788
```

## 6. Ask

In another terminal:

```sh
curl --json '{
  "query": "SELECT status, country, orders, revenue, avg_amount FROM orders WHERE bucket >= $1 AND bucket < $2 GROUP BY status, country",
  "parameters": ["2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z"]
}' http://localhost:8080/ask
```

```json
{
  "as_of": "2026-11-09T17:12:41.902318Z",
  "rows": [
    {"status": "paid", "country": "NO", "orders": 2, "revenue": 139.99, "avg_amount": 69.995},
    {"status": "paid", "country": "SE", "orders": 1, "revenue": 80.50, "avg_amount": 80.5},
    {"status": "refunded", "country": "NO", "orders": 1, "revenue": 42.00, "avg_amount": 42}
  ]
}
```

Every commit made before `as_of` is in the answer, and every ask says
which buckets it reads. [Asking](asks.md) explains the query, the
parameters and the answer.

## 7. Change the data

Straum follows commits asynchronously, so the first answer after a write
can predate it. Insert an order, refund another, and ask for paid orders
per hour on that day. If its `as_of` predates the write, ask again.
Every value in an ask is a parameter:

```sh
docker exec straum-pg psql -U postgres -c "
  INSERT INTO orders VALUES (5, '2026-01-01 11:00+00', NULL, 'paid', 2, 300.00);
  UPDATE orders SET status = 'refunded' WHERE id = 4;"
curl --json '{
  "query": "SELECT bucket, orders, revenue FROM orders WHERE status = $1 AND bucket >= $2 AND bucket < $3 GROUP BY bucket",
  "parameters": ["paid", "2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z"]
}' http://localhost:8080/ask
```

```json
{
  "as_of": "2026-11-09T17:12:57.861204Z",
  "rows": [
    {"bucket": "2026-01-01T09:00:00Z", "orders": 2, "revenue": 200.50},
    {"bucket": "2026-01-01T11:00:00Z", "orders": 1, "revenue": 300.00}
  ]
}
```

The refund took the 10:00 order out of the paid rows and the new order
added 11:00. Straum applies a transaction whole, and the shown `as_of`
is after the commit.

## 8. Check status, then clean up

`straum views` shows what Straum holds of each view, with the names
and types an ask may use; [`straum ask`](running.md#straum-ask) prints
an answer as a table:

```text
$ docker exec straum /straum views
view orders
  buckets:  3600s
  holds:    4 cells and 4 bound cells, 2026-01-01T09:00:00Z to 2026-01-01T12:00:00Z
  keys:     bucket timestamptz, status text, customer_id integer, country text
  measures: orders, revenue, avg_amount

dim public.customers
  holds:    3 rows
```

`straum status` shows what Straum has done; [Running
Straum](running.md#straum-status) explains each line. `as of` is the
same time answers carry, and it keeps moving while the tables are
quiet:

```text
$ docker exec straum /straum status
ready:        yes
applied:      0/17B2958, committed 2026-11-09T17:12:57.346371Z
as of:        2026-11-09T17:14:02.118250Z, every commit before it applied
lag:          0.0 s, at most 30 s
WAL retained: 0.0 MiB, at most 4096.0 MiB; PostgreSQL is at 0/17B2958
starts:       1
failures:     0
last failure: none
sealed:       no spill
```

Stop straum with Ctrl-C and remove PostgreSQL and the network:

```sh
docker rm -f straum-pg
docker network rm straum
```
