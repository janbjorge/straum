# Review of v0.10.2..3f55c0f

Open findings from the review of 2026-11-17 of the 122 commits from
`v0.10.2` to `3f55c0f` (+9,086, -2,171 lines), against `AGENTS.md`.
Line numbers are those of `08f163c` where the file was checked there, else
of `3f55c0f`. Find each place again by the symbol named before trusting
a line number.

The commits `cdce8f3`, `675b70a` and `08f163c` came in during the review.
`cdce8f3` was read for the documents only; `675b70a` (shell asks over
several lines) and `08f163c` (loopback by default, asks only as JSON)
were not reviewed. Review them the same way before closing this file.

## How to use this file

- One bullet is one open finding. Take one, fix it on `main` as its own
  commit (AGENTS.md: one change per commit, documents in the same
  commit, a positive and a negative test), and delete the bullet in that
  commit.
- A bullet proven wrong is deleted too; the commit message says why.
- A new finding goes at the end of its group with the next free id. An
  id is never used again, so a commit can name one.
- Delete a heading when its group is empty, and this file with the last
  bullet.
- Within a group the worst comes first.
- Run what AGENTS.md lists for the crates touched: `cargo fmt --all`,
  `cargo clippy -p <crate> --all-targets -- -D warnings`,
  `cargo test -p <crate>`, the 98% coverage gate,
  `scripts/check-layers.py`, and `mdbook build docs` for a change under
  `docs/`.

## How each finding was checked

- `[run]`: shown by running code, in a scratch crate outside the repo.
- `[lead]`: read and confirmed at HEAD by the lead reviewer.
- `[read]`: from reading only, by a slice reviewer. Reproduce it before
  fixing it.

## AGENTS.md limits

Measured at `08f163c` with a span count from signature to closing brace.
Clippy's `too_many_lines` counts code lines only, so CI does not catch
these.

- **R15** `[lead]` `crates/straum-notation/tests/rejected.rs` is 1,060
  lines (905 at `v0.10.2`), over the 1,000 cap, and
  `second_spellings_are_refused` (`:978`) is 73 lines. Fix: S8.
- **R16** `[lead]` `check_table` (`crates/straum-pg/src/catalog/mod.rs:527`)
  is 89 lines (72 at `v0.10.2`). Split out the publication and identity
  checks (`:577-605`).
- **R17** `[lead]` `physical_snapshot`
  (`crates/straum-pg/src/source.rs:204`) is 84 lines (71 at `v0.10.2`).
  Split out the snapshot retry loop (`:256-273`).
- **R18** `[lead]` `reports_wal_level_and_read_access`
  (`crates/straum-pg/src/tests/setup.rs:819`) is 71 lines. Move the
  standby and too-old block (`:869-887`) into its own test; the comment
  at `:869-871` sits above the wrong block too.
- **R19** `[lead]` Older functions still over 70: `plan`
  (`crates/straum-pg/src/plan.rs:28`, 72), `relations_that_change`
  (`decode.rs:643`, 73), `each_comparison_keeps_the_groups_it_holds_for`
  (`straum-domain/tests/store/having.rs:92`, 74). Split each when next
  touched, or now.
- **R20** `[lead]` Files near the cap, to split before the next test:
  `wal/heap/tests.rs` 978, `straum-domain/tests/cost.rs` 954 (S5 cuts
  it), `straum-app/src/app.rs` 952, `wal/transactions.rs` 926,
  `straum/tests/straum.rs` 926, `pg/src/tests/setup.rs` 909.

## AGENTS.md rules

