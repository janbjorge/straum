# ADR-0025: CLI help follows clig.dev

Status: Accepted (retroactive), 2026-11-10

## Context

The first usage text was one block for every command, printed on every
argument error, and it carried prose the guide already had.

## Decision

`straum help` lists the commands in a line each. `straum <command>
--help`, `-h` anywhere on the line, or `straum help <command>` prints
that command's usage, its flags with defaults and a link to its guide
page. An argument error prints the error and `try: straum <command>
--help`, never a manual. A bare `straum` prints the command list on
stderr with exit 2. Help names flags; the prose lives in the guide.

## Consequences

- Each fact about a flag has one home: its default in the help, its
  story in `docs/`.
- Help text is hand-written (ADR-0021), so a new flag is added in two
  places.
