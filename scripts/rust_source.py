"""Read Rust source as the check scripts need it: where its comments,
string literals and char literals lie, and where a file's modules lie.

`lex` walks the text once. It gives back the code with every comment
blanked and every literal masked, so a pattern over the code never
matches inside a comment or a string, and a brace or bracket count
never sees one; and each literal's value, escapes undone, with the line
it starts on. Text that ends inside a comment or a literal is no Rust
that compiles, and fails with its line rather than being read on.
"""

import bisect
import dataclasses
import pathlib
import re


@dataclasses.dataclass
class Literal:
    """A string or char literal: where it lies and its value."""

    start: int
    end: int
    line: int
    text: str


@dataclasses.dataclass
class Source:
    """Rust text split into its code and its literals."""

    # The text, each comment turned to spaces and each literal to a quote
    # and spaces, its newlines kept: offsets and lines still match.
    code: str
    strings: list[Literal]
    chars: list[Literal]


class LexError(Exception):
    """Text that is no Rust: a comment or literal never closed."""

    def __init__(self, line: int, what: str):
        super().__init__(f"line {line}: {what} never closed")
        self.line = line


IDENTIFIER = re.compile(r"[A-Za-z0-9_]")
RAW_STRING = re.compile(r'(?:b|c)?r(#*)"')
STRING = re.compile(r'(?:b|c)?"')
CHAR = re.compile(r"b?'")
ESCAPE = re.compile(
    r"\\(?:\n\s*|u\{([0-9A-Fa-f_]+)\}|x([0-9A-Fa-f]{2})|(.))", re.DOTALL
)
SIMPLE_ESCAPES = {"n": "\n", "r": "\r", "t": "\t", "0": "\0"}


def unescape(body: str) -> str:
    """The value of a string literal's `body`: escapes undone, and a
    backslash at a line's end dropped with the line break and the
    indent after it, as rustc does."""

    def replace(match: re.Match) -> str:
        unicode, byte, simple = match.groups()
        if unicode is not None:
            return chr(int(unicode.replace("_", ""), 16))
        if byte is not None:
            return chr(int(byte, 16))
        if simple is not None:
            return SIMPLE_ESCAPES.get(simple, simple)
        return ""

    return ESCAPE.sub(replace, body)


def mask(text: str, first: str) -> str:
    """`text` as `first` and then spaces, its newlines kept."""
    rest = re.sub(r"[^\n]", " ", text[1:])
    return first + rest if text else ""


class Lexer:
    def __init__(self, text: str):
        self.text = text
        self.breaks = [index for index, c in enumerate(text) if c == "\n"]
        self.code = []
        self.strings = []
        self.chars = []

    def line(self, offset: int) -> int:
        return bisect.bisect_right(self.breaks, offset - 1) + 1

    def starts_word(self, index: int) -> bool:
        """Whether `index` begins a token, not the middle of a name."""
        return index == 0 or not IDENTIFIER.match(self.text[index - 1])

    def run(self) -> Source:
        index = 0
        text = self.text
        while index < len(text):
            if text.startswith("//", index):
                end = text.find("\n", index)
                end = len(text) if end < 0 else end
                self.code.append(mask(text[index:end], " "))
                index = end
            elif text.startswith("/*", index):
                index = self.block_comment(index)
            elif self.starts_word(index) and (raw := RAW_STRING.match(text, index)):
                index = self.raw_string(index, raw)
            elif (self.starts_word(index) or text[index] == '"') and (
                plain := STRING.match(text, index)
            ):
                index = self.string(index, plain.end())
            elif self.starts_word(index) and (char := CHAR.match(text, index)):
                index = self.char_or_lifetime(index, char.end())
            else:
                self.code.append(text[index])
                index += 1
        return Source("".join(self.code), self.strings, self.chars)

    def block_comment(self, start: int) -> int:
        depth = 0
        index = start
        while index < len(self.text):
            if self.text.startswith("/*", index):
                depth += 1
                index += 2
            elif self.text.startswith("*/", index):
                depth -= 1
                index += 2
                if depth == 0:
                    self.code.append(mask(self.text[start:index], " "))
                    return index
            else:
                index += 1
        raise LexError(self.line(start), "block comment")

    def raw_string(self, start: int, opening: re.Match) -> int:
        closing = '"' + opening.group(1)
        end = self.text.find(closing, opening.end())
        if end < 0:
            raise LexError(self.line(start), "raw string")
        body = self.text[opening.end() : end]
        return self.literal(start, end + len(closing), body, self.strings)

    def string(self, start: int, body_start: int) -> int:
        index = body_start
        while index < len(self.text) and self.text[index] != '"':
            index += 2 if self.text[index] == "\\" else 1
        if index >= len(self.text):
            raise LexError(self.line(start), "string")
        body = unescape(self.text[body_start:index])
        return self.literal(start, index + 1, body, self.strings)

    def char_or_lifetime(self, start: int, body_start: int) -> int:
        text = self.text
        if text.startswith("\\", body_start):
            escape = ESCAPE.match(text, body_start)
            after = escape.end() if escape else body_start + 2
            if not text.startswith("'", after):
                raise LexError(self.line(start), "char literal")
            body = unescape(text[body_start:after])
            return self.literal(start, after + 1, body, self.chars)
        if body_start < len(text) and text.startswith("'", body_start + 1):
            return self.literal(start, body_start + 2, text[body_start], self.chars)
        # A lifetime or a loop label: `'a`, `'static`, `'outer:`.
        self.code.append(text[start:body_start])
        return body_start

    def literal(self, start: int, end: int, value: str, into: list) -> int:
        into.append(Literal(start, end, self.line(start), value))
        self.code.append(mask(self.text[start:end], '"'))
        return end


def lex(text: str) -> Source:
    """`text` split into its code and literals; LexError if a comment or
    literal never closes."""
    return Lexer(text).run()


def module_folder(path: pathlib.Path) -> pathlib.Path:
    """The folder that holds the files of the modules `path` declares, as
    rustc reads `#[path]` in an inline module too: the file's own folder
    for `lib.rs`, `main.rs` and `mod.rs`, else a folder named as the
    file."""
    if path.name in ("lib.rs", "main.rs", "mod.rs"):
        return path.parent
    return path.parent / path.stem


def closing(code: str, opening: int) -> int:
    """The offset just past the bracket that closes the one at `opening`
    in lexed `code`, or the code's end if none does."""
    pairs = {"(": ")", "[": "]", "{": "}"}
    stack = [pairs[code[opening]]]
    index = opening + 1
    while stack and index < len(code):
        character = code[index]
        if character in pairs:
            stack.append(pairs[character])
        elif character == stack[-1]:
            stack.pop()
        index += 1
    return index
