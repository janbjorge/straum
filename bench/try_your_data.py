#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = []
#
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Straum on a copy of your tables, in Docker.

Loads a copy made by `straum plan --copy` into a throwaway PostgreSQL
with logical decoding on, or at wal_level replica with --wal physical,
applies `straum plan` for the spec and the WAL, starts `straum run`
from the image and waits until it has loaded. Then it prints how to
ask over HTTP and reach PostgreSQL, and opens `straum shell`. Leaving
the shell removes both containers.

--data is the directory `straum plan --copy` wrote, after its
export.sql has run on the source:

    straum plan usage.sql --copy copy
    psql "$SOURCE" -f copy/export.sql

The directory may hold the tables of more specs than this one. Its
load.sql runs in the container, stops at the first error, and so does
this. Nothing here connects to the source.

Needs Docker and uv.
"""

import argparse
import json
import os
import pathlib
import re
import secrets
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
from types import TracebackType
from typing import Self

IMAGE = "ghcr.io/janbjorge/straum"
STACK_PREFIX = "straum-try-"
# Service names, which are also the host names on the stack's network.
STRAUM = "straum"
POSTGRES = "postgres"
SPEC_IN_IMAGE = "/spec/spec.sql"
COPY_IN_IMAGE = "/copy"
# What a directory from `straum plan --copy` holds once its export ran.
LOAD_SCRIPT = "load.sql"
SCHEMA_FILE = "schema.sql"
READY_POLL_S = 0.2
STRAUM_PORT = 8080
POSTGRES_PORT = 5432
# The tests' major, as the justfile's postgres setting says.
POSTGRES_MAJOR = os.environ.get("STRAUM_TEST_POSTGRES", "18")
# A throwaway copy: a crash means running the script again, so the load
# skips the fsyncs, timed checkpoints and autovacuum that keep a real
# server safe. wal_level is set by --wal, so the plan needs no restart.
# The load skips page images too, through ALTER SYSTEM, which a flag on
# the command line would outrank when --wal physical turns them back on.
POSTGRES_FLAGS = (
    "max_wal_size=8GB",
    "checkpoint_timeout=1h",
    "fsync=off",
    "synchronous_commit=off",
    "autovacuum=off",
    "maintenance_work_mem=512MB",
)
# The wal_level each --wal needs.
WAL_LEVELS = {"logical": "logical", "physical": "replica"}


def main() -> int:
    # A plain kill leaves through the stack's exit too, so its containers go.
    signal.signal(signal.SIGTERM, stop)
    args = parse()
    password = secrets.token_hex(16)
    with Stack(args.stack, compose_file(args, password)) as stack:
        run_stack(stack, args)
    return 0


def run_stack(stack: "Stack", args: argparse.Namespace) -> None:
    stack.pull(STRAUM, args.image)
    # Planned before the load: the plan needs no database, so a spec it
    # refuses stops the run at once.
    setup_sql = stack.run(STRAUM, "plan", SPEC_IN_IMAGE, "--wal", args.wal)
    log(f"starting PostgreSQL {args.postgres} in stack {stack.name}")
    stack.up(POSTGRES)
    project = stack.name
    run_sql(project, "ALTER SYSTEM SET full_page_writes = off")
    run_sql(project, "SELECT pg_reload_conf()")
    load(project, args.data)
    if args.wal == "physical":
        # The physical stream rebuilds rows from page images, which the
        # load skipped; after this checkpoint each page's next change
        # brings it whole.
        run_sql(project, "ALTER SYSTEM RESET full_page_writes")
        run_sql(project, "SELECT pg_reload_conf()")
        run_sql(project, "CHECKPOINT")
    applied = psql(project, "-q", "-v", "ON_ERROR_STOP=1", input_text=setup_sql)
    if applied.returncode:
        raise SystemExit("try: the plan's SQL failed, printed above")
    log("starting Straum")
    started = time.perf_counter()
    stack.up(STRAUM)
    port = wait_ready(stack)
    log(f"straum ready in {time.perf_counter() - started:.0f} s")
    print_next_steps(port, project)
    shell(project)
    log(f"removing stack {stack.name}")


def stop(signal_number: int, frame: object) -> None:
    raise SystemExit(f"try: stopped by signal {signal_number}")


def parse() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__.splitlines()[0], epilog="Needs Docker and uv."
    )
    parser.add_argument(
        "spec_file",
        nargs="?",
        type=existing,
        metavar="SPEC",
        help="the spec: one or more views (or --spec)",
    )
    parser.add_argument(
        "--spec", dest="spec_flag", type=existing, help="the spec, as a flag"
    )
    parser.add_argument(
        "--data",
        type=copy_directory,
        required=True,
        help="the directory straum plan --copy wrote, after its export.sql ran",
    )
    parser.add_argument(
        "--stack",
        help=f"Compose project to run in ({STACK_PREFIX}<spec file name>)",
    )
    parser.add_argument(
        "--postgres",
        default=POSTGRES_MAJOR,
        help=f"PostgreSQL major ({POSTGRES_MAJOR})",
    )
    parser.add_argument("--image", default=IMAGE, help=f"Straum image ({IMAGE})")
    parser.add_argument(
        "--wal",
        choices=sorted(WAL_LEVELS),
        default="logical",
        help="the WAL Straum reads: logical decoding's, or the physical WAL (logical)",
    )
    parser.add_argument(
        "--port",
        type=int,
        help="local port Straum answers on (a free one Docker picks)",
    )
    args = parser.parse_args()
    if args.spec_file and args.spec_flag:
        parser.error("give the spec once: as SPEC or as --spec, not both")
    args.spec = args.spec_file or args.spec_flag
    if args.spec is None:
        parser.error("the spec is required: SPEC or --spec")
    args.stack = stack_name(args.stack or STACK_PREFIX + args.spec.stem)
    return args


def stack_name(wanted: str) -> str:
    # Compose takes lowercase letters, digits, dashes and underscores.
    name = re.sub(r"[^a-z0-9_-]+", "-", wanted.lower()).strip("-_")
    if not name:
        raise SystemExit(f"try: {wanted!r} makes no stack name; pass --stack")
    return name


def existing(path: str) -> pathlib.Path:
    resolved = pathlib.Path(path).resolve()
    if not resolved.exists():
        raise argparse.ArgumentTypeError(f"{path} does not exist")
    return resolved


def copy_directory(path: str) -> pathlib.Path:
    """A directory from `straum plan --copy` whose export has run."""
    resolved = existing(path)
    if not (resolved / LOAD_SCRIPT).is_file():
        raise argparse.ArgumentTypeError(
            f"{path} has no {LOAD_SCRIPT}; make it with straum plan SPEC --copy {path}"
        )
    if not (resolved / SCHEMA_FILE).is_file():
        raise argparse.ArgumentTypeError(
            f"{path} has no {SCHEMA_FILE}; run its export.sql on the source first:"
            f' psql "$SOURCE" -f {path}/export.sql'
        )
    return resolved


def compose_file(args: argparse.Namespace, password: str) -> str:
    """The stack's Compose file, as JSON, which Compose reads as YAML."""
    settings = (f"wal_level={WAL_LEVELS[args.wal]}", *POSTGRES_FLAGS)
    flags = [part for flag in settings for part in ("-c", flag)]
    # Straum alone is published, on loopback only, since the copy holds
    # your rows; the load and psql reach PostgreSQL through
    # docker compose exec.
    published = {"target": STRAUM_PORT, "host_ip": "127.0.0.1"}
    if args.port:
        published["published"] = args.port
    services = {
        POSTGRES: {
            "image": f"postgres:{args.postgres}",
            "command": ["postgres", *flags],
            "environment": {"POSTGRES_PASSWORD": password},
            "shm_size": "1g",
            "volumes": [f"{args.data}:{COPY_IN_IMAGE}:ro"],
            # Over TCP: while the image initialises, its server listens on
            # the socket alone, and pg_isready there would pass too soon.
            "healthcheck": {
                "test": ["CMD", "pg_isready", "-h", "127.0.0.1", "-U", "postgres"],
                "interval": "1s",
                "retries": 120,
            },
        },
        STRAUM: {
            "image": args.image,
            # Every address in the container: unless told, Straum serves its
            # own loopback, which the published port never reaches.
            "command": [
                "run",
                SPEC_IN_IMAGE,
                "--wal",
                args.wal,
                "--listen",
                f"0.0.0.0:{STRAUM_PORT}",
            ],
            "environment": {"STRAUM_DSN": dsn(password)},
            "volumes": [f"{args.spec}:{SPEC_IN_IMAGE}:ro"],
            "ports": [published],
        },
    }
    return json.dumps({"name": args.stack, "services": services})


