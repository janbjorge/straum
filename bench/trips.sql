-- The view bench.py asks: NYC TLC yellow trips per hour, payment type and
-- pickup zone, the keys its asks group and filter by.
-- straum SQL: a PostgreSQL subset made only for straum specs.

CREATE VIEW trips WITH (buckets = '1 hour, 1 day, 7 days') AS
SELECT date_bin('1 hour', f.pickup_at, timestamptz 'epoch') AS hour,
       f.payment_type,
       f.pu_location_id,
       pickup.borough,
       count(*) AS trips,
       sum(f.fare_amount) AS fare,
       avg(f.fare_amount) AS avg_fare
FROM public.yellow_trips f
LEFT JOIN public.taxi_zones pickup ON pickup.location_id = f.pu_location_id
GROUP BY 1, 2, 3, 4;
