#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = []
# ///
"""Lint the Helm chart, run its unit tests, and check what it renders
against the schemas of the oldest and newest Kubernetes it targets.

The oldest is the floor of the chart's `kubeVersion` in Chart.yaml, so
the check moves with the chart; the newest is NEWEST_KUBERNETES."""

import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
CHART = ROOT / "deploy" / "helm" / "straum"
VALUES = CHART / "ci" / "values.yaml"
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


def main() -> int:
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