class Stack:
    """One Compose project: entering removes what a killed run of it left,
    leaving removes its containers, volumes and network."""

    def __init__(self, name: str, compose: str) -> None:
        self.name = name
        self.compose = compose

    def __enter__(self) -> Self:
        self.down()
        return self

    def __exit__(
        self,
        kind: type[BaseException] | None,
        error: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self.down()

    def pull(self, service: str, image: str) -> None:
        # Pull on every run, since a tag such as latest moves. A local tag,
        # such as one built from this repository, has nothing to pull and is
        # used as it is; a missing image fails at the first run.
        log(f"pulling {image}")
        if self.compose_file("pull", service).returncode:
            log(f"pull failed; using the local {image}")

    def up(self, service: str) -> None:
        # --wait waits for the health check, where the service has one.
        started = self.compose_file("up", "--detach", "--wait", service)
        if started.returncode:
            raise SystemExit(f"try: {service} did not start\n{started.stderr}")

    def run(self, service: str, *command: str) -> str:
        """The stdout of `command` in a one-off container of `service`,
        alone: no dependencies started and no ports published."""
        ran = self.compose_file("run", "--rm", "--no-deps", "-T", service, *command)
        if ran.returncode:
            raise SystemExit(f"try: {' '.join(command)} failed\n{ran.stderr}")
        return ran.stdout

    def down(self) -> None:
        # Compose finds a project's parts by its name, with no file.
        compose(self.name, "down", "--volumes", "--remove-orphans", capture=True)

    def compose_file(self, *args: str) -> subprocess.CompletedProcess[str]:
        return compose(
            self.name, "-f", "-", *args, input_text=self.compose, capture=True
        )


def load(project: str, data: pathlib.Path) -> None:
    """Runs the copy's load.sql in the PostgreSQL service. Its lines, each
    table as it starts and how many rows it took, print as they come; its
    first error stops it, rolls the load back, and stops the run."""
    log(f"loading {data}")
    started = time.perf_counter()
    loaded = psql(
        project,
        "-v",
        f"dir={COPY_IN_IMAGE}",
        "-f",
        f"{COPY_IN_IMAGE}/{LOAD_SCRIPT}",
    )
    if loaded.returncode:
        raise SystemExit(f"try: {LOAD_SCRIPT} failed, printed above")
    log(f"loaded in {time.perf_counter() - started:.0f} s")


def wait_ready(stack: Stack) -> int:
    """The local port Straum answers on, once it has loaded."""
    published = compose(stack.name, "port", STRAUM, str(STRAUM_PORT), capture=True)
    _, _, port_text = published.stdout.strip().splitlines()[0].rpartition(":")
    port = int(port_text)
    # There is no timeout, since the load takes as long as the tables need.
    url = f"http://127.0.0.1:{port}/readyz"
    # Straum stays up when its setup check fails and tries again, so the
    # wait says why it isn't ready each time the reason changes.
    said = ""
    while True:
        is_ready, why = readiness(url)
        if is_ready:
            return port
        if why and why != said:
            print(f"try: {why}", file=sys.stderr)
            said = why
        running = compose(
            stack.name, "ps", "--status", "running", "--quiet", STRAUM, capture=True
        )
        if not running.stdout.strip():
            logs = compose(stack.name, "logs", "--no-log-prefix", STRAUM, capture=True)
            raise SystemExit(f"try: Straum stopped\n{logs.stdout}{logs.stderr}")
        time.sleep(READY_POLL_S)


def readiness(url: str) -> tuple[bool, str]:
    """Whether /readyz answers 200, and the reason it gives when it doesn't:
    empty while nothing answers yet."""
    try:
        with urllib.request.urlopen(url) as response:
            return response.status == 200, ""
    except urllib.error.HTTPError as error:
        with error:
            return False, error.read().decode(errors="replace").strip()
    except (urllib.error.URLError, ConnectionError):
        return False, ""


def print_next_steps(port: int, project: str) -> None:
    print(
        f"""
Ask:        curl --json '{{"query": "SELECT <keys>, <measures> FROM <view>
              WHERE bucket >= $1 AND bucket < $2 GROUP BY <keys>",
              "parameters": ["<from>", "<to>"]}}' http://127.0.0.1:{port}/ask
PostgreSQL: docker compose -p {project} exec postgres psql -U postgres

Opening straum shell. Leaving it removes both containers.
""",
        flush=True,
    )


def shell(project: str) -> None:
    # A terminal only on a terminal, so a piped stdin still reaches the shell.
    tty = [] if sys.stdin.isatty() else ["-T"]
    subprocess.run(
        ["docker", "compose", "-p", project, "exec", *tty, STRAUM, "/straum", "shell"],
        check=False,
    )


def run_sql(project: str, sql: str) -> None:
    """Runs one statement, which ALTER SYSTEM must be alone in, or fails."""
    ran = psql(project, "-q", "-v", "ON_ERROR_STOP=1", "-c", sql)
    if ran.returncode:
        raise SystemExit(f"try: {sql} failed, printed above")


def psql(
    project: str, *args: str, input_text: str | None = None
) -> subprocess.CompletedProcess[str]:
    """psql in the PostgreSQL service, as its superuser."""
    return compose(
        project,
        "exec",
        "-T",
        POSTGRES,
        "psql",
        "-U",
        "postgres",
        *args,
        input_text=input_text,
    )


def dsn(password: str) -> str:
    return f"postgres://straum:{password}@{POSTGRES}:{POSTGRES_PORT}/postgres?sslmode=disable"


def compose(
    project: str,
    *args: str,
    capture: bool = False,
    input_text: str | None = None,
) -> subprocess.CompletedProcess[str]:
    # No input reads as empty: compose exec forwards stdin, which belongs
    # to the shell at the end.
    return subprocess.run(
        ["docker", "compose", "-p", project, *args],
        check=False,
        text=True,
        capture_output=capture,
        input=input_text or "",
    )


def log(message: str) -> None:
    print(f"try: {message}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    sys.exit(main())
