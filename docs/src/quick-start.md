# Quick start

Straum against PostgreSQL 18, both in Docker, on one month of New York
taxi trips: the TLC's yellow trips of January 2024, 2,964,624 of them,
and the 265 taxi zones. You need Docker and `curl`. Every other page
uses these tables and this spec.

## 1. Pull the image

```sh
docker pull ghcr.io/janbjorge/straum
```

`latest` is the newest stable release.

## 2. Get the month

The TLC publishes the trips as Parquet, which PostgreSQL can't read, so
a Python container writes the columns the tables use to CSV. Each
step is skipped when its file is already in `data/`, so a second run
downloads nothing:

```sh
mkdir -p data
TLC=https://d37ci6vzurychx.cloudfront.net
test -f data/yellow_tripdata_2024-01.parquet ||
  curl -fL -o data/yellow_tripdata_2024-01.parquet $TLC/trip-data/yellow_tripdata_2024-01.parquet
test -f data/taxi_zone_lookup.csv ||
  curl -fL -o data/taxi_zone_lookup.csv $TLC/misc/taxi_zone_lookup.csv
test -f data/trips_2024-01.csv ||
  docker run --rm -i -v "$PWD/data:/data" python:3.13-slim sh -c '
    pip install --quiet --disable-pip-version-check --root-user-action=ignore pyarrow &&
    python -' <<'PY'
import pyarrow.csv, pyarrow.parquet
columns = ["VendorID", "tpep_pickup_datetime", "PULocationID", "DOLocationID",
           "payment_type", "fare_amount", "tip_amount", "trip_distance"]
table = pyarrow.parquet.read_table("/data/yellow_tripdata_2024-01.parquet", columns=columns)
pyarrow.csv.write_csv(table.select(columns), "/data/trips_2024-01.csv")
PY
```

The download is 50 MB. `bench/taxi.py` keeps the same files in
`bench/data/`.

## 3. Start PostgreSQL and load the month

```sh
docker network create straum
docker run -d --name straum-pg --network straum -v "$PWD/data:/data" \
  -e POSTGRES_PASSWORD=postgres postgres:18 -c wal_level=logical
```

A few seconds later, make the tables and load them:

```sh
docker exec -i straum-pg psql -U postgres <<'SQL'
SET timezone = 'America/New_York';
CREATE TABLE taxi_zones (
  location_id int PRIMARY KEY,
  borough text,
  zone text,
  service_zone text
);
CREATE TABLE yellow_trips (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  vendor_id int,
  pickup_at timestamptz,
  pu_location_id int,
  do_location_id int,
  payment_type int,
  fare_amount numeric(12, 2),
  tip_amount numeric(12, 2),
  trip_distance numeric(12, 2)
);
COPY taxi_zones FROM '/data/taxi_zone_lookup.csv' WITH (format csv, header);
COPY yellow_trips (vendor_id, pickup_at, pu_location_id, do_location_id,
                   payment_type, fare_amount, tip_amount, trip_distance)
  FROM '/data/trips_2024-01.csv' WITH (format csv, header);
SQL
```

The TLC's times are New York wall clock, hence the time zone. Money
and distance are `numeric`, so sums are exact.

## 4. Write a spec

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

## 5. Set up PostgreSQL

Straum connects as the user in `STRAUM_DSN`. `straum plan` reads it too,
and prints the SQL that makes that role with that password, lets it read
the tables and publishes them. Run the SQL as the tables' owner:

```sh
export STRAUM_DSN='postgres://straum:straum@straum-pg:5432/postgres?sslmode=disable'
docker run --rm -e STRAUM_DSN -v "$PWD:/spec" ghcr.io/janbjorge/straum \
  plan --spec /spec/trips.sql | docker exec -i straum-pg psql -U postgres
```

This container has no TLS, hence `sslmode=disable`. Otherwise Straum
always connects with TLS.

## 6. Run straum

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

## 7. Ask

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

Every commit made before `as_of` is in the answer, and every ask says
which buckets it reads. [Asking](asks.md) explains the query, the
parameters and the answer.

## 8. Change the data

Straum follows commits asynchronously, so the first answer after a write
can predate it. Add a card trip from Newark Airport at noon, and move
JFK and LaGuardia, zones 132 and 138, out of Queens into a borough of
their own, then ask for the day again. If its `as_of` predates the
write, ask once more. Every value in an ask is a parameter:

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

The new trip added one to EWR. The two airports' trips of the day left
Queens for Airports, as PostgreSQL's view would show them: Straum
reads the borough through each trip's zone as the zones are now, so
the update moved no cell ([How Straum works](how-it-works.md#dims)).
Straum applies a transaction whole, and the shown `as_of` is after the
commit.

## 9. Check status, then clean up

`straum views` shows what Straum holds of each view, with the names
and types an ask may use; [`straum ask`](running.md#straum-ask) prints
an answer as a table, and [`straum shell`](running.md#straum-shell) is
a prompt for both, with Tab completion:

```text
$ docker exec straum /straum views
view trips
  buckets:  3600s, 86400s, 604800s
  holds:    173660 cells and 173660 bound cells, 2003-01-01T03:00:00Z to 2024-02-01T06:00:00Z
  keys:     bucket timestamptz, payment_type integer, pu_location_id integer, borough text
  measures: trips, fare, avg_fare

dim public.taxi_zones
  holds:    265 rows
```

A few trips in the month carry a wrong year, hence the range from
2003. `straum status` shows what Straum has done; [Running
Straum](running.md#straum-status) explains each line. `as of` is the
same time answers carry, and it keeps moving while the tables are
quiet:

```text
$ docker exec straum /straum status
ready:        yes
applied:      0/19440240, committed 2026-11-13T07:53:14.559406Z
as of:        2026-11-13T07:55:38.289348625Z, every commit before it applied
lag:          0.0 s, at most 30 s
WAL retained: 0.0 MiB, at most 4096.0 MiB; PostgreSQL is at 0/19599048
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

`data/` stays, so the next run starts at step 3.
