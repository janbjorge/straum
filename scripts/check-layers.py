#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = []
# ///
"""Reject dependencies that break Straum's crate layering (ADR-0002).

Cargo's metadata, with every feature on so an optional dependency shows,
gives each workspace crate's dependencies, targets and the whole
resolved graph; the crates' files, lexed, give what they pull in by
path. A crate:

- depends, through normal and build dependencies, on exactly the
  workspace crates ALLOWED lists, so dropping one fails too: the table
  stays the true picture;
- takes as a dev-dependency only a workspace crate that does not itself
  depend on it, so a test never reaches outward;
- builds each target from its own folder, and its library, binaries and
  build script from outside `tests/`, `benches/` and `examples/`;
- reaches no outside crate if it is in PURE, by any feature, and names
  no `std` module that does I/O, nor globs or renames `std`;
- reaches an async runtime only through straum-http, its tests too;
- resolves no package from a path outside the workspace, by a path
  dependency or a `[patch]`;
- holds no link that leaves its folder;
- defines a trait only if it is PORTS, where the ports are, or in a test
  folder;
- pulls in another crate's file with `#[path]` or an `include` macro
  only where it may depend on that crate: from a test folder as a
  dev-dependency, from anywhere else as a normal one. Code from outside
  every crate is refused; `include_str!` and `include_bytes!` may read
  any file, the README say. A path the macro computes can't be checked,
  so it fails.
"""

import json
import os
import pathlib
import re
import subprocess
import sys

import rust_source

ROOT = pathlib.Path(__file__).resolve().parents[1]
ALLOWED = {
    "straum-domain": set(),
    "straum-app": {"straum-domain"},
    "straum-notation": {"straum-domain"},
    "straum-pg": {"straum-app", "straum-domain"},
    "straum-http": {"straum-app", "straum-domain"},
    "straum": {
        "straum-app",
        "straum-domain",
        "straum-http",
        "straum-notation",
        "straum-pg",
    },
}
# Crates with no outside dependency and no I/O: the domain and the use
# cases. The world reaches them through a port.
PURE = {"straum-domain", "straum-app"}
# The `std` modules that reach outside the process.
IO_MODULES = "fs|net|process|env|io|os"
# Only straum-http runs an async runtime; every other crate reaches one
# through straum-http or not at all. mio is tokio's own I/O.
RUNTIMES = {"tokio", "mio", "async-std", "smol", "async-io", "async-executor"}
RUNTIME_HOME = "straum-http"
# Traits exist only at the ports, which the use cases define.
PORTS = "straum-app"
# Cargo's kinds that end up in the crate as built: normal and build.
BUILT = {None, "build"}
# Folders of a crate whose files build only for its tests.
TEST_FOLDERS = {"tests", "benches", "examples"}
TEST_TARGETS = {"test", "bench", "example"}

ATTRIBUTE = re.compile(r"#\s*!?\s*\[")
PATH_VALUE = re.compile(r"\bpath\s*=\s*(\S)")
INCLUDE = re.compile(r"\binclude(_str|_bytes)?\s*!\s*[\(\[\{]\s*(\S)")
TRAIT = re.compile(r"\btrait\s+(?:r#)?(\w+)")
INLINE_MODULE = re.compile(r"\bmod\s+(?:r#)?(\w+)\s*\{")
# A use group runs to its ";", so a nested group cannot hide a module.
# A glob or a new name for std hides the module that a later path names.
HIDES = rf"\b(?:{IO_MODULES})\b|\*|\bself\s+as\b"
IO_PATH = re.compile(
    rf"\bstd\s*::\s*(?:{HIDES}|\{{[^;]*?(?:{HIDES}))"
    r"|\bextern\s+crate\b|\bstd\s+as\b"
)


