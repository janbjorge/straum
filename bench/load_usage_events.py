#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = ["numpy", "pyarrow"]
#
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Writes the usage events of the guide as a copy that `just try` loads.

The events come from the Azure Functions trace of 2019 (Shahrad et al.,
"Serverless in the Wild", USENIX ATC 2020, CC-BY 4.0). The trace counts
the calls of each function in each minute of 14 days. Each account is a
trace owner, and each event is one call that the sample keeps.

These columns are from the trace:

- `at`: the minute of the call, and a random second in that minute.
- `account_id`: the owner of the function, as a uuid.
- `kind`: the trigger of the function, for example `http` or `queue`.
- `amount`: the duration of the call in milliseconds. It is a random
  value between the percentiles of the function on that day.

These columns are generated, because the trace does not have them:

- `device_id`: a number for the function on an `http` call, else null.
- `region` of an account: `eu`, `us` or `apac`, from the owner hash.

The sample keeps each call with the same chance, so the shape of the
time axis, the spread of calls over accounts and the mix of kinds stay
as in the trace. The script compares them with the trace for each day,
and stops if one is not in its limit.

--out gets `schema.sql`, `public.events.csv` and `public.accounts.csv`.
`just usage-events` then writes `load.sql` beside them with `straum
plan --copy` and opens `straum shell` on them through `just try`.
"""

import argparse
import collections
import datetime
import hashlib
import pathlib
import sys
import tarfile
import urllib.request

import numpy
import pyarrow
import pyarrow.compute
import pyarrow.csv

TRACE_URL = (
    "https://github.com/Azure/AzurePublicDataset/releases/download/"
    "dataset-functions-2019/azurefunctions_dataset2019_azurefunctions-dataset2019.tar.xz"
)
TRACE_FILE = "azurefunctions-dataset2019.tar.xz"
# The SHA-256 of the archive as fetched on 2026-11-25. The checks below
# hold only for these bytes.
TRACE_SHA256 = "aff8b3ca7240a41a109e4ee598e0a96e45fcb92e7b8395ac19cb3748cd260d89"
DAYS = 14

# Day 6, 7, 13 and 14 have fewer calls than the other days, so day 1 is
# taken as a Monday. The trace does not give its dates.
FIRST_DAY = datetime.datetime(2026, 11, 2, tzinfo=datetime.UTC)
MINUTES_PER_DAY = 1440
MICROSECONDS_PER_MINUTE = 60_000_000

# The sample keeps one call in this many: about 0.9 million events a day.
KEEP_ONE_IN = 1000
SEED = 2019

# The percentiles of each function's durations that the trace gives.
DURATION_PERCENTILES = ["0", "1", "25", "50", "75", "99", "100"]
QUANTILES = numpy.array([0.0, 0.01, 0.25, 0.50, 0.75, 0.99, 1.0])
# The column of the function's mean duration, after its percentiles.
AVERAGE = len(DURATION_PERCENTILES)
TAIL = len(QUANTILES) - 2
# The first segment that is bent: from the 75th percentile up.
UPPER = TAIL - 1

REGIONS = ["eu", "us", "apac"]

# The largest difference allowed between a share in the sample and the
# same share in the trace. A day keeps about 0.9 million calls, so the
# random error of a share is near 0.0003; the limits are well above it.
SHARE_LIMIT = 0.003
# The top accounts hold most calls, so one large account moves their
# share more than the random error of a single share.
TOP_SHARE_LIMIT = 0.01
# The largest part by which the mean duration of the sample can differ
# from the mean of the trace. A few calls of minutes make the most of the
# mean, so its random error is larger than that of a share.
MEAN_LIMIT = 0.05

EVENTS = pyarrow.schema(
    [
        ("at", pyarrow.timestamp("us", tz="UTC")),
        ("kind", pyarrow.string()),
        ("account_id", pyarrow.string()),
        ("device_id", pyarrow.int64()),
        ("amount", pyarrow.decimal128(12, 2)),
    ]
)

SCHEMA = """\
CREATE SCHEMA IF NOT EXISTS public;
CREATE TABLE public.events (at timestamptz, kind text, account_id uuid, device_id bigint, amount numeric(12, 2));
CREATE TABLE public.accounts (id uuid, region text);
"""


class TraceDay:
    """The counts of one day of the trace, one row for each function."""

    def __init__(self, invocations: pyarrow.Table, durations: pyarrow.Table) -> None:
        self.owners = invocations.column("HashOwner").to_numpy(zero_copy_only=False)
        self.functions = invocations.column("HashFunction").to_numpy(
            zero_copy_only=False
        )
        self.triggers = invocations.column("Trigger").to_numpy(zero_copy_only=False)
        minutes = [str(minute) for minute in range(1, MINUTES_PER_DAY + 1)]
        counts = numpy.empty((invocations.num_rows, MINUTES_PER_DAY), dtype=numpy.int64)
        for index, name in enumerate(minutes):
            counts[:, index] = invocations.column(name).to_numpy()
        self.counts = counts
        self.percentiles = function_percentiles(invocations, durations)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--out", type=pathlib.Path, required=True, help="the copy directory"
    )
    parser.add_argument(
        "--data",
        type=pathlib.Path,
        default=pathlib.Path("bench/data"),
        help="downloads",
    )
    parser.add_argument(
        "--days", type=int, default=DAYS, help=f"from day 1, at most {DAYS}"
    )
    args = parser.parse_args()
    if not 1 <= args.days <= DAYS:
        raise SystemExit(f"--days is {args.days}; give 1 to {DAYS}")
    args.data.mkdir(parents=True, exist_ok=True)
    args.out.mkdir(parents=True, exist_ok=True)
    trace = download(args.data)
    random = numpy.random.default_rng(SEED)
    regions: dict[str, str] = {}
    events_path = args.out / "public.events.csv"
    with (
        tarfile.open(trace) as archive,
        pyarrow.csv.CSVWriter(events_path, EVENTS) as writer,
    ):
        for day in range(1, args.days + 1):
            sampled = sample_day(read_day(archive, day), day, random, regions)
            writer.write_table(sampled)
            log(f"day {day}: {sampled.num_rows} events")
    write_accounts(args.out / "public.accounts.csv", regions)
    (args.out / "schema.sql").write_text(SCHEMA)
    log(f"{len(regions)} accounts")


def download(data: pathlib.Path) -> pathlib.Path:
    """The trace archive in `data`, fetched once and checked each time."""
    path = data / TRACE_FILE
    if not path.exists():
        log(f"fetching {TRACE_URL}")
        part = path.with_suffix(".part")
        urllib.request.urlretrieve(TRACE_URL, part)
        check(part)
        part.rename(path)
    check(path)
    return path


def check(path: pathlib.Path) -> None:
    with path.open("rb") as file:
        got = hashlib.file_digest(file, "sha256").hexdigest()
    if got != TRACE_SHA256:
        raise SystemExit(
            f"{path}: SHA-256 {got}, not the pinned {TRACE_SHA256}; "
            "the trace changed, so the checks no longer hold"
        )


def read_day(archive: tarfile.TarFile, day: int) -> TraceDay:
    invocations = read_member(
        archive, f"invocations_per_function_md.anon.d{day:02}.csv"
    )
    durations = read_member(
        archive, f"function_durations_percentiles.anon.d{day:02}.csv"
    )
    return TraceDay(invocations, durations)


def read_member(archive: tarfile.TarFile, name: str) -> pyarrow.Table:
    member = archive.extractfile(name)
    if member is None:
        raise SystemExit(f"the trace has no file {name}")
    with member:
        return pyarrow.csv.read_csv(member)


def function_percentiles(
    invocations: pyarrow.Table, durations: pyarrow.Table
) -> numpy.ndarray:
    """The duration percentiles and the mean duration of each function
    row, NaN where the trace has none for the function. A function is
    unique only with its owner and app, so all three hashes find it."""
    names = [f"percentile_Average_{percentile}" for percentile in DURATION_PERCENTILES]
    names.append("Average")
    found: dict[tuple[str, str, str], numpy.ndarray] = {}
    owners = durations.column("HashOwner").to_pylist()
    apps = durations.column("HashApp").to_pylist()
    functions = durations.column("HashFunction").to_pylist()
    values = numpy.column_stack(
        [durations.column(name).to_numpy().astype(numpy.float64) for name in names]
    )
    for index, key in enumerate(zip(owners, apps, functions, strict=True)):
        found[key] = values[index]
    missing = numpy.full(len(names), numpy.nan)
    rows = []
    keys = zip(
        invocations.column("HashOwner").to_pylist(),
        invocations.column("HashApp").to_pylist(),
        invocations.column("HashFunction").to_pylist(),
        strict=True,
    )
    for key in keys:
        rows.append(found.get(key, missing))
    return numpy.vstack(rows)


def sample_day(
    trace: TraceDay, day: int, random: numpy.random.Generator, regions: dict[str, str]
) -> pyarrow.Table:
    """The kept calls of one day as rows of `public.events`."""
    kept = random.binomial(trace.counts, 1 / KEEP_ONE_IN)
    functions, minutes = numpy.nonzero(kept)
    repeats = kept[functions, minutes]
    function_rows = numpy.repeat(functions, repeats)
    minute_rows = numpy.repeat(minutes, repeats)
    compare(trace, function_rows, minute_rows, day)
    milliseconds = durations(trace, function_rows, random)
    compare_means(trace, milliseconds, function_rows, day)
    count = len(function_rows)
    day_start = FIRST_DAY + datetime.timedelta(days=day - 1)
    start_us = int(day_start.timestamp()) * 1_000_000
    offsets = minute_rows * MICROSECONDS_PER_MINUTE + random.integers(
        0, MICROSECONDS_PER_MINUTE, count
    )
    at = pyarrow.array(start_us + offsets, type=pyarrow.timestamp("us", tz="UTC"))
    for owner in numpy.unique(trace.owners[functions]):
        regions.setdefault(owner, region(owner))
    accounts = pyarrow.array(
        [account_id(owner) for owner in trace.owners[function_rows]]
    )
    kinds = pyarrow.array(trace.triggers[function_rows])
    return pyarrow.table(
        {
            "at": at,
            "kind": kinds,
            "account_id": accounts,
            "device_id": devices(trace, function_rows),
            "amount": amounts(milliseconds),
        },
        schema=EVENTS,
    )


def devices(trace: TraceDay, function_rows: numpy.ndarray) -> pyarrow.Array:
    """A number for the function on an `http` call, else null."""
    numbers = numpy.array(
        [int(function[:15], 16) for function in trace.functions], dtype=numpy.int64
    )
    is_http = trace.triggers[function_rows] == "http"
    return pyarrow.array(numbers[function_rows], mask=~is_http)


def durations(
    trace: TraceDay, function_rows: numpy.ndarray, random: numpy.random.Generator
) -> numpy.ndarray:
    """A duration in milliseconds for each call, NaN where its function
    has none. Below the 75th percentile the duration lies on a straight
    line between two percentiles of its function. Above it, both lines
    are bent so that the mean of the function is the mean of the trace.
    Straight lines up to the longest call give the slowest calls too much
    weight: they made the mean of the trace's first day 28% too high."""
    chances = random.random(len(function_rows))
    segments = numpy.clip(
        numpy.searchsorted(QUANTILES, chances, side="right") - 1, 0, TAIL
    )
    low = trace.percentiles[function_rows, segments]
    high = trace.percentiles[function_rows, segments + 1]
    part = (chances - QUANTILES[segments]) / (
        QUANTILES[segments + 1] - QUANTILES[segments]
    )
    bend = upper_bends(trace.percentiles)[function_rows]
    is_upper = segments >= UPPER
    with numpy.errstate(invalid="ignore"):
        part[is_upper] = part[is_upper] ** bend[is_upper]
    return numpy.round(low + part * (high - low), 2)


