#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = ["numpy", "pyarrow"]
#
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Tests of the bench scripts' parts that need no Docker.

bench/stack.py and bench/try_your_data.py each carry the same functions
to name and find their Compose projects, since try_your_data.py runs
from a URL alone; every such test runs against both. They read `docker
compose` output handed to them here. try_your_data.py's Compose file
names the Host names Straum answers, which a test reads.

bench/answers.py says when Straum and PostgreSQL answer alike,
bench/materialized.py writes the materialized views and the asks over them
that bench/taxi.py's materialized mode times, and bench/retention.py the
specs and asks of its retention mode. bench/load_usage_events.py
compares its sample with the trace; a test moves the sample to see
the check stop it. Stack.load_taxi_trips gets a fake loader and a psql
that fails.
"""

import argparse
import contextlib
import datetime
import decimal
import hashlib
import importlib.util
import io
import json
import os
import pathlib
import socket
import subprocess
import sys
import tempfile
import unittest
import unittest.mock
import uuid

import numpy

BENCH = pathlib.Path(__file__).resolve().parent
TOOL = "straum-try-usage"


def load(name: str):
    """The script `bench/<name>.py` as a module."""
    spec = importlib.util.spec_from_file_location(name, BENCH / f"{name}.py")
    if spec is None or spec.loader is None:
        raise ImportError(f"no script {name}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


TRY = load("try_your_data")
COPIES = [load("stack"), TRY]
# bench/materialized.py imports answers as its scripts do, from bench/.
sys.path.insert(0, str(BENCH))
answers = load("answers")
usage_events = load("load_usage_events")
materialized = load("materialized")
retention = load("retention")
SPEC = (BENCH / "trips.sql").read_text()
# The newest trip, 2024-06-30T23:30:00Z, and a later server clock.
NEWEST = 1_719_790_200
NOW = 1_795_000_000
TRIPS_VIEW = next(
    view for view in materialized.MATERIALIZED_VIEWS if view.straum_view == "trips"
)
HOLISTIC_ASKS = [
    "Median fare",
    "p99 fare by payment type",
    "Most common fare by payment type",
    "Distinct fares by payment type",
    "Median fare per hour, one day",
]


def gone_pid() -> int:
    """The process id of a process that has ended."""
    child = subprocess.Popen([sys.executable, "-c", ""])
    child.wait()
    return child.pid


def listing(*names: str) -> str:
    """`docker compose ls --all --format json` output with `names`."""
    projects = [
        f'{{"Name":"{name}","Status":"exited(0)","ConfigFiles":"-"}}' for name in names
    ]
    return "[" + ",".join(projects) + "]"


class BuildVersion(unittest.TestCase):
    """A local build names the commit it is built from, as a release
    names its tag, so `straum version` and `/status` don't say 0.0.0."""

    stack = COPIES[0]

    def test_a_local_build_names_its_commit(self):
        built = self.stack.build_environment({"PATH": "/bin"}, "v0.12.0-3-gabc1234-dirty")
        self.assertEqual(
            built, {"PATH": "/bin", "STRAUM_VERSION": "0.12.0-3-gabc1234-dirty"}
        )
        untagged = self.stack.build_environment({}, "abc1234")
        self.assertEqual(untagged, {"STRAUM_VERSION": "abc1234"})

    def test_a_version_set_wins(self):
        given = {"STRAUM_VERSION": "9.9.9"}
        self.assertEqual(self.stack.build_environment(given, "v0.12.0"), given)

    def test_without_git_the_build_keeps_cargo_s_version(self):
        self.assertEqual(self.stack.build_environment({"PATH": "/bin"}, None), {"PATH": "/bin"})

    def test_this_checkout_names_a_version(self):
        described = self.stack.git_version()
        self.assertIsNotNone(described)
        self.assertNotEqual(described.strip(), "")


class RunEnvironment(unittest.TestCase):
    def test_drops_the_caller_s_straum_and_pg_variables(self):
        environ = {"STRAUM_DSN_FILE": "/run/dsn", "PGHOST": "db", "PATH": "/bin"}
        self.assertEqual(COPIES[0].run_environment(environ), {"PATH": "/bin"})


