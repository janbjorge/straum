-- The rollup of fact trips in taxi.sql: by columns, ref ids, then the 15
-- minute bucket. avg is kept as sum and count. Filtered measures store
-- nothing: their conditions name rollup-key parts only, so an ask reads them
-- from the rollup rows whose key matches.

create table trips_rollup as
select
    payment_type,
    vendor_id,
    pu_location_id,
    do_location_id,
    date_bin('15 minutes', pickup_at, timestamptz 'epoch') as bucket,
    count(*) as trips,
    sum(fare_amount) as fare,
    max(tip_amount) as tip_max,
    sum(trip_distance) as distance_sum,
    count(trip_distance) as distance_n
from yellow_trips
group by 1, 2, 3, 4, 5;

analyze trips_rollup;

-- Collapse: raw rows per rollup row, for the taxi.sql key and a few others.
select variant, n as rows, round((select count(*) from yellow_trips)::numeric / n, 1) as collapse
from (
    select 'raw trips', count(*) from yellow_trips
    union all
    select 'taxi.sql, 15m', count(*) from trips_rollup
    union all
    select 'taxi.sql, 1h', count(distinct (
        payment_type, vendor_id, pu_location_id, do_location_id,
        date_bin('1 hour', pickup_at, timestamptz 'epoch')
    )) from yellow_trips
    union all
    select 'taxi.sql, 1d', count(distinct (
        payment_type, vendor_id, pu_location_id, do_location_id,
        date_bin('1 day', pickup_at, timestamptz 'epoch')
    )) from yellow_trips
    union all
    select 'no dropoff ref, 15m', count(distinct (
        payment_type, vendor_id, pu_location_id,
        date_bin('15 minutes', pickup_at, timestamptz 'epoch')
    )) from yellow_trips
) v (variant, n)
order by n desc;
