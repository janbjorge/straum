# Aggregates 101

Straum holds [cells](how-it-works.md#cells), never rows, so every
aggregate must do two things. Its cells must merge, because an ask
merges many cells into one answer per group. A row must be able to
leave, because a `DELETE` or `UPDATE` in PostgreSQL takes a row out of a
cell, which must then read as if the row had never been there. These two
decide which aggregates Straum has and what each cell keeps for them.

## Merging

Take the amounts of two hours of events:

```text
              09:00         10:00                 both hours
amounts       10 20 30      40 50 60 70 500

min           10            40                    10
max           30            500                   500
sum           60            720                   780
count         3             5                     8
avg           20            144                   97.5
median        20            60                    45
```

Merged from each hour's own answer, `min`, `sum` and `count` come out
right: the smaller of 10 and 40 is 10, 60 and 720 add to 780, and 3 and 5
add to 8, so the merge of two counts is a sum, not a count. `avg` comes
out wrong: the average of 20 and 144 is 82, not 97.5, because the second
hour has more rows and must weigh more. So does `median`: the median of
20 and 60 is 40, not 45, and no weight fixes it.

Aggregates fall in three classes by what a part must keep for the merge to
come out right:

| Class | A part keeps | Examples |
| --- | --- | --- |
| Distributive | its own answer | `count`, `sum`, `min`, `max`, `bool_and`, `bool_or` |
| Algebraic | a few numbers, always the same few | `avg` (a sum and a count), variance, standard deviation |
| Holistic | more as it holds more values | median, percentiles, mode, `count(DISTINCT c)` |

`avg` is right once each hour keeps its sum and count: 780 over 8 is
97.5. A median has no such handful of numbers; to merge medians exactly,
each part must keep its values, or a count per value.

## Removing a row

Delete the event with an amount of 10 from the first hour. `sum` and
`count` subtract from what the cell holds: 60 less 10 is 50, 3 less 1 is
2. The `min` was 10 and is now 20, which a cell that kept only "10"
cannot know. PostgreSQL would scan the rows again; Straum has no rows.

So aggregates split a second way, independent of the first: one whose
state can subtract takes a delete, and one that keeps only the winner
does not. `min` and `max` merge well and delete badly.

So for `min` and `max` a cell keeps more than the answer: a count per
value, `10: 1, 20: 1, 30: 1`. Deleting 10 drops its
count to zero, and 20 is the smallest value left. The same count per value
tells `count(DISTINCT c)` when a value's last row has gone, and `bool_and`
and `bool_or` keep a count of true and of false for the same reason.

## What a cell keeps

| Measure | A cell keeps | Cells merge by | A row leaves by |
| --- | --- | --- | --- |
| `count(*)` | its row count | adding | subtracting one |
| `count(c)` | a count of the nulls among its rows, the one a `sum` on its column keeps | adding | subtracting one |
| `sum(c)`, `avg(c)` | a sum, and a count of the nulls among its rows | adding both | subtracting both |
| `var_samp(c)`, `var_pop(c)`, `stddev_samp(c)`, `stddev_pop(c)` | the same, and a sum of squares | adding all three | subtracting all three |
| `min(c)`, `max(c)` | a count per value | taking the smaller or larger end | lowering the value's count |
| `count(DISTINCT c)`, `sum(DISTINCT c)`, `avg(DISTINCT c)` | a count per value, numbers kept as `min` and `max` keep them | marking numbers in a set of bits, or uniting other values, then counting and adding each once | lowering the value's count |
| `percentile_disc(f) WITHIN GROUP (ORDER BY c)`, `mode() WITHIN GROUP (ORDER BY c)` | a count per value, the one `min` and `max` keep | gathering the counts, then counting up to the rank, or to the value counted most | lowering the value's count |
| `bool_and(c)`, `bool_or(c)` | a count of true and of false | adding both | subtracting one |

Every state is a count or a sum, so every one merges and takes a
delete, and every answer is exact. Sums are integers for that reason: a
float sum differs by the order it was added in and can't be subtracted
exactly, so Straum refuses float columns.

A spec's author sees two effects. The first is memory. The table's first
four rows cost a few numbers a cell; `min`, `max`, `percentile_disc` and
`count(DISTINCT c)` cost by the distinct values in each cell. A
percentile or mode beside a `min` or `max` on its column costs nothing
more. A `min` or `max` keeps a value in as few bytes as its cell's values
need: `max(fare_amount)` per hour and dropoff zone keeps 14,943,826 fares
of the [taxi trips](performance.md) in 76 MiB, 5.3 bytes a fare. A
`count(DISTINCT c)` of numbers keeps them the same way, in fewer bytes
than a `max`, and beside a `min`, `max`, percentile or mode on its column
reads their counts and costs nothing more. One of `text`, `uuid`, `date`
or `boolean` keeps a hash map per cell, over 15 times as many bytes in a
test of 40 values a cell. Either holds every distinct value.

The second is rows counted twice: a dim that holds a key twice adds the
fact row twice ([Joining a dim](spec.md#joining-a-dim)). A sum of counts
doubles; a count per value merged twice keeps the same `min`, `max` and
distinct values.

## Variance and standard deviation

A variance merges as `avg` does, from a few numbers per part. Each cell
keeps the count `n` of its values, their sum and the sum of their
squares, which give the squared distances from the mean:

```text
sum of (x - mean)^2  =  sum of x^2  -  (sum of x)^2 / n

var_pop   =  (n * sum of x^2 - (sum of x)^2) / n^2
var_samp  =  (n * sum of x^2 - (sum of x)^2) / (n * (n - 1))
```

Take the two hours above:

```text
              09:00         10:00                 both hours
n             3             5                     8
sum           60            720                   780
sum of x^2    1400          262600                264000
var_samp      100           39730                 26850
```

The three numbers add across hours, and the formula on the merged ones
gives 26850, the variance of all eight amounts. The mean of the two
hours' own variances, 19915, is wrong for the reason the mean of two
averages is. A delete subtracts its value, its square and one from `n`.

Integers add and subtract exactly, so the three never drift, in whatever
order rows come and go. Straum divides and rounds only when it answers,
and takes a standard deviation as the square root of the merged variance.
The sum of squares of values near the ends of `bigint` passes 128 bits, so
it widens to 256 bits, as a sum widens from 32 bits to 64 and 128.

A `sum` or `avg` on the same column shares the cell's sum and count, so
a variance beside them adds only its sum of squares.

## Median and percentiles

An exact median needs each cell's values or a count per value, and a `min`
or `max` already keeps the counts. So Straum has `percentile_disc`, which is
exact and follows deletes. The `usage` view of
[Spec](spec.md#synopsis) holds three such measures:

```sql
max(e.amount) AS top_amount,
percentile_disc(0.5) WITHIN GROUP (ORDER BY e.amount) AS median_amount,
percentile_disc(0.99) WITHIN GROUP (ORDER BY e.amount) AS p99_amount
```

`max` and both percentiles read one count per value of `e.amount`. An
ask over the two hours above gathers the counts of their cells,
`10 20 30 40 50 60 70 500`, and counts up to rank `ceil(fraction * n)`:
the median is the 4th of 8, 40, and p99 the 8th, 500. That 40 is a value
the rows hold; the 45 in the table above, which `percentile_cont` would
give, lies between two. Delete the 500 and p99 is the 7th of 7, 70.

The rank is taken in float8, as PostgreSQL takes it, so the answer is
PostgreSQL's to the row: 0.07 of 100 values is the 8th there, not the 7th.
Nulls are skipped, no values give null, and the answer keeps the column's
type.

What Straum refuses where PostgreSQL takes more is under
[Measures](spec.md#measures).

A percentile costs time in the ask. Where a `sum` merges a number a
cell, a percentile gathers every value of every cell in its range and
finds the one at its rank, splitting the values left at their middle one
each step without sorting them all. Over a long range of many values it
takes milliseconds where the others take microseconds: the median fare over
the six months of [benchmark](performance.md#results) trips takes about
25 ms. A group of one cell is read in place, its values in order, with
nothing gathered.

A `mode()` gathers the same way and puts the values in order, so a value
several cells hold is counted once, its counts added. A
`count(DISTINCT c)` of numbers marks the values of several cells in a
set of bits from the least to the greatest, then counts the marks;
values too far apart for that are sorted instead. One of other types
unites them by reference, without copying.

Other systems answer with a sketch, a small summary that merges and
gives an approximate answer: t-digest, KLL and DDSketch for percentiles,
HyperLogLog for distinct counts. Of these only DDSketch takes a delete.
Straum's answers are exact and follow deletes, so it has no sketch
([architecture decision record
ADR-0003](https://github.com/janbjorge/straum/blob/main/docs/adr/0003-straum-holds-cells-and-nothing-else.md)).

The sources behind this page, and how PostgreSQL and other systems handle
merging and removing a row, are in
[AGGREGATES.md](https://github.com/janbjorge/straum/blob/main/research/AGGREGATES.md).

Next: [Setting up PostgreSQL](postgresql.md), to connect a database of your
own.
