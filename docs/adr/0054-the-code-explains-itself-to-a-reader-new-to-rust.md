# ADR-0054: The code explains itself to a reader new to Rust

Status: Accepted, 2026-11-15

## Context

ADR-0053 made the code pass clippy's readability lints, and it still
reads hard to someone who knows programming but little Rust. Closure
chains, lifetimes, generics and bare `usize` ids each ask for Rust
knowledge, and with only why-comments allowed, no file says what it
holds or where it sits in the flow.

## Decision

Every file opens with a plain-English header: what it holds and how
data moves through it. Every public type gets one line saying what it
is. Inside a function, comments still say only why. A guide page,
[Reading the code](../src/reading-the-code.md), explains the Rust the
code uses and the order to read the crates in.

Plain Rust is the default. A chain with a closure is a `for` loop. An
index into a list gets a named type, such as `FactIndex`, not a bare
`usize`. A lifetime, generic or `impl Trait` stays only where a plain
owned or concrete type won't do, with a comment saying why. Dense code
stays where a benchmark (`tests/cost.rs`, `bench/`, the WAL benchmark)
shows a clear gain, with a comment naming the measurement.

A file over 1000 lines is split by subject.

## Consequences

- A reader can start at any file and know where they are.
- The code grows: a loop takes more lines than a chain.
- Headers say what, so they go stale when the code moves; they change
  in the same commit as the code, like any document.
- Speed decides only where it was measured, so a hot path keeps its
  dense form and says why.
