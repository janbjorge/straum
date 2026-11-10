# ADR-0026: The name is Straum everywhere

Status: Accepted (retroactive), 2026-11-10

## Context

A project with one name in the binary, another in the crates and a
third in PostgreSQL is harder to grep, document and rename.

## Decision

Straum, Nynorsk for a stream, is the name of the crates (`straum-*`),
the binary (`straum`), the DSN variable (`STRAUM_DSN`), the default
role and publication (`straum`), the slot prefix, the
`application_name` and the chart.

## Consequences

- One `rg -i straum` finds every place a rename touches.
- The crate names were free on crates.io on 2026-11-09; trademarks are
  unchecked, so nothing is published until they are.
