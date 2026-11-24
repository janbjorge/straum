#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = []
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
specs and asks of its retention mode.
"""

import argparse
import datetime
import decimal
import importlib.util
import json
import os
import pathlib
import socket
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


TRY = load("try_your_data")
COPIES = [load("stack"), TRY]
# bench/materialized.py imports answers as its scripts do, from bench/.
sys.path.insert(0, str(BENCH))
answers = load("answers")
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
            "view trips\n  buckets:  1 hour\n  holds:    1000 cells and 1 bound cell\n"
            "\nview dropoffs\n  holds:    1 cell\n\ndim public.zones\n  holds:    265 rows\n"
        )
        self.assertEqual(retention.cells(views), 1001)


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=1).result
    sys.exit(0 if result.wasSuccessful() else 1)