class LoadTaxiTrips(unittest.TestCase):
    def test_a_failed_psql_closes_the_pipe_from_the_loader(self):
        read, write = os.pipe()
        self.addCleanup(os.close, write)
        loader = unittest.mock.Mock(stdout=os.fdopen(read, "rb"))
        stack = COPIES[0]
        with (
            unittest.mock.patch.object(stack.subprocess, "Popen", return_value=loader),
            unittest.mock.patch.object(
                stack.subprocess, "run", side_effect=subprocess.CalledProcessError(1, "psql")
            ),
            self.assertRaises(subprocess.CalledProcessError),
        ):
            stack.Stack("straum-test", {}).load_taxi_trips(1)
        self.assertTrue(loader.stdout.closed)


class ProjectNames(unittest.TestCase):
    def test_a_run_is_named_for_its_tool_and_process(self):
        for copy in COPIES:
            with self.subTest(copy=copy.__name__):
                self.assertEqual(copy.run_name(TOOL, 4242), f"{TOOL}-pid4242")

    def test_a_project_of_a_gone_run_is_left_over(self):
        gone = gone_pid()
        for copy in COPIES:
            with self.subTest(copy=copy.__name__):
                listed = listing(copy.run_name(TOOL, gone))
                self.assertEqual(
                    copy.leftover_projects(listed, TOOL), [f"{TOOL}-pid{gone}"]
                )

    def test_a_project_of_a_running_run_stays(self):
        for copy in COPIES:
            with self.subTest(copy=copy.__name__):
                listed = listing(copy.run_name(TOOL, os.getpid()))
                self.assertEqual(copy.leftover_projects(listed, TOOL), [])

    def test_a_project_of_another_tool_stays(self):
        gone = gone_pid()
        others = [
            f"{TOOL}-2-pid{gone}",
            f"straum-taxi-pid{gone}",
            f"x{TOOL}-pid{gone}",
            f"{TOOL}-pid{gone}x",
            f"{TOOL}-{gone}",
            "straum-enumids-17",
            TOOL,
        ]
        for copy in COPIES:
            with self.subTest(copy=copy.__name__):
                listed = listing(*others)
                self.assertEqual(copy.leftover_projects(listed, TOOL), [])

    def test_no_listing_leaves_nothing_over(self):
        for copy in COPIES:
            with self.subTest(copy=copy.__name__):
                self.assertEqual(copy.leftover_projects("", TOOL), [])
                self.assertEqual(copy.leftover_projects("[]", TOOL), [])


class PublishedPorts(unittest.TestCase):
    def test_reads_the_port_compose_printed(self):
        for listed, port in [
            ("0.0.0.0:49153\n", 49153),
            ("127.0.0.1:5432\n[::]:5432\n", 5432),
            ("[::1]:8080", 8080),
        ]:
            for copy in COPIES:
                with self.subTest(copy=copy.__name__, listed=listed):
                    self.assertEqual(copy.published_port(listed), port)

    def test_no_port_printed_is_none(self):
        for listed in ["", "\n", ":\n", "127.0.0.1:\n", "no port"]:
            for copy in COPIES:
                with self.subTest(copy=copy.__name__, listed=listed):
                    self.assertIsNone(copy.published_port(listed))


class TryAllowedHosts(unittest.TestCase):
    """try_your_data.py publishes Straum on loopback, so it names the
    Host names Straum answers, as a page rebound to 127.0.0.1 sends
    another."""

    def straum(self, port: int) -> dict:
        args = argparse.Namespace(
            wal="logical",
            postgres="18",
            data=BENCH,
            spec=BENCH / "usage.sql",
            image="straum",
            stack=f"{TOOL}-pid1",
            port=port,
        )
        compose = json.loads(TRY.compose_file(args, "secret"))
        return compose["services"][TRY.STRAUM]

    def allowed(self, straum: dict) -> list[str]:
        command = straum["command"]
        return command[command.index("--allow-hosts") + 1].split(",")

    def test_names_the_published_port_and_the_shells(self):
        straum = self.straum(49153)
        self.assertEqual(
            straum["ports"],
            [{"target": 8080, "host_ip": "127.0.0.1", "published": 49153}],
        )
        self.assertEqual(
            self.allowed(straum),
            ["localhost:49153", "127.0.0.1:49153", "127.0.0.1:8080"],
        )
        # At Straum's own port, the shell's name is named once.
        self.assertEqual(
            self.allowed(self.straum(8080)), ["localhost:8080", "127.0.0.1:8080"]
        )

    def test_a_rebound_name_is_not_named(self):
        allowed = self.allowed(self.straum(49153))
        for name in ["evil.example:49153", "0.0.0.0:49153", "localhost:8080"]:
            with self.subTest(name=name):
                self.assertNotIn(name, allowed)

    def test_a_free_port_can_be_bound(self):
        port = TRY.free_port()
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", port))