def metadata() -> tuple[dict | None, str]:
    """Cargo's metadata for the workspace and every crate it resolves,
    or why cargo gave none."""
    command = [
        "cargo",
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--all-features",
    ]
    result = subprocess.run(
        command, cwd=ROOT, capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        return None, result.stderr.strip()
    return json.loads(result.stdout), ""


def reaches_inward(name: str, target: str, seen: set[str]) -> bool:
    """Whether workspace crate `name` depends on `target`, by ALLOWED."""
    for dependency in ALLOWED.get(name, set()):
        if dependency == target:
            return True
        if dependency not in seen:
            seen.add(dependency)
            if reaches_inward(dependency, target, seen):
                return True
    return False


def may_use(name: str, dependency: str, kind: str | None) -> bool:
    """Whether `name` may depend on workspace crate `dependency`."""
    if kind in BUILT:
        return dependency in ALLOWED[name]
    return dependency != name and not reaches_inward(dependency, name, set())


def check_workspace_edges(package: dict, workspace: set[str]) -> list[str]:
    """The ALLOWED, dev-dependency and PURE rules on one crate's own
    dependency list, optional ones included."""
    name = package["name"]
    errors = []
    built = set()
    for dependency in package["dependencies"]:
        other = dependency["name"]
        kind = dependency["kind"]
        if other not in workspace:
            if name in PURE and kind in BUILT:
                errors.append(f"{name}: depends on outside crate {other}")
            continue
        if kind in BUILT:
            built.add(other)
        elif not may_use(name, other, kind):
            errors.append(f"{name}: dev-dependency {other} depends on {name}")
    if built != ALLOWED[name]:
        errors.append(
            f"{name}: expected {sorted(ALLOWED[name])}, found {sorted(built)}"
        )
    return errors


def check_targets(package: dict, folder: pathlib.Path, root) -> list[str]:
    """Each target of a crate builds from its own folder, and only the
    test targets from its test folders."""
    name = package["name"]
    errors = []
    for target in package.get("targets", []):
        source = pathlib.Path(target["src_path"]).resolve()
        shown = target["name"]
        if not source.is_relative_to(folder):
            where = display(source, root)
            errors.append(f"{name}: target {shown} builds from {where}")
            continue
        in_tests = source.relative_to(folder).parts[0] in TEST_FOLDERS
        if in_tests and not TEST_TARGETS.intersection(target["kind"]):
            kind = target["kind"][0]
            errors.append(f"{name}: {kind} {shown} builds from a test folder")
    return errors


def built_graph(data: dict):
    """Each package id's name; the ids it depends on as built; and the
    ids it takes for its tests."""
    names = {package["id"]: package["name"] for package in data["packages"]}
    built = {}
    dev = {}
    for node in data["resolve"]["nodes"]:
        built[node["id"]] = []
        dev[node["id"]] = []
        for dependency in node["deps"]:
            kinds = {dep_kind["kind"] for dep_kind in dependency["dep_kinds"]}
            if kinds & BUILT:
                built[node["id"]].append(dependency["pkg"])
            if "dev" in kinds:
                dev[node["id"]].append(dependency["pkg"])
    return names, built, dev


def path_to(start, first_step, wanted, names, edges, skip) -> list[str] | None:
    """The names from `start`, through `first_step` and then built edges,
    to the first package `wanted` accepts, not passing through a package
    named `skip`; None if there is none."""
    parents = {start: None}
    queue = []
    for dependency in first_step:
        if dependency not in parents and names[dependency] != skip:
            parents[dependency] = start
            queue.append(dependency)
    while queue:
        current = queue.pop(0)
        if wanted(names[current]):
            chain = []
            while current is not None:
                chain.append(names[current])
                current = parents[current]
            return chain[::-1]
        for dependency in edges.get(current, []):
            if dependency in parents or names[dependency] == skip:
                continue
            parents[dependency] = current
            queue.append(dependency)
    return None


def check_outside(data: dict, workspace: set[str]) -> list[str]:
    """The PURE, runtime and outside-path rules, over the resolved graph."""
    names, built, dev = built_graph(data)

    def outside(found: str) -> bool:
        return found not in workspace

    def runtime(found: str) -> bool:
        return found in RUNTIMES

    errors = []
    for package_id in data["workspace_members"]:
        name = names[package_id]
        own = built.get(package_id, [])
        if name in PURE:
            chain = path_to(package_id, own, outside, names, built, "")
            if chain:
                errors.append(f"{name}: reaches outside crate {' -> '.join(chain)}")
        if name != RUNTIME_HOME:
            steps = own + dev.get(package_id, [])
            chain = path_to(package_id, steps, runtime, names, built, RUNTIME_HOME)
            if chain:
                errors.append(
                    f"{name}: reaches a runtime not through {RUNTIME_HOME}: "
                    + " -> ".join(chain)
                )
    members = set(data["workspace_members"])
    for package in data["packages"]:
        if package.get("source") is None and package["id"] not in members:
            errors.append(
                f"{package['name']}: resolved from a path outside the workspace"
            )
    return errors


def display(path: pathlib.Path, root: pathlib.Path) -> str:
    """`path` from the workspace root when it lies inside it."""
    return str(path.relative_to(root)) if path.is_relative_to(root) else str(path)


def owner(path: pathlib.Path, crates: dict[pathlib.Path, str]) -> str | None:
    """The workspace crate whose folder holds `path`, if any."""
    for folder, name in crates.items():
        if path.is_relative_to(folder):
            return name
    return None


def crate_files(folder: pathlib.Path):
    """Every file and folder of a crate but its build output, links not
    followed."""
    for top, folders, files in os.walk(folder):
        here = pathlib.Path(top)
        if here == folder and "target" in folders:
            folders.remove("target")
        for entry in folders + files:
            yield here / entry


def check_includes(packages: list[dict], root: pathlib.Path) -> list[str]:
    """The rules on links, `#[path]`, the include macros and I/O."""
    crates = {}
    for package in packages:
        folder = pathlib.Path(package["manifest_path"]).resolve().parent
        crates[folder] = package["name"]
    errors = []
    for folder, name in crates.items():
        for entry in crate_files(folder):
            if entry.is_symlink():
                if owner(entry.resolve(), crates) != name:
                    where = display(entry.resolve(), root)
                    errors.append(f"{display(entry, root)}: links to {where}")
            elif entry.suffix == ".rs" and entry.is_file():
                errors += check_file(entry, folder, name, crates, root)
    return errors


def path_base(file, offset: int, code: str) -> pathlib.Path:
    """The folder a `#[path]` at `offset` is relative to: the file's own,
    or inside inline modules, theirs."""
    names = []
    for match in INLINE_MODULE.finditer(code):
        if match.end() <= offset < rust_source.closing(code, match.end() - 1):
            names.append(match.group(1))
    if not names:
        return file.parent
    return rust_source.module_folder(file).joinpath(*names)


def value_at(source: rust_source.Source, offset: int) -> str | None:
    """The string literal that starts at `offset`, if one does."""
    for literal in source.strings:
        if literal.start == offset:
            return literal.text
    return None


def pulled_in(file, source):
    """Each path the file pulls in: its offset, the folder it is relative
    to, whether it is code, and its text, None where a macro computes
    it."""
    code = source.code
    found = []
    for attribute in ATTRIBUTE.finditer(code):
        end = rust_source.closing(code, attribute.end() - 1)
        for value in PATH_VALUE.finditer(code, attribute.end(), end):
            base = path_base(file, attribute.start(), code)
            found.append((value.start(), base, True, value_at(source, value.start(1))))
    for include in INCLUDE.finditer(code):
        is_code = include.group(1) is None
        text = value_at(source, include.start(2))
        found.append((include.start(), file.parent, is_code, text))
    return found


def check_file(file, folder, name, crates, root) -> list[str]:
    """The include and I/O rules for one Rust file of crate `name`."""
    shown = display(file, root)
    try:
        source = rust_source.lex(file.read_text())
    except UnicodeDecodeError:
        return [f"{shown}: not UTF-8, so not read"]
    except rust_source.LexError as error:
        return [f"{shown}:{error.line}: {error}, so not read"]
    kind = "dev" if file.relative_to(folder).parts[0] in TEST_FOLDERS else None
    errors = []
    for offset, base, is_code, text in pulled_in(file, source):
        line = source.code.count("\n", 0, offset) + 1
        if text is None:
            errors.append(f"{shown}:{line}: a path the macro computes")
            continue
        resolved = (base / text).resolve()
        other = owner(resolved, crates)
        where = display(resolved, root)
        if other is None and is_code:
            errors.append(f"{shown}:{line}: pulls in code from {where}")
        elif other not in (None, name) and not may_use(name, other, kind):
            errors.append(f"{shown}:{line}: pulls in {other}'s {where}")
    if name in PURE:
        for match in IO_PATH.finditer(source.code):
            line = source.code.count("\n", 0, match.start()) + 1
            errors.append(f"{shown}:{line}: I/O in a pure crate: {match.group(0)!r}")
    if name != PORTS and kind is None:
        for match in TRAIT.finditer(source.code):
            line = source.code.count("\n", 0, match.start()) + 1
            trait = match.group(1)
            errors.append(f"{shown}:{line}: trait {trait} outside {PORTS}'s ports")
    return errors


def check(data: dict) -> list[str]:
    """Every broken rule in Cargo's metadata `data` and the files of the
    workspace it names."""
    root = pathlib.Path(data["workspace_root"]).resolve()
    members = set(data["workspace_members"])
    packages = [package for package in data["packages"] if package["id"] in members]
    workspace = {package["name"] for package in packages}
    errors = []
    for package in packages:
        if package["name"] not in ALLOWED:
            # A crate the table doesn't name could depend on anything.
            errors.append(
                f"{package['name']}: not in ALLOWED; add it with what it may use"
            )
            continue
        errors += check_workspace_edges(package, workspace)
        folder = pathlib.Path(package["manifest_path"]).resolve().parent
        errors += check_targets(package, folder, root)
    errors += check_outside(data, workspace)
    errors += check_includes(packages, root)
    return errors


def main() -> int:
    data, why = metadata()
    if data is None:
        print("architecture dependency check failed: cargo metadata:", file=sys.stderr)
        print(why, file=sys.stderr)
        print(
            "a stale Cargo.lock? run cargo metadata once and commit it", file=sys.stderr
        )
        return 1
    errors = check(data)
    if errors:
        print("architecture dependency check failed:", file=sys.stderr)
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("architecture dependency check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
