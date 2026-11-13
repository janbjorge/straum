-- The views bench/taxi.py asks: NYC TLC yellow trips per hour, payment
-- type and pickup zone, and per hour and dropoff zone, the keys its asks
-- group and filter by.
-- Straum SQL: a PostgreSQL subset made only for Straum specs.

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

CREATE VIEW dropoffs WITH (buckets = '1 hour, 1 day, 7 days') AS
SELECT date_bin('1 hour', dropoffs.pickup_at, timestamptz 'epoch') AS bucket,
       dropoffs.do_location_id AS do_location_id,
       dropoff.borough AS borough,
       count(*) AS trips,
       sum(dropoffs.tip_amount) AS tips,
       avg(dropoffs.trip_distance) AS avg_distance,
       sum(dropoffs.fare_amount) AS fare
FROM public.yellow_trips AS dropoffs
LEFT OUTER JOIN public.taxi_zones AS dropoff ON dropoff.location_id = dropoffs.do_location_id
GROUP BY 1, 2, 3;
