#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = ["openapi-spec-validator==0.7.2"]
#
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Tests of the check scripts: each rule they hold catches a planted
break and lets the honest form through.

The layer check reads Cargo's metadata; these tests hand it metadata
built here, over a workspace of files in a temporary folder, so they
need no cargo and no network. The domain syntax check reads a folder of
Rust files; these tests write one. The chart check reads its oldest
Kubernetes from Chart.yaml; these tests hand it the line. The OpenAPI
check reads a document; these tests hand it one.
changed-crates.py maps paths to crates; these tests hand it paths. The
test filter check reads a justfile and a crate; these tests write both.
The justfile's guard against a crate recipe with no crate runs under
`just -n`, which prints what would run and runs nothing. Many cases
here are ways past a check that a red team found; each stays so the
way stays shut.
"""

import collections.abc
import importlib.util
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS))


def load(name: str):
    """The script `scripts/<name>.py` as a module."""
    spec = importlib.util.spec_from_file_location(
        name.replace("-", "_"), SCRIPTS / f"{name}.py"
    )
    if spec is None or spec.loader is None:
        raise ImportError(f"no script {name}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


layers = load("check-layers")
syntax = load("check-domain-syntax")
chart = load("check-chart")
changed = load("changed-crates")
filters = load("check-test-filters")
openapi = load("check-openapi")
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


class Workspace:
    """Straum's six crates as Cargo's metadata would show them, with
    their honest dependencies, in a temporary folder; a test adds the
    break it plants."""

    def __init__(self, root: pathlib.Path):
        self.root = root
        self.packages: dict[str, dict] = {}
        self.edges: dict[str, list] = {}
        for name in layers.ALLOWED:
            self.write(f"crates/{name}/src/lib.rs", "")
            self.packages[name] = {
                "id": name,
                "name": name,
                "source": None,
                "manifest_path": str(root / "crates" / name / "Cargo.toml"),
                "dependencies": [],
                "targets": [self.target(name, "lib", "src/lib.rs")],
            }
            self.edges[name] = []
        for name, allowed in layers.ALLOWED.items():
            for dependency in sorted(allowed):
                self.depend(name, dependency)
        self.depend("straum-http", "tokio")
        self.depend("tokio", "mio")

    def target(self, crate: str, kind: str, relative: str) -> dict:
        path = self.root / "crates" / crate / relative
        return {"name": crate, "kind": [kind], "src_path": str(path)}

    def outside(self, name: str, source: str | None = REGISTRY):
        """An outside crate, from the registry unless `source` says."""
        if name not in self.packages:
            self.packages[name] = {
                "id": name,
                "name": name,
                "source": source,
                "manifest_path": str(self.root / "registry" / name / "Cargo.toml"),
                "dependencies": [],
                "targets": [],
            }
            self.edges[name] = []

    def depend(self, name, dependency, kind=None, resolved=True):
        """`name` takes `dependency`; an optional one with features off
        is listed but not `resolved`."""
        if dependency not in layers.ALLOWED:
            self.outside(dependency)
        entry = {"name": dependency, "kind": kind}
        self.packages[name]["dependencies"].append(entry)
        if resolved:
            self.edges[name].append((dependency, kind))

    def write(self, relative: str, text: str | bytes):
        """A file of the workspace, its folders made."""
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(text, bytes):
            path.write_bytes(text)
        else:
            path.write_text(text)

    def metadata(self) -> dict:
        """What `cargo metadata` would print for this workspace."""
        nodes = []
        for name, edges in self.edges.items():
            deps = []
            for dependency, kind in edges:
                deps.append({"pkg": dependency, "dep_kinds": [{"kind": kind}]})
            nodes.append({"id": name, "deps": deps})
        return {
            "packages": list(self.packages.values()),
            "workspace_members": list(layers.ALLOWED),
            "workspace_root": str(self.root),
            "resolve": {"nodes": nodes},
        }

    def errors(self) -> list[str]:
        return layers.check(self.metadata())


class LayerCheck(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.workspace = Workspace(pathlib.Path(folder.name))

    def assert_refused(self, words: str):
        errors = self.workspace.errors()
        self.assertTrue(
            any(words in error for error in errors),
            f"no error says {words!r}: {errors}",
        )

    def assert_passes(self):
        self.assertEqual(self.workspace.errors(), [])

    def write(self, relative: str, text: str | bytes):
        self.workspace.write(relative, text)

    def test_the_honest_workspace_passes(self):
        self.assert_passes()

    def test_a_crate_not_in_the_table_fails(self):
        metadata = self.workspace.metadata()
        metadata["packages"].append(
            {
                "id": "x",
                "name": "straum-extra",
                "source": None,
                "manifest_path": str(self.workspace.root / "crates/x/Cargo.toml"),
                "dependencies": [],
                "targets": [],
            }
        )
        metadata["workspace_members"].append("x")
        errors = layers.check(metadata)
        self.assertTrue(any("straum-extra: not in ALLOWED" in e for e in errors))

    # Workspace edges.

    def test_a_normal_dependency_outward_fails(self):
        self.workspace.depend("straum-domain", "straum-pg")
        self.assert_refused("straum-domain: expected [], found ['straum-pg']")

    def test_a_dependency_dropped_fails(self):
        self.workspace.packages["straum-app"]["dependencies"] = []
        self.assert_refused("straum-app: expected ['straum-domain'], found []")

    def test_a_build_dependency_counts_as_normal(self):
        self.workspace.depend("straum-notation", "straum-app", kind="build")
        self.assert_refused("straum-notation: expected ['straum-domain'], found")

    def test_a_dev_dependency_outward_fails(self):
        self.workspace.depend("straum-domain", "straum-pg", kind="dev")
        self.assert_refused("dev-dependency straum-pg depends on straum-domain")

    def test_a_dev_dependency_sideways_passes(self):
        self.workspace.depend("straum-app", "straum-notation", kind="dev")
        self.assert_passes()

    # Targets.

    def test_a_target_from_another_crate_fails(self):
        target = self.workspace.target("straum-pg", "lib", "src/lib.rs")
        self.workspace.packages["straum-domain"]["targets"] = [target]
        self.assert_refused(
            "straum-domain: target straum-pg builds from crates/straum-pg"
        )

    def test_a_library_in_a_test_folder_fails(self):
        target = self.workspace.target("straum-app", "lib", "tests/lib.rs")
        self.workspace.packages["straum-app"]["targets"] = [target]
        self.assert_refused("straum-app: lib straum-app builds from a test folder")

    def test_a_test_target_in_a_test_folder_passes(self):
        target = self.workspace.target("straum-app", "test", "tests/app.rs")
        self.workspace.packages["straum-app"]["targets"].append(target)
        self.assert_passes()

    # The pure domain.

    def test_the_domain_takes_no_outside_crate(self):
        self.workspace.depend("straum-domain", "jiff")
        self.assert_refused(
            "straum-domain: reaches outside crate straum-domain -> jiff"
        )

    def test_the_domain_takes_no_optional_outside_crate(self):
        self.workspace.depend("straum-domain", "serde", resolved=False)
        self.assert_refused("straum-domain: depends on outside crate serde")

    def test_the_use_cases_take_no_outside_crate_and_do_no_io(self):
        self.workspace.depend("straum-app", "reqwest")
        self.assert_refused("straum-app: depends on outside crate reqwest")
        self.setUp()
        self.write("crates/straum-app/src/lib.rs", "use std::net::TcpStream;\n")
        self.assert_refused("crates/straum-app/src/lib.rs:1: I/O in a pure crate")

    def test_the_use_cases_may_read_the_clock_and_use_threads(self):
        self.write(
            "crates/straum-app/src/lib.rs",
            "use std::time::{Duration, Instant, SystemTime};\nuse std::thread;\n",
        )
        self.assert_passes()

    def test_the_domain_takes_outside_crates_in_its_tests(self):
        self.workspace.depend("straum-domain", "proptest", kind="dev")
        self.assert_passes()

    def test_the_domain_does_no_io(self):
        for text in [
            "use std::fs;",
            "use std::{mem, net::TcpStream};",
            'fn f() { std::process::Command::new("sh"); }',
            'fn f() { let _ = std :: env :: var("X"); }',
            "extern crate std as s;",
            "use std::{collections::{HashMap, HashSet}, fs};",
            'use std::*;\nfn f() { fs::read("x"); }',
            'use std::{self as s};\nfn f() { s::fs::read("x"); }',
        ]:
            with self.subTest(text=text):
                self.write("crates/straum-domain/src/lib.rs", text)
                self.assert_refused("I/O in a pure crate")

    def test_the_domain_may_use_threads_and_other_crates_io(self):
        self.write(
            "crates/straum-domain/src/lib.rs",
            "use std::{mem, panic, thread};\n// std::fs\n"
            "use std::{collections::{HashMap, HashSet}, sync::mpsc};\n"
            "use std::collections::*;\nuse std::fmt::{self as f};\n",
        )
        self.write("crates/straum-pg/src/lib.rs", "use std::net::TcpStream;\n")
        self.assert_passes()

    # Traits.

    def test_a_trait_outside_the_ports_fails(self):
        for crate, text in [
            ("straum-domain", "pub trait Rows { fn rows(&self); }"),
            ("straum-pg", "pub(crate) unsafe trait Reader {}"),
            ("straum-http", "mod inner {\n    trait Route {}\n}"),
        ]:
            with self.subTest(crate=crate):
                self.setUp()
                self.write(f"crates/{crate}/src/lib.rs", text)
                self.assert_refused(f"crates/{crate}/src/lib.rs")
                self.assert_refused("outside straum-app's ports")

    def test_a_trait_at_the_ports_or_in_tests_passes(self):
        self.write(
            "crates/straum-app/src/lib.rs", "pub trait Source { fn open(&self); }\n"
        )
        self.write("crates/straum-pg/tests/fake.rs", "trait Fake {}\n")
        self.write(
            "crates/straum-domain/src/lib.rs",
            "// a trait Rows would do\n"
            'const S: &str = "trait Rows";\n'
            "impl std::fmt::Display for X {}\nfn f(x: &dyn Source) {}\n"
            "impl Subtrait for X {}\n",
        )
        self.assert_passes()

    # Runtimes.

    def test_tokio_directly_outside_straum_http_fails(self):
        self.workspace.depend("straum-pg", "tokio")
        self.assert_refused(
            "straum-pg: reaches a runtime not through straum-http: straum-pg -> tokio"
        )

    def test_tokio_through_another_crate_fails(self):
        self.workspace.depend("straum-app", "reqwest")
        self.workspace.depend("reqwest", "tokio")
        self.assert_refused("straum-app -> reqwest -> tokio")

    def test_tokio_in_tests_outside_straum_http_fails(self):
        self.workspace.depend("straum-pg", "tokio", kind="dev")
        self.assert_refused("straum-pg: reaches a runtime")

    def test_another_runtime_fails(self):
        for runtime in ["mio", "async-std", "smol"]:
            with self.subTest(runtime=runtime):
                self.setUp()
                self.workspace.depend("straum-pg", runtime)
                self.assert_refused(f"straum-pg -> {runtime}")

    def test_tokio_through_straum_http_passes(self):
        self.workspace.depend("straum-http", "tokio", kind="dev")
        self.assert_passes()

    # Paths outside the workspace.

    def test_a_package_from_a_path_outside_the_workspace_fails(self):
        self.workspace.outside("vendored", source=None)
        self.workspace.depend("straum-app", "vendored", kind="dev")
        self.assert_refused("vendored: resolved from a path outside the workspace")

    # Links.

    def test_a_link_out_of_the_crate_fails(self):
        link = self.workspace.root / "crates/straum-domain/src/evil.rs"
        os.symlink(self.workspace.root / "crates/straum-pg/src/lib.rs", link)
        self.assert_refused("crates/straum-domain/src/evil.rs: links to")

    def test_a_link_inside_the_crate_passes(self):
        link = self.workspace.root / "crates/straum-domain/src/same.rs"
        os.symlink(self.workspace.root / "crates/straum-domain/src/lib.rs", link)
        self.assert_passes()

    # `#[path]` and the include macros.

    def test_a_path_attribute_into_another_crate_fails(self):
        for attribute in [
            '#[path = "../../straum-pg/src/lib.rs"]',
            '#[ path = "../../straum-pg/src/lib.rs" ]',
            '#[cfg_attr(all(), path = "../../straum-pg/src/lib.rs")]',
            '#[path = r"../../straum-pg/src/lib.rs"]',
            '#[path = "../../straum-pg/src/\\x6cib.rs"]',
        ]:
            with self.subTest(attribute=attribute):
                self.write(
                    "crates/straum-domain/src/lib.rs", f"{attribute}\nmod pulled;\n"
                )
                self.assert_refused("pulls in straum-pg's crates/straum-pg/src/lib.rs")

    def test_a_path_attribute_inside_an_inline_module_resolves_as_rustc_does(self):
        self.write(
            "crates/straum-domain/src/lib.rs",
            'mod a {\n    #[path = "../../../straum-pg/src/lib.rs"]\n    mod pulled;\n}\n',
        )
        self.assert_refused("crates/straum-domain/src/lib.rs:2: pulls in straum-pg's")

    def test_a_path_attribute_in_a_named_file_inline_module(self):
        self.write(
            "crates/straum-domain/src/store.rs",
            'mod a {\n    #[path = "../../../../straum-pg/src/lib.rs"]\n    mod pulled;\n}\n',
        )
        self.assert_refused("crates/straum-domain/src/store.rs:2: pulls in straum-pg's")

    def test_code_from_outside_every_crate_fails(self):
        self.write(
            "shared/m.rs", '#[path = "../crates/straum-pg/src/lib.rs"]\nmod evil;\n'
        )
        self.write(
            "crates/straum-domain/src/lib.rs",
            '#[path = "../../../shared/m.rs"]\nmod m;\n',
        )
        self.assert_refused("pulls in code from shared/m.rs")
        self.write(
            "crates/straum-domain/src/lib.rs", 'include!("../../../shared/m.rs");\n'
        )
        self.assert_refused("pulls in code from shared/m.rs")

    def test_an_include_into_another_crate_fails(self):
        for call in [
            'include_str!("../../straum-pg/src/lib.rs")',
            'include_str!["../../straum-pg/src/lib.rs"]',
            'include_str!{"../../straum-pg/src/lib.rs"}',
            'include_bytes! (\n    "../../straum-pg/src/lib.rs",\n)',
            '::core::include_str!("../../straum-pg/src/lib.rs")',
        ]:
            with self.subTest(call=call):
                self.write(
                    "crates/straum-domain/tests/spec.rs", f"const S: &str = {call};\n"
                )
                self.assert_refused("pulls in straum-pg's")

    def test_an_include_follows_the_dev_rule_only_from_tests(self):
        include = 'const S: &str = include_str!("../../straum-notation/src/lib.rs");\n'
        self.write("crates/straum-app/tests/spec.rs", include)
        self.assert_passes()
        self.write("crates/straum-app/src/lib.rs", include)
        self.assert_refused(
            "crates/straum-app/src/lib.rs:1: pulls in straum-notation's"
        )

    def test_an_include_a_dependency_allows_passes(self):
        self.write(
            "crates/straum/tests/straum.rs",
            '#[path = "../../straum-pg/src/lib.rs"]\nmod compose;\n',
        )
        self.write(
            "crates/straum-notation/tests/docs.rs",
            'const R: &str = include_str!("../../../README.md");\n',
        )
        self.write(
            "crates/straum-app/src/lib.rs", 'const S: &str = include_str!(r"lib.rs");\n'
        )
        self.assert_passes()

    def test_a_computed_include_fails(self):
        for call in ['include_str!(concat!(env!("OUT_DIR"), "/x"))', "include!(PATH)"]:
            with self.subTest(call=call):
                self.write("crates/straum-app/src/lib.rs", f"const S: &str = {call};\n")
                self.assert_refused("a path the macro computes")

    def test_a_comment_or_string_naming_an_include_passes(self):
        self.write(
            "crates/straum-domain/src/lib.rs",
            '// #[path = "../../straum-pg/src/lib.rs"]\n'
            '/* include_str!(concat!("a")) */\n'
            'const S: &str = "include!(x) #[path = \\"../../straum-pg\\"]";\n',
        )
        self.assert_passes()

    def test_a_folder_named_target_below_the_top_is_read(self):
        self.write(
            "crates/straum-domain/src/target/mod.rs",
            'include!("../../../straum-pg/src/lib.rs");\n',
        )
        self.assert_refused("pulls in straum-pg's")

    def test_the_build_output_is_not_read(self):
        self.write(
            "crates/straum-domain/target/debug/x.rs",
            'include!("../../../straum-pg/src/lib.rs");\n',
        )
        self.assert_passes()

    def test_a_file_that_is_no_rust_fails(self):
        for text in [
            b"\xff\xfe",
            b"/* never closed",
            b'const S: &str = "never closed;',
        ]:
            with self.subTest(text=text):
                self.write("crates/straum-app/src/lib.rs", text)
                self.assert_refused("so not read")


def domain_errors(files: collections.abc.Mapping[str, str | bytes]) -> list[str]:
    """The domain syntax check's errors over a domain of `files`."""
    with tempfile.TemporaryDirectory() as folder:
        root = pathlib.Path(folder)
        for relative, text in files.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            if isinstance(text, bytes):
                path.write_bytes(text)
            else:
                path.write_text(text)
        return syntax.check(root, root)


