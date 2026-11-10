# Straum: guide for AI coding agents

[PLAN.md](PLAN.md) holds how we work and the dated decisions. It wins over
this file.

## Commands

Scope every run to the crates you touch. CI runs the whole workspace.

```sh
cargo fmt --all
cargo clippy -p <crate> --all-targets -- -D warnings
cargo test -p <crate>
cargo llvm-cov -p <crate> --all-targets --fail-under-lines 98
python3 scripts/check-layers.py
```

PostgreSQL tests need Docker. For a change under `deploy/`, also run
`python3 scripts/check-chart.py`, which needs helm, the helm-unittest
plugin and kubeconform. For a change under `docs/`, also run
`mdbook build docs`; CI builds the book with mdBook 0.5.4.

## Rules

- Crates depend inward only; `scripts/check-layers.py` enforces it. Ask
  before changing its `ALLOWED` table.
- No `unsafe`, no macros of our own, traits only at ports. The one
  exception is the counting allocator in `straum-domain/tests/cost.rs`,
  settled in PLAN.md.
- Errors are hand-written enums with `Display` and `From` impls, no
  `thiserror` or `anyhow`. Input never panics.
- The Straum SQL walker rejects anything it doesn't list, naming the
  construct.
- Only `straum-http` runs tokio.
- Before adding a direct dependency, check `std` first. Add a crate only
  for a capability `std` lacks or a measured material gain. State why
  beside the dependency in `Cargo.toml`, or in PLAN.md for an architectural
  decision. Do not add convenience crates for small helpers. Review
  dependency changes with `cargo tree --workspace -e normal`.
- Comments say why, not what.
- Every behaviour gets a positive and a negative test.
- Settled decisions get a dated line under Decisions in PLAN.md.
- Commit straight to `main`, one change per commit. No branches, no PRs.
- Milestone commits are `Mx: <title>`, with coverage in the body. Other
  commits use a short imperative subject.
- Use `git --no-pager`. Don't read `bench/data/` or `target/`.
- Wrap markdown at about 78 columns. Don't use em dashes.
