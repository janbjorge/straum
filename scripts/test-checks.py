#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = []
# ///
"""Tests of the check scripts: each rule they hold catches a planted
break and lets the honest form through.

The domain syntax check reads a folder of Rust files; these tests write
one. Many cases here are ways past a check that a red team found; each
stays so the way stays shut.
"""

import collections.abc
import importlib.util
import pathlib
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


syntax = load("check-domain-syntax")


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
            "SELECT", "FROM", "WHERE", "GROUP BY", "ORDER BY", "HAVING",
            "FILTER", "JOIN", "DISTINCT", "EXISTS", "WITHIN GROUP",
            "IS NULL", "IS NOT NULL", "AND", "OR", "IN", "AS", "ON", "NOT",
            "WITH", "LIMIT", "OFFSET", "CREATE VIEW", "NULL", "CAST", "ASC",
            "DESC", "OVER", "LEFT", "OUTER", "UNION", "CASE", "BETWEEN",
            "LIKE", "INTERVAL",
        ]:
            cases.append((f'const S: &str = "write {word} here";', word))
        for function in [
            "date_bin", "buckets", "count", "sum", "avg", "min", "max",
            "bool_and", "bool_or", "var_samp", "var_pop", "stddev_samp",
            "stddev_pop", "percentile_disc", "mode", "percentile_cont",
            "stddev", "variance", "every",
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
            "a count of rows", "count (of rows)", "every (key, value) pair",
            "checked by Spec::new", "jiff::civil::date", "crate::uuid",
            "Value::Date", "as in", "an inner join", "asks whether it is null",
            "in order", "the mode of a column",
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
        for text in [b"/* never closed", b'const S: &str = "never;', b"\xff\xfe", b'r#"x']:
            with self.subTest(text=text):
                errors = domain_errors({"lib.rs": text})
                self.assertEqual(len(errors), 1, errors)
                self.assertIn("not read", errors[0])


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=1).result
    sys.exit(0 if result.wasSuccessful() else 1)