class TryPlanCopy(unittest.TestCase):
    """--plan-copy writes load.sql with the image that then runs, for a
    copy that a script wrote in place of export.sql."""

    def copy_without_load(self) -> pathlib.Path:
        data = pathlib.Path(self.enterContext(tempfile.TemporaryDirectory()))
        (data / TRY.SCHEMA_FILE).write_text("")
        return data

    def test_a_copy_without_load_sql_needs_plan_copy(self):
        data = self.copy_without_load()
        spec = str(BENCH / "usage.sql")
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
            TRY.parse([spec, "--data", str(data)])
        self.assertTrue(TRY.parse([spec, "--data", str(data), "--plan-copy"]).plan_copy)

    def test_plans_the_copy_in_the_service_that_runs(self):
        data = self.copy_without_load()
        command = TRY.plan_copy_command(data)
        self.assertEqual(
            command[command.index(TRY.STRAUM) :],
            [TRY.STRAUM, "plan", TRY.SPEC_IN_IMAGE, "--copy", TRY.COPY_IN_IMAGE],
        )
        self.assertIn(f"{data}:{TRY.COPY_IN_IMAGE}", command)


class Answers(unittest.TestCase):
    def test_alike_answers_pass(self):
        # Straum's avg at its places, PostgreSQL's longer, rows in
        # another order, a bucket as RFC 3339 text and as a timestamp.
        hour = datetime.datetime(2024, 1, 17, 5, tzinfo=datetime.UTC)
        straum = [
            ["2024-01-17T05:00:00Z", 2, decimal.Decimal("3.1250")],
            ["2024-01-17T06:00:00Z", 1, decimal.Decimal("1.0000")],
        ]
        postgres = [
            [hour + datetime.timedelta(hours=1), 1, decimal.Decimal(1)],
            [hour, 2, decimal.Decimal("3.12500000000000000")],
        ]
        self.assertTrue(answers.alike(straum, postgres))

    def test_a_planted_wrong_answer_fails(self):
        straum = [[1, 8, decimal.Decimal("12.50")], [2, 3, decimal.Decimal("4.00")]]
        for planted in [
            [[1, 9, decimal.Decimal("12.50")], [2, 3, decimal.Decimal("4.00")]],
            [[1, 8, decimal.Decimal("12.51")], [2, 3, decimal.Decimal("4.00")]],
            [[1, 8, decimal.Decimal("12.50")]],
            [[1, 8, decimal.Decimal("12.50")], [2, 3, decimal.Decimal("4.00")]] * 2,
        ]:
            with self.subTest(planted=planted):
                self.assertFalse(answers.alike(straum, planted))


class MaterializedViews(unittest.TestCase):
    def test_a_materialized_view_keeps_the_views_keys_at_its_narrowest_bucket(self):
        view, index = materialized.create_statements(TRIPS_VIEW)
        self.assertTrue(view.startswith("CREATE MATERIALIZED VIEW trips_hourly AS"))
        self.assertIn(
            "date_bin('1 hour', trips.pickup_at, timestamptz 'epoch') AS bucket", view
        )
        self.assertIn("FROM yellow_trips AS trips", view)
        self.assertIn("count(trips.fare_amount) AS fares", view)
        self.assertIn(
            "sum(trips.fare_amount * trips.fare_amount) AS fare_squares", view
        )
        self.assertIn("pickup.borough AS borough", view)
        self.assertTrue(view.endswith("GROUP BY 1, 2, 3, 4"))
        self.assertEqual(
            index,
            "CREATE UNIQUE INDEX ON trips_hourly "
            "(bucket, payment_type, pu_location_id, borough) NULLS NOT DISTINCT",
        )

    def test_a_measure_that_does_not_add_across_buckets_is_refused(self):
        for measure in [
            "percentile_disc(0.5) WITHIN GROUP (ORDER BY trips.fare_amount)",
            "mode() WITHIN GROUP (ORDER BY trips.fare_amount)",
            "count(DISTINCT trips.fare_amount)",
            "sum(DISTINCT trips.fare_amount)",
            "avg(trips.fare_amount)",
            "max(trips.fare_amount)",
        ]:
            planted = materialized.MaterializedView(
                name=TRIPS_VIEW.name,
                straum_view=TRIPS_VIEW.straum_view,
                source=TRIPS_VIEW.source,
                time=TRIPS_VIEW.time,
                keys=TRIPS_VIEW.keys,
                measures=[
                    *TRIPS_VIEW.measures,
                    materialized.Column(measure, "planted"),
                ],
            )
            with self.subTest(measure=measure), self.assertRaises(ValueError):
                materialized.create_statements(planted)


