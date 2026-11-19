# User-defined aggregates (2026-11-19)

Could a spec's author write an aggregate Straum doesn't have? A cell
must merge and must take a delete exactly (ADR-0003), and Straum never
goes back to the rows (ADR-0009). This note holds the sources, what
other systems do, the design those lead to, and what checking it
against the code changed. Nothing here is built or decided. A build
starts with an ADR.

Tags as in README.md (R, U, I). R here: papers and documentation pages
read through a fetch tool, PostgreSQL 18 run in a throwaway Compose
project, and this repository's code at `c0d0ad3`.

## 1. Answer in brief

- An aggregate that merges and takes a delete without the rows is a
  group homomorphism from the multisets of rows: a lift from a row
  into a group, summed. Remove is adding the negation. The author
  writes the lift and a final step, never an inverse (§2).
- Straum's integers already form that group. So a user's aggregate is
  a formula over `sum` and `count`, where the expression inside a
  `sum` is computed per row. It is plain PostgreSQL, so PostgreSQL
  stays the test oracle (§4).
- Min, max, median and mode need a count per value, which Straum keeps
  for its built-ins. They stay built-ins (§2.2).
- No engine without a mandatory inverse avoids recomputing from rows
  (§3). `CREATE AGGREGATE` with a user's add and remove steps would be
  the first thing in Straum whose laws can only be tested, not shown.

## 2. Theory

### 2.1 Delete-safe means linear

