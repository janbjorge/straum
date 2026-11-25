# Straum: guide for AI coding agents

The records in `docs/adr/` hold settled decisions. If a record and this
file do not agree, the record is correct.

## Commands

All tools run through the `justfile`, on your computer and in CI. Docs
name the recipe, not the command in it. To see that command, run
`just --show RECIPE`. Run each recipe only on the crates that you
change. CI runs `all`.

```sh
just fmt
just lint <crate>
just test <crate>
just cov <crate>
just scripts
just check <crate>   # all of the above but cov
```

If you name no crate, a crate recipe uses the crates that changed since
HEAD. The `all` recipe uses the full workspace. When you change a check
script, change `scripts/test-checks.py` in the same commit. Add a
planted break for each new rule, and a correct form that passes.

Run coverage on the crates that you change. Also run it on the crates
whose tests use them: for example, the tests of `straum-notation` use
much of `straum-domain`. If you do not have rustup, set `LLVM_COV` and
`LLVM_PROFDATA` to the tools in `$(brew --prefix llvm)/bin`.

PostgreSQL tests need Docker with Compose. The `postgres` setting
selects the major version. The default is 18. These tests use random
seeds:

- the random workloads of `just correctness`
- the physical stream against a logical slot, and from a random start
- the logical stream from a random start

They run `seeds` seeds (default 6), from seed `seed_from` (default 0).
In CI, the `correctness` job runs 50 new seeds for each major version,
on each run.

The heap unit tests replay records from
`crates/straum-pg/src/wal/heap/fixtures/`. Each major version has its
own records. After a change to the WAL layout, capture the records
again for each major version with `just postgres=<major>
heap-fixtures`.

`just taxi shell` starts Straum against a temporary PostgreSQL database
with the taxi trips. Then it opens `straum shell`. For demo and try-out
data, always use the taxi trips. Never write a dataset by hand. In docs,
ADR-0014 tells which pages use the taxi trips. It also tells which
pages use the `usage` view over `public.events AS e` and
`public.accounts AS a`.

- If you change a file in `deploy/helm/`, run `just chart`. This recipe
  needs uv, helm, the helm-unittest plugin and kubeconform.
- If you change an HTTP route or a JSON reply, change
  `crates/straum-http/openapi.json` in the same commit. The tests of
  straum-http compare the replies with this file. `just scripts` makes
  sure that the file is valid OpenAPI.
- If you change a file in `docs/`, run `just docs`. To see a preview,
  run `just docs serve --open`. CI uses mdBook 0.5.4.
- If you change the Dockerfile or `compose.yaml`, run `just image`.

`just audit` compares `Cargo.lock` with the RustSec advisories. It needs
cargo-audit. Each week, Dependabot opens pull requests for the pinned
actions and the Cargo dependencies.

`just --list` shows all recipes. In addition to the checks above, these
recipes are available:

- `just correctness`
- `just heap-fixtures`
- `just image`
- `just taxi [asks|transactions|materialized|load|retention|shell]`.
  Add `wal=physical` for the physical WAL.
- `just taxi-month`, `just wal-bench` and `just cost-bench`
- `just demo`. Add `tag=X.Y.Z` for a different release.
- `just outbox`
- `just try SPEC --data DIR`

Each setting has a default. To change a setting, give it on the command
line (`just postgres=17 seeds=50 correctness`) or set its environment
variable. To see all settings, run `just --evaluate`.

Rules for recipes:

- When you add a script, check, benchmark or setting, add a recipe or
  variable in the same commit. Give it the same default as the code.
- CI calls the recipe, never the command in it.
- Keep one recipe for each tool. Put it in a `[group]` and give it a
  one-line comment.
- Keep recipes thin. Put the logic in the script or the test.
- A recipe that runs a cargo test calls `test` with its arguments. It
  does not repeat the cargo command.
- Each test filter names a test in the form that `cargo test -- --list`
  shows. `scripts/check-test-filters.py` fails on a filter that selects
  no test.
- A recipe that passes free arguments has `[positional-arguments]` and
  passes `"$@"`. Then an argument with a space stays one argument.
