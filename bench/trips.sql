-- The view bench.py asks: NYC TLC yellow trips per hour, payment type and
-- pickup zone, the keys its asks group and filter by.
-- Straum SQL: a PostgreSQL subset made only for Straum specs.

CREATE VIEW trips WITH (buckets = '1 hour, 1 day, 7 days') AS
SELECT date_bin('1 hour', f.pickup_at, timestamptz 'epoch') AS bucket,
       f.payment_type AS payment_type,
       f.pu_location_id AS pu_location_id,
       pickup.borough AS borough,
       count(*) AS trips,
       sum(f.fare_amount) AS fare,
       avg(f.fare_amount) AS avg_fare
FROM public.yellow_trips AS f
LEFT OUTER JOIN public.taxi_zones AS pickup ON pickup.location_id = f.pu_location_id
GROUP BY 1, 2, 3, 4;
