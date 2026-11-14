# Aggregates 101

Straum never holds a row. It holds [cells](how-it-works.md#cells), and
that asks two things of every aggregate:

1. **Can cells merge?** An ask reads many cells and merges them into
   one answer per group.
2. **Can a row leave?** A `DELETE` or `UPDATE` in PostgreSQL takes a
   row out of a cell, and the cell must then read as if the row had
   never been there.

Which aggregates Straum has, and what each cell keeps for them, follow
from these two questions.

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

Merge the two hours from their own answers and compare with the last
column:

- `min`: the smaller of 10 and 40 is 10. Right.
- `sum`: 60 and 720 add to 780. Right.
- `count`: 3 and 5 add to 8. Right. The merge of two counts is a sum,
  not a count.
- `avg`: the average of 20 and 144 is 82, not 97.5. Wrong: the second
  hour has more rows and must weigh more.
- `median`: the median of 20 and 60 is 40, not 45. Wrong, and no
  weight fixes it.

Aggregates fall in three classes by what a part must keep for the
merge to come out right:

| Class | A part keeps | Examples |
| --- | --- | --- |
| Distributive | its own answer | `count`, `sum`, `min`, `max`, `bool_and`, `bool_or` |
| Algebraic | a few numbers, always the same few | `avg` (a sum and a count), variance, standard deviation |
| Holistic | more as it holds more values | median, percentiles, mode, `count(DISTINCT c)` |

`avg` is right once each hour keeps its sum and count instead of its
average: 780 over 8 is 97.5. A median has no such handful of numbers.
To merge medians exactly each part must keep its values, or a count
per value.

## Removing a row

Delete the event with an amount of 10 from the first hour.

- `sum` and `count` subtract: 60 less 10 is 50, 3 less 1 is 2. The
  cell needs nothing it does not already hold.
- `min` was 10. The new minimum is 20, and a cell that kept only "10"
  has no way to know that. PostgreSQL would scan the rows again;
  Straum has no rows.

So a second split, apart from the first: an aggregate whose state can
subtract takes a delete, and one that keeps only the winner does not.
`min` and `max` merge well and delete badly.

Straum's way out is to keep more than the answer. For `min` and `max`
a cell keeps a count per value, `10: 1, 20: 1, 30: 1`. Deleting 10
drops its count to zero and 20 is the smallest value left. The same
count per value is how `count(DISTINCT c)` knows when a value's last
row has gone, and `bool_and` and `bool_or` keep a count of true and a
count of false for the same reason.

## What a cell keeps

| Measure | A cell keeps | Cells merge by | A row leaves by |
| --- | --- | --- | --- |
| `count(*)` | its row count | adding | subtracting one |
| `count(c)` | a count of the nulls among its rows | adding | subtracting one |
| `sum(c)`, `avg(c)` | a sum, and a count of the nulls among its rows | adding both | subtracting both |
| `min(c)`, `max(c)` | a count per value | taking the smaller or larger end | lowering the value's count |
| `count(DISTINCT c)` | a count per value | uniting the values | lowering the value's count |
| `bool_and(c)`, `bool_or(c)` | a count of true and of false | adding both | subtracting one |

Every state is a count or a sum, so every one both merges and takes a
delete, and every answer is exact. Sums are integers for that reason:
a float sum gives a different answer by the order it was added in and
can't be subtracted from exactly, so Straum refuses float columns.

Two things follow that a spec's author can see:

- **Memory.** The first three rows cost a few numbers a cell. `min`,
  `max` and `count(DISTINCT c)` cost by the distinct values in each
  cell. A `min` or `max` keeps a value in as few bytes as its cell's
  values need: `max(fare_amount)` per hour and dropoff zone keeps
  14,943,826 fares of the [taxi trips](benchmark.md) in 76 MiB. A
  `count(DISTINCT c)` over a column with many values holds every one
  of them.
- **Rows counted twice.** A join whose dim holds a key twice counts the
  fact row twice ([Joining a dim](spec.md#joining-a-dim)). `count` and
  `sum` double,
  because adding a row twice adds it twice. `min`, `max`,
  `count(DISTINCT c)`, `bool_and` and `bool_or` stay, because the same
  value merged twice changes nothing.

## Median and percentiles

Straum has none, and a spec that names one is refused. An exact median
needs each cell's values or a count per value, as `min` and `max`
already keep, and an ask would have to merge those counts over its
whole range instead of adding a number a cell.

Other systems answer with a sketch: a small summary that merges and
gives an approximate answer, t-digest, KLL and DDSketch for
percentiles, HyperLogLog for distinct counts. The common sketches
merge but can't take a delete, and their answers are estimates.
Straum's answers are exact and follow deletes, so it has no sketch.

The sources behind this page, and how PostgreSQL and other systems
handle the same two questions, are in
[AGGREGATES.md](https://github.com/janbjorge/straum/blob/main/research/AGGREGATES.md).
