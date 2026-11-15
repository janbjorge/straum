# ADR-0052: The physical stream reads values as the load does

Status: Accepted, 2026-11-15

## Context

ADR-0049 has the physical stream hand the decoder pgoutput messages
with values as text, so one decoder reads both streams. That meant
printing each value as PostgreSQL's output functions do (`numeric_out`,
`timestamp_out`), only for the decoder to parse it straight back. The
text reader also takes no year before 1 or past 9999, which the binary
load reads.

## Decision

The physical stream reads a row's bytes straight into the values the
load reads from its binary `COPY`, sharing `value.rs` for numeric
digits and timestamps. The messages still go to the one decoder, as
ADR-0049 has it, with each value already typed; only "values go in as
text" no longer holds.

## Consequences

- No printers for numerics and timestamps, and none of their tests.
- A physical value equals the load's, including years the text reader
  refuses; the logical stream still errors on those.
- `pgoutput::Field` gains a typed variant only the physical stream
  makes.
