# ADR-0053: Code reads plainly, and clippy holds it there

Status: Accepted, 2026-11-15

## Context

Agents write most of Straum's code, and people read it. The code leans
on one-letter names, closures bound to `let`, long `Option` and
`Result` chains and functions of 100 lines and more. A reader new to
Rust can follow none of it without an editor showing the types. A
written rule alone has not held: AGENTS.md asked for whole words, and
721 one-letter names came in anyway.

## Decision

Code is written for a reader who knows programming but not Rust. The
rules are in AGENTS.md: whole words, early returns with `let ... else`,
`match` or `if let` once a closure needs a statement, named functions
over closures bound to `let`, enums over `bool` parameters, named
structs over tuples, named constants over magic numbers, functions
under 70 lines.

Clippy enforces what it can. `[workspace.lints]` turns on the
`pedantic` group and `min_ident_chars`; `clippy.toml` sets the function
limit to 70 lines. A crate opts in with `[lints] workspace = true` in
the commit that makes it pass, so CI's `-D warnings` stays green while
the rest catch up. Once every crate is in, the lints hold for all.

The pedantic lints that ask for doc sections (`missing_errors_doc`,
`missing_panics_doc`, `must_use_candidate`) stay off: they contradict
the rule that comments say why, not what. `shadow_unrelated` stays off:
in tests it flags each step of `let (code, out) = run(..)`, which reads
fine.

## Consequences

- Agents get a build error instead of a rule they can skip.
- Long chains still pass clippy; that rule rests on review.
- A lint that is wrong in one place is silenced with `#[expect]` and a
  reason, so it warns once it is no longer needed.
- Until every crate opts in, the crates differ in what they check.
