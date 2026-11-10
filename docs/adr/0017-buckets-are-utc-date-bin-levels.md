# ADR-0017: Buckets are UTC date_bin levels

Status: Superseded by ADR-0028, 2026-11-10

## Context

Asks range over time. Cells could be kept at one width and merged up at
ask time, which reads every fine cell for a coarse ask (1.4 s on the
taxi month by day and borough), or at several widths kept in step.

## Decision

The view's `date_bin` sets the finest bucket, from the epoch, in UTC. A
view may declare wider ones, `WITH (buckets = '1 day, 7 days')`, each a
whole multiple of the width before it. Straum keeps one level of cells
per width, updated in the same commit. An ask reads its range from the
widest buckets that fit and the finest at its ends; an ask grouped by
`bucket` reads the finest level. Ask ranges start and end on bucket
edges, so an ask never reads raw rows.

## Consequences

- Levels are an index: answers don't change, memory grows per width.
- The taxi ask by borough went from 1.4 s to 6.6 to 8.8 ms.
- The load stays one pass at the finest width; each width is filled on
  its own thread.
- Calendar grains, custom origins and sub-second grains are out.
  Ask-time zones and `hour`, `day`, `week`, `month` keys are planned
  (M8), not here.