- The `test` recipe is the exception. Its first argument is the crates,
  and its other arguments are cargo test filters. These filters do not
  contain spaces.
- `set lazy` stops a backtick from running before a recipe reads its
  value.
- `just fmt` also formats the justfile.
- The justfile holds the guard against a crate recipe with no crate.
  `scripts/test-checks.py` tests this guard with `just -n`.
- Write each script in Python and run it with uv, as the scripts in
  `scripts/` and `bench/` do.
- When you remove a tool, remove its recipe. Then fix each doc that
  names the recipe.

## Documents

A change is complete only when all documents that describe it are
correct again. Fix the documents in the same commit as the code. Do not
make a follow-up commit. Do not wait for a request.

Before you commit, use `rg` to search `docs/`, `README.md` and
`deploy/` for each item that changed: an old name, flag, message,
default or number. Then examine these documents:

- The guide in `docs/src/` (pages in `SUMMARY.md`): what a user sees or
  does. This includes spec and ask syntax, error text, flags,
  endpoints, PostgreSQL setup and limits. ADR-0014 gives the home page
  of each fact. Write the fact on that page, and link to it from other
  pages.
- The design pages and `docs/src/diagrams/`: crates, machines, the
  store, and how a change moves through them.
- `docs/src/glossary.md`: a term that you add, rename or remove.
- `README.md`: what Straum does and how to start it.
- `deploy/helm/straum/README.md` and `values.schema.json`: a chart value
  that you add, rename or give a new default.
- `docs/adr/`: if a change goes against a record, edit that record and
  change its revised date (ADR-0001).
- `ROADMAP.md`: work that you completed, started, stopped for now or
  cancelled.
- This file: a command, tool or rule that changed.

At the end of your report, list the documents that you changed. If no
document describes the change, say so.

## Rules

- Do not write shell scripts. Write each tool as a Python file and run
  it with uv. Its just recipe is a thin wrapper. Do not add a `.sh`
  file, and do not put a bash body in a recipe.
- Send each Docker command through `docker compose`. This rule applies
  to tests, scripts, benchmarks, CI and docs.
  - Use one project for each run or test. Give each project a name, so
    that parallel runs never share a network, container name or port.
  - Code builds the Compose file as JSON and sends it to
    `docker compose -f -`.
  - Docs supply their Compose files in `docs/compose/`.
  - The image builds and pushes through the root `compose.yaml`.
  - To get access to a running project, use its name:
    `docker compose -p NAME exec`, `port`, `ps`, `logs`, `cp`.
  - Do not use plain `docker run`, `exec`, `build`, `network` or a
    similar command. Do not use testcontainers. The only exception is
    `docker login`, because Compose does not have it.
- Crates depend only inward. `scripts/check-layers.py` enforces this
  rule. It reads Cargo's metadata with all features on, and the lexed
  source. It checks that:
  - normal and build dependencies agree with its `ALLOWED` table
  - dev-dependencies never point outward
  - each target builds from its own crate
  - `straum-domain` and `straum-app` use no outside crate and no `std`
    I/O
  - an async runtime comes in only through `straum-http`
  - no package comes from a path outside the workspace
  - no crate links out
  - a trait is defined only in the ports of `straum-app` or in a test
    folder
  - no `#[path]` or `include` brings in a file of another crate that the
    same rules refuse.

  Ask before you change `ALLOWED`, `PURE` or `RUNTIMES`.
- `straum-domain` holds what a spec and an ask mean. It never holds how
  Straum SQL writes them. A domain string, error text included, contains
  no keyword, function or operator of Straum SQL. Domain errors are
  data. `straum-notation` puts them into words (`describe_ask_error`),
  and the binary gives that text to `straum-http`.
  `scripts/check-domain-syntax.py` enforces this rule.
- Do not use `unsafe` or macros of our own. Use traits only at ports.
  There are two exceptions: the counting allocators in
  `straum-domain/tests/cost/main.rs` and
  `straum-pg/src/wal/test_support.rs` (ADR-0013).
- Use plain structs, enums and functions. Names and signatures follow
  the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
