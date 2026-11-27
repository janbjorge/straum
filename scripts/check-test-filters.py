#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = []
# ///
"""Reject a test filter in the justfile that selects no test.

A recipe that runs `test` with filters, as `correctness` does, passes
green when cargo's filter matches nothing. This reads each `(test
"crate" ...)` the justfile depends on, takes every argument that is no
flag as a filter, and lists the crate's tests without building it: each
`#[test] fn` under its `src`, named by the module path its file gives.
A filter passes when it is part of one of those names, as cargo's
filter is, and that test runs under the recipe's flags: an ignored test
runs only with `--ignored`, and a test that is not ignored runs only
without it. `--include-ignored` runs both. A test in an inline module
is named without that module, and the crate's `tests` and `benches`
folders are not read, so a filter naming one fails here though cargo
would find it.
"""

import pathlib
import re
import sys

import rust_source

ROOT = pathlib.Path(__file__).resolve().parents[1]

TEST_CALL = re.compile(r'\(test\s+"([^"]*)"([^)]*)\)')
ARGUMENT = re.compile(r'"([^"]*)"')
FUNCTION = re.compile(
    r"((?:#\s*\[[^\]]*\]\s*)*)"
    r"(?:pub(?:\s*\([^)]*\))?\s+)?(?:async\s+)?fn\s+(\w+)"
)
TEST_ATTRIBUTE = re.compile(r"#\s*\[\s*test\s*\]")
IGNORE_ATTRIBUTE = re.compile(r"#\s*\[\s*ignore\b")
# Cargo's options whose next argument is their value, not a filter.
OPTIONS_WITH_VALUE = {"--test", "--bench", "--bin", "--example", "--features", "-F"}


def module_path(file: pathlib.Path, source_root: pathlib.Path) -> list[str]:
    """The modules leading to `file`, from its crate's root."""
    parts = list(file.relative_to(source_root).with_suffix("").parts)
    if parts[-1] in ("lib", "main", "mod"):
        parts.pop()
    return parts


def test_names(crate: pathlib.Path) -> list[tuple[str, bool]]:
    """The path of each `#[test] fn` under the crate's `src`, as cargo
    lists it, and whether the test is ignored."""
    source_root = crate / "src"
    names = []
    for file in sorted(source_root.rglob("*.rs")):
        code = rust_source.lex(file.read_text()).code
        modules = module_path(file, source_root)
        for attributes, name in FUNCTION.findall(code):
            if TEST_ATTRIBUTE.search(attributes):
                ignored = bool(IGNORE_ATTRIBUTE.search(attributes))
                names.append(("::".join([*modules, name]), ignored))
    return names


def check(justfile: str, root: pathlib.Path) -> list[str]:
    """Each filter the justfile hands `test` that names no test."""
    errors = []
    for call in TEST_CALL.finditer(justfile):
        crate = call.group(1)
        filters = []
        previous = ""
        arguments = ARGUMENT.findall(call.group(2))
        for argument in arguments:
            if not argument.startswith("-") and previous not in OPTIONS_WITH_VALUE:
                filters.append(argument)
            previous = argument
        if not filters:
            continue
        folder = root / "crates" / crate
        if not folder.is_dir():
            errors.append(f"test {crate!r}: no crate folder {folder.relative_to(root)}")
            continue
        names = test_names(folder)
        both = "--include-ignored" in arguments
        runs_ignored = "--ignored" in arguments
        kind = "test"
        if not both:
            kind = "ignored test" if runs_ignored else "test that is not ignored"
        for wanted in filters:
            if not any(
                wanted in name and (both or ignored == runs_ignored)
                for name, ignored in names
            ):
                errors.append(f"test {crate!r}: filter {wanted!r} selects no {kind}")
    return errors


def main() -> int:
    errors = check((ROOT / "justfile").read_text(), ROOT)
    if errors:
        print("justfile test filters select nothing:", file=sys.stderr)
        print("\n".join(errors), file=sys.stderr)
        print(
            "name a test as cargo test -- --list does: module::path::name",
            file=sys.stderr,
        )
        print("pass --ignored only to run ignored tests", file=sys.stderr)
        return 1
    print("test filter check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