def upper_bends(percentiles: numpy.ndarray) -> numpy.ndarray:
    """For each function, the power that bends the lines above the 75th
    percentile. With part ** bend, a segment from low to high has the
    mean low + (high - low) / (bend + 1). One bend for both segments
    makes the mean of the function equal to its mean in the trace."""
    widths = numpy.diff(QUANTILES)
    lower = (percentiles[:, :UPPER] + percentiles[:, 1 : UPPER + 1]) / 2
    mean_lower = (lower * widths[:UPPER]).sum(axis=1)
    lows = percentiles[:, UPPER : TAIL + 1]
    highs = percentiles[:, UPPER + 1 : TAIL + 2]
    upper_widths = widths[UPPER:]
    at_lows = mean_lower + (lows * upper_widths).sum(axis=1)
    spread = ((highs - lows) * upper_widths).sum(axis=1)
    with numpy.errstate(divide="ignore", invalid="ignore"):
        reach = numpy.clip((percentiles[:, AVERAGE] - at_lows) / spread, 0, 1)
        bends = 1 / reach - 1
    # A mean that the low ends already reach puts the calls there; a
    # function with no durations gets a bend it never uses.
    return numpy.nan_to_num(bends, nan=1.0, posinf=1e9)


def amounts(milliseconds: numpy.ndarray) -> pyarrow.Array:
    values = pyarrow.array(milliseconds, mask=numpy.isnan(milliseconds))
    return pyarrow.compute.cast(values, pyarrow.decimal128(12, 2))


