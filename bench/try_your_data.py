#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = ["testcontainers"]
#
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Straum on a copy of your tables, in Docker.

Restores a pg_dump into a throwaway PostgreSQL with logical decoding on,
applies `straum plan` for the spec, starts `straum run` from the image
and waits until it has loaded. Then it prints how to ask over HTTP and
reach PostgreSQL, and opens `straum shell`. Leaving the shell removes
both containers. Nothing here connects to the database the dump came
from.

DUMP is any pg_dump output: a directory (--format=directory), a custom
or tar archive, or a plain .sql file; pg_restore -l tells them apart. A dump of a few tables (-t) lacks
the types, extensions and tables they depend on. Pass the whole schema,
from pg_dump --schema-only, as --schema: it is restored first, then the
dump's data with foreign keys and triggers off.

Needs Docker and uv.
"""

import argparse
import contextlib
import os
import pathlib
import secrets
import subprocess
import sys
import time
import urllib.error
import urllib.request
from collections.abc import Iterator, Mapping

from testcontainers.community.postgres import PostgresContainer
from testcontainers.core.container import DockerContainer
from testcontainers.core.network import Network

IMAGE = "ghcr.io/janbjorge/straum"
STRAUM = "straum-try"
POSTGRES = "straum-try-pg"
SPEC_IN_IMAGE = "/spec/spec.sql"
DUMP_IN_IMAGE = "/dump"
SCHEMA_IN_IMAGE = "/schema.sql"
READY_POLL_S = 0.2
STRAUM_PORT = 8080
POSTGRES_PORT = 5432
POSTGRES_MAJOR = "18"
RESTORE_JOBS = 4
# A throwaway copy: a crash means running the script again, so the load
# skips the fsyncs, page images, timed checkpoints and autovacuum that
# keep a real server safe. wal_level is set here, so the plan needs no
# restart.
POSTGRES_FLAGS = (
    "-c wal_level=logical -c max_wal_size=8GB -c checkpoint_timeout=1h"
    " -c fsync=off -c synchronous_commit=off -c full_page_writes=off"
    " -c autovacuum=off"
)
# The data goes in as a replica's would, with no foreign key checks and
# no triggers, since the schema names tables the dump may lack.
AS_REPLICA = "-c session_replication_role=replica"


def main() -> int:
    args = parse()
    pull(args.image)
    remove_containers()
    password = secrets.token_hex(16)
    log(f"starting PostgreSQL {args.postgres}")
    with Network() as network, postgres_container(args, password, network):
        restore(args.dump, args.schema, args.jobs)
        psql("-c", "ANALYZE")
        apply_plan(args.image, password, args.spec)
        with straum_running(args, password, network):
            print_next_steps(args.port)
            shell()
        log("removing the containers")
    return 0


def parse() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__.splitlines()[0], epilog="Needs Docker and uv."
    )
    parser.add_argument("spec", type=existing, help="the spec: one or more views")
    parser.add_argument("dump", type=existing, help="pg_dump output to restore")
    parser.add_argument(
        "--schema",
        type=existing,
        help="pg_dump --schema-only output, restored before DUMP's data",
    )
    parser.add_argument(
        "--postgres",
        default=POSTGRES_MAJOR,
        help=f"PostgreSQL major, at least the dump's ({POSTGRES_MAJOR})",
    )
    parser.add_argument("--image", default=IMAGE, help=f"Straum image ({IMAGE})")
    parser.add_argument(
        "--port",
        type=int,
        default=STRAUM_PORT,
        help=f"local port Straum answers on ({STRAUM_PORT})",
    )
    parser.add_argument(
        "--jobs",
        type=int,
        default=RESTORE_JOBS,
        help=f"pg_restore jobs for a directory or custom dump ({RESTORE_JOBS})",
    )
    return parser.parse_args()


def existing(path: str) -> pathlib.Path:
    resolved = pathlib.Path(path).resolve()
    if not resolved.exists():
        raise argparse.ArgumentTypeError(f"{path} does not exist")
    return resolved


def pull(image: str) -> None:
    # Pull on every run, since a tag such as latest moves. A local tag,
    # such as one built from this repository, has nothing to pull and is
    # used as it is.
    log(f"pulling {image}")
    if docker("pull", image, capture=True).returncode == 0:
        return
    if docker("image", "inspect", image, capture=True).returncode:
        raise SystemExit(f"try: no image {image}, here or in its registry")
    log(f"pull failed; using the local {image}")


def remove_containers() -> None:
    # A run killed before its cleanup leaves these behind, holding the names.
    docker("rm", "-f", STRAUM, POSTGRES, capture=True)


def postgres_container(
    args: argparse.Namespace, password: str, network: Network
) -> PostgresContainer:
    container = (
        PostgresContainer(
            f"postgres:{args.postgres}",
            username="postgres",
            password=password,
            dbname="postgres",
            driver=None,
        )
        .with_name(POSTGRES)
        .with_command(POSTGRES_FLAGS)
        .with_network(network)
        .with_network_aliases(POSTGRES)
        .with_volume_mapping(str(args.dump), DUMP_IN_IMAGE, "ro")
        .with_kwargs(shm_size="1g")
    )
    # On loopback only, at a port Docker picks, since the copy holds your
    # rows; the restore and psql reach it through docker exec.
    container.ports[POSTGRES_PORT] = ("127.0.0.1",)
    if args.schema:
        container.with_volume_mapping(str(args.schema), SCHEMA_IN_IMAGE, "ro")
    return container


def restore(dump: pathlib.Path, schema: pathlib.Path | None, jobs: int) -> None:
    # A statement that fails, such as an extension the image lacks, is
    # printed and skipped; straum plan names a table the restore missed.
    if schema:
        log(f"restoring the schema from {schema.name}")
        psql("-q", "-f", SCHEMA_IN_IMAGE)
    options = AS_REPLICA if schema else ""
    log(f"restoring {dump.name}")
    format_name = archive_format()
    if format_name is None:
        psql("-q", "-f", DUMP_IN_IMAGE, options=options)
        return
    flags = ["--no-owner", "--no-privileges"]
    if schema:
        flags.append("--data-only")
    if format_name != "TAR":
        flags += ["--jobs", str(jobs)]
    restored = postgres_exec(
        "pg_restore",
        "-U",
        "postgres",
        "-d",
        "postgres",
        *flags,
        DUMP_IN_IMAGE,
        options=options,
    )
    if restored.returncode:
        log(f"{dump.name} restored with errors, printed above; going on")


def archive_format() -> str | None:
    """pg_restore's name for the dump's format, such as CUSTOM, or None for
    a plain .sql file, which psql reads."""
    listed = postgres_exec("pg_restore", "-l", DUMP_IN_IMAGE, capture=True)
    if listed.returncode:
        return None
    for line in listed.stdout.splitlines():
        _, found, format_name = line.partition("Format:")
        if found:
            return format_name.strip()
    return None


def apply_plan(image: str, password: str, spec: pathlib.Path) -> None:
    planned = docker(
        "run",
        "--rm",
        "-e",
        "STRAUM_DSN",
        "-v",
        f"{spec}:{SPEC_IN_IMAGE}:ro",
        image,
        "plan",
        SPEC_IN_IMAGE,
        env=os.environ | {"STRAUM_DSN": dsn(password)},
        capture=True,
    )
    if planned.returncode:
        raise SystemExit(f"try: straum plan refused the spec\n{planned.stderr}")
    applied = psql("-q", "-v", "ON_ERROR_STOP=1", input_text=planned.stdout)
    if applied.returncode:
        raise SystemExit("try: the plan's SQL failed, printed above")


@contextlib.contextmanager
def straum_running(
    args: argparse.Namespace, password: str, network: Network
) -> Iterator[None]:
    """`straum run` on the spec, entered once Straum has loaded; leaving
    removes the container."""
    log("starting Straum")
    container = (
        DockerContainer(args.image)
        .with_name(STRAUM)
        .with_network(network)
        .with_env("STRAUM_DSN", dsn(password))
        .with_volume_mapping(str(args.spec), SPEC_IN_IMAGE, "ro")
        .with_command(["run", SPEC_IN_IMAGE])
    )
    # On loopback only, since the copy holds your rows. with_bind_ports
    # takes no address; ports goes to docker-py, which takes one.
    container.ports[STRAUM_PORT] = ("127.0.0.1", args.port)
    started = time.perf_counter()
    with container:
        wait_ready(container, args.port)
        log(f"straum ready in {time.perf_counter() - started:.0f} s")
        yield


def wait_ready(container: DockerContainer, port: int) -> None:
    # There is no timeout, since the load takes as long as the tables need.
    url = f"http://127.0.0.1:{port}/readyz"
    while not ready(url):
        wrapped = container.get_wrapped_container()
        wrapped.reload()
        if wrapped.status != "running":
            stdout, stderr = container.get_logs()
            logs = (stdout + stderr).decode(errors="replace")
            raise SystemExit(f"try: Straum stopped\n{logs}")
        time.sleep(READY_POLL_S)


def ready(url: str) -> bool:
    try:
        with urllib.request.urlopen(url) as response:
            return response.status == 200
    except (urllib.error.URLError, ConnectionError):
        return False


def print_next_steps(port: int) -> None:
    print(
        f"""
