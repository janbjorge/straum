-- NYC TLC yellow trips, loaded with snake_case columns and a bigint id.
-- Straum SQL: a PostgreSQL subset made only for Straum specs.

CREATE VIEW trips WITH (buckets = '15 minutes, 1 day, 7 days') AS
SELECT date_bin('15 minutes', f.pickup_at, timestamptz 'epoch') AS bucket,
       f.payment_type AS payment_type,
       f.vendor_id AS vendor_id,
       f.pu_location_id AS pu_location_id,
       f.do_location_id AS do_location_id,
       pickup.borough AS borough,
       pickup.zone AS zone,
       pickup.service_zone AS service_zone,
       count(*) AS trips,
       sum(f.fare_amount) AS fare,
       max(f.tip_amount) AS tip_max,
       avg(f.trip_distance) AS distance,
       count(*) FILTER (WHERE f.payment_type = 1) AS card,
       count(*) FILTER (WHERE pickup.borough = 'Manhattan') AS manhattan_pickups
FROM public.yellow_trips AS f
LEFT OUTER JOIN public.taxi_zones AS pickup ON pickup.location_id = f.pu_location_id
GROUP BY 1, 2, 3, 4, 5, 6, 7, 8;
