# Quick start

Does Straum work? One command runs it and PostgreSQL 18 in Docker on
one month of New York taxi trips: the Taxi and Limousine Commission
(TLC) yellow trips for January 2024, 2,964,624 of them, and the 265
taxi zones. It ends in Straum's shell, where you ask, change the data
and ask again. The command line and performance pages use these tables
too.

You need Docker, `curl`, and bash or zsh. The download is 50 MB.

## 1. Start

```bash
docker compose -f <(curl -fsSL https://raw.githubusercontent.com/janbjorge/straum/main/docs/compose/quick-start.yaml) \
  run --rm shell
```

The file, `docs/compose/quick-start.yaml` in the repository, names
the project `straum` and starts PostgreSQL. A load service fetches the
month, loads it, runs the setup and exits; Compose shows it as Waiting
while it fetches. Straum starts once it has exited, and the shell
opens once Straum can answer:

```text
Straum at http://straum:8080. Tab completes; \? lists the commands, \q leaves.
straum>
```

An interrupt during the wait leaves the containers running. The log,
`docker compose -p straum logs`, ends like this:

```text
load-1  | loading taxi trips
load-1  | setup applied
straum-1  | straum: listening on 0.0.0.0:8080
straum-1  | straum: loaded every table; streaming from 0/19337960
```

[`load_taxi_trips.py`](https://github.com/janbjorge/straum/blob/main/bench/load_taxi_trips.py)
downloads the trips and loads `taxi_zones` and `yellow_trips`. It
reads the TLC's times, New York wall clock, in that time zone, and
makes money and distance `numeric`, so sums are exact. Straum's own
load takes about a second.

`\q` leaves the shell; Straum keeps running. This reopens the shell
inside the Straum container, at `http://127.0.0.1:8080`:

```sh
docker compose -p straum exec straum /straum shell
```

## 2. The spec

The compose file holds this spec, one view, so there is nothing to
save; Straum never creates the view in PostgreSQL. `buckets(...)` is
the view's time: the column, then each bucket width Straum keeps, the
hour each trip goes in first, then days and weeks for long ranges.
Straum keeps one cell per hour, payment type and pickup zone, the same
per day and per week, and reads each cell's borough from `taxi_zones`
through its pickup zone. [Spec](spec.md) explains each line.

```sql
CREATE VIEW trips AS
SELECT buckets(trips.pickup_at, '1 hour', '1 day', '7 days') AS bucket,
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

## 3. Ask

At the prompt, ask for trips and fares by pickup borough on Wednesday
17 January, New York time. The semicolon ends the ask; the parameters
follow it:

```text
straum> SELECT borough, trips, fare, avg_fare
FROM trips
WHERE bucket >= $1 AND bucket < $2
GROUP BY borough; 2024-01-17T00:00:00-05:00 2024-01-18T00:00:00-05:00
```

The same ask over HTTP, from another terminal:

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

## 4. Change the data

In another terminal, add a card trip from Newark Airport at noon,
move JFK and LaGuardia, zones 132 and 138, out of Queens into a
borough of their own, and ask for the day again:

```sh
docker compose -p straum exec postgres psql -U postgres -c "
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

The new trip added one to EWR, and the airports' trips of the day
left Queens for Airports, as PostgreSQL's view would show them
([Dims](how-it-works.md#dims)). Straum follows commits asynchronously:
if `as_of` predates the write, ask again.

## 5. Look around, then clean up

At the `straum>` prompt, `\d` lists the views with the names and types
an ask may use, `\d trips` prints the view's `CREATE VIEW`, and
`\rows trips` prints the rows PostgreSQL's view would give.
[Command line](cli.md#straum-shell) has the shell and the
`straum status`, `straum views` and `straum ask` commands beside it.

Leave the shell with `\q`, then remove everything:

```sh
docker compose -p straum down --volumes
```

Next: [Try it on your data](your-data.md), on a copy of your tables.
