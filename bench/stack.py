"""A Docker Compose project, the stack, for the bench scripts.

Each run is one project named after its script and process,
`<tool>-pid<pid>`, so runs side by side get their own network,
container names and ports. A run killed too hard to clean up, by
SIGKILL, leaves its project behind; the next run of the same tool
removes each such project whose process is gone. The
Compose file is built here as JSON, which Compose reads as YAML, and
goes to `docker compose` on stdin. Every Docker command goes through
`docker compose`. Leaving the stack removes its containers, volumes and
network, on an error or SIGTERM too.

bench/try_your_data.py carries its own copy, since it runs from a URL
where this file is not beside it.

A bench that builds Straum builds it with `build_environment`, so the
binary names the commit it is built from, as a release names its tag. A
bench that starts Straum gives it `run_environment`, so the caller's
STRAUM_* and PG* variables do not change the run.
"""

import contextlib
import json
import os
import pathlib
import re
import signal
import subprocess
from collections.abc import AsyncIterator, Mapping
from types import TracebackType
from typing import TYPE_CHECKING, Any, Self

if TYPE_CHECKING:
    import asyncpg

POSTGRES = "postgres"
POSTGRES_PORT = 5432
# The PostgreSQL image the benches run: the major the tests run, as the
# justfile's postgres setting says.
POSTGRES_IMAGE = f"postgres:{os.environ.get('STRAUM_TEST_POSTGRES', '18')}"


def postgres_service(
    image: str, password: str, settings: Mapping[str, str], **extra: Any
) -> dict[str, Any]:
    """PostgreSQL with `settings` on its command line, published on
    loopback at a port Docker picks."""
    flags = [
        part for name, value in settings.items() for part in ("-c", f"{name}={value}")
    ]
    return {
        "image": image,
        "command": ["postgres", *flags],
        "environment": {"POSTGRES_PASSWORD": password},
        "ports": [{"target": POSTGRES_PORT, "host_ip": "127.0.0.1"}],
        # Over TCP: while the image initialises, its server listens on the
        # socket alone, and pg_isready there would pass too soon.
        "healthcheck": {
            "test": ["CMD", "pg_isready", "-h", "127.0.0.1", "-U", "postgres"],
            "interval": "1s",
            "retries": 120,
        },
        **extra,
    }


