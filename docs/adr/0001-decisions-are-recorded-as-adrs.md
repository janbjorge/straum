# ADR-0001: Decisions are recorded as ADRs

Status: Accepted, 2026-11-10

## Context

PLAN.md holds every settled decision as a dated bullet, 400 lines of
them, many amended in place by later ones. A reader can't tell what
holds today without reading all of it, and every change grows one file.

## Decision

Each decision gets one file in `docs/adr/`, named `NNNN-<slug>.md`,
numbered in order and never reused. A record has five parts:

- Title: the decision as one sentence, such as "Only straum-http runs
  tokio".
- Status: `Accepted, <date>` or `Superseded by ADR-NNNN`. Behaviour
  recorded after the fact says `Accepted (retroactive), <date>`.
- Context: the forces, two or three sentences.
- Decision: what we do, in full sentences.
- Consequences: what follows, good and bad, as bullets.

One decision per record. A record is for a fork with a real
alternative, where reversing would cost something. A tool pick goes
beside the dependency in `Cargo.toml`; a tunable goes in the guide.

A record fits one screen: about 40 lines at 78 columns. Once accepted,
only its Status line changes. To change a decision, write a new record
that supersedes the old one.

## Consequences

- The Decisions list in PLAN.md moves here, one record per decision
  that still holds; then that section goes. Decisions the list already
  replaced get no record.
- AGENTS.md and PLAN.md point here for settled decisions.
- Why something is the way it is takes reading one file, not a list.
- A record that outgrows a screen is two decisions, or a guide page.
