# Refactor tracker

Findings from a full read of the code on 2026-11-10, at commit `a6eaf23`.
Line numbers are as of that commit and drift as items land.

Each item is one behaviour-neutral commit with a short imperative subject.
Tick the box in the commit that lands it; `git log -- REFACTORS.md`
finds the commit.
Land them top to bottom unless a reason says otherwise: the first group
touches little, the later ones change public shapes and tests.

Gate, on the crates a commit touches:

```sh
cargo fmt --all
cargo clippy -p <crate> --all-targets -- -D warnings
cargo test -p <crate>
cargo llvm-cov -p <crate> --all-targets --fail-under-lines 98
python3 scripts/check-layers.py
```

## Small, low churn

- [x] **Drop `Walker.attrs`** (straum-notation).
  `crates/straum-notation/src/sql/mod.rs:67`, `sql/view.rs:138-193`,
  `sql/view.rs:681-685`. `select()` already puts every selected attribute
  on `j.r.attrs` (`view.rs:361-372`), and a FILTER attribute that is not
  selected is an error that returns from `view()` before `add_dim` runs
  (`view.rs:106`, `view.rs:373-387`). Let `add_dim` read `j.r.attrs`; the
  field, the `attr()` method and `self.attrs.clear()` go. With two refs to
  one dim in one view the dim's attrs come in ref order instead of SELECT
  order; nothing reads dim attrs by position.
- [x] **Fold `Agg::scale` into `Rollup::scale_of`** (straum-domain).
  `crates/straum-domain/src/agg.rs:38-43` is `input` when the aggregate
  reads numbers, else 0. In `rollup.rs:343-348` match `agg.state()`:
  `Some((kind, c)) if kind.numeric()` gives the column's scale, anything
  else 0. Delete `Agg::scale`.
- [x] **`Spec::resolve` becomes `Fact::resolve`** (straum-domain, straum-pg).
  `crates/straum-domain/src/spec.rs:307-320` ignores `self`. Callers:
  `ask.rs:61`, `ask.rs:84`, `store.rs:219`,
  `crates/straum-pg/src/catalog.rs:540`, tests `spec.rs:94-101`.
  `Store::slot` then needs no `self.spec`.
- [x] **`Store` lookup helpers and one convention for unknown facts**
  (straum-domain, straum-app). The fact lookup repeats at
  `crates/straum-domain/src/store.rs:52`, `59`, `65`, `73`, `94`, `102`.
  Add `fn rollup_mut(&mut self, fact) -> &mut Rollup` and
  `fn checked(&self, ask) -> Result<(&Fact, &Rollup), Vec<AskError>>`
  shared by `ask` and `cells_read`. `clear_fact` returns `Option<()>`
  (`store.rs:82`) while its siblings panic with `IN_SPEC`, and App then
  calls `.expect(IN_SPEC)` on it (`crates/straum-app/src/app.rs:261`):
  make it panic like the others.
- [x] **Merge `Partials::Sum` and `Partials::Avg`** (straum-domain).
  `crates/straum-domain/src/agg.rs:60-67` have one shape and are told
  apart only in `finish` (`agg.rs:209-217`) by arm order.
  `Sum { sums, counts, average: bool }`; `finish` reads
  `if count == 0 { Null } else if average { Avg } else { Int }`.
- [x] **Shared GROUP BY prologue** (straum-notation).
  `crates/straum-notation/src/sql/view.rs:826-835` and
  `sql/ask.rs:143-152` hold the same `Expressions` / `All` match. One
  `Walker::group_by_exprs(&mut self, at, group_by) -> Option<&[Expr]>`
  in `sql/mod.rs`.
- [x] **Partition `select()` instead of two passes** (straum-notation).
  `crates/straum-notation/src/sql/view.rs:293-326` loops
  `for measures in [false, true]` with `if is_key == measures { continue }`
  and `if !measures { reject }`. Partition the projection into keys and
  measures first (rejecting other items once), then one loop over the
  items sorted keys first, so a FILTER can name a null-ness key selected
  after it.
- [x] **`Change::table()` goes `pub`** (straum-app).
  `crates/straum-app/src/source.rs:108` is `pub(crate)`, so the fake
  copies it as `table_of` (`crates/straum-app/tests/fake/mod.rs:238`).

## Public shapes

- [x] **One `Option<Following>` in `State`** (straum-app).
  `crates/straum-app/src/app.rs:47-66` keeps `position`, `time`, `lag`
  and `wal` as four `Option`s set together (`app.rs:326-331`) and cleared
  together (`app.rs:300-303`). `Following { position, time, lag, wal }`
  makes "set with position" true by construction; `behind()` and
  `as_of()` move onto it, and the `STARTED` expects at `app.rs:179` and
  `app.rs:342` go (one may stay in `apply_next`). `Status` stays flat.
- [x] **`Answer::rows` as a struct, not a tuple** (straum-domain and
  every crate that reads answers). `crates/straum-domain/src/store.rs:28`
  is `Vec<(Vec<Datum>, Vec<Value>)>`; PLAN.md asks for named fields over
  tuples. `AnswerRow { keys: Vec<Datum>, values: Vec<Value> }`, named
  like `AnswerColumn`. `.0` / `.1`
  hits: `straum-domain/tests/store.rs`, `straum-app/tests/app.rs`,
  `straum-pg/src/tests.rs`, `straum-http/src/answer.rs:60`.