class MaterializedAsks(unittest.TestCase):
    def test_an_ask_a_materialized_view_answers_reads_only_materialized_views(self):
        names = [view.name for view in materialized.MATERIALIZED_VIEWS]
        answered = {
            name: sql
            for name, sql in materialized.MATERIALIZED_ASKS.items()
            if sql is not None
        }
        self.assertIn("Trips", answered)
        self.assertIn("Tips by dropoff borough", answered)
        for name, sql in answered.items():
            with self.subTest(ask=name):
                self.assertNotIn("yellow_trips", sql)
                self.assertNotIn("taxi_zones", sql)
                self.assertTrue(any(f"FROM {view}" in sql for view in names))

    def test_an_ask_no_materialized_view_answers_is_marked(self):
        for name in HOLISTIC_ASKS:
            with self.subTest(ask=name):
                self.assertIsNone(materialized.MATERIALIZED_ASKS[name])
        self.assertEqual(materialized.answer_cell(None), "needs the raw rows")
        self.assertEqual(materialized.answer_cell(0.0005), "500 µs")
        self.assertEqual(materialized.answer_cell(0.0123), "12.3 ms")


class Retention(unittest.TestCase):
    def test_each_horizon_falls_on_its_edge_or_before(self):
        a, b, c = retention.runs(SPEC, NEWEST, NOW, ("weekly", "", "", []))
        hours_kept = int(b.kept.split(": ")[1].split()[0]) * retention.DAY_SECONDS
        days_kept = int(c.kept.split("1 day: ")[1].split()[0]) * retention.DAY_SECONDS
        self.assertEqual(days_kept % retention.WEEK_SECONDS, 0)
        self.assertLessEqual(hours_kept, days_kept)
        hourly = b.asks[2][3]
        self.assertEqual(hourly, ["2024-06-16T00:00:00Z", "2024-07-01T00:00:00Z"])
        self.assertEqual(c.asks[1][3][0], "2024-03-28T00:00:00Z")
        # The load reads now() up to a day later; its horizon never passes
        # the first bound an ask reads.
        for late in [0, retention.DAY_SECONDS - 1]:
            hours_at = retention.floor(NOW + late - hours_kept, retention.DAY_SECONDS)
            days_at = retention.floor(NOW + late - days_kept, retention.WEEK_SECONDS)
            self.assertLessEqual(hours_at, NEWEST - 14 * retention.DAY_SECONDS)
            self.assertLessEqual(retention.stamp(hours_at), hourly[0])
            self.assertLessEqual(retention.stamp(days_at), c.asks[1][3][0])
        self.assertEqual(a.spec, SPEC)
        self.assertEqual(b.spec.count("keep('1 hour'"), 2)
        self.assertEqual(c.spec.count("keep('1 day'"), 2)
        self.assertEqual([len(run.refused) for run in [a, b, c]], [0, 1, 2])

    def test_a_spec_without_the_widths_in_both_views_is_refused(self):
        planted = SPEC.replace(retention.WIDTHS, "'1 hour', '1 day'", 1)
        with self.assertRaises(ValueError):
            retention.spec_with(planted, "'1 day'")

    def test_cells_sum_each_views_narrowest_width_and_skip_dims(self):
        views = (
            "view trips\n  buckets:  1 hour (1000 cells)\n  holds:    1000 cells and 1 bound cell\n"
            "\nview dropoffs\n  holds:    1 cell\n\ndim public.zones\n  holds:    265 rows\n"
        )
        self.assertEqual(retention.cells(views), 1001)