- **R21** `[lead]` Stale restart wording contradicts ADR-0009 (Straum
  exits with 1, never loads again in the process). Each place says
  "loads again", "starting over" or "starts over":
  `deploy/helm/straum/values.yaml:38,59,65` (and `:41` "start it over",
  with `deploy/helm/straum/README.md:60`),
  `crates/straum/src/command_line.rs:36`,
  `crates/straum/tests/straum.rs:571`,
  `crates/straum-app/tests/app/changes.rs:121`,
  `crates/straum-app/src/app.rs:119`,
  `crates/straum-pg/src/tests/tls.rs:37`, `taxi_month.rs:151`,
  `logical.rs:5` and `logical.rs:309` in the same directory,
  `docs/adr/0008-...md:61` ("answers a start-over"),
  `bench/taxi.py:29-30`. Also `docs/src/design.md:72` and `:386-387`
  describe dropping the old store, which `Writer::start` no longer does,
  and `docs/src/diagrams/machines.svg:3,105` draw a writer opening again.
  Run `rg -n "starts? over|starting over|loads again|start-over"` across
  `crates docs deploy bench README.md` and fix each. ADR-0009:21 is
  history and may stay; its line 25 runs 121 columns.
- **R22** `[read]` `bench/try_your_data.py:452-481` prints each new
  `/readyz` reason on the premise that "Straum stays up when its setup
  check fails and tries again". It exits with 1 now, and the `running`
  check already sees that. Go back to `ready() -> bool`.