def compare_means(
    trace: TraceDay, milliseconds: numpy.ndarray, function_rows: numpy.ndarray, day: int
) -> None:
    """Stops unless the mean duration of the sample is near the mean of
    the trace: the mean of each function, weighted by its calls."""
    calls = trace.counts.sum(axis=1)
    averages = trace.percentiles[:, AVERAGE]
    known = ~numpy.isnan(averages)
    trace_mean = (averages[known] * calls[known]).sum() / calls[known].sum()
    sample_mean = numpy.nanmean(milliseconds)
    apart = abs(sample_mean - trace_mean) / trace_mean
    if apart > MEAN_LIMIT:
        raise SystemExit(
            f"day {day}, mean duration: the sample has {sample_mean:.1f} ms "
            f"and the trace {trace_mean:.1f} ms, {apart:.1%} apart"
        )


def compare(
    trace: TraceDay, function_rows: numpy.ndarray, minute_rows: numpy.ndarray, day: int
) -> None:
    """Stops unless the sample has the shape of the trace on this day:
    calls by hour, by kind, and the share of the top accounts."""
    hours = trace.counts.reshape(len(trace.counts), 24, 60).sum(axis=(0, 2))
    sampled_hours = numpy.bincount(minute_rows // 60, minlength=24)
    check_shares(f"day {day}, hour", shares(hours), shares(sampled_hours), SHARE_LIMIT)
    calls = trace.counts.sum(axis=1)
    check_shares(
        f"day {day}, kind",
        totals_by(trace.triggers, calls),
        totals_by(trace.triggers[function_rows], None),
        SHARE_LIMIT,
    )
    trace_owners = totals_by(trace.owners, calls)
    sampled_owners = totals_by(trace.owners[function_rows], None)
    for top in (1, 10, 100):
        check_share(
            f"day {day}, top {top} accounts",
            top_share(trace_owners, top),
            top_share(sampled_owners, top),
            TOP_SHARE_LIMIT,
        )


def shares(counts: numpy.ndarray) -> dict[int, float]:
    total = counts.sum()
    return {index: count / total for index, count in enumerate(counts)}


def totals_by(keys: numpy.ndarray, weights: numpy.ndarray | None) -> dict[str, float]:
    """The share of the calls that each key has."""
    totals: collections.Counter[str] = collections.Counter()
    if weights is None:
        weights = numpy.ones(len(keys), dtype=numpy.int64)
    unique, inverse = numpy.unique(keys, return_inverse=True)
    summed = numpy.bincount(inverse, weights=weights)
    grand = summed.sum()
    for key, value in zip(unique, summed, strict=True):
        totals[key] = value / grand
    return dict(totals)


def top_share(shares_by_key: dict[str, float], top: int) -> float:
    return sum(sorted(shares_by_key.values(), reverse=True)[:top])


def check_shares(what: str, trace: dict, sampled: dict, limit: float) -> None:
    for key, share in trace.items():
        check_share(f"{what} {key}", share, sampled.get(key, 0.0), limit)


def check_share(what: str, trace: float, sampled: float, limit: float) -> None:
    if abs(trace - sampled) > limit:
        raise SystemExit(
            f"{what}: the sample has {sampled:.4f} of the calls and the trace "
            f"{trace:.4f}, more than {limit} apart"
        )


def account_id(owner: str) -> str:
    """The owner hash as a uuid: its first 32 hex digits."""
    return f"{owner[0:8]}-{owner[8:12]}-{owner[12:16]}-{owner[16:20]}-{owner[20:32]}"


def region(owner: str) -> str:
    return REGIONS[int(owner[-8:], 16) % len(REGIONS)]


def write_accounts(path: pathlib.Path, regions: dict[str, str]) -> None:
    owners = sorted(regions)
    table = pyarrow.table(
        {
            "id": [account_id(owner) for owner in owners],
            "region": [regions[owner] for owner in owners],
        }
    )
    pyarrow.csv.write_csv(table, path)


def log(message: str) -> None:
    print(message, file=sys.stderr)


if __name__ == "__main__":
    main()
