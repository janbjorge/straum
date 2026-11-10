# ADR-0021: The DSN never goes on the command line

Status: Accepted (retroactive), 2026-11-10

## Context

A DSN holds the password. On the command line it shows in `ps`, shell
history and pod specs.

## Decision

The DSN comes from `STRAUM_DSN` or `--dsn-file`, never an argument. The
binary reads its arguments by hand. `straum plan` reads the DSN when
there is one and makes the role the DSN's user, with its password as a
SCRAM-SHA-256 verifier, so the role, the password and the setup are
said once.

## Consequences

- The chart reads the DSN only from an existing Secret and never holds
  it.
- `straum plan` can run with no DSN and print a setup with the default
  role `straum`.
- No argument-parsing crate; help text is written by hand (ADR-0025).
