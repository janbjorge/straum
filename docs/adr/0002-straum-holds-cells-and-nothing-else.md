# ADR-0002: Straum holds cells and nothing else

Status: Accepted (retroactive), 2026-11-10; amended by ADR-0030

## Context

An aggregate server can keep the raw rows and compute on ask, keep a
column store and index it, or keep only the aggregate cells its specs
declare. Rows and columns are what PostgreSQL already holds, and holding
them again sets Straum's memory by the table's size, not the spec's.

## Decision

Straum keeps aggregate cells (count, sum, min, max, avg, count distinct)
keyed by the spec's rollup key and time bucket, and nothing else: no raw
rows, no column store, no copy of a dim table. Everything a cell needs
arrives with the fact row that feeds it.

## Consequences

- Memory is set by the number of distinct keys times buckets, not by
  the row count. On a copy of a warehouse's tables M12 went from 897 MiB with
  dim rows held to 84 MiB.
- An ask never reads raw rows, so ask ranges start and end on bucket
  edges (ADR-0017).
- Anything a cell can't be updated from in place has to come from
  PostgreSQL again: old rows on update and delete (ADR-0014), dim
  attributes (ADR-0013), or a fresh start (ADR-0015).
- Earlier plans held dim rows in memory and bound attributes late or at
  ingest; both went with M12.
