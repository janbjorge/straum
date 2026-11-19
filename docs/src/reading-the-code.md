# Reading the code

This page is for a reader who knows programming but not Rust. It gives
the order to read the crates in, then the Rust the code uses, each
with an example in Straum's own words. [Design](design.md) is the
model behind the code; the [glossary](glossary.md) names each thing.

## Where to start

Read the crates from the inside out. Each one only uses the ones
before it.

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
   asks, and `Writer`, the loop that loads the tables and applies the
   stream. `source.rs` is the one trait PostgreSQL sits behind;
   `lag.rs` keeps the WAL readings `as_of` comes from.
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

Every file opens with a few lines saying what it holds. Read those
first; they tell you whether you need the rest.

## The Rust you'll meet

**`&` and `&mut`: borrowing.** A value has one owner. `&rows` lends
it out to read; `&mut rows` lends it out to change, and while it is
lent that way nothing else may touch it. A function that takes
`table: &str` reads the name and gives it back. `&[Datum]` is a
borrowed list: a row, read in place, never copied.

**`String` and `&str`, `Vec<T>` and `&[T]`.** The first of each pair
owns its text or list; the second borrows one. `Box<[u32]>` is a list
that owns its items and never grows.

**`Option<T>`: maybe a value.** `Some(value)` or `None`, never null.
`Store::cells` returns an `Option<usize>`: the view's cell count, or
`None` for a name no view has.

**`Result<T, E>`: a value or an error.** `Ok(value)` or `Err(error)`.
An error type is most often an `enum` listing what can go wrong, such
as `RowError` or `ClientError`, with the text a person reads written
in its `Display`.

**`?`: return the error.** `self.remove_row(table, old)?;` means: if
that returned `Err`, return the same error from here; otherwise carry
on with the value. It lets most functions read as a straight line of
steps.

**Errors kept in a list: `Reported`.** The Straum SQL walker in
`straum-notation` reports the errors in a spec at once, the first 50
and a count of the rest, so it pushes each one onto a list instead of
returning it. A walk that failed returns `Err(Reported)`, an empty
marker saying the error is already on the list, and the caller stops
there without adding its own. A plain `Option` stays for "there is
none", which is no error.

**`let ... else`: stop early.**

```rust
let Some(bound) = &mut self.bound[fact.0] else {
    return Ok(());
};
```

reads: take the value out of `Some`; if there is none, return. After
it, `bound` is the value itself. (`fact.0` is explained under named
index types below.)

**`if let` and `match`.** `if let Some(dim) = self.dim_index(table)`
runs its block only when there is a value. `match` picks one arm per
case of an enum, and the compiler refuses a `match` that forgets one.
`matches!(value, Pattern)` is `true` when the value has that shape.

**`if let ... && ...`: several conditions at once.**

```rust
if let Some(scale) = key.get("scale")
    && let Some(scale) = scale.as_u64()
{
    column_type = format!("{column_type} scale {scale}");
}
```

runs its block only when every part holds; a later part may use what
an earlier `let` took out.

**`enum` with data.** An enum case can carry values:
`Direction::Add` carries none, `Step::Started(lsn)` carries a
position in the WAL. `match` takes them out again.

**A function as a value.** `fn(Identifier) -> Aggregate` is the type
of a function from a column name to an aggregate. A case that carries
one value is such a function: `Aggregate::Sum` turns `column` into
`Aggregate::Sum(column)`. The walker picks the case by the call's name
and applies it once it has read the column.

**Named index types.** `struct FactIndex(usize);` is a struct with
one unnamed field, read as `fact.0`. It costs nothing at run time and
makes a signature say what a number points at: `fn bind(&mut self,
fact: FactIndex, ..)` takes a position among the facts, and the
compiler refuses a `DimIndex` there. `PartIndex`, `AttributeIndex`
and `InputIndex` do the same for a part of the rollup key, a dim's
attribute and a cell state.

**Iterators.** `for row in rows` walks anything that is an
`Iterator`: a type with a `next` function that hands out one item at
a time until it returns `None`. `impl Iterator<Item = CoveredBucket>`
as a return type means "some iterator of buckets", without naming
it. The domain returns one where a `Vec` would allocate on a path a
test counts allocations on, such as `Rollup::cover`; elsewhere it
returns a `Vec`. `Entries` in `value_counts/` is an iterator written
out as a struct, `impl Iterator for Entries` its `next`.

**`Cow`: borrowed or owned.** `Cow<'_, Datum>` holds either a
borrowed value or one of its own. A rollup key part reads a column's
value in place, and only a null-ness key makes a new `Datum`, so no
row's text is copied.

**`impl` blocks and `self`.** `impl Store { .. }` holds the functions
of a `Store`. A first parameter `&self` reads the store, `&mut self`
changes it, `self` uses it up.

