# Straum: guide for AI coding agents

`docs/adr/` holds the settled decisions. They win over this file.

## Commands

Scope every run to the crates you touch. CI runs the whole workspace.

```sh
cargo fmt --all
cargo clippy -p <crate> --all-targets -- -D warnings
cargo test -p <crate>
cargo llvm-cov -p <crate> --all-targets --fail-under-lines 98
python3 scripts/check-layers.py
```

Coverage runs over the touched crates and the crates whose tests exercise
them (`straum-notation` tests much of `straum-domain`). Without rustup,
set `LLVM_COV` and `LLVM_PROFDATA` to the tools in
`$(brew --prefix llvm)/bin`.

PostgreSQL tests need Docker. For a change under `deploy/`, also run
`python3 scripts/check-chart.py`, which needs helm, the helm-unittest
plugin and kubeconform. For a change under `docs/`, also run
`mdbook build docs`; CI builds the book with mdBook 0.5.4.

## Rules

- Crates depend inward only; `scripts/check-layers.py` enforces it. Ask
  before changing its `ALLOWED` table.
- No `unsafe`, no macros of our own, traits only at ports. The one
  exception is the counting allocator in `straum-domain/tests/cost.rs`
  (ADR-0027).
- Plain structs, enums and functions. Named fields over tuples, whole
  words over abbreviations. Names and signatures follow the
  [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
- Errors are hand-written enums with `Display` and `From` impls, no
  `thiserror` or `anyhow`. Input never panics.
- The Straum SQL walker rejects anything it doesn't list, naming the
  construct, and holds one spelling per meaning with nothing inferred
  (ADR-0029).
- Only `straum-http` runs tokio.
- Before adding a direct dependency, check `std` first. Add a crate only
  for a capability `std` lacks or a measured material gain. State why
  beside the dependency in `Cargo.toml`, or in an ADR for an
  architectural decision. Do not add convenience crates for small
  helpers. Review dependency changes with
  `cargo tree --workspace -e normal`.
- No doc comments by default; names and types carry the what. Write a
  comment only for a why the code can't show: a constraint, an
  invariant, a PostgreSQL quirk, a rejected alternative. Never reference
  callers or tickets, and never leave `TODO` or commented-out code.
- Every behaviour gets a positive and a negative test. State that
  changes over time gets a property test against a naive reference;
  planting a bug must make it fail.
- A settled decision gets a record in `docs/adr/`; ADR-0001 says how.
- Commit straight to `main`, one change per commit, with a short
  imperative subject. No branches, no PRs, no tags. Code goes in with
  the commit that first uses it.
- Use `git --no-pager`. Don't read `bench/data/` or `target/`.
- Wrap markdown at about 78 columns. Don't use em dashes.
