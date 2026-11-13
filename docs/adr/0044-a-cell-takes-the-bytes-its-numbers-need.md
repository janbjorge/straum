# ADR-0044: A cell takes the bytes its numbers need

Status: Accepted, 2026-11-13

## Context

A cell is a key, a row count and a number or two per measure: nine
bytes of content in the benchmark's `dropoffs` view. Straum held 184.
Each sum took sixteen bytes and each count eight, whatever they held.
Each sum kept a count of its values that are not null, which is the
row count wherever no value is null. A map from keys to places kept
every key a second time, in an allocation of its own. And a column
doubled when full, so a third of it lay empty.

## Decision

- A bucket finds a cell through a table of places (`Index`). A slot
  is four bytes, and the key column says whose it is.
- A column grows by an eighth.
- A column of counts or sums is as wide as its widest entry needs
  (`Counts`, `Sums`): one, two, four or eight bytes a count, and
  four, eight or sixteen a sum. An entry that outgrows its column
  moves the column to the next width.
- A count and a sum keep the nulls among a cell's rows, not the
  values that are not null. A column of zeros takes no bytes.

Cells stay what they were to an ask and to a change: found by key,
changed in place, read as flat runs. Old buckets packed read-only
would come to the nine bytes, but need a rule for when to pack, and
an ask over them would unpack first.

## Consequences

- The benchmark's two views hold 65 MiB where they held 296, 27 bytes
  of heap a cell from 147, and Straum is ready in 12.9 s where it
  took 17.8. Asks and changes take as long as before.
- A removed row with a null where its cell holds none is a row never
  added, which was passed over before. A sealed bucket that counts
  more values than a cell has rows is refused.
- A key part still takes four bytes, and a min or max 48 a cell
  beside its values (ADR-0043).
