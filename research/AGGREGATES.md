# Aggregates: which merge, which take a delete (2026-11-12)

Straum answers an ask by merging cells and follows PostgreSQL by adding
and removing rows from cells. Both put a demand on an aggregate, and
the two demands are separate. This note holds the sources on each, how
other systems meet them, and where Straum's cell states sit. The guide's
version is `docs/src/aggregates.md`. Nothing here is a decision; the
order in §4.4 is a plan.

Tags as in README.md (R, U, I). R here: the four papers of §1.1 to §1.4
and `pg_aggregate.dat`, read as text; documentation pages read through
a fetch tool that quotes them. A search snippet is U.

## 1. Sources

1. **The three classes.** Gray, Chaudhuri, Bosworth, Layman, Reichart,
   Venkatrao, Pellow and Pirahesh, "Data Cube: A Relational Aggregation
   Operator Generalizing Group-By, Cross-Tab, and Sub-Totals", Data
   Mining and Knowledge Discovery 1(1), 1997
   ([arXiv cs/0701155](https://arxiv.org/abs/cs/0701155), R), §5:
   - *Distributive*: there is a G with F(all) = G(F of each part).
     "COUNT(), MIN(), MAX(), SUM() are all distributive. In fact, F = G
     for all but COUNT(). G= SUM() for the COUNT() function."
   - *Algebraic*: an M-tuple per part and a function H over the merged
     tuples. Average, standard deviation, MaxN, MinN. "The key to
     algebraic functions is that a fixed size result (an M-tuple) can
     summarize the sub-aggregation."
   - *Holistic*: "there is no constant bound on the size of the storage
     needed to describe a sub-aggregate." Median, mode, rank.

   The class is about storage per part, not about possibility: a
   holistic aggregate merges exactly if each part keeps enough (I).
2. **Delete is a second axis.** Same paper, §6 (R): the classes were
   "completely focused on SELECT statements, not on UPDATE, INSERT, or
   DELETE", and "max is a distributive for SELECT and INSERT, but it is
   holistic for DELETE." It names the case "delete-holistic" and ends
   "These ideas deserve more study."
3. **Self-maintainable.** Mumick, Quass and Mumick, "Maintenance of
   Data Cubes and Summary Tables in a Warehouse", SIGMOD 1997
   ([PDF](https://sigmodrecord.org/publications/sigmodRecord/9706/pdfs/253262.253277.pdf),
   R), §3.1, Definition 3.1: a set of aggregates is self-maintainable
   "if the new value of the functions can be computed solely from the
   old values of the aggregation functions and from the changes to the
   base data", with respect to insertions, deletions or both. From the
   same section:
   - Every distributive aggregate is self-maintainable for insertions.
   - `COUNT(*)` always is for deletions, and makes `COUNT(E)` so. With
     nulls, `SUM(E)` needs both `COUNT(*)` and `COUNT(E)`.
   - "MIN and MAX are not self-maintainable with respect to deletions,
     and cannot be made self-maintainable": when the minimum is
     deleted, the new one "must be recomputed from the changes and the
     base data."
   - `COUNT(DISTINCT E)` is not distributive.

   "Cannot be made" holds for a view that stores aggregate values. It
   stops holding when the state is a count per value (I, §3).
4. **Mergeable summaries.** Agarwal, Cormode, Huang, Phillips, Wei and
   Yi, "Mergeable Summaries", PODS 2012
   ([PDF](https://users.cs.utah.edu/~jeffp/papers/merge-summ.pdf), R).
   §1.1: a summary is mergeable "if there exists an algorithm A that
   produces a summary S(D1 ⊎ D2, ε) from any two input summaries",
   keeping the error and the size bound. Merging one item at a time is
   the streaming model, so "mergeability is a strictly stronger
   requirement than streaming." Linear sketches (AMS, Count-Min) are
   mergeable by construction, and summaries that keep a maximum or a
   top-k, the distinct-count ones among them, merge easily (§1.2).
   Results: heavy hitters in O(1/ε) deterministic; quantiles fully
   mergeable only with randomisation, O((1/ε) log^1.5(1/ε)); the
   deterministic Greenwald-Khanna summary is only one-way mergeable
   (§3).
5. **The algebra.** Tangwongsan, Hirzel and Schneider, "Optimal and
   General Out-of-Order Sliding-Window Aggregation", PVLDB 12(10), 2019
   ([PDF](https://www.vldb.org/pvldb/vol12/p1167-tangwongsan.pdf), R),
   Table 1, by invertible, associative and commutative:
   - Sum-like (sum, count, mean, stddev): all three.
   - Collect-like (collect list, concat strings): one cross.
   - Max-like (max, min, argMax, maxCount): not invertible.
   - Mergeable sketch (Bloom, CountMin, HyperLogLog): one cross.

   The check marks did not survive text extraction; which column each
   lone cross sits in is I (commutative for collect-like, invertible
   for sketches). The text says an invertible and commutative operator,
   "one with an explicit 'subtract' operation, e.g., sum", is the
   simplest to make incremental, and that a monoid needs associativity
   only.
6. **PostgreSQL says the same in its catalog.** A combine function
   makes an aggregate partial (parallel) and an inverse transition
   function lets a moving window drop a row
   ([User-Defined Aggregates](https://www.postgresql.org/docs/current/xaggr.html),
   R). "Partial (including parallel) aggregation is currently not
   supported for ordered-set aggregates"
   ([CREATE AGGREGATE](https://www.postgresql.org/docs/current/sql-createaggregate.html),
   R). In `src/include/catalog/pg_aggregate.dat` at `0f3b42ce806` (R,
   counted by script):
   - 136 aggregates; 110 have a combine function, 36 an inverse.
   - All 26 `min` and all 26 `max` have a combine function and none an
     inverse (`max(int4)`, lines 98 to 100).
   - `count`, `avg`, the variances and the integer and numeric sums
     have both. `sum(float4)` and `sum(float8)` have no inverse (lines
     70 to 73).
   - The 7 ordered-set aggregates (`percentile_cont`,
     `percentile_disc`, `mode`) and the 4 hypothetical-set ones have
     neither.
   - `bool_and` and `bool_or` have an inverse, through a separate
     16-byte moving state (`bool_accum`, `bool_accum_inv`, lines 523 to
     527). ADR-0003 is the same move.
7. **Floats are not invertible.** PostgreSQL's own example: a float
   sum with subtraction as inverse "returns `0` as its second result,
   rather than the expected answer of `1`", since adding 1 to 1e20
   gives 1e20 (xaggr.html §36.12.1, R). Feldera: the linear aggregates
   "are actually non-linear when applied to DOUBLE or FLOAT values"
   ([Aggregate Operations](https://docs.feldera.com/sql/aggregates/),
   R). ADR-0003 refuses float columns for this reason.
8. **Exact median in one pass needs the data.** Munro and Paterson,
   "Selection and sorting with limited storage", Theoretical Computer
   Science 12, 1980: a deterministic one-pass exact median needs
   storage linear in the input; p passes need about N^(1/p) (U, from
   citing papers; the theorem was not read).
9. **Quantile and distinct sketches.** All from abstracts (R) unless
   tagged:
   - t-digest, Dunning and Ertl
     ([arXiv 1902.04023](https://arxiv.org/abs/1902.04023)): accuracy
     "relative to max(q, 1-q)", best near the tails; summaries combine
     "with no loss in accuracy". The abstract states no error bound.
   - KLL, Karnin, Lang and Liberty
     ([arXiv 1603.05346](https://arxiv.org/abs/1603.05346)):
     O((1/ε) log log(1/δ)) space with a matching lower bound, rank
     error.
   - DDSketch, Masson, Rim and Lee, PVLDB 12(12), 2019
     ([arXiv 1908.10693](https://arxiv.org/abs/1908.10693)): "fully-
     mergeable, relative-error", since rank error guarantees "do not
     preclude arbitrarily large relative errors" on skewed data.
   - HyperLogLog, Flajolet, Fusy, Gandouet and Meunier, 2007: standard
     error about 1.04/sqrt(m) for m registers (U). A register keeps a
     maximum, so a delete can't be applied (I).
   - Deletes: KLL±, Zhao, Maiyya, Wiener, Agrawal and El Abbadi, PVLDB
     14(7), 2021 ([PDF](http://www.vldb.org/pvldb/vol14/p1215-zhao.pdf),
     U), is "the first quantile approximation algorithm to operate in
     the bounded deletion model", where deletes are a capped fraction
     of inserts. Unbounded deletes need a linear sketch over a bounded
     integer universe (Dyadic Count Sketch, U).

## 2. What other systems do

| System | min and max under delete | Holistic aggregates |
| --- | --- | --- |
| PostgreSQL window | no inverse; "will recompute the whole aggregation whenever the start of the frame moves" (R) | ordered-set; no partial aggregation (R) |
| pg_ivm | recomputed from the base tables when the current end is deleted; "it can takes a long time" (R) | none; `count`, `sum`, `avg`, `min`, `max` only (R) |
| Feldera | whole group kept: "O(N) space"; O(G) on tables declared append-only (R) | not looked up |
| Materialize | "builds a hierarchy of aggregations"; "demanding in terms of main memory"; a group-size hint cuts levels (R) | not looked up |
| Flink | a count per value in the accumulator, scanned when the end is retracted (U, memory) | not looked up |
| ClickHouse | states merge (`-State`, `-Merge`); the engine page says nothing of deletes (R) | `quantileExact` "consumes O(n) memory"; the rest approximate (R) |
| Druid | exact, insert only (I) | approximate only: Theta and HLL for distinct, DataSketches Quantiles "in general" (R) |
| TimescaleDB | not looked up | sketches: "uses `uddsketch`, but you can also choose to use `tdigest`" (R) |

Sources: [pg_ivm README](https://github.com/sraoss/pg_ivm), [Feldera
aggregates](https://docs.feldera.com/sql/aggregates/), [Materialize
optimization](https://materialize.com/docs/transform-data/optimization/),
[ClickHouse
AggregatingMergeTree](https://clickhouse.com/docs/engines/table-engines/mergetree-family/aggregatingmergetree),
[quantileExact](https://clickhouse.com/docs/sql-reference/aggregate-functions/reference/quantileexact),
[Druid aggregations](https://druid.apache.org/docs/latest/querying/aggregations/),
[TimescaleDB approximate
percentiles](https://www.tigerdata.com/docs/use-timescale/latest/hyperfunctions/percentile-approx).

Three answers to min under delete, then: recompute from the source
(PostgreSQL, pg_ivm), keep the group's values (Feldera, Flink,
Materialize), or forbid deletes (append-only modes, Druid). Straum
holds no rows to recompute from (ADR-0003), reading PostgreSQL again
is a start over (ADR-0009), and it must take deletes, so only the
second is open to it.

## 3. Where Straum sits

All I, read against `crates/straum-domain/src/rollup.rs` (`State`,
`add`, `remove`) and `aggregate.rs` (`Partials`).

- **Every cell state is a count or a sum.** A count of nulls, which
  the row count less gives the non-null count; a sum and that count;
  a count per value; counts of true and false. Each merges by adding
  and takes a delete by subtracting. Straum has no state that only
  merges.
- **min, max and count distinct are bought with size.** Their state is
  a count per value, so it is not fixed size: in Gray's terms Straum
  stores them as holistic to make them delete-safe. Memory follows
  distinct values per cell (ADR-0003's open question for count
  distinct). `ValueCounts` keeps each value in as many bytes as its
  cell's values need: a few values in the cell itself, more in one
  sorted run, and only many in several (ADR-0003).
- **An ask merges cheaper than it stores.** min and max read the two
  ends mirrored beside the counts, so a merge is one comparison a cell.
  Count distinct has no such mirror: a merge unions the value sets, so
  its ask cost follows values, not cells.
- **The duplicate rule is idempotence.** `spec.md` says a dim key held
  twice doubles `count` and `sum` and leaves `min`, `max`,
  `count(DISTINCT)`, `bool_and` and `bool_or`. Those five are the ones
  whose merge gives the same answer when a row is merged twice.
- **Exact median, percentile and mode fit the state min and max
  keep.** The count per value is ordered. Merging the counts of the
  cells in range and walking to the middle gives an exact answer and
  takes a delete as min does. The costs: ask time follows distinct
  values in range, as count distinct's does, and there is no mirror to
  shortcut it.
- **Sketches fit badly.** The quantile and distinct sketches of §1.9
  merge but are insert-only, so a `DELETE` in the WAL could not apply,
  and their answers are approximate where every Straum answer is
  exact. The ones that take deletes need a bounded integer universe or
  a cap on deletes.

## 4. What to add next (2026-11-19)

What people use and ask for, read against what Straum can answer
exactly and take a delete for. Straum answers exactly or refuses: no
sketch, no estimate (jb, 2026-11-19). The order in §4.4 is jb's plan,
not yet an ADR.

### 4.1 What people use

- **Query logs.** One study counts aggregates by function: over 667M
  Snowflake queries from BI tools, the top five by share of aggregating
  queries are `anyvalue` 58%, `sum` 15%, `count` 12%, `max` 11% and
  `min` 5%. BI tools use `anyvalue` to carry a descriptive column
  without growing the `GROUP BY` (Bress et al., PVLDB 18, 2025,
  Table 6, [PDF](https://www.vldb.org/pvldb/vol18/p5126-bress.pdf), R).
  Redset and Snowset hold no SQL text, so they count no functions (R).
- **Benchmarks.** Share of BIRD and Spider queries: counting 31.29% and
  39.85%, distinct 15.45% and 8.41%, average 3.59% and 6.48%, min or
  max 2.48% and 5.71%, sum 2.67% and 2.61% (FuncEvalGMN, appendix K,
  [arXiv 2407.14530](https://arxiv.org/pdf/2407.14530), R).
- **Built into BI layers.** Of nine (Looker, Cube, MetricFlow,
  Metabase, Superset, Tableau, Power BI quick measures, Grafana reduce,
  PromQL), count distinct is in 8; median or percentile in 5 (Looker,
  MetricFlow, Metabase, Grafana, PromQL); variance or stddev in 5
  (Metabase, Tableau, Power BI as variance only, Grafana, PromQL).
  Superset's pages disagree on both. Looker, Cube and MetricFlow have
  no variance. First and last: Grafana only. `sum(DISTINCT)`: Looker
  only. `corr` and weighted average: Power BI only. `mode`, covariance,
  regression and `min_by`: none (R, from the
  [Looker](https://docs.cloud.google.com/looker/docs/reference/param-measure-types),
  [Cube](https://docs.cube.dev/reference/data-modeling/measures),
  [MetricFlow](https://docs.getdbt.com/docs/build/measures),
  [Metabase](https://www.metabase.com/docs/latest/questions/query-builder/expressions-list),
  [Tableau](https://help.tableau.com/current/pro/desktop/en-us/calculations_aggregation.htm),
  [Power BI](https://learn.microsoft.com/en-us/power-bi/transform-model/desktop-quick-measures),
  [Grafana](https://grafana.com/docs/grafana/latest/panels-visualizations/query-transform-data/calculation-types/)
  and [PromQL](https://prometheus.io/docs/prometheus/latest/querying/operators/)
  pages).

### 4.2 What people ask for

Thumbs-up on GitHub issues, read 2026-11-19 (R; totals I):

- Time-weighted average, 43 over four issues:
  [toolkit#208](https://github.com/timescale/timescaledb-toolkit/issues/208)
  18,
  [timescaledb#2536](https://github.com/timescale/timescaledb/issues/2536)
  15, [toolkit#46](https://github.com/timescale/timescaledb-toolkit/issues/46)
  5, [timescaledb#7155](https://github.com/timescale/timescaledb/issues/7155)
  5.
- Count distinct that rolls up, about 30, spread over TimescaleDB,
  ClickHouse, Pinot, RisingWave and Cube
  ([timescaledb#3247](https://github.com/timescale/timescaledb/issues/3247)
  10, [ClickHouse#33358](https://github.com/ClickHouse/ClickHouse/issues/33358)
  10). Straum has it exactly.
- Last value per key, about 30
  ([ClickHouse#57240](https://github.com/ClickHouse/ClickHouse/issues/57240)
  11, [ClickHouse#78869](https://github.com/ClickHouse/ClickHouse/issues/78869)
  10 and 6 hearts).
- Exact percentile and median: an issue in almost every product, about
  7 thumbs-up in all. Users take a sketch instead (I).
- No request found for variance, stddev or weighted average alone.
  That shows no demand was found, not that there is none.

No request has more than 18. The searches missed known issues, so the
counts are a floor (I).

### 4.3 What other incremental systems do

- Feldera: `COUNT`, `SUM`, `AVG` and the `STDDEV` and `VAR` family are
  linear for integer and `DECIMAL`, not for `DOUBLE`
  ([aggregates](https://docs.feldera.com/sql/aggregates), R).
- Oracle fast refresh: `STDDEV` and `VARIANCE` need `COUNT(x)` and
  `SUM(x)`, with `SUM(x*x)` recommended
  ([basic materialized views](https://docs.oracle.com/en/database/oracle/oracle-database/19/dwhsg/basic-materialized-views.html),
  R).
- Databricks refreshes `SUM`, `AVG` and covariance over `FLOAT` in
  full, and over `DECIMAL` incrementally
  ([incremental refresh](https://docs.databricks.com/aws/en/optimizations/incremental-refresh),
  R).
- Flink moved variance to Welford's M2 to stop float cancellation
  giving NaN and negative variances
  ([FLINK-38740](https://issues.apache.org/jira/browse/FLINK-38740), R).
  Exact integers cancel nothing, so plain sums do (I).
- `min_by` and `max_by`: Feldera keeps O(N), BigQuery and Snowflake
  refresh them incrementally ([Feldera](https://docs.feldera.com/sql/aggregates),
  [Snowflake](https://docs.snowflake.com/en/release-notes/2026/other/2026-03-19-min-max-incremental-dynamic-table),
  R). PostgreSQL has neither (U).
- TimescaleDB recomputes a whole bucket after any change, which is why
  any aggregate works in its continuous aggregates
  ([about](https://www.tigerdata.com/docs/use-timescale/latest/continuous-aggregates/about-continuous-aggregates),
  R). Its time-weighted average and counter state keep first and last
  points and never retract
  ([time_weight](https://github.com/timescale/timescaledb-toolkit/blob/main/docs/time_weighted_average.md),
  R).

### 4.4 What fits Straum

Cell state stays one enum per input column, with `add`, `remove`,
`add_from` and `sub_from` (`crates/straum-domain/src/rollup/state.rs`,
ADR-0003); an accumulator trait per aggregate would keep shared state
twice and lose the column layout (jb, 2026-11-19). A new aggregate is
a new answer over a state, or one more column in one.

| Order | Aggregate | State | Note |
| --- | --- | --- | --- |
| built | `var_pop`, `var_samp`, `stddev_pop`, `stddev_samp` | `Sum` and a column of squares | §4.5, §4.6; `variance` and `stddev` refused, one spelling (ADR-0004) |
| 2 | `mode() WITHIN GROUP (ORDER BY c)` | `Extent` | ties take the first value (U) |
| 3 | `sum(DISTINCT c)`, `avg(DISTINCT c)` | `Distinct` | |
| open | `percentile_cont` | `Extent` | PostgreSQL answers in `double precision` |
| no | `min_by`, `max_by` | | not PostgreSQL (ADR-0004) |
| no | `any_value` | | BI tools write it into their own SQL; asks are not pgwire |
| no | time-weighted average, counter rate | | follows row order |
| no | `corr`, `covar_*`, `regr_*` | | needs state over two columns |
| no | `string_agg`, `array_agg` | | holds every value |
| no | top-k, any sketch | | not exact |

`mode` and `percentile_cont` are refused by name in ADR-0003, which
changes with them.

### 4.5 Variance merges exactly

With n the values that are not null, S1 their sum and S2 the sum of
their squares, and the mean m = S1/n (I):

1. Sum of (x - m)^2 = S2 - 2m·S1 + n·m^2 = S2 - S1^2/n. So
   `var_pop` = (n·S2 - S1^2) / n^2 and `var_samp` = (n·S2 - S1^2) /
   (n·(n - 1)), from n, S1 and S2 alone.
2. n, S1 and S2 are sums over rows. Over two disjoint sets of rows each
   is the sum of the two, so a merge adds them, in any split and order.
3. A delete subtracts (1, x, x^2). Integers subtract exactly.
4. S1^2 <= n·S2 (Cauchy-Schwarz), so the square root never meets a
   negative.
5. A `numeric(p, s)` value is held as x·10^s. The numerator scales by
   10^2s and the denominator not at all, so the answer is divided by
   10^2s exactly.

A script checked this once against the definition in exact fractions:
20000 random trials, values up to 2^62 in magnitude, scales 0, 2 and 4,
1 to 6 cells merged, random rows deleted. All 40000 comparisons, sample
and population, were equal. Averaging the cells' own variances, planted
as a bug, gave a wrong answer in 17423 trials. Merging variances or
standard deviations directly is the trap `aggregates.md` shows for
averages.

PostgreSQL agrees on the edges (R, `crates/straum-pg/src/tests/`
against a running server): one value gives null for `var_samp` and
`stddev_samp`, 0 for the `_pop` ones. Straum rounds half to even at
four places past the answer's scale, as `avg` does; PostgreSQL's own
answer carries more places, so the tests round it half to even there
before comparing.

### 4.6 Overflow of the squares

In `i128` the sum of squares overflows soon for wide values (computed):
after 170 rows a cell of `numeric(18, s)` at its largest, and after 1
row of `bigint` near its largest. Values up to 10^9 take
170141183460469231731 rows.

`Sums` widens a column from `I32` to `I64` to `I128` when a sum needs
it (`crates/straum-domain/src/column.rs`). One more width, 256 bits,
ends the overflow: below 2^64 rows, the bound ADR-0003 gives for sums,
S2 needs at most 191 bits. The ask needs 256 bits in any case, as S1^2
takes up to 255. Only a column whose values need it pays. `std` has no
256-bit integer, so Straum has its own, `U256` in
`crates/straum-domain/src/wide.rs` (jb, 2026-11-19: no crate): add and
subtract that wrap, products, division by a `u128`, square root and
decimal digits. A variance past an `i128` at its scale answers as
`Value::Wide`.

Rejected: refusing variance on `bigint` and wide `numeric` when the
spec is checked, which shuts out the common amount column; and a
checked add that stops Straum (ADR-0009), which stops on values a `sum`
takes.

### 4.7 How the tests hold it

- `crates/straum-domain/src/spread.rs` checks the answer of merged
  cells, with rows taken out, against the definition in exact integers,
  ties and the values at the ends of `bigint` included; `wide.rs`
  checks `U256` against `u128` and against its own inverses.
- The workloads, `crates/straum-domain/tests/store/workload.rs` and
  `tests/joins/`, add, update and remove random rows and check random
  asks across buckets and widths against a reference,
  `tests/common/mod.rs` `spread`. It computes `sum((n * x - s1)^2)` in
  a pass of its own, not `n * s2 - s1^2`, so it shares no formula with
  the code.
- `crates/straum-pg/src/tests/references.rs` compares the taxi asks
  with PostgreSQL's `var_samp`, `var_pop`, `stddev_samp` and
  `stddev_pop` over the same rows.
- Planted bugs, each run once and each failing (2026-11-19): `remove`
  that leaves x^2 in; `add_from` that drops a cell's squares when it
  moves; an ask that merges only the first cell's squares when one
  group takes a bucket; a `var_samp` over n^2; a sample of one that
  answers 0; a lost carry in `U256::product`.

## Open points

- Exact median over the min and max state, built as `percentile_disc`
  (ADR-0003): over six months of taxi trips the median fare takes
  32.3 ms against PostgreSQL's 6,584.6 ms, where a sum takes under half
  a millisecond (benchmark.md, 2026-11-15). A running count per run
  would make it a binary search on value; unbuilt.
- Count distinct's memory (ADR-0003) is the same question as a
  percentile's.
- `percentile_cont`: exact to PostgreSQL's bit, but a `double
  precision` answer. Add it, or keep `percentile_disc` as the median
  (§4.4).
