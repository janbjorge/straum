-- The view bench/demo.py asks: NYC TLC yellow trips of January, per
-- hour, payment type and pickup zone. Counts, sums and averages.
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
