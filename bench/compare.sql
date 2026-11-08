-- Each ask twice: once over the raw join, once from trips_rollup with zones
-- mapped after aggregation. Both sides name the zone rows pickup and dropoff
-- and the fact time t.at, so one key and filter text serves both. Ranges start
-- and end on bucket edges. A ref id missing from taxi_zones maps to null
-- attributes on both sides.

create function pg_temp.ask(name text, keys text, cond text)
returns table (ask text, groups bigint, missing bigint, extra bigint)
language plpgsql as $fn$
declare
    head text := case when keys = '' then '' else keys || ', ' end;
    tail text := case when keys = '' then '' else 'group by ' || keys end;
    zones text := '
        left join taxi_zones pickup on pickup.location_id = t.pu_location_id
        left join taxi_zones dropoff on dropoff.location_id = t.do_location_id';
    raw text := format($q$
        select %s
            count(*),
            sum(t.fare_amount),
            max(t.tip_amount),
            avg(t.trip_distance),
            count(*) filter (where t.payment_type = 1),
            count(*) filter (where pickup.borough = 'Manhattan')
        from (select *, pickup_at as at from yellow_trips) t %s
        where %s %s$q$, head, zones, cond, tail);
    rolled text := format($q$
        select %s
            sum(t.trips),
            sum(t.fare),
            max(t.tip_max),
            sum(t.distance_sum) / nullif(sum(t.distance_n), 0),
            coalesce(sum(t.trips) filter (where t.payment_type = 1), 0),
            coalesce(sum(t.trips) filter (where pickup.borough = 'Manhattan'), 0)
        from (select *, bucket as at from trips_rollup) t %s
        where %s %s$q$, head, zones, cond, tail);
begin
    return query execute format($q$
        with raw as (%s), rolled as (%s)
        select %L,
            (select count(*) from raw),
            (select count(*) from (table raw except all table rolled) d),
            (select count(*) from (table rolled except all table raw) d)$q$,
        raw, rolled, name);
end
$fn$;

create temp table result as
select a.* from (values
    ('pickup borough by day',
        $$pickup.borough, date_trunc('day', t.at, 'UTC')$$,
        'true'),
    ('payment type by hour, odd week',
        $$t.payment_type, date_trunc('hour', t.at, 'UTC')$$,
        $$t.at >= '2024-01-08 06:15+00' and t.at < '2024-01-15 18:45+00'$$),
    ('borough pair by month',
        $$pickup.borough, dropoff.borough, date_trunc('month', t.at, 'UTC')$$,
        'true'),
    ('dropoff zone and vendor by bucket, two pickup boroughs',
        $$dropoff.zone, t.vendor_id, date_bin('15 minutes', t.at, 'epoch')$$,
        $$pickup.borough in ('Manhattan', 'Queens')$$),
    ('pickup ref id by week',
        $$t.pu_location_id, date_trunc('week', t.at, 'UTC')$$,
        'true'),
    ('card trips to airports, total',
        '',
        $$t.payment_type = 1 and dropoff.service_zone = 'Airports'$$),
    ('service zone pair, vendor 2, first half',
        'pickup.service_zone, dropoff.service_zone',
        $$t.vendor_id = 2 and t.at >= '2024-01-01 00:00+00' and t.at < '2024-01-16 00:00+00'$$)
) s (name, keys, cond), pg_temp.ask(s.name, s.keys, s.cond) a;

table result;

do $$
begin
    if exists (select from result where groups = 0 or missing > 0 or extra > 0) then
        raise exception 'rollup-then-map differs from the raw join';
    end if;
end
$$;