- **DBSP.** Budiu, McSherry, Ryzhyk, Tannen, "DBSP: Automatic
  Incremental View Maintenance for Rich Query Languages", VLDB 2023
  ([arXiv 2203.16684](https://arxiv.org/abs/2203.16684), R).
  - Def. 2.14: an operator is "linear if it is a group homomorphism".
  - §7.2: `COUNT` and `SUM` "are in fact linear transformations
    between the group Z[A] and the result group". AVG is "a more
    complex linear function that computes a pair of values using SUM
    and COUNT, followed by" a division.
  - "some aggregate functions, such as MIN, are not incremental in
    general, since for handling deletions they may need to know the
    full set."
- **Koch.** "Incremental query evaluation in a ring of databases",
  PODS 2010 ([EPFL](https://infoscience.epfl.ch/record/166019), R).
  Def. 3.1: with a ring of multiplicities "tuples can have negative
  multiplicities", which the paper calls necessary for a full
  additive inverse.
- **F-IVM.** Nikolic and Olteanu, SIGMOD 2018
  ([arXiv 1703.07484](https://arxiv.org/abs/1703.07484), R). Inserts
  and deletes "map to positive and respectively negative ring
  values"; the covariance ring (Def. 32) is a count, a sum and a sum
  of products, all added componentwise.
- **The algebra (R, I).** Z[X], the multisets of X with integer counts,
  is the free abelian group on X
  ([Wikipedia](https://en.wikipedia.org/wiki/Free_abelian_group)). A
  homomorphism from it into a group G is fixed by a lift X to G:
  `state = sum of lift(row)`. An idempotent monoid such as max
  collapses to the trivial group under the Grothendieck construction
  ([Wikipedia](https://en.wikipedia.org/wiki/Grothendieck_group)), so
  min and max have no inverse in any group.

So, if a state depends only on the current multiset of rows, merge is
associative and commutative, and remove undoes add, then the state is
`final(sum of lift(row))` for a lift into a group (I, from the above).
The author's whole job is the lift and the final step.

### 2.2 What isn't linear

- **Exact min (I).** Insert any subset S of 1..n, then ask min and
  delete it, repeatedly: S comes back. So 2^n subsets need 2^n states,
  n bits at least. The same holds for max, median, mode, and first or
  last by a key. No fixed-size state takes a delete for these.
- **Counts per value (I)** are the identity homomorphism on Z[V], the
  universal state. Min, max, `percentile_disc`, `mode()` and
  `count(DISTINCT c)` are each a final step over it, which is how
  Straum holds them now.
- **Overflow (I).** A checked `i64` is not a group: whether a sequence
  of adds and removes fails depends on its order, which breaks merge.
  Either widen so that no reachable value overflows, as ADR-0003 does
  for sums and squares, or wrap and show the answer fits.
- **Floats (R, I).** Not associative, and `x + y - y` need not be `x`.
  PostgreSQL's own docs show `1e20 + 1 - 1e20 = 0`
  ([xaggr](https://www.postgresql.org/docs/current/xaggr.html));
  Feldera calls a float `SUM` non-linear
  ([aggregates](https://docs.feldera.com/sql/aggregates/)). An exact
  float sum exists, the superaccumulator of Neal
  ([arXiv 1505.05571](https://arxiv.org/abs/1505.05571), R), about
  536 bytes; Straum refuses floats instead.
- **Laws of user code (I).** Whether a user's add and remove commute
  and invert is undecidable in general; deciding it for a narrow
  fragment takes an SMT solver. Property tests find counterexamples,
  never a proof.

## 3. What other systems do

From the vendors' pages (R unless marked):

| System | Inverse | Without one |
| --- | --- | --- |
| PostgreSQL `CREATE AGGREGATE` | `MINVFUNC`, window frames only | recompute the frame |
| Kafka Streams `KGroupedTable` | `subtractor`, mandatory | n/a |
| ksqlDB | `TableUdaf.undo` (U) | refused on tables (U) |
| Flink `AggregateFunction` | `retract` | refused where needed (I) |
| Oracle ODCIAggregate | `ODCIAggregateDelete` (U) | recompute the window (U) |
| Hazelcast | `deductFn`, an accumulator less another | recompute from frames |
| RisingWave `CREATE AGGREGATE` | optional `retract` | "not updated incrementally" |
| Feldera `CREATE LINEAR AGGREGATE` | none: a weight of -1 | non-linear refused |
| SQL Server, BigQuery, Snowflake, DuckDB, Spark, Beam | none | batch only |

Sources:
[PostgreSQL](https://www.postgresql.org/docs/current/sql-createaggregate.html),
[Kafka](https://kafka.apache.org/39/javadoc/org/apache/kafka/streams/kstream/KGroupedTable.html),
[ksqlDB](https://docs.ksqldb.io/en/latest/reference/user-defined-functions/),
[Flink](https://nightlies.apache.org/flink/flink-docs-stable/docs/dev/table/functions/udfs/),
[Oracle](https://docs.oracle.com/database/121/ADDCI/ext_agg_ref.htm),
[Hazelcast](https://docs.hazelcast.org/docs/latest/javadoc/com/hazelcast/jet/aggregate/AggregateOperation.html),
[RisingWave](https://docs.risingwave.com/sql/commands/sql-create-aggregate),
[Feldera](https://docs.feldera.com/sql/udf/),
[BigQuery](https://docs.cloud.google.com/bigquery/docs/user-defined-aggregates).

- Feldera is the precedent. A linear aggregate is a `map` from a row
  to an accumulator with zero, add and multiply by an integer weight,
  and a `post` step. "Removed elements are represented by negative
  weights": no separate inverse to drift from add.
- BigQuery's SQL aggregate is an expression over built-in aggregates,
  `AS (AVG(dividend / divisor))`: the formula shape of §4, for batch.
- Every other system with an incremental path either makes the author
  write the inverse or recomputes from rows. Straum can do neither
  safely: it holds no rows, and a wrong inverse corrupts a cell for
  good.

## 4. Design: a formula over sums

No new statement. A measure becomes a formula, still a PostgreSQL
`CREATE VIEW` expression with one spelling (ADR-0004), over the taxi
trips:

```sql
sum(t.fare_amount * t.trip_distance)::numeric
    / NULLIF(sum(t.trip_distance), 0) AS fare_per_distance,
count(*) * sum(t.fare_amount * t.fare_amount)
    - sum(t.fare_amount) * sum(t.fare_amount) AS fare_spread
```

- **Lift**, the expression inside a `sum`: columns of the fact table
  and literals, `+ - *` and parentheses. Every operand is numeric: a
  numeric column, an integer column cast `::numeric`, or a literal.
  PostgreSQL's numeric never overflows, so it never fails where
  Straum answers. `*` adds scales, `+` and `-` take the larger, as in
  PostgreSQL.
- **Formula**: `sum` and `count` joined by `+ - *`, and one division,
  `X::numeric / NULLIF(Y, 0)`. Every `*` has a numeric operand: a
  `sum` of a `bigint` or numeric column, or a cast. PostgreSQL types
  `count * sum(int)` as `bigint * bigint`, which fails past 2^63; a
  count of 10^6 times a sum of 10^13 already does.
- **Nulls**: a lift with a null operand is null and its row is
  skipped, as PostgreSQL skips it in `sum`. A null part makes the
  formula null.
- **State**: a lift is one more input with a null count and a sum.
- **Answer**: an exact fraction out of the domain, rounded at the edge
  as `avg` is, half to even at four places past the scale
  (`straum-http/src/answer.rs:54`, `docs/src/spec.md:336`). PostgreSQL's
  own division scale is not matched; `avg` doesn't match it either.

Refused, each by name: `CASE` (write `FILTER`), integer `/`, a
division without `NULLIF`, `/` in a lift, `avg` and the variances as
formula operands (their answers are rounded already), floats, `sqrt`,
a dim column in a lift, a lift in `min`, `max` or `count(DISTINCT)`.

### 4.1 What the code changed in the design

The first draft had five errors, found against the code (R):

1. The taxi fares, tips and distances are `numeric(12, 2)`
   (`bench/load_taxi_trips.py:69-71`). A product of two has 24
   digits, past what a `numeric(p <= 18)` column holds as an `i64`. A
   rule "a lift's value fits one column" would refuse the example
   above.
2. `Datum::Int` is an `i64` (`straum-domain/src/value.rs:103`), and a
   product of two `i64` takes 126 bits. So a lift can't go through
   `State::add(&Datum)`: it needs an input that carries an `i128`.
   `Squares` (`straum-domain/src/column.rs:467`) is a sum that starts
   as `i128` and widens to 256 bits; its wrapping arithmetic holds a
   signed sum too, so the lift's state is one new `State` variant
   over it, not a new column type.
3. Computing exactly where PostgreSQL fails is not PostgreSQL's
   answer. Hence numeric-only lifts and a numeric operand on each `*`.
4. Straum doesn't follow PostgreSQL's division scale today (`avg`,
   above), which removes the hardest part of matching it.
5. `avg` as an operand would compute on a rounded value.

### 4.2 Sizes

Bounds below 2^64 rows, from the declared types (I, computed):

| Value | Bits |
| --- | --- |
| a lift per row, two `i64` multiplied | 126 |
| `sum` of a lift | 190 |
| `count(*) * sum(lift)` | 254 |
| two such terms added | 255 |
| `sum(lift) * sum(column)` | 317, refused |

A lift is refused past 127 bits a row and a formula past 255, each by
name. That one rule replaces any cap on nodes or depth. A formula is
computed in a signed 256-bit type, which `wide.rs` lacks: it has an
unsigned `U256`.

## 5. How PostgreSQL reads an expression

A parser is reused (`sqlparser`, ADR-0004), so the risk is not the
parser but reading the tree as PostgreSQL does: grouping, the reach of
`::` and unary minus, a literal's type, implicit casts. PostgreSQL
prints its reading. On PostgreSQL 18 (R), for
`CREATE TABLE t (a int, b int, c numeric(10,2), d bigint)`,
`pg_get_viewdef(oid, false)` of a view gave:

```text
SELECT sum((((a - b))::numeric - c)) AS x,
    sum((a - (b - d))) AS y,
    sum((((a * b))::numeric + (- (c)::numeric))) AS z,
    ((((count(a))::numeric * sum((c * c))) - (sum(c) * sum(c))) / (NULLIF(count(a), 0))::numeric) AS w,
    sum((a * '3000000000'::bigint)) AS u
```

- Every group is in parentheses, and each implicit cast is written
  out: `(a - b)` is computed as `int` and only then cast, so it can
  overflow first.
- `::` binds tighter than unary minus: `- (c)::numeric`.
- `3000000000` is a `bigint`; `b - d` takes no cast.
- Each node's type comes from a view with one column per
  subexpression, through `format_type(atttypid, atttypmod)`:
  `a * b` integer, `c * c` numeric without its `(10,2)`,
  `a * 3000000000` bigint.

A test can print Straum's resolved tree the same way and compare it
with `pg_get_viewdef` per major: the two match only if `sqlparser`
grouped the text as PostgreSQL did. The output can change between
majors, and PostgreSQL leaves out some parentheses and casts
(`ruleutils.c`), which the printer must follow (I).

## 6. Tests

- Notation: accepted and refused examples, one refusal per rule, each
  naming its construct.
- Domain: add, remove and merge in random order with nulls, against
  a recompute from all rows.
- The random workloads of CI's `correctness` job gain a lift and a
  formula.
- PostgreSQL: the answers of §4's examples, the zero denominator,
  nulls and each size bound, with PostgreSQL's value rounded by the
  `avg` rule; the `pg_get_viewdef` comparison of §5.
- Planted bugs that must fail: a remove that skips the lift, a size
  rule a bit too loose, a wrong scale after `*`.
- Cost (`straum-domain/tests/cost/main.rs`): adding a row with a lift
  allocates nothing; an ask with a formula allocates per group, not
  per cell, as `an_ask_allocates_per_group_not_per_cell` holds now.

## 7. Size of the change

Two commits, by the lines added in like commits (I, an estimate):

| Commit | Code | Tests | Docs | Like |
| --- | --- | --- | --- | --- |
| Lift | ~400 | ~300 | ~60 | `27f2de1` `mode()`: 360, 75, 73 |
| Formula | ~700 to 900 | ~400 | ~80 | `39a1ee3` variance: 1245, 290, 170 |

Lift: the parser, the `State` variant, the 127-bit rule, `plan --copy`
copying the lift's columns. Formula: the signed 256-bit type, the
255-bit rule, evaluation per group, the answer in `straum-http`. No new
crate, dependency or trait, and no change to `scripts/check-layers.py`.

## 8. Left out

- **Conditions on rows** (2026-11-19, jb: left out for now).
  `count(*) FILTER (WHERE t.tip_amount > 0)` is linear: the condition
  is a lift that is 0 when false, and `REPLICA IDENTITY FULL` gives the
  old row to remove. It costs state per measure and filter and a
  comparison per row. Today a `FILTER` takes `=` and `IN` on keys only
  (`docs/src/spec.md:378`); the same spelling would then be checked
  per row or per cell by whether its columns are keys.
- **`<` and `>` on a key in a `FILTER`**: the key is in each cell, so
  an ask checks it with no new state. Small, and apart from this.
- **Comparing a formula in an ask's `HAVING`**: `HAVING` takes
  `= <> < <= > >=` on a measure already (`docs/src/asks.md:40`); a
  formula is a fraction, as `Value::Avg` is.
- **`CREATE AGGREGATE`** with the author's add and remove: §2.2.

## Open points

- What Straum answers for `sum(int)` past 2^63, where PostgreSQL
  fails. A formula's `+` and `-` of `bigint` sums sit on the same edge
  and should do the same.
- Whether the PostgreSQL tests of `avg` compare after rounding, so the
  formula's tests can do the same.
- Glossary names: "lift" and "formula".
