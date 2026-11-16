# ADR-0057: Page state keeps whole rows

Status: Accepted, 2026-11-16

## Context

ADR-0049 has page state keep each row's bytes only up to the end of
the last column the spec reads. The physical stream shipped keeping
whole rows. Cutting them means rebuilding an update's unchanged prefix
and suffix from a row whose length Straum no longer knows, with a
partial walk of its columns: a new path to get wrong for each major.

## Decision

Page state keeps each row whole, as PostgreSQL has it on the page. Its
size stays bounded the way ADR-0049 has it: a row goes when a prune or
vacuum frees its slot, a page goes once a checkpoint's redo point
passes its last change, and past `--max-page-memory` Straum starts
over. This supersedes only ADR-0049's "bytes up to the end of the last
column the spec reads".

## Consequences

- An update is rebuilt from the old row as PostgreSQL's redo does, and
  the oracles against `pageinspect` compare whole rows.
- Memory follows the bytes of the spec tables' pages changed in one
  checkpoint cycle, plus open transactions' changes: about 1.2 times
  those bytes when measured. Wide tables of which the spec reads few
  columns cost more than ADR-0049 expected.
- A shorter `checkpoint_timeout` or a larger `--max-page-memory` is
  the answer to a start-over past the limit. Cutting rows stays open
  for a later record, should a measurement call for it.
