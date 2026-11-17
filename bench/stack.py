"""A Docker Compose project, the stack, for the bench scripts.

Each run is one project named after its script and process, so runs
side by side get their own network, container names and ports. The
Compose file is built here as JSON, which Compose reads as YAML, and
goes to `docker compose` on stdin. Every Docker command goes through
`docker compose`. Leaving the stack removes its containers, volumes and
network, on an error or SIGTERM too.

bench/try_your_data.py carries its own copy, since it runs from a URL
where this file is not beside it.
"""

import json
import os
import signal
import subprocess
from collections.abc import Mapping
from types import TracebackType
from typing import Any, Self

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
    """One Compose project: entering removes what a killed run left under
    the same name, leaving removes all of it."""

    def __init__(self, tool: str, services: Mapping[str, Mapping[str, Any]]) -> None:
        self.name = f"{tool}-{os.getpid()}"
        self.compose = json.dumps({"name": self.name, "services": services})

    def __enter__(self) -> Self:
        signal.signal(signal.SIGTERM, stopped)
        self.down()
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
        return int(published.stdout.splitlines()[0].rpartition(":")[2])

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