Ask:        curl --json '{{"query": "SELECT <keys>, <measures> FROM <view>
              WHERE bucket >= $1 AND bucket < $2 GROUP BY <keys>",
              "parameters": ["<from>", "<to>"]}}' http://127.0.0.1:{port}/ask
PostgreSQL: docker exec -it {POSTGRES} psql -U postgres

Opening straum shell. Leaving it removes both containers.
""",
        flush=True,
    )


def shell() -> None:
    # -t only on a terminal, so a piped stdin still reaches the shell.
    tty = ["-t"] if sys.stdin.isatty() else []
    docker("exec", "-i", *tty, STRAUM, "/straum", "shell")


def psql(
    *args: str, options: str = "", input_text: str | None = None
) -> subprocess.CompletedProcess[str]:
    return postgres_exec(
        "psql", "-U", "postgres", *args, options=options, input_text=input_text
    )


def postgres_exec(
    *command: str,
    options: str = "",
    input_text: str | None = None,
    capture: bool = False,
) -> subprocess.CompletedProcess[str]:
    """`command` in the PostgreSQL container; `options` become PGOPTIONS,
    the settings of each connection it makes."""
    flags = ["-e", f"PGOPTIONS={options}"]
    if input_text is not None:
        flags.append("-i")
    return docker(
        "exec", *flags, POSTGRES, *command, input_text=input_text, capture=capture
    )


def dsn(password: str) -> str:
    return f"postgres://straum:{password}@{POSTGRES}:{POSTGRES_PORT}/postgres?sslmode=disable"


def docker(
    *args: str,
    capture: bool = False,
    env: Mapping[str, str] | None = None,
    input_text: str | None = None,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["docker", *args],
        check=False,
        text=True,
        capture_output=capture,
        env=env,
        input=input_text,
    )


def log(message: str) -> None:
    print(f"try: {message}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    sys.exit(main())
