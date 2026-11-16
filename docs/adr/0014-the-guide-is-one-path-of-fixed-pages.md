# ADR-0014: The guide is one path of fixed pages

Status: Accepted 2026-11-16; revised 2026-11-16

## Context

The guide under `docs/src` was restructured about ten times in a week.
The page set hardly moved; what each page held did. How Straum works
became the design page and was split from it again, and "say each thing
once" passes ran twice while the same rule, flag or mechanism grew back
on two to four pages. No record said what a page is for or which facts
it owns, so every pass decided again.

Tier-1 guides (Stripe, ngrok, Materialize, the PostgreSQL manual) and
Diátaxis agree on three things: a page has one job, a fact has one
home that others link to, and internals sit apart after the reference.

## Decision

`docs/src/SUMMARY.md` is the reading order. The guide is these pages,
in this order, each with one job and the facts it owns. A fact is
written on its home page; any other page links to it.

The path, read in order, each page answering the question the one
before leaves:

1. `introduction.md`, why Straum: the problem, the `usage` view, one
   ask, what Straum is not. It routes and owns no fact.
2. `quick-start.md`, does it work: the one tutorial, on the taxi trips.
3. `how-it-works.md`, how it works: load, follow, cells, dims, the
   physical stream's mechanism, `as_of` and its consistency.
4. `aggregates.md`, what a measure costs: merging, deletes, what a
   cell keeps per measure.
5. `postgresql.md`, connecting a database: logical and physical setup,
   the start checks, what Straum costs the server.
6. `deploy.md`, running it for real: the binary, the image, the chart.
   Chart values live in `deploy/helm/straum/README.md`.
7. `performance.md`, what to expect: the benchmark and memory sizing.

Off the path, looked up rather than read:

- Reference: `spec.md` (every spec rule and `straum plan`'s errors),
  `asks.md` (the ask language, answers, errors, `Straum-LSN`, every
  endpoint), `cli.md` (every command and flag), `glossary.md` (a line
  per term and a link to its home).
- Contributing: `design.md` (crates, store, invariants, machines, which
  record shaped what) and `reading-the-code.md`. Design names the code;
  the mechanism it links to How Straum works.

Rules for every page:

- A path page opens with its question and ends with one link to the
  next page. It uses only terms defined on a page before it.
- A path page may link into the reference or contributing pages; none
  of them is needed to read the path.
- Outside the taxi pages (quick start, CLI, performance) every example
  is the `usage` view over `public.events AS e` and
  `public.accounts AS a`, never a new one.
- A how-to page gives steps and at most a sentence of why, linking the
  rest. A reference page is tables and short examples.

A new page, a moved fact or a changed order edits this record first.

## Consequences

- A restructure is a diff against this list, not a rewrite.
- A reader on the path never meets a term before its page.
- Some repetition remains on purpose: a link's sentence of context.
- Thirteen pages to keep true, one more than before: running splits
  into deploy and the CLI reference.
