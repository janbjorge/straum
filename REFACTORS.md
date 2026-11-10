# Refactor tracker

Findings from a full read of the code on 2026-11-10, at commit `a6eaf23`.
Every item found has landed or is listed below as left alone;
`git log -- REFACTORS.md` finds the commits. Add new items as unticked
boxes, one behaviour-neutral commit each.

Gate, on the crates a commit touches:

```sh
cargo fmt --all
cargo clippy -p <crate> --all-targets -- -D warnings
cargo test -p <crate>
cargo llvm-cov -p <crate> --all-targets --fail-under-lines 98
python3 scripts/check-layers.py
```

## Leave alone

- The ask engine in `crates/straum-domain/src/store.rs` (`Test`, `Part`,
  `Groups`, the lanes in `Partials`): tuned, documented, property-tested,
  and `tests/cost.rs` guards its allocation profile.
- `Walker::conds` with three closures (`sql/mod.rs:298`): landed on
  purpose in 99e7bc1.
- The test `Rng` (four copies) and `id()` (eight copies): crate-local
  test helpers. A shared test crate would break the dependency rule in
  AGENTS.md.
- `fact.columns()` built three times per load
  (`crates/straum-pg/src/load.rs`): once would mean passing the list
  through `copy_sql`, `fact_types` and their tests to save three small
  allocations a load.
- `mib` in both `straum-app/src/app.rs` and `straum-http/src/status.rs`:
  two lines; exporting a formatter from straum-app costs more than the
  copy.
- The full catalog read twice per open
  (`crates/straum-pg/src/source.rs`): the first read, before the slot
  (f8378e1), reports every setup problem at once. Checking only
  `wal_level` and the publication there would have an admin fix the
  setup in rounds.
