#!/usr/bin/env python3
"""Lint the Helm chart, run its unit tests, and check what it renders
against the schemas of the oldest and newest Kubernetes it targets."""

import pathlib
import subprocess
import sys


ROOT = pathlib.Path(__file__).resolve().parents[1]
CHART = ROOT / "deploy" / "helm" / "straum"
VALUES = CHART / "ci" / "values.yaml"
KUBERNETES = ["1.33.0", "1.36.0"]


def main() -> int:
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
        for version in KUBERNETES:
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