class DomainSyntaxCheck(unittest.TestCase):
    def assert_flags(self, item: str, words: str):
        errors = domain_errors({"lib.rs": item + "\n"})
        self.assertEqual(len(errors), 1, errors)
        self.assertIn(repr(words), errors[0])

    def assert_passes(self, files: dict[str, str]):
        self.assertEqual(domain_errors(files), [])

    def test_flags_each_word_the_walker_reads_or_names(self):
        # Written down here, not taken from the script's own lists.
        cases = []
        for word in [
            "SELECT",
            "FROM",
            "WHERE",
            "GROUP BY",
            "ORDER BY",
            "HAVING",
            "FILTER",
            "JOIN",
            "DISTINCT",
            "EXISTS",
            "WITHIN GROUP",
            "IS NULL",
            "IS NOT NULL",
            "AND",
            "OR",
            "IN",
            "AS",
            "ON",
            "NOT",
            "WITH",
            "LIMIT",
            "OFFSET",
            "CREATE VIEW",
            "NULL",
            "CAST",
            "ASC",
            "DESC",
            "OVER",
            "LEFT",
            "OUTER",
            "UNION",
            "CASE",
            "BETWEEN",
            "LIKE",
            "INTERVAL",
        ]:
            cases.append((f'const S: &str = "write {word} here";', word))
        for function in [
            "date_bin",
            "buckets",
            "keep",
            "count",
            "sum",
            "avg",
            "min",
            "max",
            "bool_and",
            "bool_or",
            "var_samp",
            "var_pop",
            "stddev_samp",
            "stddev_pop",
            "percentile_disc",
            "mode",
            "percentile_cont",
            "stddev",
            "variance",
            "every",
        ]:
            cases.append((f'const S: &str = "write {function}(x)";', f"{function}("))
        for item, words in cases:
            with self.subTest(item=item):
                self.assert_flags(item, words)

    def test_flags_syntax_however_the_literal_spells_it(self):
        for item, words in [
            ('const S: &str = "COUNT(*)";', "COUNT("),
            ('const S: &str = "group by x";', "group by"),
            ('const S: &str = r#"raw SELECT"#;', "SELECT"),
            ('const S: &[u8] = b"SELECT";', "SELECT"),
            ('const S: &str = "GROUP  BY";', "GROUP BY"),
            ('const S: &str = "GROUP\\tBY";', "GROUP BY"),
            ('const S: &str = "GROUP \\\n        BY";', "GROUP BY"),
            ('const S: &str = "\\x53ELECT";', "SELECT"),
            ('const S: &str = "\\u{53}ELECT";', "SELECT"),
            ('const S: &str = concat!("SEL", "ECT");', "SELECT"),
            ('const S: &str = "a date_bin width";', "date_bin"),
            ('const Q: &str = "one \\" quote"; const S: &str = "SELECT";', "SELECT"),
        ]:
            with self.subTest(item=item):
                self.assert_flags(item, words)

    def test_flags_operators_parameters_and_casts(self):
        for item, words in [
            ('const S: &str = "x >= y";', ">="),
            ('const S: &str = "=";', "="),
            ('const S: &str = " < ";', "<"),
            ('const S: &str = "write a = b";', "a = b"),
            ("const S: char = '=';", "="),
            ('const S: &str = "bound $1";', "$1"),
            ('const S: &str = "and $n";', "$n"),
            ('const S: &str = "$ 1";', "$ 1"),
            ('const S: &str = "write x::int";', "x::int"),
            ('const S: &str = "write x::TEXT";', "x::TEXT"),
            ('const S: &str = "write x :: text";', "x :: text"),
            ("const S: &str = \"write '1 day'::interval\";", "'::interval"),
            ('const S: &str = "e.kind::double precision";', "e.kind::double precision"),
        ]:
            with self.subTest(item=item):
                self.assert_flags(item, words)

    def test_passes_english_and_rust_paths(self):
        for text in [
            "a count of rows",
            "count (of rows)",
            "every (key, value) pair",
            "checked by Spec::new",
            "jiff::civil::date",
            "crate::uuid",
            "Value::Date",
            "as in",
            "an inner join",
            "asks whether it is null",
            "in order",
            "the mode of a column",
            "keep it for 1 day",
        ]:
            with self.subTest(text=text):
                self.assert_passes({"lib.rs": f'const S: &str = "{text}";\n'})

    def test_reads_past_a_test_module_declared_as_a_file(self):
        errors = domain_errors(
            {
                "lib.rs": '#[cfg(test)]\nmod tests;\nconst S: &str = "ORDER BY";\n',
                "tests.rs": 'const T: &str = "SELECT";\n',
            }
        )
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("lib.rs:3", errors[0])

    def test_skips_a_test_module_file_and_its_folder(self):
        self.assert_passes(
            {
                "store/mod.rs": "#[cfg(test)]\nmod tests;\n",
                "store/tests/mod.rs": 'const T: &str = "SELECT";\n',
                "store/tests/more.rs": 'const T: &str = "SELECT";\n',
            }
        )

    def test_a_test_module_in_a_comment_skips_nothing(self):
        errors = domain_errors(
            {
                "lib.rs": "// #[cfg(test)] mod width;\n"
                '/* #[cfg(test)] mod note { */ const S: &str = "SELECT"; // }\n',
                "width.rs": 'const W: &str = "ORDER BY";\n',
            }
        )
        self.assertEqual(len(errors), 2, errors)

    def test_reads_past_an_inline_test_module_with_its_lines(self):
        source = (
            "#[cfg(test)]\nmod tests {\n"
            "    const C: char = '{';\n"
            '    const T: &str = "SELECT { x";\n'
            "    // }\n"
            "}\n"
            'const S: &str = "ORDER BY";\n'
        )
        errors = domain_errors({"lib.rs": source})
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("lib.rs:7", errors[0])

    def test_a_closing_brace_in_a_test_module_ends_nothing(self):
        source = (
            "#[cfg(test)]\nmod tests {\n"
            "    const C: char = '}';\n"
            '    const T: &str = "SELECT";\n'
            "}\n"
        )
        self.assert_passes({"lib.rs": source})

    def test_reads_a_test_item_that_is_no_module(self):
        errors = domain_errors({"lib.rs": '#[cfg(test)]\nconst S: &str = "SELECT";\n'})
        self.assertEqual(len(errors), 1, errors)

    def test_nested_comments_hide_their_quotes(self):
        source = '/* a /* b */ " */\nconst S: &str = "SELECT";\n/* " */\n'
        errors = domain_errors({"lib.rs": source})
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("lib.rs:2", errors[0])

    def test_lifetimes_are_no_char_literals(self):
        source = "fn f<'a>(x: &'a str) -> &'a str { x }\nconst S: &str = \"SELECT\";\n"
        self.assertEqual(len(domain_errors({"lib.rs": source})), 1)

    def test_a_file_that_is_no_rust_fails_without_hanging(self):
        for text in [
            b"/* never closed",
            b'const S: &str = "never;',
            b"\xff\xfe",
            b'r#"x',
        ]:
            with self.subTest(text=text):
                errors = domain_errors({"lib.rs": text})
                self.assertEqual(len(errors), 1, errors)
                self.assertIn("not read", errors[0])


