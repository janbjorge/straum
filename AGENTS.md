# Straum: guide for AI coding agents

`docs/adr/` holds settled decisions. They win over this file.

## Commands

Scope every run to crates you touch. CI runs the whole workspace.

```sh
cargo fmt --all
cargo clippy -p <crate> --all-targets -- -D warnings
cargo test -p <crate>
cargo llvm-cov -p <crate> --all-targets --fail-under-lines 98
python3 scripts/check-layers.py
```

Coverage: touched crates plus crates whose tests exercise them
(`straum-notation` tests much of `straum-domain`). No rustup? Set
`LLVM_COV` and `LLVM_PROFDATA` to the tools in `$(brew --prefix llvm)/bin`.

PostgreSQL tests need Docker. `bench/taxi.py shell` runs Straum against
a throwaway PostgreSQL with the taxi trips and ends in `straum shell`.
With `--feed` it needs no Docker and no PostgreSQL: `straum-parquet`
feeds Straum the trips' Parquet files, in seconds, which is the way to
measure a change to Straum alone. Demo and try-out data is always the
taxi trips, never a hand-written dataset.

- Change under `deploy/`: run `python3 scripts/check-chart.py`. Needs
  helm, the helm-unittest plugin and kubeconform.
- Change under `docs/`: run `mdbook build docs`. CI uses mdBook 0.5.4.

The `justfile` wraps these commands for convenience: `just check <crate>`,
`just cov <crate>`, `just docs` (live preview), `just docs-build`,
`just chart`, `just taxi [asks|transactions|shell] [--feed]`,
`just taxi-month`. Nothing depends on it; the commands here and in
`docs/src/benchmark.md` stay the reference. Keep them in step.

## Documents

A change is not done until every document describing it is true again.
Fix documents in the same commit as the code. No follow-up commit. Do
not wait to be asked.

Before commit, `rg` `docs/`, `README.md` and `deploy/` for what changed:
old name, flag, message, default, number. Then check:

- `docs/src/` guide (pages in `SUMMARY.md`): what a user sees or does.
  Spec and ask syntax, error text, flags, endpoints, PostgreSQL setup,
  limits.
- Design pages, `docs/src/diagrams/`: crates, machines, store, how a
  change moves through them.
- `docs/src/glossary.md`: term added, renamed or dropped.
- `README.md`: what Straum does, how to start it.
- `deploy/helm/straum/README.md`, `values.schema.json`: chart value
  added, renamed or given a new default.
- `docs/adr/`: a change against an accepted record needs a new record
  superseding it (ADR-0001).
- This file: command, tool or rule changed.

End the report with the documents changed, or say none describes the
change.

## Rules

- Crates depend inward only; `scripts/check-layers.py` enforces it. Ask
  before changing its `ALLOWED` table.
- No `unsafe`, no macros of our own, traits only at ports. One
  exception: the counting allocator in `straum-domain/tests/cost.rs`
  (ADR-0027).
- Plain structs, enums, functions. Named fields over tuples, whole words
  over abbreviations. Names and signatures follow the
  [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
- Errors: hand-written enums with `Display` and `From` impls. No
  `thiserror`, no `anyhow`. Input never panics.
- The Straum SQL walker rejects anything it doesn't list, naming the
  construct. One spelling per meaning, nothing inferred (ADR-0029).
- Only `straum-http` runs tokio.
- Dependencies: check `std` first. Add a crate only for a capability
  `std` lacks or a measured material gain. State why beside it in
  `Cargo.toml`, or in an ADR if architectural. No convenience crates for
  small helpers. Review with `cargo tree --workspace -e normal`.
- `docs/src/glossary.md` names each thing once. Use its terms in docs,
  comments and error text. Its Clashes table lists renames still open.
- No doc comments by default; names and types carry the what. Comment
  only a why the code can't show: constraint, invariant, PostgreSQL
  quirk, rejected alternative. Never reference callers or tickets. Never
  leave `TODO` or commented-out code.
- Every behaviour gets a positive and a negative test. State that
  changes over time gets a property test against a reference; a planted
  bug must make it fail.
- A settled decision gets a record in `docs/adr/`; ADR-0001 says how.
- Commit straight to `main`. One change per commit, short imperative
  subject. No branches, no PRs. The maintainer makes releases and tags
  on the release page (ADR-0036). Code goes in with the commit that
  first uses it.
- Use `git --no-pager`. Don't read `bench/data/` or `target/`.
- Wrap markdown at about 78 columns. No em dashes.
