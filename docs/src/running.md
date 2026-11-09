# Running Straum

`straum` is one static binary with four commands:

```text
straum run --spec FILE [--listen ADDRESS] [--publication NAME] [--dsn-file FILE]
           [--max-lag SECONDS] [--max-retained-wal BYTES]
straum plan --spec FILE [--publication NAME] [--role NAME]
straum status [--url URL]
straum help
```

## straum run

Loads every table the spec names, follows PostgreSQL's changes to them,
and answers asks over HTTP.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--spec FILE` | required | The [spec](spec.md). |
| `--listen ADDRESS` | `0.0.0.0:8080` | Where to serve HTTP. |
| `--publication NAME` | `straum` | The publication to read, as `straum plan` made it. |
| `--dsn-file FILE` | `STRAUM_DSN` | A file holding the [DSN](postgresql.md#3-connect). Without it, Straum reads the `STRAUM_DSN` environment variable. |
| `--max-lag SECONDS` | `30` | How far behind PostgreSQL Straum may fall before `/readyz` turns off. |
| `--max-retained-wal BYTES` | `4Gi` | How much WAL PostgreSQL may hold for Straum's slot before Straum starts over. Takes a `Ki`, `Mi`, `Gi` or `Ti` suffix. |

It logs to stderr, one line per event:

```text
straum: listening on 0.0.0.0:8080
straum: loaded every table; streaming from 0/17B2788
```

SIGTERM or SIGINT stops it. It stops accepting, finishes the asks in
flight and exits, and PostgreSQL drops its slot as the connection closes.

## straum plan

Checks the spec and prints the setup SQL for it. `--role` and
`--publication` name the role and the publication it makes. See
[Setting up PostgreSQL](postgresql.md#2-run-the-setup-sql).

## straum status

Asks a running Straum what it has done and prints the answer:

```text
$ straum status
ready:        yes
applied:      0/17B2958, committed 2026-11-09T17:12:57.346371Z
lag:          0.0 s, at most 30 s
WAL held:     0.0 MiB, at most 4096.0 MiB; the server is at 0/17B2958
starts:       1
failures:     0
last failure: none
```

- `ready`: `yes`, or `no` and why.
- `applied`: the last commit applied and when it committed.
- `lag`: how far behind PostgreSQL Straum is, against `--max-lag`.
- `WAL held`: the WAL PostgreSQL keeps for Straum's slot, against
  `--max-retained-wal`, and the server's current WAL position.
- `starts` and `failures`: loads that finished, and failures since the
  process started.
- `last failure`: when, and the error.

`--url` points it at another Straum; it defaults to
`http://127.0.0.1:8080`. It reads the plain-text `GET /status`, so `curl`
works too.

## Endpoints

| Endpoint | Answers |
| --- | --- |
| `POST /ask` | An [ask](asks.md). |
| `GET /livez` | 200 while the process serves. Lag never fails it. |
| `GET /readyz` | 200 `ready`, or 503 `not ready: ` and why. |
| `GET /status` | What `straum status` prints. |

## Readiness and lag

`/readyz` turns on once every table is loaded and Straum is at most
`--max-lag` seconds behind PostgreSQL. It turns off again while Straum
lags, while it loads, and after a failure:

```text
not ready: 45 s behind PostgreSQL
not ready: loading
```

Asks keep answering while Straum lags, each with the `lsn` it reflects.
Only a load makes them wait, with 503.

Straum reads the server's WAL position every second. Lag is how long ago
Straum was last caught up with a position the server had reached, so it
can overstate how far behind Straum is, but never understate it.

## Failures and starting over

Straum starts over after any failure: a lost connection, a change it can't
apply, a table altered under it, or too much WAL held for its slot. It
turns not ready, closes the connection, so PostgreSQL drops the slot, and
waits: 1 second, then twice as long after each failure in a row, up to 60
seconds. Then it loads every table again from a new slot. A load that
finishes resets the wait.

```text
straum: PostgreSQL holds 5120.0 MiB of WAL for straum's slot, past the limit of 4096.0 MiB
starting over in 1 s
```

While it waits and loads, asks answer 503 and `straum status` shows the
last failure.

## Memory

Straum keeps every rollup in memory and nothing on disk. Every start loads
every table again, so give it the memory its rollups take, and a little
more for asks. [Writing a spec](spec.md#memory) says what that depends on.