class ChartKubernetesFloor(unittest.TestCase):
    def test_reads_the_floor_of_kube_version(self):
        for text, floor in [
            ('kubeVersion: ">=1.30.0-0"\n', "1.30.0"),
            ("name: straum\nkubeVersion: >=1.31.2\n", "1.31.2"),
        ]:
            with self.subTest(text=text):
                self.assertEqual(chart.oldest_kubernetes(text), floor)

    def test_refuses_a_kube_version_with_no_plain_floor(self):
        for text in [
            'kubeVersion: "<=1.30.0"\n',
            'kubeVersion: ">=1.30.0 <1.40.0"\n',
            "# kubeVersion: >=1.30.0\n",
            "name: straum\n",
        ]:
            with self.subTest(text=text):
                self.assertIsNone(chart.oldest_kubernetes(text))

    def test_the_chart_has_a_floor(self):
        text = (chart.CHART / "Chart.yaml").read_text()
        self.assertIsNotNone(chart.oldest_kubernetes(text))


class OpenApiCheck(unittest.TestCase):
    def document(self) -> dict:
        return json.loads(openapi.DOCUMENT.read_text())

    def test_the_document_is_valid(self):
        self.assertEqual(openapi.errors(self.document()), [])

    def test_refuses_a_broken_document(self):
        no_info = self.document()
        del no_info["info"]
        optional_path = self.document()
        optional_path["components"]["parameters"]["View"]["required"] = False
        undeclared = self.document()
        del undeclared["paths"]["/views/{name}/spec"]["parameters"]
        no_description = self.document()
        no_description["paths"]["/livez"]["get"]["responses"]["200"] = {}
        dangling = self.document()
        answer = dangling["paths"]["/ask"]["post"]["responses"]["200"]
        answer["content"]["application/json"]["schema"]["$ref"] = "#/nowhere"
        for name, broken in [
            ("no info", no_info),
            ("optional path parameter", optional_path),
            ("undeclared path parameter", undeclared),
            ("response with no description", no_description),
            ("dangling $ref", dangling),
        ]:
            with self.subTest(name=name):
                self.assertNotEqual(openapi.errors(broken), [])


