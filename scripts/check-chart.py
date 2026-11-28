#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = []
# ///
"""Lint the Helm chart, run its unit tests, and check what it renders
against the schemas of the oldest and newest Kubernetes it targets.

The oldest is the floor of the chart's `kubeVersion` in Chart.yaml, so
the check moves with the chart; the newest is NEWEST_KUBERNETES. First
it checks that the helm command in the release notes of ci.yml sets each
value that a template marks required and values.yaml leaves empty."""

import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
CHART = ROOT / "deploy" / "helm" / "straum"
VALUES = CHART / "ci" / "values.yaml"
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
NEWEST_KUBERNETES = "1.36.0"
# kubeVersion: ">=1.30.0-0"; the "-0" lets pre-releases of the floor pass.
KUBE_VERSION_FLOOR = re.compile(
    r'^kubeVersion:\s*["\']?>=\s*v?(\d+\.\d+\.\d+)(?:-0)?["\']?\s*$',
    re.MULTILINE,
)


def oldest_kubernetes(chart_yaml: str) -> str | None:
    """Return the floor of a `kubeVersion: ">=X.Y.Z-0"` line, or None."""
    match = KUBE_VERSION_FLOOR.search(chart_yaml)
    if match is None:
        return None
    return match.group(1)


def release_command(workflow: str) -> str | None:
    """The helm command of the release notes in `workflow`, its
    continued lines too, or None."""
    match = re.search(r"helm upgrade --install(?:[^\n]*\\\n)*[^\n]*", workflow)
    return None if match is None else match.group(0)


def unset_in_release(command: str, templates: str, values: str) -> list[str]:
    """Each value that a template marks required and values.yaml leaves
    empty, but that `command` does not set."""
    given = {
        pair.split("=", 1)[0]
        for flag in re.findall(r"--set(?:-file|-string)?[= ](\S+)", command)
        for pair in flag.split(",")
    }
    required = re.findall(r'required\s+"[^"]*"\s+\.Values\.([\w.]+)', templates)
    unset = []
    for name in sorted(set(required)):
        leaf = name.rsplit(".", 1)[-1]
        empty = re.search(rf'^\s*{leaf}:\s*""\s*$', values, re.MULTILINE)
        if empty and name not in given:
            unset.append(name)
    return unset


def release_errors() -> list[str]:
    """What the helm command of the release notes in ci.yml leaves unset."""
    command = release_command(WORKFLOW.read_text())
    if command is None:
        return [f"{WORKFLOW.relative_to(ROOT)} has no helm upgrade --install"]
    templates = "".join(p.read_text() for p in (CHART / "templates").iterdir())
    unset = unset_in_release(command, templates, (CHART / "values.yaml").read_text())
    return [f"the release notes' helm command does not set {name}" for name in unset]


def main() -> int:
    errors = release_errors()
    for error in errors:
        print(f"check-chart: {error}", file=sys.stderr)
    if errors:
        return 1
    oldest = oldest_kubernetes((CHART / "Chart.yaml").read_text())
    if oldest is None:
        print(
            'check-chart: Chart.yaml has no kubeVersion of the form ">=X.Y.Z-0"',
            file=sys.stderr,
        )
        return 1
    try:
        subprocess.run(
            ["helm", "lint", "--strict", str(CHART), "--values", str(VALUES)],
            check=True,
        )
        subprocess.run(["helm", "unittest", str(CHART)], check=True)
        rendered = subprocess.run(
            ["helm", "template", "straum", str(CHART), "--values", str(VALUES)],
            check=True,
            capture_output=True,
        ).stdout
        for version in [oldest, NEWEST_KUBERNETES]:
            subprocess.run(
                [
                    "kubeconform",
                    "-strict",
                    "-summary",
                    "-kubernetes-version",
                    version,
                ],
                input=rendered,
                check=True,
            )
    except subprocess.CalledProcessError as e:
        print(f"check-chart: {' '.join(e.cmd)} failed", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
