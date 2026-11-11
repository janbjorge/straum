# ADR-0035: Only the narrowest width is sealed

Status: Accepted, 2026-11-11

## Context

ADR-0033 seals every bucket older than `--seal-after`, at every width.
An ask reads the widest buckets that fit its range and the narrowest
only at its ends (ADR-0028), and the narrowest width holds most cells:
76% of the taxi month's, 50% of the warehouse copy's. A rule per width,
a quiet period after a change, or a memory budget each add state to
explain.

## Decision

Only the `date_bin` width is sealed: a bucket of it is sealable once its
end is older than `--seal-after`. Wider widths stay in memory. A change
unseals the buckets it falls in, and the next seal pass seals them
again. A bucket read back from the spill is thawed, checked against the
store, before an ask or a change reads it.

## Consequences

- An ask over a long range reads the spill only at its ends. An ask
  grouped by `bucket` over old time reads it for every bucket.
- Memory follows the wider widths in full; a view whose wider widths
  collapse little frees less.
- A bucket changed often is read and written again each time. That
  churn is unmeasured; a quiet period is the first fix if it shows.
- A spill that fails starts Straum over (ADR-0015). An ask that can't
  read a sealed bucket answers 500 and names it.
- Amends ADR-0033.
