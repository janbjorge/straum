# Straum: guide for AI coding agents

`docs/adr/` holds settled decisions. They win over this file.

## Commands

Scope every run to crates you touch. CI runs the whole workspace.

```sh
cargo fmt --all
cargo clippy -p <crate> --all-targets -- -D warnings
cargo test -p <crate>
cargo llvm-cov -p <crate> --all-targets --fail-under-lines 98
scripts/check-layers.py
```

Coverage: touched crates plus crates whose tests exercise them
(`straum-notation` tests much of `straum-domain`). No rustup? Set
`LLVM_COV` and `LLVM_PROFDATA` to the tools in `$(brew --prefix llvm)/bin`.

PostgreSQL tests need Docker. `STRAUM_TEST_POSTGRES` picks the major
(18 by default). The random workloads of the physical stream run
`STRAUM_TEST_SEEDS` seeds (6) from `STRAUM_TEST_SEED_FROM` (0); CI's
`correctness` job runs 50 new ones per major each run. `bench/taxi.py shell` runs Straum against
a throwaway PostgreSQL with the taxi trips and ends in `straum shell`.
Demo and try-out data is always the taxi trips, never a hand-written
dataset. In docs, ADR-0014 says which pages use the taxi trips and
which the `usage` view over `public.events AS e` and
`public.accounts AS a`.

- Change under `deploy/`: run `scripts/check-chart.py`. Needs uv,
  helm, the helm-unittest plugin and kubeconform.
- Change under `docs/`: run `mdbook build docs`. CI uses mdBook 0.5.4.

The `justfile` is the entry point to every tool: `just check`, `just fmt`,
`just lint`, `just test`, `just cov`, `just layers`, `just correctness`,
`just chart`, `just docs` (live preview), `just docs-build`,
`just taxi [asks|transactions|shell]` (`wal=physical` for the
physical WAL), `just taxi-month`,
`just wal-bench`. With no crate named, crate recipes take the crates
changed since HEAD. Every setting has a default, overridden on the
command line (`just postgres=17 seeds=50 correctness`) or through its
environment variable; `just --evaluate` shows them. A new script, check,
benchmark or setting gets a recipe or variable in the same commit, with
the same default as the code. Keep it tidy: one recipe per tool, in a
`[group]`, with a one-line comment; recipes are thin wrappers, logic
stays in the script or test; drop a recipe when its tool goes. Nothing
depends on it; the commands here and in `docs/src/performance.md` stay the
reference. Keep them in step.

## Documents

A change is not done until every document describing it is true again.
Fix documents in the same commit as the code. No follow-up commit. Do
not wait to be asked.

Before commit, `rg` `docs/`, `README.md` and `deploy/` for what changed:
old name, flag, message, default, number. Then check:

- `docs/src/` guide (pages in `SUMMARY.md`): what a user sees or does.
  Spec and ask syntax, error text, flags, endpoints, PostgreSQL setup,
  limits. ADR-0014 names each fact's home page; write it there and
  link to it from elsewhere.
- Design pages, `docs/src/diagrams/`: crates, machines, store, how a
  change moves through them.
- `docs/src/glossary.md`: term added, renamed or dropped.
- `README.md`: what Straum does, how to start it.
- `deploy/helm/straum/README.md`, `values.schema.json`: chart value
  added, renamed or given a new default.
- `docs/adr/`: a change against a record edits that record and moves
  its revised date (ADR-0001).
- This file: command, tool or rule changed.

End the report with the documents changed, or say none describes the
change.

## Rules

- Crates depend inward only; `scripts/check-layers.py` enforces it. Ask
  before changing its `ALLOWED` table.
- No `unsafe`, no macros of our own, traits only at ports. One
  exception: the counting allocator in `straum-domain/tests/cost.rs`
  (ADR-0013).
- Plain structs, enums, functions. Names and signatures follow the
  [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
- Write for a reader who doesn't know Rust (ADR-0013):
  - Whole words, units last (`timeout_ms`), verbs for functions.
  - Early returns with `let ... else`, so the main path stays flat.
  - A `for` loop, not a chain with a closure. Name a closure as a
    function instead of binding it to `let`.
  - Enums over `bool` parameters, named structs over tuples, named
    constants over magic numbers, named index types (`FactIndex`)
    over bare `usize`.
  - A lifetime, generic or `impl Trait` only where a plain type won't
    do, saying why.
  - Dense code only where a benchmark shows a clear gain, naming it.
  - Functions under 70 lines, files under 1000.
- Every crate, a new one too, takes `[workspace.lints]` with `[lints]
  workspace = true`. Silence a lint with `#[expect(.., reason = ..)]`,
  never `#[allow]`. One exception: `dead_code` on a helper module that
  several test binaries share, where `#[expect]` fails in the binary
  that uses all of it.
- Errors: hand-written enums with `Display` and `From` impls. No
  `thiserror`, no `anyhow`. Input never panics.
- The Straum SQL walker rejects anything it doesn't list, naming the
  construct. One spelling per meaning, nothing inferred (ADR-0004).
- Only `straum-http` runs tokio.
- Dependencies: check `std` first. Add a crate only for a capability
  `std` lacks or a measured material gain. State why beside it in
  `Cargo.toml`, or in an ADR if architectural. No convenience crates for
  small helpers. Review with `cargo tree --workspace -e normal`.
- `docs/src/glossary.md` names each thing once. Use its terms in docs,
  comments and error text. Its Clashes table lists renames still open.
- Each file opens with a `//!` header in plain English: what it holds
  and how data moves through it. Each public type gets one `///` line
  saying what it is. Other comments say only a why the code can't show:
  constraint, invariant, PostgreSQL quirk, rejected alternative. Never
  reference callers or tickets. Never leave `TODO` or commented-out
  code. `docs/src/reading-the-code.md` explains the Rust used; add to it
  when code needs a feature it doesn't cover.
- Every behaviour gets a positive and a negative test. State that
  changes over time gets a property test against a reference; a planted
  bug must make it fail.
- A settled decision gets a record in `docs/adr/`; ADR-0001 says how.
- Commit straight to `main`. One change per commit, short imperative
  subject. No branches, no PRs. The maintainer makes releases and tags
  on the release page (ADR-0012). Code goes in with the commit that
  first uses it.
- Use `git --no-pager`. Don't read `bench/data/` or `target/`.
- Wrap markdown at about 78 columns. No em dashes.
