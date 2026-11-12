# ADR-0041: bool_and and bool_or keep counts of true and false

Status: Accepted, 2026-11-12

## Context

`bool_and` and `bool_or` are min and max over booleans with false
below true. ADR-0016 has a cell state for min and max, a count per
value read as a scaled `i64`, and reads integers for every measure but
count distinct. PostgreSQL gives `bool_and` and `bool_or` an inverse
transition for the same reason Straum needs one: a deleted row has to
come out of the answer exactly.

## Decision

A boolean measure keeps a cell state of its own: per input column, the
count of true and the count of false. `bool_and` and `bool_or` share
it. An ask adds the counts of the cells it reads and answers when it
is done: `bool_and` is true when no value is false, `bool_or` when any
is true, and both are null when the group holds no value. The input
column must be `boolean`, and the answer is a boolean, `true` or
`false` in JSON.

The min and max state is not reused. It would read a boolean as 0 or
1 and answer an integer, and it keeps a tree per cell for a column
that has two values.

`every`, the SQL standard's name for `bool_and`, is refused, naming
`bool_and` (ADR-0029).

## Consequences

- A boolean measure costs two counts a cell, and a removed row costs
  one subtraction. Removing the last false turns a `bool_and` true.
- A dim key held twice doubles the counts and leaves the answers as
  they were, as it does for min and max.
- `Value` gains a boolean, so an answer's measure is no longer always
  a number or null. A table prints it on the left, as it does a key.
- ADR-0016's "sum, avg, min and max read integers" still holds; its
  list of cell states grows by this one and by the non-null count that
  `count(column)` keeps.
