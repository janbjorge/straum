#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = []
# ///
"""Tests of the bench scripts' parts that need no Docker.

bench/stack.py and bench/try_your_data.py each carry the same functions
to name and find their Compose projects, since try_your_data.py runs
from a URL alone; every such test runs against both. They read `docker
compose` output handed to them here.

bench/answers.py says when Straum and PostgreSQL answer alike, and
bench/materialized.py writes the materialized views and the asks over them
that bench/taxi.py's materialized mode times.
"""

import datetime
import decimal
import importlib.util
import os
import pathlib
import subprocess
import sys
import unittest

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


COPIES = [load("stack"), load("try_your_data")]
# bench/materialized.py imports answers as its scripts do, from bench/.
sys.path.insert(0, str(BENCH))
answers = load("answers")
materialized = load("materialized")
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


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=1).result
    sys.exit(0 if result.wasSuccessful() else 1)
