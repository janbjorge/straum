# ADR-0014: The guide is one path of fixed pages

Status: Accepted 2026-11-16; revised 2026-11-25

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
   ask, what you gain, what Straum is not. It routes, and owns one
   fact: what pg_ivm and TimescaleDB continuous aggregates do instead,
   as their documentation says.
2. `quick-start.md`, does it work: the one tutorial, on the taxi trips.
3. `your-data.md`, does it answer my queries: the spec's tables
   copied into Docker by `docs/compose/try.yaml`, or a copy from
   `straum plan --copy` loaded by `bench/try_your_data.py` for a large
   one or the physical stream;
   comparing an ask with SQL, and the steps on the database itself.
4. `how-it-works.md`, how it works: load, follow, cells, dims, the
   physical stream's mechanism, `as_of` and its consistency.
5. `aggregates.md`, what a measure costs: merging, deletes, what a
   cell keeps per measure.
6. `postgresql.md`, connecting a database: logical and physical setup,
   the start checks, what Straum costs the server.
7. `deploy.md`, running it for real: the binary, the image, and what
   Straum requires on Kubernetes, Docker Compose and Amazon ECS. The
   commands that set each one up live in the file, not on the
   page. Chart values live in `deploy/helm/straum/README.md`; each
   other file under `deploy/` opens with a comment.
8. `performance.md`, what to expect: the benchmark and memory sizing.

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
- The rows of `usage` are the usage events that
  `bench/load_usage_events.py` makes from the Azure Functions trace of
  2019. `bench/usage.sql` holds the view. A new answer of `usage` on a
  page comes from these rows. The answers that pages showed before
  2026-11-25 were written by hand, and the `purchase` kind is not in
  these rows.
- A how-to page gives steps and at most a sentence of why, linking the
  rest. A reference page is tables and short examples.

`docs/src/llms.txt` lists these pages for an agent. The introduction
links it, so mdBook copies it onto the site. It is not a guide page.
mdBook has no setting that writes each page's markdown beside its
HTML. Its markdown renderer is a debug dump, and a second renderer
would put the HTML in `book/html/`, off the site root. A new page is
added to `llms.txt` in the same commit.

A new page, a moved fact or a changed order edits this record first.

## Consequences

- A restructure is a diff against this list, not a rewrite.
- A reader on the path never meets a term before its page.
- Some repetition remains on purpose: a link's sentence of context.
- Fourteen pages to keep true: running splits into deploy and the CLI
  reference, and trying Straum on your own tables has a page of its
  own, with its compose file and its script.
- The introduction's sentences on pg_ivm and TimescaleDB describe
  projects that release without us, so they go stale by themselves;
  `research/ALTERNATIVES.md` holds the quotes to read again.
- `llms.txt` is the agent index. It points at the HTML pages.
