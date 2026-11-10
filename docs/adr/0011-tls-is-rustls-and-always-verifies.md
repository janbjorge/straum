# ADR-0011: TLS is rustls and always verifies

Status: Accepted (retroactive), 2026-11-10

## Context

Managed PostgreSQL requires TLS. OpenSSL and `ring` need a C compiler,
which the static musl image avoids. libpq's `sslmode` has modes that
silently fall back to plain TCP or skip verification.

## Decision

TLS is `rustls` 0.23 on `rustls-graviola`, pure Rust. `sslmode=disable`
is plain TCP. Every other connection checks the certificate chain and
the host name: `require`, `verify-ca`, `verify-full` and no `sslmode`
all mean that. `allow` and `prefer` are refused. Roots are Mozilla's
built in (`webpki-roots`) or the PEM file `sslrootcert` names, read at
each connect.

## Consequences

- graviola needs AES, AVX, AVX2, ADX, BMI1/2 and PCLMULQDQ on x86_64,
  or AES, NEON, PMULL and SHA2 on aarch64, and otherwise panics, which
  aborts Straum.
- A DSN that would be weaker than libpq's `verify-full` can't be
  written.
- The chart can't mount a CA bundle yet, so a server whose chain needs
  `sslrootcert` can't be reached through it.
