"""The runs bench/taxi.py's retention mode makes: bench/trips.sql as is,
with its hours kept for a time, and with its hours and days kept for a
time, each with the asks it must still answer and those it must refuse.

The trips are from 2024 and PostgreSQL's clock is today's, and a width
is kept for a duration back from PostgreSQL's time, so a duration of
14 days would drop every hour. `runs` takes the time of the newest trip
before July 2024, the end of the months loaded, and
PostgreSQL's `now()`, in seconds since the epoch, and offsets each
duration by the gap between them: the hours are kept from 14 days
before the day of the newest trip, the days from 91 days before its
7-day bucket. Each duration is then rounded up to a whole multiple of
the next wider width, as a spec must write it. The load reads `now()`
again, less than a day later, so each horizon falls on that edge or
one wider bucket before it, never after. The asks bound their time on
those edges, which are edges of every narrower width too.

`spec_with` writes a variant of the spec by replacing its widths,
`cells` reads the cells at the narrowest width from `GET /views`,
`refused` asks what a run must refuse, and `table` lays out what each
run measured. Imports nothing outside the standard library and
bench/answers.py, so bench/test-stack.py runs it without the bench's
packages.
"""

import dataclasses
import datetime
import json

from answers import duration

WIDTHS = "'1 hour', '1 day', '7 days'"
DAY_SECONDS = 86_400
WEEK_SECONDS = 7 * DAY_SECONDS
HOURS_KEPT_DAYS = 14
DAYS_KEPT_DAYS = 91
# The newest trip of the months loaded and the server's time, both in
# whole seconds. A few trips carry a wrong year, one of them in 2026,
# which would put every horizon past the months.
EDGES_SQL = """SELECT floor(extract(epoch FROM max(pickup_at)))::bigint,
                      ceil(extract(epoch FROM now()))::bigint
                 FROM yellow_trips
                WHERE pickup_at < '2024-07-01T00:00:00Z'"""
REFUSAL = "kept only from"


@dataclasses.dataclass(frozen=True)
class Run:
    """One `straum run` of the mode: its spec, the asks it must answer
    alike with PostgreSQL, and the asks it must refuse."""

    name: str
    kept: str
    spec: str
    asks: list[tuple[str, str, str, list]]
    refused: list[tuple[str, str]]


@dataclasses.dataclass(frozen=True)
class Measured:
    """What one run measured: its ready time and each answered ask's in
    seconds, the bytes it held once ready, and its cells."""

    run: Run
    ready: float
    held: int
    cells: int
    ask_seconds: list[float]


HEADERS = [
    "Run",
    "Kept for a time",
    "Ready",
    "Held",
    "Against a",
    "Cells",
    "Per week",
    "Per day",
    f"Per hour by borough, {HOURS_KEPT_DAYS} days",
]


def runs(spec: str, newest: int, now: int, weekly: tuple) -> list[Run]:
    """The three runs over `spec`, its hours kept from 14 days before the
    day of the `newest` trip and its days from 91 days before its week,
    with `now` the server's time. `weekly` is the ask by week over every
    bucket, which every run answers."""
    hours_from = floor(newest, DAY_SECONDS) - HOURS_KEPT_DAYS * DAY_SECONDS
    days_from = floor(newest, WEEK_SECONDS) - DAYS_KEPT_DAYS * DAY_SECONDS
    hours_kept = days(round_up(now - hours_from, DAY_SECONDS))
    days_kept = days(round_up(now - days_from, WEEK_SECONDS))
    hourly = per_hour(hours_from, floor(newest, DAY_SECONDS) + DAY_SECONDS)
    every_day = per_day([])
    last_days = per_day(
        [stamp(days_from), stamp(floor(newest, WEEK_SECONDS) + WEEK_SECONDS)]
    )
    every_hour = (
        "Trips per hour, every bucket",
        "SELECT bucket, trips FROM trips GROUP BY bucket",
    )
    every_day_refused = (every_day[0], every_day[2])
    hours = f"keep('1 hour', '{hours_kept}'), '1 day', '7 days'"
    both = f"keep('1 hour', '{hours_kept}'), keep('1 day', '{days_kept}'), '7 days'"
    return [
        Run("a", "none", spec, [weekly, every_day, hourly], []),
        Run(
            "b",
            f"1 hour: {hours_kept}",
            spec_with(spec, hours),
            [weekly, every_day, hourly],
            [every_hour],
        ),
        Run(
            "c",
            f"1 hour: {hours_kept}; 1 day: {days_kept}",
            spec_with(spec, both),
            [weekly, last_days, hourly],
            [every_hour, every_day_refused],
        ),
    ]


