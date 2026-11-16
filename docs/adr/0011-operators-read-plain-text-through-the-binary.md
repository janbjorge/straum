# ADR-0011: Operators read plain text through the binary

Status: Accepted 2026-11-10; revised 2026-11-16

## Context

A DSN holds the password; on the command line it shows in `ps`, shell
history and pod specs. The first binary served hand-written Prometheus
text at `/metrics`, before lag had a server WAL position or OTLP a
collector, and a half metric surface would be kept for compatibility.
The image has no shell or curl (ADR-0012), so seeing what Straum holds
took curl, the spec and a hand-written JSON ask. The first usage text
was one block printed on every argument error.

## Decision

The DSN comes from `STRAUM_DSN` or `--dsn-file`, never an argument.
Arguments are read by hand. `straum plan` makes the DSN's user the
role, with its password as a SCRAM-SHA-256 verifier; without a DSN it
prints a setup for the default role `straum`.

Help follows clig.dev: `straum help` lists the commands a line each;
`straum <command> --help`, `-h` anywhere or `straum help <command>`
prints its usage, flags with defaults and a guide link; `straum ask
--help` adds the rules of an ask and examples a test runs against the
README's spec. An argument error prints the error and `try: straum
<command> --help`. A bare `straum` prints the list on stderr with exit
2. Prose lives in the guide.

No metrics and no OTLP. Straum writes what people read, as plain text:
`GET /status` (ready or why not, lag, WAL held against the cap, LSN,
`as_of`, starts and failures); `GET /views` (each view's widths,
cells, bucket range, typed keys and measures, each dim's rows);
`GET /views/NAME` (the first 1000 rows of `App::rows_ask`); and `POST
/ask` with `Accept: text/plain`. `/status` and `/views` with `Accept:
application/json` serve programs. The binary prints those bodies:
`straum status`, `straum views`, `straum ask QUERY [PARAMETER...]`.
They find Straum at `--url`, else `STRAUM_URL`, else
`http://127.0.0.1:8080`, as `STRAUM_DSN` stands in for `--dsn-file`.

`straum ask` reads a parameter's type from its text: an integer,
`true` or `false`, a number, else a string; single quotes make a
string. `straum shell` is a prompt: `\d` lists views, `\rows VIEW`
prints rows, any other line is `QUERY; PARAMETER...`. It completes
from `/views` by cursor position: views after `FROM`, names after
`SELECT`, a condition with the next `$n` after `WHERE`, keys after
`GROUP BY`, the bucket range held for a bound. The terminal is
`rustyline`, since raw mode and line editing take `unsafe` that `std`
lacks; without a terminal the shell reads lines from its input.

## Consequences

- The chart reads the DSN only from an existing Secret.
- No argument-parsing crate, so a new flag is written in the help and
  in `docs/`.
- Nothing to scrape. Telemetry may return as OTLP or `/metrics`
  without breaking a client.
- A pod is inspected with `kubectl exec`. The binary reads no spec,
  draws no table and parses no Straum SQL, so completion can offer
  what the walker then refuses.
- Text layout is for people and may change; JSON is the contract.
- `/views` is 503 until every table is loaded: only a load knows the
  types. `\rows` reads every cell of the view to print 1000.
- `straum ask` infers types; the HTTP ask infers nothing (ADR-0005).
- The terminal glue is untested; the completion under it is plain
  functions with tests.
