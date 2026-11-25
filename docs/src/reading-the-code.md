# Reading the code

The order to read the crates in, then the patterns the code keeps and
the ones it refuses. It doesn't explain Rust.
[Design](design.md) is the model behind the code; the
[glossary](glossary.md) names each thing.

## Where to start

Read the crates from the inside out; each uses only the ones before
it.

1. `straum-domain`: what a view is and how its cells add up. No I/O.
   Start with `spec.rs` (the parsed views) and `ask.rs` (what an ask
   may say), then `store/mod.rs` (where rows land), `store/ask.rs`
   and `store/cell_loop.rs` (how an ask reads the cells). `lib.rs`
   says where the rest fits: `rollup/`, `projection.rs`,
   `aggregate.rs` and the compact columns under them.
2. `straum-notation`: reads Straum SQL, the `CREATE VIEW` statements
   and the asks, into the domain's types. The walker is in `sql/`;
   `ask_error.rs` words an ask error.
3. `straum-app`: the use cases. `app.rs` holds `App`, which answers
   asks; `writer.rs` holds `Writer`, the loop that loads the tables and
   applies the stream; `held.rs` reports what the store holds.
   `source.rs` is the one trait PostgreSQL sits behind; `lag.rs` keeps
   the write-ahead log (WAL) readings `as_of` comes from.
4. `straum-pg`: talks to PostgreSQL. `source.rs` is where it meets
   `straum-app`: it checks the setup (`catalog/`), loads each table
   (`load.rs`), then hands a thread the stream (`stream.rs`), which
   reads pgoutput (`pgoutput.rs`, `decode.rs`) or the physical WAL
   (`physical.rs`, `wal/`). For `wal/`, read `wal.rs` first, then the
   files in the order bytes pass through them: `reader.rs` (pages into
   records), `record.rs` (one record), `heap.rs` and `heap/` (row
   changes), `xact.rs` and `transactions.rs` (commits), `messages.rs`
   and `tuple.rs` (messages and values), `pipeline.rs` (all of it in a
   row).
5. `straum-http`: serves asks and status over HTTP. `lib.rs` is the
   server loop; `routes.rs` picks the handler for each request, once
   `host.rs` has checked its `Host` header.
6. `straum`: the binary. `main.rs` starts it all.

Every file opens with a few lines saying what it holds; they tell you
whether you need the rest.

## What the code looks like

Code is written for a reader who knows programming but not Rust
(architecture decision record ADR-0013). Clippy enforces what it can;
review holds the rest.

| Write | Not |
| --- | --- |
| Whole words, units last: `timeout_ms` | One-letter names, abbreviations |
| `let ... else` to return early, so the main path stays flat | Nested `if let` and `match` |
| A `for` loop, or a named function | A chain of closures, a closure bound to `let` |
| An enum: `Direction::Add` | A `bool` parameter |
| A named struct | A tuple |
| A named constant: `RECORD_XID: Range<usize> = 4..8` | A bare number |
| A named index type: `FactIndex` | A bare `usize` |
| A plain type | A lifetime, generic or `impl Trait` one type would do; where one stays, a comment says why |
| A trait only at a port: `Source` | A trait anywhere else |
| A hand-written error enum with `Display` and `From` | `thiserror`, `anyhow`, a panic on input |
| A walk's errors pushed onto a list, then `Err(Reported)`, so a spec reports them all | Returning the first error of a spec |
| Dense code, such as `#[inline]` or a closure in a hot loop, where a benchmark measured the plain form slower, naming it | Dense code no measurement asks for |
| A `//!` header per file, a `///` line per public type, other comments saying why | Comments that restate the code or name callers or tickets, `TODO`, commented-out code |
| `#[expect(.., reason = ..)]` | `#[allow]`, except `dead_code` on a helper module that several test binaries share |
| `[lints] workspace = true` in each crate | Lint settings of one crate |
| Names and signatures from the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) | |
| Functions under 70 lines, files under 1000 | |
| `unsafe` only in the two counting allocators of the cost tests | `unsafe` anywhere else, macros of our own |