class ChangedCrates(unittest.TestCase):
    def test_a_file_in_a_crate_names_the_crate_once(self):
        paths = [
            "crates/straum-pg/src/lib.rs",
            "crates/straum-domain/Cargo.toml",
            "crates/straum-pg/tests/a.rs",
        ]
        self.assertEqual(changed.crate_names(paths), ["straum-domain", "straum-pg"])

    def test_a_file_outside_a_crate_names_none(self):
        paths = ["crates", "crates/README.md", "docs/crates/x/y.md", "justfile", ""]
        self.assertEqual(changed.crate_names(paths), [])



def filter_errors(call: str) -> list[str]:
    """The test filter check's errors over a justfile holding `call`, and
    a straum-pg with three tests in `tests::decoding`: two ignored and
    one not."""
    files = {
        "lib.rs": "#[cfg(test)]\nmod tests;\n",
        "tests/mod.rs": "mod decoding;\n",
        "tests/decoding.rs": (
            '#[test]\n#[ignore = "slow"]\nfn follows_at_random() {}\n'
            "#[ignore]\n#[test]\nfn skips_at_random() {}\n"
            "#[test]\nfn decodes_in_order() {}\n"
            "fn helper_at_random() {}\n"
            "// #[test] fn commented_at_random() {}\n"
        ),
    }
    with tempfile.TemporaryDirectory() as folder:
        root = pathlib.Path(folder)
        for relative, text in files.items():
            path = root / "crates" / "straum-pg" / "src" / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        return filters.check(f"recipe: {call}\n", root)


