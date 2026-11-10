# ADR-0003: Crates depend inward, traits only at ports

Status: Accepted (retroactive), 2026-11-10

## Context

A single crate would let the per-row path reach the socket and the JSON
reader in one step, and a trait-everywhere layering would let every
piece be swapped at the cost of dynamic dispatch on the hot path and
abstractions nothing uses twice.

## Decision

Straum is clean architecture as Cargo crates: `straum-domain` with no
dependencies and no I/O, `straum-app` with the use cases and the ports,
the adapters `straum-notation`, `straum-pg` and `straum-http`, and the
`straum` binary as composition root. Crates depend inward only, enforced
by `scripts/check-layers.py`. Traits exist only at the ports (`Source`);
the per-row path is concrete structs, enums and functions.

## Consequences

- The domain can be property-tested against a naive reference with no
  PostgreSQL, and the app against an in-memory fake source.
- A new adapter touches only its crate and the composition root.
- Cross-crate test helpers can't be shared without breaking the rule,
  so small helpers (`Rng`, `id()`) are copied per crate.
- `straum-http` must not know Straum SQL; the binary hands it a reader.