**Traits.** A trait is a set of functions a type promises to have.
Straum has one of its own, at its edge: `Source` (PostgreSQL, in
`straum-app/src/source.rs`). `impl Display for X` and
`impl From<A> for B` are the standard ones: how to print an `X`, and
how to turn an `A` into a `B`, which is what lets `?` convert one
error into another.

**`#[derive(..)]`.** Asks the compiler to write the obvious functions:
`Debug` to print for a developer, `Clone` to copy, `PartialEq` to
compare. `PartialOrd` and `Ord` order a struct by its fields in turn:
a `ValueCount` in `value_counts/` sorts by its value, then its count.

**`impl Trait` in a parameter.** `input: impl BufRead` takes any type
that can read lines, without naming it. `straum shell` passes the
terminal, its tests pass bytes in memory. Like a generic, it is kept
only where callers pass different types.

**Generics: `<T>`.** `Writer<S>` is a writer over any source `S`. The
binary uses `Writer<Postgres>`; tests use a fake source. A generic is
kept only where two types fill it.

**Lifetimes: `'a`.** A name for how long borrowed data lives.
`Visitor<'de>` in `straum-http`'s `json.rs` reads a request body that
lives as long as `'de`; serde's trait asks for it. A lifetime is kept
only where copying would cost or a library asks for one; the comment
beside it says so. The walker's `'tree` is one such: its structs point
into the tree `sqlparser` built, which lives as long as the walk, so
nothing is copied out of it.

**Closures: `|value| ...`.** A function without a name, written where
it is used, which may use the names around it. Straum writes one only
where code is handed over to run later, or where a benchmark measured
a loop slower. `thread::spawn(move || run_writer(writer))` runs
`run_writer` on a new thread, and `move` hands the closure the values
it uses. `load::copy` takes one as `each: impl FnMut(&[Datum])`, "any
function of a batch", and calls it on every batch as it comes, so a
table never sits in memory whole. `Counts::each` hands each count to
one because a loop calling `Counts::get` per cell measured half again
slower (`an_ask_stays_within_its_time_per_cell`). Everywhere else a
`for` loop or a named function does the work.

**Threads: `Arc` and `RwLock`.** `App` keeps its state in
`Arc<RwLock<State>>`: `Arc` lets the writer thread and the HTTP
threads share it, `RwLock` lets many asks read at once while one
commit writes. `read()` and `write()` in `app.rs` take the lock.
The writer's progress sits beside it in `Arc<Mutex<Progress>>`: a
`Mutex` lets one thread at a time in, which is enough for a few
fields read and set in an instant. `progress()` takes it.

**Modules.** `mod store;` pulls in `store.rs`, or `store/mod.rs`
when the module is a folder of files. `pub` is visible to other
crates, `pub(crate)` only inside this one, `pub(super)` only in the
folder's other files, `pub(in crate::sql)` only inside the module
named and the ones under it, nothing at all only inside the file.
`#[cfg(test)] mod tests` at the end of a file holds its unit tests,
written in place or in a `tests.rs` beside the file; `tests/` beside
`src/` holds the tests that use the crate from outside.

**Type aliases.** `type TransactionId = u32;` gives `u32` a second
name, so `xid: TransactionId` says what the number is. The two are the
same type, so the compiler doesn't stop a slot number going where an
xid should; the name only tells the reader.

**Reading bytes.** The WAL code reads PostgreSQL's structs from raw
bytes. A field's place is a named constant, often a range:
`RECORD_XID: Range<usize> = 4..8` is bytes 4 to 7, and
`&bytes[RECORD_XID]` is those four bytes, borrowed. Indexing past the
end panics, so code that reads input it hasn't checked uses
`bytes.get(4..8)`, which gives `None` instead. `u32::from_le_bytes`
turns four bytes, least significant first ("little-endian"), into a
`u32`; `read_u32` in `wal.rs` wraps it. `first_chunk()` takes the first
bytes as a fixed-size array, if there are enough, and
`as_chunks::<2>()` cuts a slice into two-byte arrays.

**Shared and maybe-copied bytes.** `Arc<[u8]>` is bytes several owners
share without copying: a row on a page and the change that names it.
`Cow<'_, [u8]>` is bytes that are either borrowed (`Cow::Borrowed`) or,
when they had to be changed, such as decompressed, owned
(`Cow::Owned`).

**Dense code.** Where a loop would be slower in a way a benchmark
measured, the code keeps a denser form and a comment names the
benchmark. Everywhere else it is a plain `for` loop. `#[inline]` above
a function is such a case: it asks the compiler to copy the function
into each caller. Four functions an ask calls per cell need it
(`Lookup::code` in `store/lookup.rs` names the benchmark).