class TestFilterCheck(unittest.TestCase):
    def assert_refused(self, call: str, words: str):
        errors = filter_errors(call)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn(words, errors[0])

    def test_a_module_path_to_a_test_passes(self):
        call = '(test "straum-pg" "--" "tests::decoding::decodes_in_order")'
        self.assertEqual(filter_errors(call), [])

    def test_part_of_a_test_name_passes_as_cargo_matches(self):
        call = '(test "straum-pg" "--lib" "at_random" "--" "--ignored")'
        self.assertEqual(filter_errors(call), [])

    def test_a_path_missing_its_module_fails(self):
        self.assert_refused(
            '(test "straum-pg" "--" "tests::follows_at_random")',
            "'tests::follows_at_random' selects no test",
        )

    def test_a_function_that_is_no_test_fails(self):
        call = '(test "straum-pg" "--" "helper_at_random")'
        self.assert_refused(call, "helper_at_random")

    def test_a_test_in_a_comment_fails(self):
        call = '(test "straum-pg" "--" "commented_at_random")'
        self.assert_refused(call, "commented_at_random")

    def test_a_crate_with_no_folder_fails(self):
        call = '(test "straum-nothing" "--" "follows")'
        self.assert_refused(call, "no crate folder")

    def test_flags_and_their_values_are_no_filters(self):
        call = '(test "straum-pg" "--release" "--test" "cost" "--" "--ignored")'
        self.assertEqual(filter_errors(call), [])

    def test_an_ignored_test_without_ignored_fails(self):
        call = '(test "straum-pg" "--" "follows_at_random")'
        self.assert_refused(
            call, "'follows_at_random' selects no test that is not ignored"
        )

    def test_an_ignore_before_the_test_attribute_counts(self):
        call = '(test "straum-pg" "--" "skips_at_random")'
        self.assert_refused(
            call, "'skips_at_random' selects no test that is not ignored"
        )

    def test_an_ignored_test_with_ignored_passes(self):
        call = (
            '(test "straum-pg" "--" "follows_at_random" "skips_at_random"'
            ' "--ignored")'
        )
        self.assertEqual(filter_errors(call), [])

    def test_a_test_not_ignored_with_ignored_fails(self):
        call = '(test "straum-pg" "decodes_in_order" "--" "--ignored")'
        self.assert_refused(call, "'decodes_in_order' selects no ignored test")

    def test_include_ignored_takes_both(self):
        call = (
            '(test "straum-pg" "--" "follows_at_random" "decodes_in_order"'
            ' "--include-ignored")'
        )
        self.assertEqual(filter_errors(call), [])

    def test_the_justfile_passes(self):
        justfile = (SCRIPTS.parent / "justfile").read_text()
        self.assertEqual(filters.check(justfile, SCRIPTS.parent), [])