class Stack:
    """One Compose project for this run: entering removes what killed runs
    of the same tool left, leaving removes all of this run's."""

    def __init__(self, tool: str, services: Mapping[str, Mapping[str, Any]]) -> None:
        self.tool = tool
        self.name = run_name(tool, os.getpid())
        self.compose = json.dumps({"name": self.name, "services": services})

    def __enter__(self) -> Self:
        signal.signal(signal.SIGTERM, stopped)
        listed = subprocess.run(
            ["docker", "compose", "ls", "--all", "--format", "json"],
            check=False,
            text=True,
            capture_output=True,
        )
        for name in leftover_projects(listed.stdout, self.tool):
            compose(name, "down", "--volumes", "--remove-orphans")
        return self

    def __exit__(
        self,
        kind: type[BaseException] | None,
        error: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self.down()

    def up(self, service: str) -> None:
        # --wait waits for the health check, where the service has one.
        started = self.compose_command("up", "--detach", "--wait", service)
        if started.returncode:
            raise SystemExit(f"{service} did not start\n{started.stderr}")

    def pull(self, service: str) -> None:
        pulled = self.compose_command("pull", service)
        if pulled.returncode:
            raise SystemExit(f"{service} did not pull\n{pulled.stderr}")

    def run(self, service: str, *command: str) -> str:
        """The stdout of `command` in a one-off container of `service`,
        alone: no dependencies started and no ports published."""
        ran = self.compose_command("run", "--rm", "--no-deps", "-T", service, *command)
        if ran.returncode:
            raise SystemExit(f"{service} {' '.join(command)} failed\n{ran.stderr}")
        return ran.stdout

    def stop(self, service: str) -> None:
        self.compose_command("rm", "--stop", "--force", service)

    def down(self) -> None:
        # Compose finds a project's parts by its name, with no file.
        compose(self.name, "down", "--volumes", "--remove-orphans")

    def port(self, service: str, target: int) -> int:
        """The loopback port Docker published for `target`."""
        published = compose(self.name, "port", service, str(target))
        port = published_port(published.stdout)
        if port is None:
            raise SystemExit(
                f"{service} published no port for {target}"
                f"\n{published.stderr}{self.logs(service)}"
            )
        return port

    def running(self, service: str) -> bool:
        listed = compose(self.name, "ps", "--status", "running", "--quiet", service)
        return bool(listed.stdout.strip())

    def logs(self, service: str) -> str:
        out = compose(self.name, "logs", "--no-log-prefix", service)
        return out.stdout + out.stderr

    def exec_args(self, service: str, *command: str) -> list[str]:
        """`command` in `service` with no terminal, for a caller that pipes
        into it."""
        return ["docker", "compose", "-p", self.name, "exec", "-T", service, *command]

    def compose_command(self, *args: str) -> subprocess.CompletedProcess[str]:
        return compose(self.name, "-f", "-", *args, input_text=self.compose)

    def load_taxi_trips(self, months: int, database: str = "postgres") -> None:
        """Pipe `months` months of taxi trips from bench/load_taxi_trips.py
        into psql, in `database` of PostgreSQL."""
        bench = pathlib.Path(__file__).resolve().parent
        loader = subprocess.Popen(
            [bench / "load_taxi_trips.py", "--months", str(months), "--data", bench / "data"],
            stdout=subprocess.PIPE,
        )
        stdout = loader.stdout
        if stdout is None:
            raise SystemExit("load_taxi_trips.py has no stdout")
        # Closed when psql fails too, so that the loader stops.
        with stdout:
            psql = self.exec_args(POSTGRES, "psql", "-U", "postgres", "-q", "-d", database)
            subprocess.run(psql, stdin=stdout, check=True, stdout=subprocess.DEVNULL)
        if loader.wait():
            raise SystemExit("load_taxi_trips.py failed")


@contextlib.asynccontextmanager
async def connected(url: str) -> AsyncIterator[asyncpg.Connection]:
    """A connection to PostgreSQL at `url`, in New York's zone, closed on
    the way out."""
    # Imported here, so that bench/test-stack.py loads this file without it.
    import asyncpg

    pg = await asyncpg.connect(url, server_settings={"timezone": "America/New_York"})
    try:
        yield pg
    finally:
        await pg.close()


def git_version() -> str | None:
    """The commit this checkout is at, as `git describe` names it: the
    last tag, the commits since and the hash, `-dirty` with changes not
    committed. None without git or a repository."""
    try:
        described = subprocess.run(
            ["git", "describe", "--tags", "--always", "--dirty"],
            cwd=pathlib.Path(__file__).resolve().parent,
            capture_output=True,
            text=True,
            check=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return None
    return described.stdout.strip() or None


def build_environment(environ: Mapping[str, str], described: str | None) -> dict[str, str]:
    """`environ` for `cargo build`, with STRAUM_VERSION set to the commit
    `described` names, without its tag's leading `v`, as CI sets a
    release's. A STRAUM_VERSION already set wins; without a description
    the build keeps Cargo.toml's version."""
    built = dict(environ)
    if "STRAUM_VERSION" in built or described is None:
        return built
    built["STRAUM_VERSION"] = described.removeprefix("v")
    return built


def run_environment(environ: Mapping[str, str]) -> dict[str, str]:
    """`environ` for a Straum that a bench starts, without each STRAUM_*
    and PG* variable of the caller. An exported STRAUM_DSN_FILE wins over
    the bench's STRAUM_DSN, and points Straum at another database."""
    return {
        name: value
        for name, value in environ.items()
        if not name.startswith(("STRAUM_", "PG"))
    }


def run_name(tool: str, pid: int) -> str:
    """The project name of `tool`'s run in process `pid`."""
    return f"{tool}-pid{pid}"


def leftover_projects(listed: str, tool: str) -> list[str]:
    """The projects in `docker compose ls --format json` output that runs
    of `tool` left: named for a process that is gone."""
    pattern = re.compile(re.escape(tool) + r"-pid(\d+)")
    names = []
    for project in json.loads(listed or "[]"):
        match = pattern.fullmatch(project.get("Name", ""))
        if match is not None and not process_alive(int(match.group(1))):
            names.append(match.group(0))
    return names


def process_alive(pid: int) -> bool:
    """Whether process `pid` runs. One that can't be asked counts as
    running, so a project in doubt stays."""
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except (PermissionError, OverflowError):
        return True
    return True


def published_port(listed: str) -> int | None:
    """The port in `docker compose port` output, `host:port` per line, or
    None when there is none."""
    for line in listed.splitlines():
        _, colon, port = line.strip().rpartition(":")
        if colon and port.isdigit():
            return int(port)
    return None


def stopped(signal_number: int, frame: object) -> None:
    raise SystemExit(f"stopped by signal {signal_number}")


def compose(
    project: str, *args: str, input_text: str | None = None
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["docker", "compose", "-p", project, *args],
        check=False,
        text=True,
        capture_output=True,
        input=input_text,
    )
