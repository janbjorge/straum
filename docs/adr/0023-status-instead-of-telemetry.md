# ADR-0023: straum status instead of telemetry, for now

Status: Accepted (retroactive), 2026-11-10

## Context

The first binary served Prometheus text at `/metrics`, written by hand.
Lag and retained WAL need the server's WAL position, and OTLP needs a
collector; neither was in place, and a half metric surface would be
kept for compatibility.

## Decision

No metrics and no OTLP. `GET /status` is plain text: ready or why not,
lag, WAL held against the cap, LSN and `as_of`, starts, failures and
the last failure. `straum status` prints it, and `App` keeps the counts
for it.

## Consequences

- Nothing to scrape yet. Telemetry comes back as OTLP or metrics in M8,
  and the chart wires it then.
- `/readyz` and `/status` are the only operator surface.
- `/metrics` can return without breaking a client, since none used it.
