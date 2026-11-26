# ADR-0005: Asks are Straum SQL with parameters

Status: Accepted 2026-11-10; revised 2026-11-26

## Context

Asks were JSON fields (`fact`, `measures`, `by`, `filter`, ...) until
2026-11-10: a second language for what the spec says in SQL. Speaking
pgwire would make Straum look like a database it isn't. The first
answers carried an LSN, which a client can't act on without a WAL
barrier, and a commit time, which stops on a quiet database. Without a
stated target, an aggregate could land at any speed.

## Decision

`POST /ask` takes `{"query", "parameters"}`, sent as
`application/json` and refused with 415 otherwise (ADR-0011). The query
is one `SELECT` over a view the client names: keys then measures by
name; `WHERE`, if
written, holds `=` and `IN` on keys joined by `AND`, and at most one
each of `time >= $n` and `time < $m`, `time` the name the view gives
its time. The walk doesn't know the view; the spec check says which
name is the time, and refuses `>=` or `<` on any other. A bound left out leaves its
end open, at the edge of the buckets held, so an ask with no `WHERE`
reads them all. `GROUP BY` names exactly the selected keys; `HAVING
measure op $n [AND ...]`, `op` one of `= <> < <= > >=`, compares
selected measures. A key may be `buckets(time, '<width>')`, the
same in `SELECT` and `GROUP BY`: rows that width apart, at a width the
view keeps wider than its narrowest, with bounds on its edges, so
each row is one stored bucket. It is the spec's own `buckets`, so one
function names the time's widths in a view and in an ask; until
2026-11-19 an ask wrote `date_bin('<width>', time)`, PostgreSQL's name
without the origin PostgreSQL requires, and it is now refused, naming
the `buckets` to write. A key in `HAVING` or a measure in `WHERE` is
refused, naming the other place. Every value is a `$n`; a literal is
an error, so values never become query text.

Parameters are typed exactly. A key takes the integer, string or
boolean of its column, else `AskError::WrongType`. A JSON number with
a fraction or exponent is read from its digits (`serde_json`'s
`arbitrary_precision`), up to 38 after the point, and compares with
measures only. `HAVING` runs on finished groups after the cell loop,
without floats: a sum at either scale, an avg as sum over count, a
variance or standard deviation from its exact count, sum and sum of
squares, never from its rounded answer. A null meets no comparison; a
boolean takes `=` and `<>`.

An answer is `{"as_of", "rows"}`, each row keyed by its `SELECT`
names. `as_of` is when Straum last had applied all WAL the server had
written (ADR-0009): every commit before it is in the answer. The LSN
goes in a `Straum-LSN` header. avg rounds half to even four places past
its column's scale, trailing zeros dropped, in `straum-http`. A
variance or standard deviation rounds half to even in `straum-domain`,
four places past its answer's scale (ADR-0003), and prints every place.

Every ask in `bench/taxi.py` answers in under 1 ms, HTTP round trip
included, on the machine `docs/src/performance.md` names. An aggregate
whose asks miss it is not done, and a new one gets an ask there before
it lands.

## Consequences

- An ask is not valid PostgreSQL: measures go by bare name and refs
  are in scope without a join. No name is reserved: the time is
  whatever the view calls it.
- `straum-notation` walks an ask like a spec; `straum-http` reads only
  the JSON around it.
- A key or measure asked twice is refused, so `Answer` names its own
  columns and the writer needs no ask.
- `as_of` lags the truth by up to a second, never leads it, and moves
  on a quiet database. A WAL barrier can use the header later.
- Until 2026-11-17 both bounds were required, so an ask said what it
  read. Reading everything then took sentinel bounds such as
  `2000-01-01` and `2100-01-01`: many spellings of one meaning, and no
  cheaper, since the store reads only the buckets it holds. No `WHERE`
  is the one spelling; `WHERE true` stays refused.
- A time past the year 9999, which PostgreSQL keeps and RFC 3339 can't
  write, is reachable only through an open end. The answer writes it as
  `Unix second` and its number rather than fail.
- Straum refuses a bound with an offset on a view whose time is a
  `timestamp` (`AskError::OffsetOnWallClock`). PostgreSQL does not use
  the offset when it reads such text, so the bound gives a wall clock
  time. Straum reads an instant, so without this refusal it would
  answer for other buckets with no error. A bound with `Z`, or in Unix
  seconds, gives the wall clock time.
- A `buckets` row reads one stored bucket: 24 times fewer cells than
  hours for a day, 168 for a week, where every hour holds the same
  keys (`rows_at_a_wider_width_read_its_buckets` in
  `tests/cost/main.rs`).
  The width is query text, not a `$n`, since it sets the answer's
  shape. Rejected: any multiple of the narrowest width, built from
  narrower buckets, which costs what asking by those does.
- Rows follow the epoch grid: UTC days, weeks from Thursday. Rejected:
  a grid from the first row held, which would move as older rows
  arrive and part from PostgreSQL's `date_bin`. Local days, Monday
  weeks and months wait for an origin or zone on the view.
- `HAVING` costs one pass over the groups. An unselected measure can't
  be compared; allowing it later breaks no ask. `OR`, parentheses and
  expressions are refused. So is `!=`: Straum SQL keeps one spelling per
  meaning (ADR-0004), and `<>` is SQL's standard one.
- Percentile asks miss the target. The roadmap, each step measured:
  count the work; pick the rank without a sort; a running count per run
  (revises ADR-0003); counts per wider bucket across keys.
- Views with a cell per row, as the warehouse runs, are outside the
  target. `tests/cost/main.rs` guards counts and bytes on any machine.
