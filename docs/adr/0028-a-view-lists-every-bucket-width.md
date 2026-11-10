# ADR-0028: A view lists every bucket width it keeps

Status: Accepted, 2026-11-10

## Context

ADR-0017 made `WITH (buckets = ...)` optional and had it name only the
widths wider than the body's `date_bin`. To know what Straum keeps, a
reader added the body's width to the option, and a view with no option
looked as if it kept nothing. The notation is meant to be strict:
nothing implied, nothing optional.

## Decision

Every view declares `WITH (buckets = '...')`, listing every width Straum
keeps, narrowest first. The first width must equal the body's `date_bin`
width, or the spec is refused by name. A view without the clause is
refused. `buckets` stays the only view option.

The rest of ADR-0017 holds: each width is a whole multiple of the one
before and starts at the epoch, Straum keeps one level of cells per
width, updated in the same commit, and an ask reads the widest buckets
that fit with the narrowest at its ends.

## Consequences

- The finest width is written twice, in the option and in `date_bin`,
  on purpose: the option holds the whole list, and the body stays a
  view PostgreSQL runs unchanged.
- Every spec changes: `WITH (buckets = '1 hour')` on a view that keeps
  hours only. Straum isn't live, so nothing is kept compatible.
- The domain is untouched; the parser checks the first width and hands
  the wider ones on as before.
