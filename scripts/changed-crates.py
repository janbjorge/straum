#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = []
# ///
"""The workspace crates changed since HEAD, for the justfile's crate
recipes when no crate is named.

With no argument it prints their names on one line, space separated:
every crate with a file git sees changed against HEAD, staged or not,
or untracked and not ignored. Nothing changed prints an empty line.

With `--named CRATES` it prints nothing and fails, saying how to name a
crate, when CRATES is empty: the recipes' guard against running cargo
on no crate, which would take the whole workspace.
"""

import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
CRATES = "crates"
NONE_NAMED = "no crate changed since HEAD; name one, as in just test straum-pg"


def crate_names(paths: list[str]) -> list[str]:
    """The sorted crates that `paths`, relative to the root, fall in."""
    names = set()
    for path in paths:
        parts = path.split("/")
        if len(parts) >= 3 and parts[0] == CRATES and parts[1]:
            names.add(parts[1])
    return sorted(names)


def changed_paths() -> list[str]:
    """The paths under crates/ that differ from HEAD or are untracked."""
    commands = [
        ["git", "diff", "--name-only", "HEAD", "--", CRATES],
        ["git", "ls-files", "--others", "--exclude-standard", "--", CRATES],
    ]
    paths = []
    for command in commands:
        # Outside a git checkout nothing counts as changed, so a recipe
        # asks for a crate by name rather than failing here.
        listed = subprocess.run(
            command, cwd=ROOT, check=False, text=True, capture_output=True
        )
        paths.extend(listed.stdout.splitlines())
    return paths


def main(arguments: list[str]) -> int:
    match arguments:
        case []:
            print(" ".join(crate_names(changed_paths())))
            return 0
        case ["--named", crates]:
            if crates.strip():
                return 0
            print(NONE_NAMED, file=sys.stderr)
            return 1
        case _:
            print("usage: changed-crates.py [--named CRATES]", file=sys.stderr)
            return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