def per_day(bounds: list[str]) -> tuple[str, str, str, list]:
    """Trips and fare per day, over every bucket or between `bounds`."""
    sql_range = ""
    query_range = ""
    name = "Fares per day, every bucket"
    if bounds:
        start, end = bounds
        sql_range = f" AND pickup_at >= '{start}' AND pickup_at < '{end}'"
        query_range = " WHERE bucket >= $1 AND bucket < $2"
        name = f"Fares per day, the last {DAYS_KEPT_DAYS} days"
    sql = f"""SELECT date_bin('1 day', pickup_at, timestamptz 'epoch'), count(*),
                     sum(fare_amount)
                FROM yellow_trips
               WHERE pickup_at IS NOT NULL{sql_range}
               GROUP BY 1"""
    query = f"""SELECT buckets(bucket, '1 day'), trips, fare FROM trips{query_range}
                 GROUP BY buckets(bucket, '1 day')"""
    return name, sql, query, bounds


def per_hour(start: int, end: int) -> tuple[str, str, str, list]:
    """Trips per hour by pickup borough from `start` until `end`."""
    bounds = [stamp(start), stamp(end)]
    sql = f"""SELECT date_bin('1 hour', t.pickup_at, timestamptz 'epoch'), z.borough,
                     count(*)
                FROM yellow_trips t
                LEFT JOIN taxi_zones z ON z.location_id = t.pu_location_id
               WHERE t.pickup_at >= '{bounds[0]}' AND t.pickup_at < '{bounds[1]}'
               GROUP BY 1, 2"""
    query = """SELECT bucket, borough, trips FROM trips
                WHERE bucket >= $1 AND bucket < $2
                GROUP BY bucket, borough"""
    name = f"Trips per hour by borough, the last {HOURS_KEPT_DAYS} days"
    return name, sql, query, bounds


def spec_with(spec: str, widths: str) -> str:
    """`spec` with every view's widths replaced by `widths`."""
    if spec.count(WIDTHS) != 2:
        raise ValueError(f"the spec should write {WIDTHS} in both views")
    return spec.replace(WIDTHS, widths)


def cells(views: str) -> int:
    """The cells at the narrowest width of every view, from the text
    `GET /views` sends: each view's `holds:` line starts with them."""
    total = 0
    in_view = False
    for line in views.splitlines():
        if not line.startswith(" "):
            in_view = line.startswith("view ")
        elif in_view and line.strip().startswith("holds:"):
            total += int(line.split(":", 1)[1].split()[0])
    return total


def floor(seconds: int, width: int) -> int:
    """`seconds` down to an edge of `width` on the epoch grid."""
    return seconds - seconds % width


def round_up(seconds: int, width: int) -> int:
    return -(-seconds // width) * width


def days(seconds: int) -> str:
    return f"{seconds // DAY_SECONDS} days"


def stamp(seconds: int) -> str:
    moment = datetime.datetime.fromtimestamp(seconds, datetime.UTC)
    return moment.strftime("%Y-%m-%dT%H:%M:%SZ")


def note(newest: int, now: int) -> str:
    """The lines above the table: the gap each duration is offset by."""
    return (
        f"Newest trip before July 2024 {stamp(newest)}, PostgreSQL's now() {stamp(now)}.\n"
        "Each duration is offset by that gap: hours are kept from"
        f" {HOURS_KEPT_DAYS} days before the newest trip's day,\n"
        f"days from {DAYS_KEPT_DAYS} days before its 7-day bucket."
        f" Per day reads every bucket in a and b, the last {DAYS_KEPT_DAYS}"
        " days in c."
    )


def table(measured: list[Measured]) -> list[list[str]]:
    """A row per run, its memory also against the first run's."""
    first = measured[0].held
    rows = []
    for each in measured:
        rows.append(
            [
                each.run.name,
                each.run.kept,
                f"{each.ready:.1f} s",
                f"{each.held / 2**20:,.0f} MiB",
                f"{each.held / first:.0%}",
                f"{each.cells:,}",
                *[duration(seconds) for seconds in each.ask_seconds],
            ]
        )
    return rows


async def refused(http, name: str, query: str) -> None:
    """Fails unless the Straum `http` asks refuses `query` for reading
    buckets it dropped."""
    body = json.dumps({"query": query, "parameters": []})
    async with http.post("/ask", data=body) as response:
        said = await response.text()
    if response.status == 200 or REFUSAL not in said:
        raise SystemExit(f"taxi: {name}: should be refused, got {said[:200]}")
