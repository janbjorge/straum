# ADR-0013: Code reads plainly, and clippy holds it there

Status: Accepted 2026-11-10; revised 2026-11-17

## Context

Agents write most of Straum's code, and people read it. It leaned on
one-letter names, closures bound to `let`, long `Option` chains and
100-line functions; a written rule alone didn't hold, and 721
one-letter names came in after AGENTS.md asked for whole words. Every
crate forbids `unsafe`, yet testing that an ask allocates per group
and never per cell, or that the physical stream allocates a fixed few
times per row, needs a counting allocator, and `GlobalAlloc` is an
unsafe trait.

## Decision

Code is written for a reader who knows programming but not Rust. The
rules live in AGENTS.md: whole words, `let ... else` early returns, a
`for` loop over a closure chain, named functions over closures bound
to `let`, enums over `bool` parameters, named structs over tuples,
named constants, named index types (`FactIndex`) over `usize`. A
lifetime, generic or `impl Trait` stays only where a plain type won't
do, saying why. Dense code stays only where a benchmark
(`tests/cost/main.rs`, `bench/`, the WAL benchmark) shows a gain, naming
it. Functions stay under 70 lines, files under 1000.

Every file opens with a plain-English header saying what it holds and
how data moves through it; every public type gets one line. Other
comments say only why. `docs/src/reading-the-code.md` explains the
Rust used and the order to read the crates in.

Clippy enforces what it can. `[workspace.lints]` turns on `pedantic`
and `min_ident_chars`, every crate takes it, and `clippy.toml` caps
functions at 70 lines. `missing_errors_doc`, `missing_panics_doc` and
`must_use_candidate` stay off, against the why-only comment rule;
`shadow_unrelated` stays off, as it flags test steps like `let (code,
out) = run(..)`. A lint is silenced with `#[expect]` and a reason.

Two counting global allocators are the only `unsafe` in the
repository: `crates/straum-domain/tests/cost/main.rs`, and
`crates/straum-pg/src/wal/test_support.rs`, whose WAL internals only
the crate's own test build reaches. Library and binary crates keep
`#![forbid(unsafe_code)]`; `straum-pg`'s test build has `deny` instead,
so that one allocator can carry an `#[expect]`. Don't revisit.

## Consequences

- Agents get a build error, not a rule to skip. Long chains still pass
  clippy, so that rule rests on review.
- A reader can start at any file. The code grows: a loop takes more
  lines than a chain.
- Headers say what, so they change in the same commit as the code.
- A hot path keeps its dense form and says which measurement keeps it.
- A per-cell allocation, or one more per WAL row, fails a test instead
  of hiding in timing noise.
