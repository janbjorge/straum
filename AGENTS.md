# Straum: guide for AI coding agents

The records in `docs/adr/` hold settled decisions. If a record and this
file do not agree, the record is correct.

## Commands

Run each tool through a recipe in the `justfile`, on your computer and
in CI. Run `just` to list the recipes. Do not run the command in a
recipe directly.

Run each crate recipe only on the crates that you change. CI runs
`all`. PostgreSQL tests need Docker with Compose. If you do not have
rustup, set `LLVM_COV` and `LLVM_PROFDATA` to the tools in
`$(brew --prefix llvm)/bin`.

Run these recipes for these changes:

- Rust code: `just check <crate>` and `just cov <crate>`.
- A file in `deploy/helm/`: `just chart`.
- A file in `docs/`: `just docs`.
- The Dockerfile or `compose.yaml`: `just image`.
- The WAL layout: `just heap-fixtures` for each PostgreSQL major
  version.
- An HTTP route or a JSON reply: change
  `crates/straum-http/openapi.json` in the same commit.
- A check script: change `scripts/test-checks.py` in the same commit.
  Add a planted break for each new rule, and a correct form that
  passes.

For demo and try-out data, always use the taxi trips. Never write a
dataset by hand. ADR-0014 tells which doc pages use the taxi trips.

Rules for the justfile:

- When you add a script, check or benchmark, add a recipe in the same
  commit.
- Do not add a variable for a setting. A script takes a flag. A test
  reads an environment variable: name it in the recipe comment.
  `postgres` is the only exception, because many recipes use it.
- CI calls the recipe, never the command in it.
- Keep one recipe for each tool. Put it in a `[group]` and give it a
  short one-line comment. `just` lists the recipes in file order.
- Keep recipes thin. Put the logic in the script or the test.
- A recipe that runs a cargo test calls `test`. Each test filter names
  a test in the form that `cargo test -- --list` shows.
- A recipe that runs a script gives it all its arguments with `"$@"`.
  Then `just RECIPE --help` shows the flags of the script.
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
  it with uv. Do not add a `.sh` file, and do not put a bash body in a
  recipe.
- Send each Docker command through `docker compose`, also in tests,
  scripts, benchmarks, CI and docs.
  - Use one project for each run or test. Give each project a name, so
    that parallel runs never share a network, container name or port.
  - Use the project name to get access to a running project:
    `docker compose -p NAME exec`, `port`, `ps`, `logs`, `cp`.
  - Do not use plain `docker` commands or testcontainers. The only
    exception is `docker login`, because Compose does not have it.
- Keep the crate layers of ADR-0002. `scripts/check-layers.py` and
  `scripts/check-domain-syntax.py` enforce them. Ask before you change
  `ALLOWED`, `PURE` or `RUNTIMES`.
- Write code as `docs/src/reading-the-code.md` and ADR-0013 tell. When
  you change a rule, change that page too.
- The Straum SQL walker rejects each construct that it does not list,
  and names that construct (ADR-0004).
- Before you add a dependency, examine `std`. Add a crate only for a
  capability that `std` does not have, or for a large measured gain.
  Write the reason next to it in `Cargo.toml`. If the reason is
  architectural, write it in an ADR. Do not add a convenience crate for
  a small helper.
- Use the terms of `docs/src/glossary.md` in docs, comments and error
  text.
- Give each behaviour a positive test and a negative test. For state
  that changes over time, write a property test against a reference. A
  planted bug must make that test fail.
- Record each settled decision in `docs/adr/`, as ADR-0001 tells.
- Commit directly to `main`. Make one change in each commit, with a
  short imperative subject. Do not use branches or PRs. Add code in the
  commit that first uses it. Do not make releases or tags (ADR-0012).
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
