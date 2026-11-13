# Aggregates: which merge, which take a delete (2026-11-12)

Straum answers an ask by merging cells and follows PostgreSQL by adding
and removing rows from cells. Both put a demand on an aggregate, and
the two demands are separate. This note holds the sources on each, how
other systems meet them, and where Straum's cell states sit. The guide's
version is `docs/src/aggregates.md`. Nothing here is a decision.

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
     527). ADR-0041 is the same move.
7. **Floats are not invertible.** PostgreSQL's own example: a float
   sum with subtraction as inverse "returns `0` as its second result,
   rather than the expected answer of `1`", since adding 1 to 1e20
   gives 1e20 (xaggr.html §36.12.1, R). Feldera: the linear aggregates
   "are actually non-linear when applied to DOUBLE or FLOAT values"
   ([Aggregate Operations](https://docs.feldera.com/sql/aggregates/),
   R). ADR-0016 refuses float columns for this reason.
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
holds no rows to recompute from (ADR-0002), reading PostgreSQL again
is a start over (ADR-0015), and it must take deletes, so only the
second is open to it.

## 3. Where Straum sits

All I, read against `crates/straum-domain/src/rollup.rs` (`State`,
`add`, `remove`) and `aggregate.rs` (`Partials`).

- **Every cell state is a count or a sum.** A non-null count; a sum
  and a count; a count per value; counts of true and false. Each
  merges by adding and takes a delete by subtracting. Straum has no
  state that only merges.
- **min, max and count distinct are bought with size.** Their state is
  a count per value, so it is not fixed size: in Gray's terms Straum
  stores them as holistic to make them delete-safe. Memory follows
  distinct values per cell (ADR-0016's open question for count
  distinct). `ValueCounts` keeps the common single-value cell in
  place, a few values in one sorted run, and only many in a tree.
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

## Open points

- Exact median over the min and max state: ask cost on a wide range
  with many values is unmeasured.
- Count distinct's memory (ADR-0016) is the same question as a
  percentile's would be.
- Variance and standard deviation are algebraic and invertible over
  integers: a sum of squares beside the sum. `i128` overflow of the
  squares is unchecked.
