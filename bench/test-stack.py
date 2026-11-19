#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = []
# ///
"""Tests of how the bench scripts name and find their Compose projects.

bench/stack.py and bench/try_your_data.py each carry the same functions,
since try_your_data.py runs from a URL alone; every test runs against
both. They read `docker compose` output handed to them here, so they
need no Docker.
"""

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


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=1).result
    sys.exit(0 if result.wasSuccessful() else 1)
