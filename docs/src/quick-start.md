# Quick start

Straum against PostgreSQL 18, both in Docker, on one month of New York
taxi trips: the TLC's yellow trips of January 2024, 2,964,624 of them,
and the 265 taxi zones. You need Docker, [uv](https://docs.astral.sh/uv/)
and `curl`, and the download is 50 MB. Four steps end in an answer.
Every other page uses these tables and this spec.

## 1. Start PostgreSQL with the trips

```sh
docker network create straum
docker run -d --name straum-pg --network straum -p 5432:5432 \
  -e POSTGRES_PASSWORD=postgres postgres:18 -c wal_level=logical
```

A few seconds later, load the trips:

```sh
uv run https://raw.githubusercontent.com/janbjorge/straum/main/bench/load_taxi_trips.py \
  postgres://postgres:postgres@localhost:5432/postgres --data data
```

[`load_taxi_trips.py`](https://github.com/janbjorge/straum/blob/main/bench/load_taxi_trips.py)
downloads the TLC's Parquet file and zone list into `data/`, makes
`taxi_zones` and `yellow_trips`, and streams the rows in with `COPY`.
The TLC's times are New York wall clock, so it loads them in that
time zone. Money and distance are `numeric`, so sums are exact.

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
[Writing a spec](spec.md) explains each line.

## 3. Set up PostgreSQL and run Straum

Straum connects as the user in `STRAUM_DSN`. `straum plan` reads it too,
and prints the SQL that makes that role with that password, lets it read
the tables and publishes them. Run the SQL as the tables' owner:

```sh
export STRAUM_DSN='postgres://straum:straum@straum-pg:5432/postgres?sslmode=disable'
docker run --rm -e STRAUM_DSN -v "$PWD:/spec" ghcr.io/janbjorge/straum \
  plan --spec /spec/trips.sql | docker exec -i straum-pg psql -U postgres
```

This container has no TLS, hence `sslmode=disable`. Otherwise Straum
always connects with TLS. Then run it:

```sh
docker run --rm --name straum --network straum -p 8080:8080 \
  -e STRAUM_DSN -v "$PWD:/spec" ghcr.io/janbjorge/straum \
  run --spec /spec/trips.sql
```

```text
straum: listening on 0.0.0.0:8080
straum: loaded every table; streaming from 0/1933EF18
```

The load takes about a second.

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
an ask is a parameter. [Asking](asks.md) explains the query, the
parameters and the answer.

## Change the data

Add a card trip from Newark Airport at noon, and move JFK and
LaGuardia, zones 132 and 138, out of Queens into a borough of their
own, then ask for the day again:

```sh
docker exec straum-pg psql -U postgres -c "
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

`docker exec -it straum /straum shell` is a prompt at the running
Straum, with Tab completion: `\d` lists the views with the names and
types an ask may use, and `\rows trips` prints a view's rows. [Running
Straum](running.md#straum-shell) has the shell and the `straum status`,
`straum views` and `straum ask` commands beside it.

Stop straum with Ctrl-C and remove PostgreSQL and the network:

```sh
docker rm -f straum-pg
docker network rm straum
```

## Your own tables

The same steps work on a database you have:

1. Take the `GROUP BY` your app runs most and write it as a view: the
   time column in `date_bin`, the columns it filters and groups by as
   keys, its aggregates as measures ([Writing a spec](spec.md)).
2. `straum plan --spec FILE` prints the setup SQL, or what is wrong
   with the spec. A table owner runs the SQL once. PostgreSQL 16 to
   18 also need `wal_level = logical`, which takes a restart ([Setting
   up PostgreSQL](postgresql.md)).
3. `straum run --spec FILE` with `STRAUM_DSN` set, then ask.

Start with few keys. Memory follows cells, one per distinct
combination of keys and bucket ([Cells](how-it-works.md#cells)).
