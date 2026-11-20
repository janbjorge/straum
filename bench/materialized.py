"""The materialized views bench/taxi.py's materialized mode measures, and
the asks written over them.

Each `MaterializedView` holds one view of bench/trips.sql as PostgreSQL
would keep it: the same keys, at the view's narrowest bucket, one hour
on the epoch grid. `create_statements` writes its `CREATE MATERIALIZED
VIEW` and the unique index `REFRESH MATERIALIZED VIEW CONCURRENTLY`
needs. It keeps only counts and sums, which add across buckets and
groups, so an avg, a variance and a standard deviation come from them in
the ask. A median, percentile, mode or distinct count does not merge
from per-bucket rows; `MATERIALIZED_ASKS` marks those asks None, and
their row says they need the raw rows. Imports only bench/answers.py, so
bench/test-stack.py runs it without the bench's packages.
"""

import dataclasses

from answers import duration

NEEDS_RAW_ROWS = "needs the raw rows"
NARROWEST_BUCKET = "1 hour"
# Measures a materialized view may keep: each adds across buckets and groups.
ADDITIVE = ("count(", "sum(")
# Neither merges from per-bucket rows: a distinct count must see every
# value, an ordered-set aggregate every row.
NOT_ADDITIVE = ("DISTINCT", "WITHIN GROUP")


@dataclasses.dataclass(frozen=True)
class Column:
    """One output column of a materialized view: an expression and its name."""

    expression: str
    name: str


@dataclasses.dataclass(frozen=True)
class MaterializedView:
    """A materialized view keeping one Straum view's keys at its narrowest
    bucket."""

    name: str
    straum_view: str
    source: str
    time: str
    keys: list[Column]
    measures: list[Column]


MATERIALIZED_VIEWS = [
    MaterializedView(
        name="trips_hourly",
        straum_view="trips",
        source="""yellow_trips AS trips
  LEFT JOIN taxi_zones AS pickup ON pickup.location_id = trips.pu_location_id""",
        time="trips.pickup_at",
        keys=[
            Column("trips.payment_type", "payment_type"),
            Column("trips.pu_location_id", "pu_location_id"),
            Column("pickup.borough", "borough"),
        ],
        measures=[
            Column("count(*)", "trips"),
            Column("count(trips.fare_amount)", "fares"),
            Column("sum(trips.fare_amount)", "fare"),
            Column("sum(trips.fare_amount * trips.fare_amount)", "fare_squares"),
        ],
    ),
    MaterializedView(
        name="dropoffs_hourly",
        straum_view="dropoffs",
        source="""yellow_trips AS dropoffs
  LEFT JOIN taxi_zones AS dropoff ON dropoff.location_id = dropoffs.do_location_id""",
        time="dropoffs.pickup_at",
        keys=[
            Column("dropoffs.do_location_id", "do_location_id"),
            Column("dropoff.borough", "borough"),
        ],
        measures=[
            Column("count(*)", "trips"),
            Column("sum(dropoffs.tip_amount)", "tips"),
            Column("count(dropoffs.trip_distance)", "distances"),
            Column("sum(dropoffs.trip_distance)", "distance"),
            Column("sum(dropoffs.fare_amount)", "fare"),
        ],
    ),
]


def create_statements(view: MaterializedView) -> list[str]:
    """The statements that create `view`: the view, then the unique
    index on its keys that a concurrent refresh needs."""
    for measure in view.measures:
        additive = measure.expression.startswith(ADDITIVE)
        if not additive or any(
            word in measure.expression.upper() for word in NOT_ADDITIVE
        ):
            raise ValueError(
                f"{view.name}: {measure.expression} does not add across buckets"
            )
    bucket = Column(
        f"date_bin('{NARROWEST_BUCKET}', {view.time}, timestamptz 'epoch')", "bucket"
    )
    keys = [bucket, *view.keys]
    columns = ",\n       ".join(
        f"{column.expression} AS {column.name}" for column in keys + view.measures
    )
    positions = ", ".join(str(position) for position in range(1, len(keys) + 1))
    names = ", ".join(column.name for column in keys)
    # A trip of a zone missing from taxi_zones has a null borough, so the
    # key holds nulls, and only NULLS NOT DISTINCT makes it a key.
    return [
        (
            f"CREATE MATERIALIZED VIEW {view.name} AS\n"
            f"SELECT {columns}\n"
            f"  FROM {view.source}\n"
            f" GROUP BY {positions}"
        ),
        f"CREATE UNIQUE INDEX ON {view.name} ({names}) NULLS NOT DISTINCT",
    ]


