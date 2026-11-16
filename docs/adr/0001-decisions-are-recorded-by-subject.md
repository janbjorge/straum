# ADR-0001: Decisions are recorded by subject

Status: Accepted 2026-11-10; revised 2026-11-16

## Context

PLAN.md held every settled decision as 400 lines of dated bullets. One
record per decision, never edited, then grew to 57 records in a week,
most of them amending or superseding another: the joins took five
records to read, the PostgreSQL floor three. A reader still had to walk
a chain to learn what holds.

## Decision

`docs/adr/` holds one record per subject, `NNNN-<slug>.md`, numbered in
order and never reused. A record says what holds today, in five parts:
a title stating the subject as one sentence; a status, `Accepted
<date>; revised <date>`; the context, the forces at play; the decision,
in full sentences; the consequences, good and bad, as bullets.

A decision belongs in a record when it is a fork with a real
alternative and reversing it costs something. A tool pick goes beside
its dependency in `Cargo.toml`; a tunable goes in the guide.

A changed decision edits its record in the same commit as the code and
moves the revised date. The rejected alternative stays as a line, with
the measurement that settled it; superseded text goes, and `git log`
keeps it. A new record is only for a subject no record covers. A
record stays dense, about 80 lines at 78 columns; past that, the
subject splits or the detail moves to a guide page.

## Consequences

- Why something is the way it is takes one file, not a chain.
- A decision's history is `git log -p` on its record.
- Records go stale like any document, so AGENTS.md's rule holds for
  them: fixed in the same commit as the code.
- `README.md` maps the 57 record numbers used before 2026-11-16 to the
  records that now hold them, for old commit messages.
