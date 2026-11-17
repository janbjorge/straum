# Quick start

Straum against PostgreSQL 18, both in Docker, on one month of New York
taxi trips: the TLC's yellow trips of January 2024, 2,964,624 of them,
and the 265 taxi zones. You need Docker, [uv](https://docs.astral.sh/uv/)
and `curl`, and the download is 50 MB. Four steps end in an answer.
The command line and performance pages use these tables too.

## 1. Start PostgreSQL with the trips

Save as `compose.yaml`:

```yaml
name: straum
services:
  postgres:
    image: postgres:18
    command: postgres -c wal_level=logical
    environment:
      POSTGRES_PASSWORD: postgres
    healthcheck:
      test: pg_isready -h 127.0.0.1 -U postgres
      interval: 1s
  straum:
    image: ghcr.io/janbjorge/straum
    command: run /spec/trips.sql --listen 0.0.0.0:8080
    environment:
      STRAUM_DSN: postgres://straum:straum@postgres:5432/postgres?sslmode=disable
    volumes:
      - ./trips.sql:/spec/trips.sql:ro
    ports:
      - 127.0.0.1:8080:8080
```

Start PostgreSQL, wait until it answers, and load the trips:

```sh
docker compose up --detach --wait postgres
uv run https://raw.githubusercontent.com/janbjorge/straum/main/bench/load_taxi_trips.py |
  docker compose exec -T postgres psql -U postgres -q
```

[`load_taxi_trips.py`](https://github.com/janbjorge/straum/blob/main/bench/load_taxi_trips.py)
downloads the trips into `data/` and prints them as SQL: the tables
`taxi_zones` and `yellow_trips`, then their rows. The TLC's times are
New York wall clock, so it reads them in that time zone. Money and
distance are `numeric`, so sums are exact.

## 2. Write a spec

Save as `trips.sql`:

```sql
CREATE VIEW trips WITH (buckets = '1 hour, 1 day, 7 days') AS
SELECT date_bin('1 hour', trips.pickup_at, timestamptz 'epoch') AS bucket,
       trips.payment_type AS payment_type,
       trips.pu_location_id AS pu_location_id,
       pickup.borough AS borough,
       count(*) AS trips,
       sum(trips.fare_amount) AS fare,
       avg(trips.fare_amount) AS avg_fare
FROM public.yellow_trips AS trips
LEFT OUTER JOIN public.taxi_zones AS pickup ON pickup.location_id = trips.pu_location_id
GROUP BY 1, 2, 3, 4;
```

PostgreSQL would give the same rows for this view. Straum never creates
it; it keeps one cell per hour, payment type, pickup zone and borough
in memory. `WITH (buckets = ...)` lists every bucket width it keeps:
the hour of the `date_bin`, then days and weeks for long ranges.
[Spec](spec.md) explains each line.

## 3. Set up PostgreSQL and run Straum

Straum connects as the user in `STRAUM_DSN`, set in `compose.yaml`.
`straum plan` reads it too, and prints the SQL that makes that role
with that password, lets it read the tables and publishes them. Run
the SQL as the tables' owner:

```sh
docker compose run --rm --no-deps -T straum plan /spec/trips.sql |
  docker compose exec -T postgres psql -U postgres
```

This PostgreSQL has no TLS, hence `sslmode=disable`. Otherwise Straum
always connects with TLS. Then run it:

```sh
docker compose up straum
```

```text
straum-1  | straum: listening on 0.0.0.0:8080
straum-1  | straum: opened a stream at 0/1933EF18; loading 3 tables
straum-1  | straum: loaded dim taxi_zones in 0.0 s
straum-1  | straum: loaded view trips in 0.6 s
straum-1  | straum: loaded view dropoffs in 0.4 s
straum-1  | straum: loaded every table; streaming from 0/1933EF18
```

The load takes about a second; each table logs when it is done, with
how long it took.

## 4. Ask

In another terminal, ask for trips and fares by pickup borough on
Wednesday 17 January, New York time:

```sh
curl --json '{
  "query": "SELECT borough, trips, fare, avg_fare FROM trips WHERE bucket >= $1 AND bucket < $2 GROUP BY borough",
  "parameters": ["2024-01-17T00:00:00-05:00", "2024-01-18T00:00:00-05:00"]
}' http://localhost:8080/ask
```

```json
{
  "as_of": "2026-11-13T07:52:38.699695916Z",
  "rows": [
    {"borough": "Bronx", "trips": 220, "fare": 7196.07, "avg_fare": 32.709409},
    {"borough": "Brooklyn", "trips": 924, "fare": 27585.30, "avg_fare": 29.854221},
    {"borough": "EWR", "trips": 10, "fare": 1001.50, "avg_fare": 100.15},
    {"borough": "Manhattan", "trips": 99225, "fare": 1502063.48, "avg_fare": 15.137954},
    {"borough": "N/A", "trips": 68, "fare": 6436.59, "avg_fare": 94.655735},
    {"borough": "Queens", "trips": 9388, "fare": 454949.34, "avg_fare": 48.460731},
    {"borough": "Staten Island", "trips": 3, "fare": 107.40, "avg_fare": 35.8},
    {"borough": "Unknown", "trips": 527, "fare": 9738.86, "avg_fare": 18.47981}
  ]
}
```

Every commit made before `as_of` is in the answer, and every value in
an ask is a parameter. [Asks](asks.md) explains the query, the
parameters and the answer.

## Change the data

Add a card trip from Newark Airport at noon, and move JFK and
LaGuardia, zones 132 and 138, out of Queens into a borough of their
own, then ask for the day again:

```sh
docker compose exec postgres psql -U postgres -c "
  INSERT INTO yellow_trips (vendor_id, pickup_at, pu_location_id, do_location_id,
                            payment_type, fare_amount, tip_amount, trip_distance)
  VALUES (2, '2024-01-17 12:00-05', 1, 132, 1, 120.00, 0, 17.50);
  UPDATE taxi_zones SET borough = 'Airports' WHERE location_id IN (132, 138);"
curl --json '{
  "query": "SELECT borough, trips, fare FROM trips WHERE bucket >= $1 AND bucket < $2 GROUP BY borough",
  "parameters": ["2024-01-17T00:00:00-05:00", "2024-01-18T00:00:00-05:00"]
}' http://localhost:8080/ask
```

```json
{
  "as_of": "2026-11-13T07:53:13.818919375Z",
  "rows": [
    {"borough": "Airports", "trips": 7975, "fare": 409124.98},
    {"borough": "Bronx", "trips": 220, "fare": 7196.07},
    {"borough": "Brooklyn", "trips": 924, "fare": 27585.30},
    {"borough": "EWR", "trips": 11, "fare": 1121.50},
    {"borough": "Manhattan", "trips": 99225, "fare": 1502063.48},
    {"borough": "N/A", "trips": 68, "fare": 6436.59},
    {"borough": "Queens", "trips": 1413, "fare": 45824.36},
    {"borough": "Staten Island", "trips": 3, "fare": 107.40},
    {"borough": "Unknown", "trips": 527, "fare": 9738.86}
  ]
}
```

The new trip added one to EWR, and the two airports' trips of the day
left Queens for Airports, as PostgreSQL's view would show them
([Dims](how-it-works.md#dims)). Straum follows commits asynchronously,
so if the answer's `as_of` predates the write, ask once more.

## Look around, then clean up

`docker compose exec straum /straum shell` is a prompt at the running
Straum, with Tab completion: `\d` lists the views with the names and
types an ask may use, `\d trips` prints the view's `CREATE VIEW`, and
`\rows trips` prints its rows. [Command
line](cli.md#straum-shell) has the shell and the `straum status`,
`straum views` and `straum ask` commands beside it.

Stop straum with Ctrl-C and remove both containers, their data and
the network:

```sh
docker compose down --volumes
```

Next: [Try it on your data](your-data.md), the same steps on a copy
of your own tables.