- Write for a reader who does not know Rust (ADR-0013):
  - Use whole words, and put units last (`timeout_ms`). Use verbs for
    function names.
  - Use early returns with `let ... else`, so that the main path stays
    flat.
  - Use a `for` loop, not a chain with a closure. Do not bind a closure
    to `let`. Make it a named function.
  - Use enums, not `bool` parameters. Use named structs, not tuples.
    Use named constants, not magic numbers. Use named index types
    (`FactIndex`), not bare `usize`.
  - Use a lifetime, generic or `impl Trait` only where a plain type
    does not work. Write a comment that tells why.
  - Write dense code only where a benchmark shows a clear gain. Name
    that benchmark.
  - Keep each function shorter than 70 lines and each file shorter than
    1000 lines.
- Each crate, also a new crate, uses `[workspace.lints]` through
  `[lints] workspace = true`. To silence a lint, use
  `#[expect(.., reason = ..)]`, never `#[allow]`. There is one
  exception: `dead_code` on a helper module that several test binaries
  share. There, `#[expect]` fails in the binary that uses all of the
  module.
- Write errors as hand-written enums with `Display` and `From` impls.
  Do not use `thiserror` or `anyhow`. Input never causes a panic.
  `AskError` is the one exception: its text names Straum SQL, so
  `straum-notation` writes that text.
- The Straum SQL walker rejects each construct that it does not list,
  and names that construct. Each meaning has one spelling. The walker
  infers nothing (ADR-0004).
- Only `straum-http` runs tokio.
- Before you add a dependency, examine `std`. Add a crate only for a
  capability that `std` does not have, or for a large measured gain.
  Write the reason next to it in `Cargo.toml`. If the reason is
  architectural, write it in an ADR. Do not add a convenience crate for
  a small helper. To review dependencies, run
  `cargo tree --workspace -e normal`.
- `docs/src/glossary.md` gives each thing one name. Use its terms in
  docs, comments and error text. Its Clashes table lists the renames
  that are still open.
- Comments:
  - Start each file with a `//!` header in plain English. The header
    tells what the file holds and how data moves through it.
  - Give each public type one `///` line that tells what it is.
  - Other comments tell only a reason that the code cannot show: a
    constraint, an invariant, a PostgreSQL quirk or a rejected
    alternative.
  - Never refer to callers or tickets. Never leave a `TODO` or code in
    a comment.
  - `docs/src/reading-the-code.md` lists the patterns that the code
    keeps and refuses. When you change a rule, change that page too.
    The page does not teach Rust.
- Give each behaviour a positive test and a negative test. For state
  that changes over time, write a property test against a reference. A
  planted bug must make that test fail.
- Record each settled decision in `docs/adr/`. ADR-0001 tells how.
- Commit directly to `main`. Make one change in each commit, with a
  short imperative subject. Do not use branches or PRs. The maintainer
  makes releases and tags on the release page (ADR-0012). Add code in
  the commit that first uses it.
- Use `git --no-pager`. Do not read `bench/data/` or `target/`.
- Write prose in ASD-STE100 Simplified Technical English. This rule
  applies to docs, ADRs, comments, error text, log lines and commit
  messages.
  - Write one topic in each sentence. An instruction has 20 words at
    most, and a description has 25. A paragraph has 6 sentences at
    most.
  - Use the active voice. Write instructions in the imperative: "Run
    `just docs`".
  - Give each word one meaning. Use the STE approved word in its
    approved sense, or a technical name from `docs/src/glossary.md`.
    Do not use a synonym for variety.
  - Use only simple tenses: past, present and future. Do not use an
    "-ing" form as a noun or adjective where a plain verb works.
  - Keep articles and "that". Do not write telegraphic sentences.
  - Do not change code, commands, paths, SQL or quoted PostgreSQL text.
    STE applies only to the words around them.
- Wrap markdown at approximately 78 columns. Do not use em dashes.
- Docs do not explain tools or syntax that a developer knows (shell,
  SQL, HTTP, Compose). Write out a domain acronym at its first use on
  each page: "write-ahead log (WAL)". Do not write out common acronyms
  (SQL, HTTP, JSON, URL, TLS).
