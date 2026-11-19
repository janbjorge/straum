#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = []
# ///
"""Reject Straum SQL in straum-domain's strings.

The domain holds what a spec and an ask mean, not how they are written:
straum-notation owns the syntax and the words that name it, error text
included. This reads every string literal in straum-domain/src outside
its test modules and fails on SQL keywords, Straum SQL's functions and
its operators, naming the file and line.
"""

import pathlib
import re
import sys


ROOT = pathlib.Path(__file__).resolve().parents[1]
DOMAIN = ROOT / "crates" / "straum-domain" / "src"

# Keywords in uppercase only, as Straum SQL writes them: "as" and "in"
# are English words too.
KEYWORDS = (
    "SELECT|FROM|WHERE|GROUP BY|ORDER BY|HAVING|FILTER|JOIN|DISTINCT|EXISTS"
    "|WITHIN GROUP|IS NULL|IS NOT NULL|AND|OR|IN|AS|ON|NOT|WITH"
)
FUNCTIONS = (
    "date_bin|buckets|count|sum|avg|min|max|bool_and|bool_or|var_samp"
    "|var_pop|stddev_samp|stddev_pop|percentile_disc|mode"
)
SYNTAX = re.compile(
    rf"\b(?:{KEYWORDS})\b"
    rf"|\b(?:{FUNCTIONS})\("
    # No English word, so bare too: "date_bin width".
    r"|\bdate_bin\b"
    r"|>=|<=|<>|!=|::text|\$\d"
)
# The test module, by convention the end of a file, may say anything.
TEST_MODULE = "#[cfg(test)]"


def literals(source: str) -> list[tuple[int, str]]:
    """Each string literal in Rust `source` with the line it starts on."""
    found = []
    index = 0
    line = 1
    while index < len(source):
        character = source[index]
        if source.startswith("//", index):
            end = source.find("\n", index)
            index = len(source) if end < 0 else end
            continue
        if source.startswith("/*", index):
            end = source.find("*/", index) + 2
            line += source.count("\n", index, end)
            index = end
            continue
        raw = re.match(r'b?r(#*)"', source[index:])
        if raw:
            closing = '"' + raw.group(1)
            start = index + raw.end()
            end = source.find(closing, start)
            found.append((line, source[start:end]))
            line += source.count("\n", index, end)
            index = end + len(closing)
            continue
        if character == '"':
            start = index + 1
            end = start
            while source[end] != '"':
                end += 2 if source[end] == "\\" else 1
            found.append((line, source[start:end]))
            line += source.count("\n", index, end)
            index = end + 1
            continue
        # A char literal such as '"' would read as a string's start.
        char = re.match(r"'(?:\\.|[^\\'])'", source[index:])
        if char:
            index += char.end()
            continue
        if character == "\n":
            line += 1
        index += 1
    return found


def main() -> int:
    errors = []
    for path in sorted(DOMAIN.rglob("*.rs")):
        source = path.read_text()
        source = source.split(TEST_MODULE, 1)[0]
        for line, text in literals(source):
            match = SYNTAX.search(text)
            if match:
                name = path.relative_to(ROOT)
                errors.append(f"{name}:{line}: {match.group(0)!r} in {text!r}")
    if errors:
        print("straum-domain strings hold Straum SQL:", file=sys.stderr)
        print("\n".join(errors), file=sys.stderr)
        print(
            "word errors without syntax, or move the text to straum-notation",
            file=sys.stderr,
        )
        return 1
    print("domain syntax check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
