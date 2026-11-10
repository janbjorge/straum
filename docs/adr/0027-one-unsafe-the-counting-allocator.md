# ADR-0027: One unsafe, the counting allocator in cost.rs

Status: Accepted (retroactive), 2026-11-10

## Context

Every crate forbids `unsafe`. The ask engine's property that an ask
allocates per group and never per cell is worth a test, and
`GlobalAlloc` is an unsafe trait, so no safe allocator can count. A
crate for it would hold the same lines.

## Decision

`crates/straum-domain/tests/cost.rs` holds the one `unsafe` in the
repository: a counting global allocator. The lib and binary crates keep
`#![forbid(unsafe_code)]`; this test is outside them and is the only
exception. Don't revisit.

## Consequences

- A per-cell allocation fails a test instead of showing as noise in
  the per-cell time budget.
- The exception is named in AGENTS.md so a reviewer doesn't reopen it.
