-- The tables taxi.sql names: snake_case columns and a bigint key. Money and
-- distance are numeric so sums are exact on both sides of compare.sql.

create table taxi_zones (
    location_id int primary key,
    borough text,
    zone text,
    service_zone text
);

create table yellow_trips (
    id bigint generated always as identity primary key,
    vendor_id int,
    pickup_at timestamptz,
    pu_location_id int,
    do_location_id int,
    payment_type int,
    fare_amount numeric(12, 2),
    tip_amount numeric(12, 2),
    trip_distance numeric(12, 2)
);

-- TLC times are New York wall clock.
set timezone = 'America/New_York';

copy taxi_zones from '/data/taxi_zone_lookup.csv' with (format csv, header);
copy yellow_trips (
    vendor_id, pickup_at, pu_location_id, do_location_id, payment_type,
    fare_amount, tip_amount, trip_distance
) from :'trips' with (format csv, header);

analyze;