def usage_trace() -> object:
    """A trace day of three functions with every call in the first or the
    last six hours, so a sample that moves the calls fails the check."""
    trace = usage_events.TraceDay.__new__(usage_events.TraceDay)
    trace.owners = numpy.array(["a" * 64, "a" * 64, "b" * 64])
    trace.functions = numpy.array(["1" * 64, "2" * 64, "3" * 64])
    trace.triggers = numpy.array(["http", "queue", "timer"])
    counts = numpy.zeros((3, usage_events.MINUTES_PER_DAY), dtype=numpy.int64)
    counts[0, :360] = 20
    counts[1, -360:] = 10
    counts[2, :] = 1
    trace.counts = counts
    # Percentiles 0, 1, 25, 50, 75, 99 and 100, then the mean. The second
    # function has one call of 10 seconds in each 100.
    trace.percentiles = numpy.array(
        [
            [1, 2, 3, 4, 5, 6, 7, 4],
            [10, 20, 30, 40, 50, 60, 10_000, 140],
            [numpy.nan] * 8,
        ]
    )
    return trace


def every_call(trace) -> tuple:
    """The function and minute of each call of `trace`: a sample that
    keeps all of them."""
    functions, minutes = numpy.nonzero(trace.counts)
    repeats = trace.counts[functions, minutes]
    return numpy.repeat(functions, repeats), numpy.repeat(minutes, repeats)


class UsageEvents(unittest.TestCase):
    def test_runs_try_on_the_copy_with_the_image_s_plan(self):
        self.assertEqual(
            usage_events.try_arguments(pathlib.Path("copy"), ["--image", "straum:dev"]),
            [
                str(BENCH / "try_your_data.py"),
                str(BENCH / "usage.sql"),
                "--data",
                "copy",
                "--plan-copy",
                "--image",
                "straum:dev",
            ],
        )

    def test_a_sample_with_the_shape_of_the_trace_passes(self):
        trace = usage_trace()
        functions, minutes = every_call(trace)
        usage_events.compare(trace, functions, minutes, 1)

    def test_a_sample_with_its_calls_moved_in_time_is_refused(self):
        trace = usage_trace()
        functions, minutes = every_call(trace)
        moved = (minutes + 360) % usage_events.MINUTES_PER_DAY
        with self.assertRaisesRegex(SystemExit, "hour"):
            usage_events.compare(trace, functions, moved, 1)

    def test_a_sample_with_another_mix_of_kinds_is_refused(self):
        trace = usage_trace()
        functions, minutes = every_call(trace)
        only_http = numpy.zeros_like(functions)
        with self.assertRaisesRegex(SystemExit, "kind|accounts"):
            usage_events.compare(trace, only_http, minutes, 1)

    def test_an_owner_hash_becomes_a_valid_uuid(self):
        owner = hashlib.sha256(b"owner").hexdigest()
        account = usage_events.account_id(owner)
        self.assertEqual(str(uuid.UUID(account)), account)
        self.assertEqual(account.replace("-", ""), owner[:32])

    def test_a_duration_lies_between_the_percentiles_of_its_function(self):
        trace = usage_trace()
        rows = numpy.array([0, 1] * 500)
        got = usage_events.durations(trace, rows, numpy.random.default_rng(0))
        for row, amount in zip(rows, got, strict=True):
            low, high = trace.percentiles[row, 0], trace.percentiles[row, 6]
            self.assertTrue(low <= amount <= high, f"{amount} of function {row}")

    def test_the_mean_duration_of_a_function_is_its_mean_in_the_trace(self):
        trace = usage_trace()
        rows = numpy.ones(200_000, dtype=numpy.int64)
        got = usage_events.durations(trace, rows, numpy.random.default_rng(0))
        self.assertAlmostEqual(got.mean() / 140, 1, delta=0.05)

    def test_a_sample_with_another_mean_duration_is_refused(self):
        trace = usage_trace()
        # Twenty times every call, so that its few slow calls give a steady mean.
        functions = numpy.tile(every_call(trace)[0], 20)
        got = usage_events.durations(trace, functions, numpy.random.default_rng(0))
        usage_events.compare_means(trace, got, functions, 1)
        with self.assertRaisesRegex(SystemExit, "mean duration"):
            usage_events.compare_means(trace, got * 2, functions, 1)

    def test_a_function_without_durations_has_no_amount(self):
        trace = usage_trace()
        rows = numpy.array([2, 2])
        got = usage_events.durations(trace, rows, numpy.random.default_rng(0))
        self.assertEqual(usage_events.amounts(got).to_pylist(), [None, None])


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=1).result
    sys.exit(0 if result.wasSuccessful() else 1)