ALL_TIME = "bucket >= '2000-01-01T00:00:00Z' AND bucket < '2030-01-01T00:00:00Z'"
ONE_DAY = (
    "bucket >= '2024-01-17T00:00:00-05:00' AND bucket < '2024-01-18T00:00:00-05:00'"
)
ONE_MONTH = "bucket >= '2024-01-01T00:00:00Z' AND bucket < '2024-02-01T00:00:00Z'"

# Each of bench/taxi.py's asks by name, as SQL over the materialized
# views with the ask's own range, or None where none answers it.
# Counts are summed back to bigint, as Straum answers them; an avg is a
# sum over a count of the values that are not null.
MATERIALIZED_ASKS: dict[str, str | None] = {
    "Trips": f"SELECT sum(trips)::bigint FROM trips_hourly WHERE {ALL_TIME}",
    "Card trips": f"""SELECT sum(trips)::bigint FROM trips_hourly
            WHERE payment_type = 1 AND {ALL_TIME}""",
    "Trips by pickup borough": f"""SELECT borough, sum(trips)::bigint FROM trips_hourly
            WHERE {ALL_TIME}
            GROUP BY borough""",
    "Fares by payment type": f"""SELECT payment_type, sum(trips)::bigint, sum(fare),
                  sum(fare) / NULLIF(sum(fares), 0)
             FROM trips_hourly
            WHERE {ALL_TIME}
            GROUP BY payment_type""",
    # The sample variance from the count, sum and sum of squares.
    "Fare spread by payment type": f"""SELECT payment_type, variance, sqrt(variance)
             FROM (SELECT payment_type,
                          (sum(fares) * sum(fare_squares) - sum(fare) * sum(fare))
                          / NULLIF(sum(fares) * (sum(fares) - 1), 0) AS variance
                     FROM trips_hourly
                    WHERE {ALL_TIME}
                    GROUP BY payment_type) AS spread""",
    "Median fare": None,
    "p99 fare by payment type": None,
    "Most common fare by payment type": None,
    "Distinct fares by payment type": None,
    "Median fare per hour, one day": None,
    "Manhattan pickups, one week": """SELECT sum(trips)::bigint FROM trips_hourly
            WHERE borough = 'Manhattan'
              AND bucket >= '2024-01-08T00:00:00-05:00'
              AND bucket < '2024-01-15T00:00:00-05:00'""",
    "Trips per hour, one day": f"""SELECT bucket, sum(trips)::bigint FROM trips_hourly
            WHERE {ONE_DAY}
            GROUP BY bucket""",
    "Trips per hour by borough, one month": f"""SELECT bucket, borough, sum(trips)::bigint
             FROM trips_hourly
            WHERE {ONE_MONTH}
            GROUP BY bucket, borough""",
    "Trips per day by borough, one month": f"""SELECT date_bin('1 day', bucket, timestamptz 'epoch'), borough,
                  sum(trips)::bigint
             FROM trips_hourly
            WHERE {ONE_MONTH}
            GROUP BY 1, 2""",
    # Straum's open range reads every bucket it holds; a trip with no
    # pickup time has none.
    "Fares per week": """SELECT date_bin('7 days', bucket, timestamptz 'epoch'),
                  sum(trips)::bigint, sum(fare)
             FROM trips_hourly
            WHERE bucket IS NOT NULL
            GROUP BY 1""",
    "Tips by dropoff borough": f"""SELECT borough, sum(trips)::bigint, sum(tips),
                  sum(distance) / NULLIF(sum(distances), 0), sum(fare)
             FROM dropoffs_hourly
            WHERE {ALL_TIME}
            GROUP BY borough""",
}


def answer_cell(seconds: float | None) -> str:
    """The table's cell for a materialized view's answer time, or what
    the ask needs where none answers it."""
    if seconds is None:
        return NEEDS_RAW_ROWS
    return duration(seconds)
