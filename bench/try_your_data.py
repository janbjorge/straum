#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = []
#
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Straum on a copy of your tables, in Docker.

Restores a pg_dump into a throwaway PostgreSQL with logical decoding on,
or at wal_level replica with --wal physical, applies `straum plan` for
the spec and the WAL, starts `straum run` from the image
and waits until it has loaded. Then it prints how to ask over HTTP and
reach PostgreSQL, and opens `straum shell`. Leaving the shell removes
both containers.

Without --data, the tables the spec reads are copied from the database
the libpq variables of this shell name (PGHOST, PGUSER, PGPASSWORD and
the rest): their definitions from pg_dump --schema-only and their rows
as COPY, into a temporary file that is restored and removed when the
run ends. COPY reads only the tables, so a read-only role can make it.
Those are the only connections to that database.

--data is any pg_dump output: a directory (--format=directory), a
custom or tar archive, or a plain .sql file; pg_restore -l tells them
apart. A dump of a few tables (-t) lacks the types, extensions and
tables they depend on. Pass the whole schema, from pg_dump
--schema-only, as --schema: it is restored first, then only the data
of the tables the spec reads, with foreign keys and triggers off.
Their indexes, other than primary keys and unique constraints, are
dropped before the data and built again after it, in parallel.

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
import tempfile
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from types import TracebackType
from typing import Self

IMAGE = "ghcr.io/janbjorge/straum"
STACK_PREFIX = "straum-try-"
# Service names, which are also the host names on the stack's network.
STRAUM = "straum"
POSTGRES = "postgres"
SPEC_IN_IMAGE = "/spec/spec.sql"
DUMP_IN_IMAGE = "/dump"
SCHEMA_IN_IMAGE = "/schema.sql"
READY_POLL_S = 0.2
STRAUM_PORT = 8080
POSTGRES_PORT = 5432
# The tests' major, as the justfile's postgres setting says.
POSTGRES_MAJOR = os.environ.get("STRAUM_TEST_POSTGRES", "18")
RESTORE_JOBS = 4
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
# The data goes in as a replica's would, with no foreign key checks and
# no triggers, since the schema names tables the dump may lack.
AS_REPLICA = "-c session_replication_role=replica"
# psql's field separator for INDEXES_SQL: the unit separator, which no
# index name or definition holds.
FIELD_SEPARATOR = "\x1f"
# The indexes on the spec's tables that back no primary key, unique or
# exclusion constraint. COPY into a table updates each of its indexes a
# row at a time; building one after the load sorts the table once, which
# on millions of rows is many times faster. Constraints stay, since
# foreign keys of other tables may hang on them.
INDEXES_SQL = """
SELECT i.indexrelid::regclass::text, pg_get_indexdef(i.indexrelid)
FROM pg_index AS i
WHERE i.indrelid = ANY (ARRAY[{tables}]::regclass[])
  AND NOT EXISTS (
    SELECT FROM pg_constraint AS c
    WHERE c.conindid = i.indexrelid AND c.contype IN ('p', 'u', 'x'))
"""


def main() -> int:
    # A plain kill leaves through the stack's exit too, so its containers go.
    signal.signal(signal.SIGTERM, stop)
    args = parse()
    password = secrets.token_hex(16)
    with tempfile.TemporaryDirectory() as work:
        dump = args.data or touch(pathlib.Path(work) / "source.sql")
        with Stack(args.stack, compose_file(args, password, dump)) as stack:
            run_stack(stack, args, dump)
        return 0


