#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = ["botocore==1.40.76"]
# ///
"""Check the deployments under deploy/ besides the chart: Compose reads
both compose files, the ECS task definition fits the shape ECS's API
takes, and systemd-analyze verifies the unit in a Debian container."""

import json
import os
import pathlib
import subprocess
import sys
import tempfile

import botocore.session
import botocore.validate


ROOT = pathlib.Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy"
RELEASE = "1.2.3"
# systemd in Debian 13, the distroless base's release, pinned by digest.
SYSTEMD_IMAGE = (
    "debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2"
)


class CheckFailed(Exception):
    """A check that found the file wrong, with what it said."""


def check_compose_reads(compose: pathlib.Path, environment: dict[str, str]) -> None:
    """Compose reads `compose` with `environment` set, and refuses it
    without, since each file names a required variable."""
    with tempfile.TemporaryDirectory() as directory:
        for name in ["dsn", "spec.sql", "global-bundle.pem"]:
            (pathlib.Path(directory) / name).touch()
        command = [
            "docker", "compose", "-f", str(compose),
            "--project-directory", directory, "config", "--quiet",
        ]
        with_variables = os.environ | environment
        subprocess.run(command, check=True, env=with_variables)
        without = {k: v for k, v in os.environ.items() if k not in environment}
        refused = subprocess.run(command, env=without, capture_output=True)
        if refused.returncode == 0:
            raise CheckFailed(f"{compose}: read without {', '.join(environment)}")


def check_task_definition(path: pathlib.Path) -> None:
    """The task definition is what RegisterTaskDefinition takes."""
    model = botocore.session.get_session().get_service_model("ecs")
    shape = model.operation_model("RegisterTaskDefinition").input_shape
    report = botocore.validate.ParamValidator().validate(json.loads(path.read_text()), shape)
    if report.has_errors():
        raise CheckFailed(f"{path}:\n{report.generate_report()}")


def check_systemd_unit(unit: pathlib.Path) -> None:
    """systemd-analyze verifies the unit, with a stand-in binary and
    credential where it looks for them. It exits 0 on an unknown key and
    only warns, so any line it prints fails the check."""
    script = (
        "apt-get update -qq >/dev/null 2>&1"
        " && apt-get install -qq -y systemd >/dev/null 2>&1"
        " && install -D -m 755 /bin/true /usr/local/bin/straum"
        " && install -D -m 600 /dev/null /etc/straum/dsn"
        f" && systemd-analyze verify /check/{unit.name} 2>&1"
    )
    project = {
        "services": {
            "verify": {
                "image": SYSTEMD_IMAGE,
                "environment": {"DEBIAN_FRONTEND": "noninteractive"},
                "volumes": [f"{unit.parent}:/check:ro"],
                "command": ["sh", "-c", script],
            }
        }
    }
    name = f"straum-check-deploy-{os.getpid()}"
    try:
        verified = subprocess.run(
            ["docker", "compose", "-p", name, "-f", "-", "run", "--rm", "-T", "verify"],
            input=json.dumps(project).encode(),
            stdout=subprocess.PIPE,
            check=True,
        )
        said = verified.stdout.decode().strip()
        if said:
            raise CheckFailed(f"{unit}:\n{said}")
    finally:
        subprocess.run(
            ["docker", "compose", "-p", name, "-f", "-", "down"],
            input=json.dumps(project).encode(),
            capture_output=True,
        )


def main() -> int:
    try:
        check_compose_reads(
            DEPLOY / "compose" / "compose.yaml", {"STRAUM_VERSION": RELEASE}
        )
        check_compose_reads(
            DEPLOY / "ecs" / "compose.yaml",
            {"STRAUM_VERSION": RELEASE, "STRAUM_ECS_IMAGE": "registry.example/straum:1"},
        )
        check_task_definition(DEPLOY / "ecs" / "task-definition.json")
        check_systemd_unit(DEPLOY / "systemd" / "straum.service")
    except subprocess.CalledProcessError as e:
        print(f"check-deploy: {' '.join(e.cmd)} failed", file=sys.stderr)
        return 1
    except CheckFailed as e:
        print(f"check-deploy: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
