# Running Straum

`straum` is one static binary:

```text
straum run --spec FILE [--listen ADDRESS] [--publication NAME] [--dsn-file FILE]
           [--max-lag SECONDS] [--max-retained-wal BYTES]
straum plan --spec FILE [--publication NAME] [--role NAME]
straum status [--url URL]
straum help
```

`straum plan` prints the [setup SQL](postgresql.md#2-run-the-setup-sql).

## straum run

Loads every table the spec names, follows their changes and answers asks
over HTTP.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--spec FILE` | required | The [spec](spec.md). |
| `--listen ADDRESS` | `0.0.0.0:8080` | Where to serve HTTP. |
| `--publication NAME` | `straum` | The publication `straum plan` made. |
| `--dsn-file FILE` | `STRAUM_DSN` | A file holding the [DSN](postgresql.md#3-connect); without it, the `STRAUM_DSN` environment variable. |
| `--max-lag SECONDS` | `30` | How far behind PostgreSQL before `/readyz` turns off. |
| `--max-retained-wal BYTES` | `4Gi` | WAL held for the slot before Straum starts over. Takes `Ki`, `Mi`, `Gi` or `Ti`. |

It logs to stderr, one line per event:

```text
straum: listening on 0.0.0.0:8080
straum: loaded every table; streaming from 0/17B2788
```

SIGTERM or SIGINT stops it: it stops accepting, finishes the asks in
flight and exits, and PostgreSQL drops the slot as the connection closes.

| Endpoint | Answers |
| --- | --- |
| `POST /ask` | An [ask](asks.md). |
| `GET /readyz` | 200 `ready`, or 503 `not ready: ` and why. |
| `GET /livez` | 200 while the process serves. Lag never fails it. |
| `GET /status` | What `straum status` prints, in plain text. |

## States

![Straum moves from Loading to Ready once every table is loaded. Ready turns Lagging past --max-lag and back once it catches up. Any failure leads to Waiting, then Loading from a new slot. SIGTERM leads to Stopping.](diagrams/lifecycle.svg)

| State | `/readyz` | Asks |
| --- | --- | --- |
| Loading | 503 `not ready: loading`, or the failure that led here | 503 |
| Ready | 200 `ready` | Answered |
| Lagging | 503 `not ready: 45 s behind PostgreSQL` | Answered, each with its `as_of` |
| Waiting | 503 `not ready: ` and the failure | 503 |

Straum reads the server's WAL position every second. Lag is how long ago
Straum last caught up with a position the server had reached, so it can
overstate how far behind Straum is but never understate it.

Any failure leads to Waiting: a lost connection, a change Straum can't
apply, a table altered under it, or too much WAL held for its slot.
Straum closes the connection, so PostgreSQL drops the slot, and waits 1
second, twice as long after each failure in a row, up to 60 seconds. Then
it loads every table again from a new slot. A load that finishes resets
the wait.

```text
straum: PostgreSQL holds 5120.0 MiB of WAL for straum's slot, past the limit of 4096.0 MiB
starting over in 1 s
```

## straum status

Prints what a running Straum has done, from `GET /status`. `--url`
defaults to `http://127.0.0.1:8080`.

```text
$ straum status
ready:        yes
applied:      0/17B2958, committed 2026-11-09T17:12:57.346371Z
as of:        2026-11-09T17:14:02.118250Z, every commit before it applied
lag:          0.0 s, at most 30 s
WAL held:     0.0 MiB, at most 4096.0 MiB; the server is at 0/17B2958
starts:       1
failures:     0
last failure: none
```

- `ready`: `yes`, or `no` and why, as `/readyz` says.
- `applied`: the last commit applied and when it committed.
- `as of`: every commit made before this time is applied, the same
  `as_of` answers carry. It keeps moving while the database is quiet.
- `lag`: against `--max-lag`.
- `WAL held`: what PostgreSQL keeps for the slot, against
  `--max-retained-wal`, and the server's current WAL position.
- `starts` and `failures`: loads that finished, and failures, since the
  process started.
- `last failure`: when, and the error.