def dry_run(recipe: str, crates: str) -> subprocess.CompletedProcess[str]:
    """What `just -n RECIPE CRATES` prints, against the repository's justfile."""
    return subprocess.run(
        ["just", "--justfile", str(SCRIPTS.parent / "justfile"), "-n", recipe, crates],
        check=False,
        text=True,
        capture_output=True,
    )


class CrateRecipes(unittest.TestCase):
    def test_no_crate_named_fails_and_says_how(self):
        for crates in ["", "  "]:
            with self.subTest(crates=crates):
                ran = dry_run("lint", crates)
                self.assertNotEqual(ran.returncode, 0)
                self.assertIn("name one, as in just test straum-pg", ran.stderr)

    def test_a_crate_named_takes_each(self):
        ran = dry_run("lint", "straum-pg straum-domain")
        self.assertEqual(ran.returncode, 0, ran.stderr)
        self.assertIn("cargo clippy -p straum-pg -p straum-domain ", ran.stderr)

    def test_all_takes_the_workspace(self):
        for recipe in ["lint", "test", "cov"]:
            with self.subTest(recipe=recipe):
                ran = dry_run(recipe, "all")
                self.assertEqual(ran.returncode, 0, ran.stderr)
                self.assertIn(" --workspace", ran.stderr)
                self.assertNotIn("-p ", ran.stderr)


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=1).result
    sys.exit(0 if result.wasSuccessful() else 1)
