#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = []
# ///
"""Reject Straum SQL in straum-domain's strings.

The domain holds what a spec and an ask mean, not how they are written:
straum-notation owns the syntax and the words that name it, error text
included. This lexes every Rust file in straum-domain/src, drops its
test modules, and reads each string literal with its escapes undone and
its runs of white space made one space, the pieces of a `concat!` joined
into one, and each char literal. It fails on SQL keywords, Straum SQL's
functions, its operators and casts, naming the file and line. Text that
`format!` builds at run time can't be read here.
"""

import pathlib
import re
import sys

import rust_source

ROOT = pathlib.Path(__file__).resolve().parents[1]
DOMAIN = ROOT / "crates" / "straum-domain" / "src"

# Keywords in uppercase only, as Straum SQL writes them: "as" and "in"
# are English words too. The ones the walker reads, then the ones it
# names when it refuses them, then SQL's other common ones.
KEYWORDS = (
    "SELECT|FROM|WHERE|GROUP BY|ORDER BY|HAVING|FILTER|JOIN|DISTINCT|EXISTS"
    "|WITHIN GROUP|IS NULL|IS NOT NULL|AND|OR|IN|AS|ON|NOT|WITH|LIMIT"
    "|OFFSET|CREATE VIEW|NULL|CAST|ASC|DESC|USING|NULLS FIRST|NULLS LAST"
    "|INNER|LEFT|RIGHT|FULL|OUTER|CROSS|LATERAL|ALL|ANY|OVER|WINDOW"
    "|PARTITION BY|FETCH|UNION|INTERSECT|EXCEPT|ROLLUP|CUBE|GROUPING SETS"
    "|TABLESAMPLE|QUALIFY|MATERIALIZED|TEMPORARY|OR REPLACE|INTO|VALUES"
    "|CASE|WHEN|THEN|ELSE|BETWEEN|LIKE|ILIKE|INTERVAL|ORDINALITY"
)
# Clauses that no English sentence needs, so in any case. "inner join"
# and "is null" name what a join or a test means, so stay out.
PHRASES = "group by|order by|partition by|within group|create view"
# The functions Straum SQL reads and the ones it names when it refuses
# them, in any case: the walker reads `COUNT(` as `count(`.
FUNCTIONS = (
    "date_bin|buckets|keep|count|sum|avg|min|max|bool_and|bool_or|var_samp"
    "|var_pop|stddev_samp|stddev_pop|percentile_disc|mode|percentile_cont"
    "|stddev|variance|every|coalesce|nullif"
)
CAST_TYPES = (
    "text|varchar|character varying|char|int|int2|int4|int8|smallint"
    "|integer|bigint|numeric|decimal|real|float|float4|float8"
    "|double precision|bool|boolean|date|time|timetz|timestamp|timestamptz"
    "|interval|uuid|json|jsonb|bytea"
)
# A Rust path ends in `::date` too: one led by these, by a capital, or
# by a second `::`, is no cast.
RUST_PATH_ROOTS = "crate|self|super|std|core|alloc"
SYNTAX = re.compile(
    rf"\b(?:{KEYWORDS})\b"
    rf"|\b(?i:{PHRASES})\b"
    rf"|\b(?i:{FUNCTIONS})\("
    # No English word, so bare too: "date_bin width".
    r"|\b(?i:date_bin)\b"
    r"|>=|<=|<>|!=|\|\|"
    # A comparison written between two terms.
    r"|\w\s+[=<>]\s+\w"
    # A parameter, as `$1` or as the rule's `$n`.
    r"|\$\s*(?:\d|n\b)"
    rf"|(?:(?<![\w:])(?!(?:{RUST_PATH_ROOTS})\b)[a-z_][\w.]*|\)|')"
    rf"\s*::\s*(?i:{CAST_TYPES})\b"
)
# A string that is nothing but an operator, as a table of them holds.
OPERATOR = re.compile(r"\s*(?:=|<|>|<=|>=|<>|!=|\|\||::)\s*")
# Char literals of Straum SQL's operators.
OPERATOR_CHARS = set("=<>!|$")

TEST_BLOCK_MODULE = re.compile(
    r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*(?:pub\s+)?mod\s+\w+\s*\{"
)
TEST_FILE_MODULE = re.compile(
    r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*(?:pub\s+)?mod\s+(\w+)\s*;"
)
CONCAT = re.compile(r"\bconcat\s*!\s*[\(\[\{]")


def module_folder(path: pathlib.Path) -> pathlib.Path:
    """The folder that holds the files of the modules `path` declares."""
    if path.name in ("lib.rs", "main.rs", "mod.rs"):
        return path.parent
    return path.parent / path.stem


def test_files(sources: dict[pathlib.Path, rust_source.Source]) -> set[pathlib.Path]:
    """The files of every test module declared as `mod name;`, and of
    every module beneath one."""
    found = set()
    for path, source in sources.items():
        for name in TEST_FILE_MODULE.findall(source.code):
            folder = module_folder(path)
            found.add(folder / f"{name}.rs")
            found.add(folder / name / "mod.rs")
            found.update((folder / name).rglob("*.rs"))
    return found


def test_blocks(code: str) -> list[range]:
    """The offsets of each inline test module, braces counted on the
    lexed code so a brace in a string or comment counts for nothing."""
    blocks = []
    for match in TEST_BLOCK_MODULE.finditer(code):
        blocks.append(range(match.start(), rust_source.closing(code, match.end() - 1)))
    return blocks


def texts(source: rust_source.Source, blocks: list[range]) -> list[tuple[int, str]]:
    """Each piece of text outside the test blocks, with its line: every
    string, every `concat!` joined, every char literal."""

    def outside(literal: rust_source.Literal) -> bool:
        return not any(literal.start in block for block in blocks)

    found = []
    for literal in source.strings:
        if outside(literal):
            found.append((literal.line, literal.text))
    for match in CONCAT.finditer(source.code):
        end = rust_source.closing(source.code, match.end() - 1)
        pieces = [s for s in source.strings if match.start() < s.start < end]
        if pieces and outside(pieces[0]):
            found.append((pieces[0].line, "".join(piece.text for piece in pieces)))
    for literal in source.chars:
        if outside(literal) and literal.text in OPERATOR_CHARS:
            found.append((literal.line, literal.text))
    return found


def syntax_in(text: str) -> str | None:
    """The Straum SQL in `text`, if any."""
    spaced = " ".join(text.split())
    if spaced in OPERATOR_CHARS or OPERATOR.fullmatch(text):
        return spaced
    match = SYNTAX.search(spaced)
    return match.group(0) if match else None


def check(domain: pathlib.Path, root: pathlib.Path) -> list[str]:
    """Each string in the Rust files under `domain` that holds Straum
    SQL, named by its path from `root` and its line."""
    errors = []
    sources = {}
    for path in sorted(domain.rglob("*.rs")):
        name = path.relative_to(root)
        try:
            sources[path] = rust_source.lex(path.read_text())
        except UnicodeDecodeError:
            errors.append(f"{name}: not UTF-8, so not read")
        except rust_source.LexError as error:
            errors.append(f"{name}:{error.line}: {error}, so not read")
    skipped = test_files(sources)
    for path, source in sources.items():
        if path in skipped:
            continue
        name = path.relative_to(root)
        for line, text in texts(source, test_blocks(source.code)):
            found = syntax_in(text)
            if found is not None:
                errors.append(f"{name}:{line}: {found!r} in {text!r}")
    return errors


def main() -> int:
    errors = check(DOMAIN, ROOT)
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
