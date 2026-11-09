-- Each ask over the raw join and from trips_rollup with zones mapped after
-- aggregation. Both sides name the zone rows pickup and dropoff and the fact
-- time t.at, so one key and filter text serves both. Ranges start and end on
-- 15 minute edges. A ref id missing from taxi_zones maps to null attributes
-- on both sides.
--
-- From the rollup, an ask reads the 15 minute buckets in its range. An ask
-- without a time key is also read as Straum reads it: from the widest
-- buckets that fit and narrower ones at the ends (`Rollup::cover`).

-- The buckets of `width_at` (900, 86400, 604800) and narrower that cover
-- `[lo, hi)` once: the run of whole widest buckets inside it, then the ends.
create function pg_temp.cover(lo timestamptz, hi timestamptz, width_at int default 2)
returns table (width int, run_lo timestamptz, run_hi timestamptz)
language plpgsql as $fn$
declare
    w int := (array[900, 86400, 604800])[width_at + 1];
    edge_lo timestamptz;
    edge_hi timestamptz;
begin
    if lo >= hi then
        return;
    end if;
    if width_at = 0 then
        return query select w, lo, hi;
        return;
    end if;
    edge_lo := date_bin(make_interval(secs => w), lo, timestamptz 'epoch');
    if edge_lo < lo then
        edge_lo := edge_lo + make_interval(secs => w);
    end if;
    edge_hi := date_bin(make_interval(secs => w), hi, timestamptz 'epoch');
    if edge_lo >= edge_hi then
        return query select * from pg_temp.cover(lo, hi, width_at - 1);
        return;
    end if;
    return query
        select * from pg_temp.cover(lo, edge_lo, width_at - 1)
        union all select w, edge_lo, edge_hi
        union all select * from pg_temp.cover(edge_hi, hi, width_at - 1);
end
$fn$;

create function pg_temp.ask(name text, keys text, cond text, lo timestamptz, hi timestamptz)
returns table (ask text, groups bigint, missing bigint, extra bigint,
               covered_missing bigint, covered_extra bigint)
language plpgsql as $fn$
declare
    head text := case when keys = '' then '' else keys || ', ' end;
    tail text := case when keys = '' then '' else 'group by ' || keys end;
    timed boolean := keys like '%t.at%';
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
        where %s and t.at >= %L and t.at < %L %s$q$, head, zones, cond, lo, hi, tail);
    measures text := '
            sum(t.trips),
            sum(t.fare),
            max(t.tip_max),
            sum(t.distance_sum) / nullif(sum(t.distance_n), 0),
            coalesce(sum(t.trips) filter (where t.payment_type = 1), 0),
            coalesce(sum(t.trips) filter (where pickup.borough = ''Manhattan''), 0)';
    rolled text := format($q$
        select %s %s
        from (select *, bucket as at from trips_rollup where width = 900) t %s
        where %s and t.at >= %L and t.at < %L %s$q$, head, measures, zones, cond, lo, hi, tail);
    covered text := format($q$
        select %s %s
        from (
            select r.*, r.bucket as at
            from trips_rollup r
            join pg_temp.cover(%L, %L) c
              on c.width = r.width and r.bucket >= c.run_lo and r.bucket < c.run_hi
        ) t %s
        where %s %s$q$, head, measures, lo, hi, zones, cond, tail);
begin
    if timed then
        -- A time key reads each 15 minute bucket, as Straum does.
        return query execute format($q$
            with raw as (%s), rolled as (%s)
            select %L,
                (select count(*) from raw),
                (select count(*) from (table raw except all table rolled) d),
                (select count(*) from (table rolled except all table raw) d),
                null::bigint, null::bigint$q$,
            raw, rolled, name);
    else
        return query execute format($q$
            with raw as (%s), rolled as (%s), covered as (%s)
            select %L,
                (select count(*) from raw),
                (select count(*) from (table raw except all table rolled) d),
                (select count(*) from (table rolled except all table raw) d),
                (select count(*) from (table raw except all table covered) d),
                (select count(*) from (table covered except all table raw) d)$q$,
            raw, rolled, covered, name);
    end if;
end
$fn$;

create temp table result as
select a.* from (values
    ('pickup borough by day',
        $$pickup.borough, date_trunc('day', t.at, 'UTC')$$,
        'true', '2000-01-01+00', '2030-01-01+00'),
    ('payment type by hour, odd week',
        $$t.payment_type, date_trunc('hour', t.at, 'UTC')$$,
        'true', '2024-01-08 06:15+00', '2024-01-15 18:45+00'),
    ('borough pair by month',
        $$pickup.borough, dropoff.borough, date_trunc('month', t.at, 'UTC')$$,
        'true', '2000-01-01+00', '2030-01-01+00'),
    ('dropoff zone and vendor by bucket, two pickup boroughs',
        $$dropoff.zone, t.vendor_id, date_bin('15 minutes', t.at, 'epoch')$$,
        $$pickup.borough in ('Manhattan', 'Queens')$$, '2000-01-01+00', '2030-01-01+00'),
    ('pickup ref id by week',
        $$t.pu_location_id, date_trunc('week', t.at, 'UTC')$$,
        'true', '2000-01-01+00', '2030-01-01+00'),
    ('card trips to airports, total',
        '',
        $$t.payment_type = 1 and dropoff.service_zone = 'Airports'$$,
        '2000-01-01+00', '2030-01-01+00'),
    ('service zone pair, vendor 2, first half',
        'pickup.service_zone, dropoff.service_zone',
        't.vendor_id = 2', '2024-01-01 00:00+00', '2024-01-16 00:00+00'),
    ('pickup borough, odd range across weeks and days',
        'pickup.borough, t.payment_type',
        'true', '2024-01-03 05:15+00', '2024-01-27 13:30+00')
) s (name, keys, cond, lo, hi), pg_temp.ask(s.name, s.keys, s.cond, s.lo::timestamptz, s.hi::timestamptz) a;

table result;

-- The pieces the odd range reads: weeks start on a Thursday, at the epoch.
select width, run_lo, run_hi from pg_temp.cover('2024-01-03 05:15+00', '2024-01-27 13:30+00');

do $$
begin
    if exists (select from result
               where groups = 0 or missing > 0 or extra > 0
                  or covered_missing > 0 or covered_extra > 0) then
        raise exception 'rollup-then-map differs from the raw join';
    end if;
end
$$;