- **R23** `[read]` The WAL reading retry (`straum-pg/src/source.rs:476-483`,
  from `f26beaa`) contradicts `docs/src/deploy.md:68` ("never retries
  inside the process"), `docs/src/glossary.md` and
  `docs/src/design.md:65-66`, and ADR-0009's Consequences. Either name
  the one retry in each, or drop it. If kept, retry only on `Error::Io`:
  today a `Setup` error from `still_followed` runs the catalog queries
  twice.
- **R24** `[read]` Doc and comment lines the code contradicts:
  - `wal/heap/fixture.rs:5-6` places `capture_heap_fixture` in
    `tests/records.rs`; it is in `tests/heap_fixtures.rs`.
  - `rollup/cells.rs:176-178`: `recode` was put between
    `swap_remove`'s doc line and `swap_remove`. `recode`'s doc now opens
    with "Drops `cell`, the last cell taking its place." and
    `swap_remove` (`:194`) has none.
  - `problem.rs:33`, `problem.rs:203`, `source.rs:366` say "physical
    start", but `WaitProblem::SlotCreation` comes from the logical
    start (`source.rs:168`).
  - `catalog/followed.rs:30-34`: `still_followed` does not mention the
    altered columns `altered()` checks.
  - `wal/pipeline.rs:719` says "The changes of a commit, with every
    `Unfit` among them"; it returns only the `Unfit` changes, of all
    commits.
  - `straum-notation/src/sql/mod.rs:5-6` "collects every error" (it
    keeps the first `MAX_ERRORS`), `mod.rs:281-282` "folded to lower
    case as PostgreSQL does" (now refused), `tests/ask.rs:525`
    "`!=` in a comment" (comments are refused outright).
  - `straum-domain/src/store/mod.rs:75`: `Start::FromLast(0)` is said to
    act "as a Python slice does", but gives no rows where `rows[-0:]`
    gives all. The reference `sliced` (`tests/store/having.rs`) copies
    the formula, so it cannot catch it. Drop the Python claim or define
    0, and write the reference as an independent model.
  - `main.rs:393-395` "a panic aborts" holds only inside `run`.
- **R25** `[read]` No property test reaches a recode. `RECODE_FLOOR` is
  4,096 (`codes.rs:73`); the random workload (`tests/store/main.rs:39-41`)
  never gives a column that many codes. A planted bug in
  `Cells::recode`, `Codes::keep` or `Projection::reclaim_codes` is
  caught only by `answers_hold_across_a_recode`. `worth_recoding` and
  `Codes::keep` have no unit test. Fix: let the store take the floor as
  a value so the workload can run with a small one, and add unit tests
  for `Codes::keep` (code 0 stays 0, a dead code maps to `DEAD`).
- **R26** `[read]` Fast paths that name no benchmark (rule: dense code
  only where a benchmark shows a gain, naming it):
  `Distinct::OneCell` and `Gathered::OneCell` (`aggregate.rs:249-252`,
  `:285-286`), the dense-memo threshold (`store/ask.rs:276-279`),
  `KeyCodes` (`store/bound.rs:59-60`), the same-key `update_row` path
  (`change.rs:137`, no benchmark at all), `Wanted`
  (`store/lookup.rs:283-285`, cites 45 s). Name the test in
  `tests/cost.rs` that shows each gain, or add one, or drop the path.
- **R27** `[lead]` A closure bound to `let` at
  `crates/straum-domain/tests/cost.rs:676`
  (`let spec = || spec_measuring(...)`). Make it a named function.
- **R28** `[read]` Strings or bools where an enum belongs:
  `load(.., kind: &str, ..)` with `"dim"` and `"view"`
  (`straum-app/src/app.rs:554`), `ReplicaIdentity { kind: &'static str }`
  (`straum-pg/src/problem.rs:130`) and `Member.kind`
  (`catalog/mod.rs:517`) with `"fact"` and `"dim"`. One `TableKind` enum
  with `Display` serves all three. Also `for twice in [false, true]`
  (`straum/tests/straum.rs:871`) and `for rewrite in [false, true]`
  (`pg/src/tests/followed.rs:183-201`).
- **R29** `[read]` Tuples in signatures or bindings where a named struct
  belongs: `labels(..) -> Option<(&str, Vec<&str>)>`
  (`catalog/mod.rs:816`), `Vec<(Filter, &Vec<Condition>)>`
  (`catalog/mod.rs:763-767`), `resume -> (usize, u64)`
  (`wal/reader.rs:401`, return a `Gathering`), the 4-tuple of `Signal`s
  (`straum-http/src/lib.rs:149-157`), `Names::View(&'walk Scope, Clause)`
  (`straum-notation/src/sql/mod.rs:178`), and the tuple-bool patterns at
  `view.rs:555` and `joins.rs:239` (two `let ... else`).
- **R30** `[read]` Magic numbers:
  `head[17]` (`straum-notation/src/sql/ask.rs:527`),
  `0xA0 | 0xB0` (`wal/reader.rs:566`) and a second
  `XLOG_FULL_PAGE_IMAGE = 0xB0` (`wal/heap/tests.rs:25`), `0x10` for
  checkpoint online (`wal/record.rs:625`), `8_093`/`8_092`
  (`record.rs:596`), `> 400` (`fixture.rs:181`), `/ 10_000`
  (`pg/src/tests/setup.rs:828`), `25_i32` for text
  (`tests/protocol.rs:480`), `* 4` for a memo entry (`cost.rs:612`),
  the bare catalog codes `"r"`, `"u"`, `"f"`, `"v"`, `"e"`
  (`catalog/mod.rs:550,569,603,673,683,819`), the drain bounds
  `1..3` and `9..15` and the 2 s sleep (`straum/tests/straum.rs:895,922`),
  `Duration::from_secs(10)` beside `TIMEOUT`
  (`command_line/tests.rs:510`).
- **R31** `[read]` A lifetime with no why: `Followed<'a>`
  (`catalog/followed.rs:19`). S2 removes it.
- **R32** `[read]` Glossary: "a key column" in error text
  (`straum-notation` `columns.rs:296`, `tests/refs.rs:102`) where the
  glossary term is "Dim key". "Stops Straum" (ADR-0009 title,
  `cli.md`, `glossary.md`, `how-it-works.md`) also names the exit with 1,
  while "Stopping" is the SIGTERM state; add the clash to the Clashes
  table or pick one name. `Phase` (`app.rs:113`) has no glossary entry,
  and its `Following` and `Loading` share names with the `Following`
  struct and the Loading state.
- **R33** `[read]` Missing positive or negative tests: `is_chunked` with
  `Transfer-Encoding: identity` (`client.rs:574-576`); the trailing-bytes
  check of `fixture.rs:89`; a planted-bug case for
  `real_records_rebuild_the_rows_selected` (`fixture.rs:177`); the
  in-time body that `a_stalled_body_times_out`'s doc promises
  (`straum-http/tests/http/asks.rs:309-310`).
- **R34** `[read]` No `just` recipe for capturing the heap fixtures
  (AGENTS.md: a new tool gets a recipe). Add
  `just heap-fixtures` wrapping `STRAUM_TEST_POSTGRES={{ postgres }}
  cargo test -p straum-pg --lib capture_heap_fixture -- --ignored`, and
  list it in AGENTS.md.
- **R35** `[read]` Bench and docs disagree: `docs/src/performance.md:208`
  says "121 MiB resident", but on macOS `bench/taxi.py` prints the
  footprint (`held()`, `:339,359`); `performance.md:41-44` mixes the two
  methods and says "now"; "Memory grown" (`taxi.py:604,608`) still
  peaks on `rss`, which `held()`'s docstring says misses compressed
  pages; `performance.md:90-93` does not say the physical bench runs
  PostgreSQL at `wal_level = replica`; `bench/demo.py:45-46` pins
  `straum:0.10.1` as "the newest release" while `v0.10.2` is tagged
  (ADR-0012:64); `try_your_data.py` gained `--wal` with no just
  variable, while `justfile:21` says `wal` is for `taxi` only.
- **R36** `[read]` `docs/src/deploy.md:53-58`: the states table has no
  Stopping row; the glossary and `design.md:394` list five states.
- **R37** `[read]` Markdown lines past about 78 columns: `design.md`
  87, 92, 241, 370, 380; `asks.md:109`; `performance.md` 44, 113;
  `postgresql.md` 57, 161; `how-it-works.md` 132, 223; ADR-0009:25.
- **R38** `[lead]` The `justfile` recipes `changed` (line 31, a
  `cut | sort | paste` pipeline) and `_named` (line 114, a
  `[ -n ] || { ...; exit 1; }` body) are shell in a recipe. Both predate
  `v0.10.2`. Move `changed` into a Python script under `scripts/`;
  write `_named` with just's `error()`.

## Simplifications

Ordered by lines saved and clarity gained. Each keeps behaviour: run the
crate's tests before and after.

- **S1** `[read]` Bench helpers live twice (about 55 lines). `connected`,
  `median`, `duration` are the same in `bench/demo.py` and
  `bench/taxi.py`; `NEW_YORK`, `ROOT`, `DATA` too; `ask`, loading the
  trips and applying the plan differ only in a parameter. Move them to
  `bench/stack.py` or a new `bench/trips.py`. Also in `taxi.py`:
  `status_field` (`:611-623`) parses the text status, which `cli.md` says
  scripts must not; read `GET /status` as JSON. The `ALTER SYSTEM` plus
  `pg_reload_conf()` pair is written three times (`:246-264`). The
  `STRAUM_DSN` filter (`:217-224`) can become `--role straum`. The
  `wal_level` choice (`taxi.py:410`) duplicates `WAL_LEVELS` in
  `try_your_data.py:75`, and `try_your_data.py:122-124` duplicates
  `run_sql`.
- **S2** `[read]` Followed-table state is held twice (about 20 lines).
  `Postgres.followed`, `labels` and `versions` (`pg/src/source.rs:81-85`)
  copy `Snapshot` fields (`:449-451`), and `Followed<'a>`
  (`catalog/followed.rs:19-28`) borrows them back. `followed(&tables)`
  runs twice per logical start (`source.rs:188`, `:449`). One owned
  `Following { tables, labels, versions }`, built once in `snapshot()`.
- **S3** `[read]` Catalog parsing repeats (about 50 lines):
  - The `TableNames` literal from a `Checked` is written five times
    (`source.rs:320-323`, `catalog/followed.rs:67-70,133-136,222-225`,
    `decode.rs:200-203`). Add `Checked::names()`.
  - `checked(tables)` (`source.rs:383-385`) and `followed(&tables)`
    (`:572-576`) both give facts then dims. Keep one.
  - The row chain `<[Option<String>; N]>::try_from(row).map_err(..)?
    .map(Option::unwrap_or_default)` is in four places and the OID parse
    in seven (`catalog/followed.rs`, `catalog/query.rs`). Add
    `fn oid(text, what) -> Result<Oid, Error>` in `catalog/mod.rs`.
  - Three builders of `(schema, table)` lists: `catalog/query.rs:35-45`,
    `physical.rs:344-354`, `catalog/followed.rs:42-49`. One beside
    `catalog::escaped`.
  - `query.rs:124-130` compares `== Some("t")` where the same function
    uses `is_true`; `Checked::column_type` (`catalog/mod.rs:63-69`) scans
    the whole list where it could return on the first match;
    `followed.rs:207-218` has two lookup loops that never `break`.
  - IPv6 bracketing is written twice (`dsn.rs:149-154`,
    `wire.rs:455-461`). Add `Dsn::place()`.
- **S4** `[read]` straum-pg test setup repeats (about 100 lines):
  - `Postgres::new(Dsn::parse(..).unwrap(), name, ROW_LIMIT,
    MAX_RETAINED_WAL_BYTES, Replication::Logical)` beside the helper
    `logical_source` (`tests/setup.rs:668-676`): `setup.rs:65,615,896`,
    `protocol.rs:35,49,88`. Move the helper to `support.rs`.
  - `load_all` (`tests/support.rs:707-715`) duplicates `load_everything`
    (`tests/watchdog.rs:494-509`), and `tests/followed.rs:154-160`
    inlines it. Return the store from one helper.
  - The read-until-the-switch-record loop is in six places
    (`tests/heap_fixtures.rs:51-75`, `records.rs:44-55,276-289`,
    `decoding.rs:625-645`, `transactions.rs:86-97`). One
    `support::records_to_now`.
  - Step-until-error loops with a deadline: `tests/followed.rs:122-129`,
    `physical.rs:111-118`, `watchdog.rs:342-349`. One helper.
  - Parsing the major from `tag()`: `physical_setup.rs:62-66`,
    `physical.rs:180`, `setup.rs:826`. One `support::major()`.
  - The "no longer fits" message built three times
    (`tests/logical.rs:525-532,555-562,685-692`), misindented.
  - `TABLE_OID = 77` and the `Checked { oid: TABLE_OID, .. }` builder in
    `wal/pipeline.rs` and `wal/messages.rs` tests. Move to
    `test_support`.
  - `ALL_WAL` (`tests/watchdog.rs:25`) duplicates `LOGICAL_FROM`
    (`source.rs:50`).
  - `.replace("TABLE", "UNLOGGED TABLE")` (`physical_setup.rs:225`):
    give `tables()` a persistence argument.
  - `follows_a_vacuum_on_19_without_checksums` (`tests/physical.rs:156`)
    starts PostgreSQL 19 in every major's job. Guard it with `tag()`.
- **S5** `[read]` `tests/cost.rs` (about 50 lines, and it sits at 954):
  six heap-peak helpers with one body (`peak :487`,
  `repeated_filter_peak :552`, `borough_filter_peak :580`,
  `by_every_key_peak :621`, `by_bucket_peak :659`, `peak_by_borough
  :789`) become one `ask_peak(store, ask, window)`. Rows hand-built by
  position (`:600-610`, `:772-783`, `:828-834`, `:886-892`) duplicate
  `trip_row` (`:756-768`) and hang on column order.
- **S6** `[read]` straum-domain source (about 40 lines):
  - `condition_type` (`store/ask.rs:188-204`) is redundant since the
    sparse join memo of `2fb80ed`: `lookup(.., 0).column_type(..)`
    allocates nothing.
  - `distinct_none` and `gathered_none` (`aggregate.rs:628-636`) become
    `#[derive(Default)]` with `#[default] None`.
  - The `DimChange` function pointer and `DimRowChange<'r>`
    (`store/change.rs:13-22`) become an enum `Add(row)` / `Remove(row)`.
  - `update_row`'s slow path reclaims codes twice; call the
    `_coded` functions and reclaim once. `_coded` is the wrong name:
    they apply a change before the reclaim.
  - The cells an ask reads are counted three times
    (`store/ask.rs:93,110,135-136`). Count once in `ask_window`.
  - `condition_checks` (`store/ask.rs:328-356`) keeps a `keys` vector
    beside `checks` and scans it per condition. Store the key on
    `ConditionCheck` and drop `position_of_key`.
  - `Rows` (`cell_loop.rs:174-180`) repeats `WindowAnswer`.
  - `Wanted.bools: [bool; 2]` indexed by `usize::from(flag)`
    (`lookup.rs:291,306,328`): two named fields.
  - `DEAD` and `RECODE_FLOOR` (`codes.rs:69,73`) are `pub(crate)` but
    used only in `codes.rs`.
  - The aggregate test's `shuffled` (`aggregate.rs:720-721`) is reversed,
    not shuffled; use the `Rng` in scope.
- **S7** `[read]` straum-domain behaviour worth a measurement, not a
  line count: `answer_rows` (`cell_loop.rs:145-156`) holds every group
  twice under HAVING (compact `kept` in place); `ConditionCheck` still
  keeps a dense memo per code (`store/ask.rs:339`), unlike the join
  memo; `Rollup::reclaim_codes` walks every `date_bin` cell once per
  part (`rollup/mod.rs:434-463`).
- **S8** `[read]` Move the ask cases out of
  `straum-notation/tests/rejected.rs` (`:1011-1050`) into
  `tests/ask.rs` with `assert_each_names_what_is_wrong`; `errors_of`
  (`rejected.rs:1052-1059`) duplicates `errors` (`tests/ask.rs:24`).
  Fixes R15.
- **S9** `[read]` straum-notation source (about 45 lines):
  - Key column resolution lives in `columns.rs:296-320` and
    `joins.rs:419-434`, with `Near::LeftKey` and two ways to format the
    left-key message. One `key_column_read` serves both. Fixes R2.
  - `spelled_with_bang` (`ask.rs:630-645`) tokenizes again, though
    comments are refused before the parse; `between.trim() == "!="` will
    do, and its doc is stale.
  - `parse_width` (`view.rs:550-579`): `plain_digits` adds nothing, since
    `text != width_text(seconds)` already refuses `+1 hour` with the
    better message; the `UNITS` loop lacks a `break`.
  - The `Clause` to text match is written twice
    (`columns.rs:355-358,384-387`); give `Clause` a `keyword()`.
    `Clause` derives `Debug, PartialEq, Eq` it never uses
    (`mod.rs:183`).
  - `extra_tokens` (`ask.rs:603`) tokenizes the ask once more beside
    `statements` (`mod.rs:60`); pass the tokens.
  - `ask.rs:655-656` clones `name` twice; `ask.rs:176-179` works out the
    GROUP BY length again.
  - `tests/refs.rs:604,636,647` repeat one `.replace(..)`.
- **S10** `[read]` straum CLI (about 45 lines):
  - `exchange` returns its own `Exchange` enum that `fetch_timed` maps to
    `ClientError` (`client.rs:497-513,667-673`). Return
    `Result<String, ClientError>` from `exchange`.
  - `phases` (`client.rs:590-607`) reimplements `header`'s
    case-insensitive loop (`:561-571`); add `header_values`. Name
    `"content-length"` and `"transfer-encoding"` as `SERVER_TIMING` is.
  - `Display for Server` (`client.rs:42-46`) serves one `println!`.
  - The view name check is written twice (`client.rs:304-307,323-326`).
  - `command_line.rs:397-401` strips a leading `--` that `query_at`
    could skip (`:354`).
  - `client/tests.rs`: the "Nothing listens on port 1" assertion three
    times (`:151-159,331-345,451-459`); the same `ask(..)` four times
    (`:236,251-257,279-285,324-329`); `:460-467` repeats `:427-431`;
    `asks_for_the_views` (`:111-133`) tests readiness, which
    `says_whether_straum_is_ready` covers. The holder thread at `:84-88`
    sleeps three times the timeout.
  - `tests/straum.rs`: `exited` (`:320-332`) reimplements `wait()`'s poll
    loop; name the 60 s and 100 ms (`:250,259,327,331,614-615`).
    `silent_server` could return its address instead of the test parsing
    the DSN (`:686-691`).
  - `"whole seconds above 0"` is written twice (`command_line.rs:446,634`).
- **S11** `[read]` straum-app and straum-http (about 20 lines): the
  applied LSN is kept in `State.applied` and `Following.applied`
  (`app.rs:71,88`), hence the `FOLLOWING` invariant; `Format::errors`
  repeats `lines()` (`routes.rs:94-98,203-209`); `App` is wrapped in an
  `Arc` it does not need (`straum-http/src/lib.rs:103`,
  `routes.rs:114`); the spec is read three ways (`app.rs:262,310,519`)
  where `self.spec` serves; the "no view" text is written twice
  (`routes.rs:226-227,235`); `DRAIN_DEADLINE` (`lib.rs:64`) need not be
  `pub`. Recheck each against `cdce8f3` and `08f163c`, which changed
  these files.
- **S12** `[read]` straum-pg WAL (about 30 lines):
  - `pipeline.rs` flushes a pending `Unfit` in `read()` (`:140-178`)
    and `see()` (`:260-275`); one `flush_unfit`. Build the carrier only
    when `unfit_pending()`.
  - `reader.rs:275` reads the length back from `bytes[RECORD_LENGTH]`,
    leaning on state kept across feeds; `gathering.length` already holds
    it. Removes a fragile index too.
  - The page image check is written three times (`record.rs:309-310`,
    `reader.rs:565-566`, `heap/tests.rs:25`); one
    `is_page_image(resource_manager, info)`.
  - `reader.rs` `wait` (`:417-425`) always returns `None`.
  - `TransactionSnapshot::precedes` and `widen`
    (`transactions.rs:427-439`) repeat `follows`; `seeing_nothing()`
    (`:407`) uses `xmax: 0` as a sentinel, set again on every
    `RUNNING_XACTS` (`pipeline.rs:162-167`).
  - `read_to: u64` (`pipeline.rs:55`) is wrapped as `Lsn` at `:250`;
    store an `Lsn`.
  - The fixture format carries `major`, which repeats the `FIXTURES`
    key (`fixture.rs:30,42,70,133`), and `LoggedRecord.end`, which
    `Heap` never reads. Dropping both needs the fixtures captured again.
- **S13** `[lead]` The untracked `scripts/audit-progress.py` and the
  uncommitted `just audit` recipe hard-code a copy of `AUDIT.md` in
  `BASELINE`, a file its own rules say to delete with its last bullet.
  Do not commit them; `rg -c '^- \*\*[A-Z]+[0-9]+\*\*' AUDIT.md` counts
  the open findings.

## Checked and found sound

So nobody checks these again without cause.

- No `#[allow`, `TODO`, `unsafe`, macro of our own or `docker run` in the
  range. Each `impl Trait` and lifetime carries its why, but for
  `Followed<'a>` (R31).
- No `unwrap` or `expect` on input outside tests; the new ones guard
  internal invariants. No new panic on server or WAL bytes found.
- Every Docker call goes through `docker compose` with a project named
  per run; no testcontainers.
- `straum-app` runs no tokio. No new dependency in the range.
- The split of `catalog.rs`, `complete.rs` and `client.rs` only moved
  code and tests.
- `cdce8f3` removed the in-process start-over machinery (`Backoff`,
  the `starts` and `failures` counts) from code and most documents.
- No em dashes in the range. Each ADR the range edits moved its revised
  date. ADR-0014's split between taxi pages and `usage` pages holds.
- Every `STRAUM_*` variable is in the justfile or the docs. The error
  texts in `docs/src/postgresql.md` match `problem.rs`.
