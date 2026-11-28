"""When Straum and PostgreSQL answer alike, and how the bench times a run
and writes a time.

bench/taxi.py hands `alike` Straum's rows, decoded with their decimals as
decimals, and PostgreSQL's rows as asyncpg read them. Each row goes to
one plain form, and the two answers are equal when they hold the same
rows the same number of times, in any order. Imports nothing outside
the standard library, so bench/test-stack.py runs it without the
bench's packages.
"""

import datetime
import decimal
import statistics
import time
from collections import Counter
from collections.abc import Awaitable, Callable


def alike(straum: list[list], postgres: list[list]) -> bool:
    """Whether both answers hold the same rows, decimals exact at
    Straum's places."""
    places = scales(straum)
    return Counter(plain(row, places) for row in straum) == Counter(
        plain(row, places) for row in postgres
    )


def scales(rows: list[list]) -> list[decimal.Decimal | None]:
    """Per column, the finest place Straum's decimals reach: a sum keeps
    its column's scale, an avg and a standard deviation four places more,
    a variance four past twice its column's."""
    finest: list[decimal.Decimal | None] = [None] * (len(rows[0]) if rows else 0)
    for row in rows:
        for column, value in enumerate(row):
            if not isinstance(value, decimal.Decimal):
                continue
            place = decimal.Decimal(1).scaleb(value.as_tuple().exponent)
            if finest[column] is None or place < finest[column]:
                finest[column] = place
    return finest


def plain(row: list, places: list[decimal.Decimal | None]) -> tuple:
    """The row in one form for both sides. A decimal is exact on both,
    PostgreSQL's avg, variance and standard deviation rounded half to even
    at the place Straum's has; a
    bucket comes from Straum as RFC 3339 text."""
    values = []
    for column, value in enumerate(row):
        match value:
            case decimal.Decimal() if places[column] is not None:
                value = value.quantize(places[column], rounding=decimal.ROUND_HALF_EVEN)
            case int() if places[column] is not None:
                value = decimal.Decimal(value).quantize(places[column])
            case datetime.datetime():
                value = value.astimezone(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
        values.append(value)
    return tuple(values)


async def median(run: Callable[[], Awaitable[object]]) -> float:
    """Median seconds of `run` over at least a second and five runs."""
    times: list[float] = []
    end = time.perf_counter() + 1
    while len(times) < 5 or time.perf_counter() < end:
        start = time.perf_counter()
        await run()
        times.append(time.perf_counter() - start)
    return statistics.median(times)


def duration(seconds: float) -> str:
    if seconds >= 1e-3:
        return f"{seconds * 1e3:.1f} ms"
    return f"{seconds * 1e6:.0f} µs"
