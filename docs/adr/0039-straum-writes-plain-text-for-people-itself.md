# ADR-0039: Straum writes plain text for people itself

Status: Accepted, 2026-11-12

## Context

To see what a running Straum holds, a person needed curl, the spec
file and an ask written as JSON by hand. The image holds no shell and
no curl, so in a pod `straum status` was all there was (ADR-0023). A
client that drew the tables itself would need a JSON reader in the
binary and the spec beside it.

## Decision

Straum writes what people read, and the binary prints it, as `straum
status` prints `/status`. `GET /views` is plain text: every view with
its bucket widths, the cells and the range of buckets it holds, its
keys with their types and its measures, then every dim with its rows.
`POST /ask` with `Accept: text/plain` answers with a table, and its
errors are a line each; any other `Accept` gets JSON as before.
`straum views` and `straum ask QUERY [PARAMETER...]` print those
bodies.

A command line holds only text, so `straum ask` reads a parameter's
type from it: an integer, `true` or `false`, else a string. Single
quotes around it make a string of what they hold.

## Consequences

- A pod is inspected with `kubectl exec`, and the binary prints what
  it is sent: it reads no spec and draws no table.
- The text is for people and may change its layout. Programs read
  JSON, which is unchanged.
- `/views` is 503 until every table is loaded: only a load knows the
  types.
- `straum ask` infers a parameter's type; the ask over HTTP infers
  nothing (ADR-0006, ADR-0029). A string that reads as an integer
  needs quotes.
- Amends ADR-0023: `/views` joins `/readyz` and `/status` as what an
  operator reads. Still no telemetry.
