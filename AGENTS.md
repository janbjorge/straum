# Straum: guide for AI coding agents

`docs/adr/` holds settled decisions. They win over this file.

## Commands

Every tool runs through the `justfile`, locally and in CI. Docs name
the recipe, not the command under it; `just --show RECIPE` prints that.
Scope every run to crates you touch; CI runs `all`.

```sh
just fmt
just lint <crate>
just test <crate>
just cov <crate>
just scripts
just check <crate>   # all of the above but cov
```

With no crate named, crate recipes take the crates changed since HEAD;
`all` takes the workspace. A change to a check script changes
`scripts/test-checks.py` with it: a planted break for each rule it
adds, and the honest form passing.

Coverage: touched crates plus crates whose tests exercise them
(`straum-notation` tests much of `straum-domain`). No rustup? Set
`LLVM_COV` and `LLVM_PROFDATA` to the tools in `$(brew --prefix llvm)/bin`.

PostgreSQL tests need Docker with Compose. `postgres` picks the major
(18 by default). The random workloads of `just correctness`, the
physical stream against a logical slot and from a random start, and
the logical stream from a random start, run `seeds` seeds (6) from
`seed_from` (0); CI's `correctness` job runs 50 new ones per major each
run. The heap unit
tests replay records captured per major in
`crates/straum-pg/src/wal/heap/fixtures/`; after a WAL layout change,
capture them again for each major with `just postgres=<major>
heap-fixtures`. `just taxi shell` runs Straum against a throwaway
PostgreSQL with the taxi trips and ends in `straum shell`. Demo and
try-out data is always the taxi trips, never a hand-written dataset. In
docs, ADR-0014 says which pages use the taxi trips and which the
`usage` view over `public.events AS e` and `public.accounts AS a`.

- Change under `deploy/helm/`: run `just chart`. Needs uv, helm, the
  helm-unittest plugin and kubeconform.
- Change to an HTTP route or a JSON reply: change
  `crates/straum-http/openapi.json` with it. straum-http's tests check
  the replies against it; `just scripts` checks it is valid OpenAPI.
- Change under `docs/`: run `just docs`; `just docs serve --open`
  previews. CI uses mdBook 0.5.4.
- Change to the Dockerfile or `compose.yaml`: run `just image`.

`just audit` checks `Cargo.lock` against the RustSec advisories; it
needs cargo-audit. Dependabot opens weekly pull requests for the
pinned actions and the Cargo dependencies.

`just --list` names every recipe: the checks above, `just correctness`,
`just heap-fixtures`, `just image`, `just taxi
[asks|transactions|materialized|load|retention|shell]` (`wal=physical` for the
physical WAL), `just taxi-month`, `just wal-bench`, `just cost-bench`,
`just demo`, `just outbox`, `just try SPEC --data DIR`. Every setting
has a default, overridden on the command line (`just postgres=17
seeds=50 correctness`) or through its environment variable; `just
--evaluate` shows them.

A new script, check, benchmark or setting gets a recipe or variable in
the same commit, with the same default as the code, and CI calls the
recipe, never the command under it. Keep it tidy: one recipe per tool,
in a `[group]`, with a one-line comment; recipes are thin wrappers,
logic stays in the script or test. A recipe that runs a cargo test
calls `test` with its arguments rather than repeating cargo; each
filter names a test as `cargo test -- --list` does, and
`scripts/check-test-filters.py` fails on one that selects none. A recipe
that passes free arguments on takes `[positional-arguments]` and passes
`"$@"`, so an argument with a space stays one; `test` is the
exception, since its first argument is the crates and its others are
cargo's test filters, which hold no space. `set lazy` keeps a
backtick from running until a recipe reads it. `just fmt` formats the
justfile too. The guard against a crate recipe with no crate lives in
the justfile, and `scripts/test-checks.py` tests it under `just -n`.
The script is Python, run with uv, as `scripts/` and `bench/` are. No
shell script: no `.sh` file, and no bash body in a recipe. Drop a
recipe when its tool goes, and fix every doc that names it.

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
- `ROADMAP.md`: work shipped, started, parked or dropped.
- This file: command, tool or rule changed.

End the report with the documents changed, or say none describes the
change.

## Rules

- No shell scripts. A tool is a Python file run with uv, and its just
  recipe is a thin wrapper. No `.sh` file, and no bash body in a recipe.
- Every Docker command goes through `docker compose`: tests, scripts,
  benchmarks, CI and docs. One project per run or test, named so runs
  side by side never share a network, container name or port. Code
  builds the Compose file as JSON and pipes it to `docker compose -f -`;
  docs ship the files in `docs/compose/`; the image builds and pushes
  through the root `compose.yaml`. Reach a running project by name:
  `docker compose -p NAME exec`, `port`, `ps`, `logs`, `cp`. No plain
  `docker run`, `exec`, `build`, `network` or the like, and no
  testcontainers. The one exception is `docker login`, which Compose
  lacks.
- Crates depend inward only; `scripts/check-layers.py` enforces it,
  over Cargo's metadata with every feature on and the lexed source:
  normal and build dependencies as its `ALLOWED` table lists,
  dev-dependencies never outward, each target built from its own
  crate, no outside crate and no `std` I/O in `straum-domain` and
  `straum-app`, an async runtime only through `straum-http`, no
  package from a path outside the workspace, no link out of a crate,
  no trait defined outside `straum-app`'s ports but in a test folder,
  and no `#[path]` or `include` of another crate's file that the same
  rules would refuse. Ask before
  changing `ALLOWED`, `PURE` or `RUNTIMES`.
- `straum-domain` holds what a spec and an ask mean, never how Straum
  SQL writes them: no keyword, function or operator of it in a domain
  string, error text included. Its errors are data; `straum-notation`
  words them (`describe_ask_error`), and the binary hands that to
  `straum-http`. `scripts/check-domain-syntax.py` enforces it.
- No `unsafe`, no macros of our own, traits only at ports. Two
  exceptions: the counting allocators in
  `straum-domain/tests/cost/main.rs` and
  `straum-pg/src/wal/test_support.rs` (ADR-0013).
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
  `thiserror`, no `anyhow`. Input never panics. The one exception is
  `AskError`, whose words name Straum SQL: `straum-notation` says it.
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
  code. `docs/src/reading-the-code.md` lists the patterns the code keeps
  and refuses; change it with the rule. It does not teach Rust.
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
- Docs don't explain tools or syntax a developer knows (shell, SQL,
  HTTP, Compose). A domain acronym is spelled out at its first use on
  each page: "write-ahead log (WAL)". Common ones (SQL, HTTP, JSON,
  URL, TLS) are not.