def run_stack(stack: "Stack", args: argparse.Namespace, dump: pathlib.Path) -> None:
    stack.pull(STRAUM, args.image)
    # Planned before the restore: the plan needs no database, so a spec
    # it refuses stops the run at once, and it names the tables to
    # restore.
    setup_sql = stack.run(STRAUM, "plan", SPEC_IN_IMAGE, "--wal", args.wal)
    log(f"starting PostgreSQL {args.postgres} in stack {stack.name}")
    stack.up(POSTGRES)
    project = stack.name
    run_sql(project, "ALTER SYSTEM SET full_page_writes = off")
    run_sql(project, "SELECT pg_reload_conf()")
    tables = spec_tables(setup_sql)
    if args.data is None:
        dump_source(dump, tables)
    restore(project, dump, args.schema, args.jobs, tables)
    psql(project, "-c", "ANALYZE")
    if args.wal == "physical":
        # The physical stream rebuilds rows from page images, which the
        # restore skipped; after this checkpoint each page's next
        # change brings it whole.
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
        type=existing,
        help="pg_dump output to restore (default: copy from the libpq source)",
    )
    parser.add_argument(
        "--schema",
        type=existing,
        help="pg_dump --schema-only output, restored before --data",
    )
    parser.add_argument(
        "--stack",
        help=f"Compose project to run in ({STACK_PREFIX}<spec file name>)",
    )
    parser.add_argument(
        "--postgres",
        default=POSTGRES_MAJOR,
        help=f"PostgreSQL major, at least the dump's ({POSTGRES_MAJOR})",
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
    parser.add_argument(
        "--jobs",
        type=int,
        default=RESTORE_JOBS,
        help=f"pg_restore jobs and index builds at a time ({RESTORE_JOBS})",
    )
    args = parser.parse_args()
    if args.schema and not args.data:
        parser.error("--schema goes with --data")
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


def compose_file(args: argparse.Namespace, password: str, dump: pathlib.Path) -> str:
    """The stack's Compose file, as JSON, which Compose reads as YAML."""
    volumes = [f"{dump}:{DUMP_IN_IMAGE}:ro"]
    if args.schema:
        volumes.append(f"{args.schema}:{SCHEMA_IN_IMAGE}:ro")
    settings = (f"wal_level={WAL_LEVELS[args.wal]}", *POSTGRES_FLAGS)
    flags = [part for flag in settings for part in ("-c", flag)]
    # Straum alone is published, on loopback only, since the copy holds
    # your rows; the restore and psql reach PostgreSQL through
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
            "volumes": volumes,
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


def restore(
    project: str,
    dump: pathlib.Path,
    schema: pathlib.Path | None,
    jobs: int,
    tables: list[str],
) -> None:
    # A statement that fails, such as an extension the image lacks, is
    # printed and skipped; straum plan names a table the restore missed.
    if not schema:
        restore_data(project, dump, schema, jobs, tables)
        return
    log(f"restoring the schema from {schema.name}")
    psql(project, "-q", "-f", SCHEMA_IN_IMAGE)
    definitions = drop_indexes(project, tables)
    started = time.perf_counter()
    restore_data(project, dump, schema, jobs, tables)
    log(f"data restored in {time.perf_counter() - started:.0f} s")
    started = time.perf_counter()
    build_indexes(project, definitions, jobs)
    log(f"indexes built in {time.perf_counter() - started:.0f} s")


def touch(path: pathlib.Path) -> pathlib.Path:
    """An empty file at `path`, so Compose mounts a file, not a directory."""
    path.touch()
    return path


def dump_source(dump: pathlib.Path, tables: list[str]) -> None:
    """Writes the spec's tables from the libpq source of this shell into
    `dump`, which is mounted in the PostgreSQL service: the definitions of
    their schemas from pg_dump --schema-only, then each table's rows as COPY.

    pg_dump -t leaves out the types the tables use, so the schemas are
    dumped whole, as definitions only. pg_dump's data dump also reads each
    owned sequence, which a read-only role may not; COPY ... TO STDOUT
    reads the table alone."""
    log(f"dumping {', '.join(tables)} from the source")
    schemas = sorted({table.rsplit(".", 1)[0] for table in tables})
    # -e takes the extensions too: an index may use one, such as pg_trgm's.
    flags = [part for name in schemas for part in ("-n", name)] + ["-e", "*"]
    # The same inode the container mounts, so the file is written in place.
    with dump.open("w") as sql:
        # The dump's rows may name rows of tables it doesn't carry, so the
        # restore's foreign keys are not checked, as a replica's are not.
        sql.write("SET session_replication_role = replica;\n")
        sql.flush()
        definitions = subprocess.run(
            ["pg_dump", "--schema-only", "--no-owner", "--no-privileges", *flags],
            stdout=sql,
            check=False,
        )
        if definitions.returncode:
            raise SystemExit("try: pg_dump failed, printed above")
        for table in tables:
            sql.write(f"COPY {table} FROM stdin;\n")
            sql.flush()
            rows = subprocess.run(
                ["psql", "-X", "-q", "-c", f"COPY {table} TO STDOUT"],
                stdout=sql,
                check=False,
            )
            if rows.returncode:
                raise SystemExit(f"try: copying {table} failed, printed above")
            sql.write("\\.\n")


def restore_data(
    project: str,
    dump: pathlib.Path,
    schema: pathlib.Path | None,
    jobs: int,
    tables: list[str],
) -> None:
    options = AS_REPLICA if schema else ""
    format_name = archive_format(project)
    if format_name is None:
        log(f"restoring {dump.name}")
        psql(project, "-q", "-f", DUMP_IN_IMAGE, options=options)
        return
    flags = ["--no-owner", "--no-privileges"]
    if schema:
        # The schema has every table, so only the data the spec reads goes
        # in; the dump's other tables would only fill the disk.
        log(
            f"restoring {', '.join(bare_name(table) for table in tables)} from {dump.name}"
        )
        flags.append("--data-only")
        for table in tables:
            flags += ["--table", bare_name(table)]
    else:
        log(f"restoring {dump.name}")
    if format_name != "TAR":
        flags += ["--jobs", str(jobs)]
    restored = postgres_exec(
        project,
        "pg_restore",
        "-U",
        "postgres",
        "-d",
        "postgres",
        *flags,
        DUMP_IN_IMAGE,
        options=options,
    )
    if not restored.returncode:
        return
    log(f"{dump.name} restored with errors, printed above; going on")
    if not schema:
        log(
            "a dump of a few tables (pg_dump -t) lacks the types and tables"
            " they depend on; pass pg_dump --schema-only output as --schema"
        )


def drop_indexes(project: str, tables: list[str]) -> list[str]:
    """Drops the spec tables' indexes that back no constraint and returns
    their definitions, to build again after the data."""
    literals = ", ".join("'" + table.replace("'", "''") + "'" for table in tables)
    listed = postgres_exec(
        project,
        "psql",
        "-U",
        "postgres",
        "-At",
        "-F",
        FIELD_SEPARATOR,
        "-c",
        INDEXES_SQL.format(tables=literals),
        capture=True,
    )
    if listed.returncode:
        raise SystemExit(f"try: listing the indexes failed\n{listed.stderr}")
    names = []
    definitions = []
    for line in listed.stdout.splitlines():
        name, _, definition = line.partition(FIELD_SEPARATOR)
        names.append(name)
        definitions.append(definition)
    if not names:
        return definitions
    log(f"dropping {len(names)} indexes until the data is in")
    psql(project, "-q", "-c", f"DROP INDEX {', '.join(names)}")
    return definitions


def build_indexes(project: str, definitions: list[str], jobs: int) -> None:
    if not definitions:
        return
    log(f"building {len(definitions)} indexes, {jobs} at a time")
    with ThreadPoolExecutor(max_workers=jobs) as pool:
        builds = [
            pool.submit(psql, project, "-q", "-c", definition)
            for definition in definitions
        ]
    # A failed build is printed by psql; the copy still answers asks.
    failed = 0
    for build in builds:
        if build.result().returncode:
            failed += 1
    if failed:
        log(f"{failed} indexes failed to build, printed above; going on")


def archive_format(project: str) -> str | None:
    """pg_restore's name for the dump's format, such as CUSTOM, or None for
    a plain .sql file, which psql reads."""
    listed = postgres_exec(project, "pg_restore", "-l", DUMP_IN_IMAGE, capture=True)
    if listed.returncode:
        return None
    for line in listed.stdout.splitlines():
        _, found, format_name = line.partition("Format:")
        if found:
            return format_name.strip()
    return None


def spec_tables(setup_sql: str) -> list[str]:
    """The quoted names of the tables the spec reads, from the plan's GRANT
    SELECT line, such as
    GRANT SELECT ON "public"."tasks", "public"."taskgroups" TO "straum";"""
    grant = "GRANT SELECT ON "
    for line in setup_sql.splitlines():
        if not line.startswith(grant):
            continue
        names, _, _ = line.removeprefix(grant).partition(" TO ")
        return names.split(", ")
    raise SystemExit("try: straum plan grants no table; is the spec empty?")


def bare_name(table: str) -> str:
    """The table name without schema or quotes, which pg_restore's --table
    takes."""
    return table.rsplit(".", 1)[-1].strip('"')


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
    project: str, *args: str, options: str = "", input_text: str | None = None
) -> subprocess.CompletedProcess[str]:
    return postgres_exec(
        project,
        "psql",
        "-U",
        "postgres",
        *args,
        options=options,
        input_text=input_text,
    )


def postgres_exec(
    project: str,
    *command: str,
    options: str = "",
    input_text: str | None = None,
    capture: bool = False,
) -> subprocess.CompletedProcess[str]:
    """`command` in the PostgreSQL service; `options` become PGOPTIONS,
    the settings of each connection it makes."""
    return compose(
        project,
        "exec",
        "-T",
        "-e",
        f"PGOPTIONS={options}",
        POSTGRES,
        *command,
        input_text=input_text,
        capture=capture,
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
