#!/usr/bin/env python3
"""Reject workspace dependencies that violate Straum's crate layering.

Each crate must depend on exactly the workspace crates ALLOWED lists, so
dropping one fails too: the table stays the true picture. Dev-dependencies
are free.
"""

import json
import pathlib
import subprocess
import sys


ROOT = pathlib.Path(__file__).resolve().parents[1]
ALLOWED = {
    "straum-domain": set(),
    "straum-app": {"straum-domain"},
    "straum-notation": {"straum-domain"},
    "straum-pg": {"straum-app", "straum-domain"},
    "straum-feed": {"straum-app", "straum-domain"},
    "straum-http": {"straum-app", "straum-domain"},
    "straum": {
        "straum-app",
        "straum-domain",
        "straum-feed",
        "straum-http",
        "straum-notation",
        "straum-pg",
    },
}


def main() -> int:
    metadata = subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT,
        text=True,
    )
    packages = json.loads(metadata)["packages"]
    workspace = {package["name"] for package in packages}
    errors = []

    for package in packages:
        name = package["name"]
        if name not in ALLOWED:
            continue
        actual = {
            dependency["name"]
            for dependency in package["dependencies"]
            if dependency["name"] in workspace and dependency["kind"] is None
        }
        if actual != ALLOWED[name]:
            errors.append(
                f"{name}: expected {sorted(ALLOWED[name])}, found {sorted(actual)}"
            )

    if errors:
        print("architecture dependency check failed:", file=sys.stderr)
        print("\n".join(errors), file=sys.stderr)
        return 1

    print("architecture dependency check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
