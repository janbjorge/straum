-- The rollup of fact trips in taxi.sql: by columns, ref ids, then the
-- bucket, at the 15 minute `date_bin` width and at each wider width its
-- `buckets` option declares, 1 and 7 days. Each width is grouped from the
-- trips, as Straum adds each row to one cell per width. avg is kept as sum
-- and count. Filtered measures store nothing: their conditions name
-- rollup-key parts only, so an ask reads them from the rollup rows whose
-- key matches.

create table trips_rollup as
select
    w.width,
    payment_type,
    vendor_id,
    pu_location_id,
    do_location_id,
    date_bin(make_interval(secs => w.width), pickup_at, timestamptz 'epoch') as bucket,
    count(*) as trips,
    sum(fare_amount) as fare,
    max(tip_amount) as tip_max,
    sum(trip_distance) as distance_sum,
    count(trip_distance) as distance_n
from yellow_trips, (values (900), (86400), (604800)) w (width)
group by 1, 2, 3, 4, 5, 6;

analyze trips_rollup;

-- Collapse: raw rows per rollup row, for the taxi.sql key and a few others.
select variant, n as rows, round((select count(*) from yellow_trips)::numeric / n, 1) as collapse
from (
    select 'raw trips', count(*) from yellow_trips
    union all
    select 'taxi.sql, ' || case width when 900 then '15m' when 86400 then '1d' else '7d' end,
        count(*)
    from trips_rollup group by width
    union all
    select 'taxi.sql, 1h', count(distinct (
        payment_type, vendor_id, pu_location_id, do_location_id,
        date_bin('1 hour', pickup_at, timestamptz 'epoch')
    )) from yellow_trips
    union all
    select 'no dropoff ref, 15m', count(distinct (
        payment_type, vendor_id, pu_location_id,
        date_bin('15 minutes', pickup_at, timestamptz 'epoch')
    )) from yellow_trips
) v (variant, n)
order by n desc;
