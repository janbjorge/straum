# ADR-0003: Straum holds cells and nothing else

Status: Accepted 2026-11-10; revised 2026-11-19

## Context

An aggregate server can keep the raw rows, keep a column store, or keep
only the cells its specs declare. Rows are what PostgreSQL holds, and
holding them again sets memory by the table, not the spec. A cell must
take a deleted row out exactly, which a float sum, a sketch or a
lone min cannot.

## Decision

Straum keeps aggregate cells keyed by rollup key and bucket, and
nothing else: no raw rows, no column store. Beside them sits only what
a join needs of a dim (ADR-0006) and, in physical mode, page state
(ADR-0008).

Cell state is per input column, shared by measures, one enum with no
accumulator trait. Sum and avg share a sum and a null count; the
variances and standard deviations keep a sum of squares beside them,
in the same state, so `sum` on their column adds nothing;
`count(column)` keeps a null count; min, max and `percentile_disc`
share value counts; `count(DISTINCT c)` keeps value counts of its own
while its column holds only numbers, and a hash map per cell from its
first value of another type; `bool_and` and `bool_or` share counts of
true and false. Filtered measures keep no state: a `FILTER` names rollup-key
parts only, so asks apply it to cells.

Numbers are integers: integer columns, and `numeric(p <= 18, s)` as a
scaled `i64` with the scale per column. Sums are `i128`. A sum of
squares is too while it fits; a column of them then widens to 256
bits, a type of Straum's own, as `std` stops at 128. Floats are
refused. Readable types are integer, that numeric, boolean, text,
`timestamp`, `timestamptz` and enums cast to text (ADR-0004); `uuid`,
`date`, domains, arrays and wider numerics are refused by name.

A min or max can't subtract, so a cell keeps every value with its
count, as fixed-width records in runs: the value less the run's base
and the count less one, each in the bytes the widest of its run needs,
found by halving. A value too wide re-encodes its run. A few values
stay inline; a run holds at most 4 KiB; more runs go in a tree.
Variable-length gap records were 30% smaller but walk a run on every
change.

Columns take the bytes their numbers need: `Index` slots of 4 bytes;
`Counts` of 1, 2, 4 or 8 bytes and `Sums` of 4, 8 or 16, widening on
overflow; growth by an eighth; nulls counted, not non-nulls, so a
column of zeros takes no bytes. Packing old buckets read-only was
passed over: it needs a rule for when, and an unpack per ask.

`percentile_disc(f) WITHIN GROUP (ORDER BY alias.column)` reads the
value counts and picks rank `ceil(f * n)`, at least 1, computed in
float8 as PostgreSQL does: 0.07 of 100 values is the 8th. Sketches
were rejected: t-digest and KLL can't delete, and an estimate under a
PostgreSQL name differs from PostgreSQL. `percentile_cont`, `median`,
`mode` and `every` are refused by name, and `variance` and `stddev`
for `var_samp` and `stddev_samp`.

A variance is `(n * s2 - s1^2) / (n * (n - 1))`, or over `n^2` for
`var_pop`, of the count, sum and sum of squares an ask merged; a
standard deviation its root. All three add and subtract exactly, so
no Welford update is needed, which float engines use against
cancellation. The answer rounds half to even, four places past twice
the column's scale for a variance and past its scale for a deviation.

## Consequences

- Memory follows distinct keys times buckets: dropping held dim rows
  took a warehouse copy from 897 MiB to 84 MiB.
- An `i128` sum can't overflow below 2^64 rows; answers print exact
  decimals, with each column's `ColumnType` set by the load. Below 2^64
  rows a sum of squares stays under 2^190 and `n * s2` under 2^254.
- A variance of values near the ends of `bigint` passes an `i128` at
  four places, so an answer can be a 256-bit `Value::Wide`.
- The taxi `max(fare_amount)` view went from 447 to 222 MiB, 5.3 bytes
  a value; 64% of cells keep values inline. Changes cost as before.
- The benchmark's two views went from 296 to 65 MiB, 27 bytes of heap
  a cell; Straum is ready in 12.9 s, not 17.8.
- A percentile adds no bytes beyond a min or max, but costs by values
  in range: milliseconds where a sum takes microseconds (ADR-0005).
- `count(DISTINCT c)` of numbers holds less than a max on the same
  values, where its hash map per cell held over 15 times as much; an
  ask merges several cells' values in order, one value of each held.
  Text, uuid, date and boolean keep the map; its memory is open.