- [ ] **`last_failure` as a struct** (straum-app, straum-http).
  `crates/straum-app/src/app.rs:63` and `app.rs:122` hold
  `Option<(SystemTime, String)>`; `Failure { at, error }`. Readers:
  `straum-http/src/status.rs:56-59` and the tests beside each.
- [ ] **`KeyPath: FromStr`** (straum-domain, tests everywhere).
  `crates/straum-domain/src/spec.rs:167`; `Display` at `spec.rs:652`
  already round-trips. The `path("a.b")` helper is copied in
  `straum-domain/tests/store.rs:61`, `tests/spec.rs:54`,
  `tests/cost.rs:52`, `straum-app/tests/app.rs:131`,
  `straum-notation/tests/notation.rs:17` and inline in
  `straum-pg/src/tests.rs:437`.

## straum-pg

- [ ] **`check_table` takes a `Member`, not eight arguments**.
  `crates/straum-pg/src/catalog.rs:396-406` carries
  `#[allow(clippy::too_many_arguments)]`.
  `enum Member<'a> { Dim(&Dim), Fact(&Fact) }` yields the name, table,
  key width, roles and whether the table is streamed, so `Access`
  (`catalog.rs:273-281`) and the allow go, and the `relreplident` check
  at `catalog.rs:347-354` moves under `Member::Fact`. `check()` becomes
  two plain loops.
- [ ] **`Decoder` holds changes through a `Transaction`**.
  `crates/straum-pg/src/decode.rs:207-221` `hold()` is a free function
  taking three `self` fields, called at `decode.rs:86`, `108`, `125`,
  `139` to split borrows. `Transaction { changes, max_rows, lsn }` with
  `fn hold(&mut self, change)`: `self.transaction.hold(change)?` inside
  the `mappings(&self.relations, ..)` loop borrows disjoint fields.
- [ ] **`Lookup` keeps one column list**.
  `crates/straum-pg/src/resolve.rs:36-49` has `key`, `key_types`,
  `attrs`, `attr_types` as parallel Vecs and `fetch`
  (`resolve.rs:153-196`) branches on `i < width`. Replace with
  `columns: Vec<(Ident, ColumnType)>` plus `key_width`; `fetch` zips the
  fields with `columns` and `split_at(key_width)`.
- [ ] **One decimal formatter** (straum-domain, straum-http, straum-pg).
  `crates/straum-http/src/json.rs:107`,
  `crates/straum-pg/src/resolve.rs:214` and
  `crates/straum-pg/src/tests.rs:470` each print an integer in units of
  `10^-scale`, which `straum-domain/src/value.rs:11` defines. Add
  `pub fn decimal(units: i128, scale: u32) -> String` to domain
  `value.rs`; straum-http appends its bytes. (`straum-pg/src/value.rs:173`
  is the reader and stays.)

## Nits

- [ ] `crates/straum-domain/src/spec.rs:454`: `named.into_iter()` on an
  iterator; drop it.
- [ ] `crates/straum-pg/src/plan.rs:18` and `:25`: `dedup()` drops only
  neighbours, so `a, b, a` stays. Harmless today, since PostgreSQL skips a
  repeated table or schema, but say what is meant: `contains`, as
  `Spec::tables` does (`straum-domain/src/spec.rs:286-295`).
- [ ] `crates/straum-domain/src/rollup.rs:191`: `Cells::swap_remove`
  allocates `moved` to re-index. Update the index entry through
  `self.index.get_mut(&self.keys[last * w..][..w])` before
  `copy_within`; the fields are disjoint.
- [ ] `crates/straum-pg/src/load.rs:32`, `:75`, `:89`: `fact.columns()` is
  built three times per load; build it once in `fact()`.
- [ ] `crates/straum-app/src/app.rs:412` and
  `crates/straum-http/src/status.rs:84` both define `mib`; expose it from
  straum-app or accept the copy.
- [ ] `crates/straum-domain/src/store.rs:247`: `enum Source` shares a name
  with `straum_app::Source`; rename, e.g. `KeyOf`.
- [ ] `crates/straum-pg/src/catalog.rs:164-171`: `lists` calls
  `list(Some(l.to_owned()))`; let `list` take `&str`.
- [ ] `crates/straum-pg/src/source.rs:87` and `:105`: the full catalog is
  read twice per open, on purpose (f8378e1 checks before the slot). The
  first call could check `wal_level` and the publication alone.
- [ ] `crates/straum/src/main.rs:112-126`: six `Command::Run` fields are
  unpacked into six `run()` parameters; a `RunArgs` struct would cut it.
  Low value.

## Leave alone

- The ask engine in `crates/straum-domain/src/store.rs` (`Test`, `Part`,
  `Groups`, the lanes in `Partials`): tuned, documented, property-tested,
  and `tests/cost.rs` guards its allocation profile.
- `Walker::conds` with three closures (`sql/mod.rs:298`): landed on
  purpose in 99e7bc1.
- The test `Rng` (four copies) and `id()` (eight copies): crate-local
  test helpers. A shared test crate would break the dependency rule in
  AGENTS.md.
