# Running Straum

`straum` is one static binary:

```text
straum run --spec FILE [--listen ADDRESS] [--publication NAME] [--dsn-file FILE]
           [--max-lag SECONDS] [--max-retained-wal BYTES]
           [--max-transaction-rows ROWS]
straum plan --spec FILE [--publication NAME] [--role NAME] [--dsn-file FILE]
straum status [--url URL]
straum version
straum help
```

`straum <command> --help` lists the command's flags and their defaults.
`straum plan` prints the [setup SQL](postgresql.md#2-run-the-setup-sql).

## Getting straum

Each [release](https://github.com/janbjorge/straum/releases) is an
image, `ghcr.io/janbjorge/straum:X.Y.Z`, and the same binary on its own
for Linux on x86-64 and arm64, with no libraries it needs. To install
the newest on x86-64:

```sh
base=https://github.com/janbjorge/straum/releases/latest/download
curl -fL -O "$base/straum-x86_64-unknown-linux-musl" -O "$base/SHA256SUMS"
sha256sum -c --ignore-missing SHA256SUMS
sudo install -m 755 straum-x86_64-unknown-linux-musl /usr/local/bin/straum
```

`gh attestation verify straum-x86_64-unknown-linux-musl --repo
janbjorge/straum` checks it was built by this repository's CI, and
`gh attestation verify oci://ghcr.io/janbjorge/straum:X.Y.Z --repo
janbjorge/straum` does the same for an image. The image tagged `latest`
is the newest stable release.

## straum run

Loads every table the spec names, follows their changes and answers asks
over HTTP.

| Flag | Meaning |
| --- | --- |
| `--spec FILE` | The [spec](spec.md). |
| `--listen ADDRESS` | Where to serve HTTP. |
| `--publication NAME` | The publication `straum plan` made. |
| `--dsn-file FILE` | A file holding the [DSN](postgresql.md#3-connect); without it, the `STRAUM_DSN` environment variable. |
| `--max-lag SECONDS` | How far behind PostgreSQL before `/readyz` turns off. |
| `--max-retained-wal BYTES` | WAL retained for the slot before Straum starts over. Takes `Ki`, `Mi`, `Gi` or `Ti`. |
| `--max-transaction-rows ROWS` | Rows one transaction may change in the spec's tables before Straum [starts over](how-it-works.md#applying-a-commit) instead of applying it. |

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

![The states of a running Straum](diagrams/lifecycle.svg)

*Loading turns Ready once every table is in. Ready turns Lagging past
`--max-lag` and back when it catches up. Any failure leads to Waiting,
then Loading from a new slot. SIGTERM leads to Stopping.*

| State | `/readyz` | Asks |
| --- | --- | --- |
| Loading | 503 `not ready: loading`, or the failure that led here | 503 |
| Ready | 200 `ready` | Answered |
| Lagging | 503 `not ready: 45 s behind PostgreSQL` | Answered, each with its `as_of` |
| Waiting | 503 `not ready: ` and the failure | 503 |

Straum reads the server's WAL position every second. Lag is the time
since Straum last caught up with a position the server had reached. It
can overstate the true delay by up to one second, never understate it.

Any failure leads to Waiting: a lost connection, a change Straum can't
apply, a table altered under it, or too much WAL retained for its slot.
Straum closes the connection, which drops the slot, and waits: 1 second
after the first failure, twice as long after each failure in a row, up
to 60 seconds. Then it loads every table again from a new slot. A load
that finishes resets the wait.

```text
straum: PostgreSQL holds 5120.0 MiB of WAL for straum's slot, past the limit of 4096.0 MiB
starting over in 1 s
```

## straum status

Prints what the Straum at `--url` has done, from its `GET /status`.

```text
$ straum status
ready:        yes
applied:      0/17B2958, committed 2026-11-09T17:12:57.346371Z
as of:        2026-11-09T17:14:02.118250Z, every commit before it applied
lag:          0.0 s, at most 30 s
WAL retained: 0.0 MiB, at most 4096.0 MiB; PostgreSQL is at 0/17B2958
starts:       1
failures:     0
last failure: none
sealed:       no spill
```

- `ready`: `yes`, or `no` and why, as `/readyz` says.
- `applied`: the last commit applied and when it committed.
- `as of`: every commit made before this time is applied, the same
  `as_of` answers carry. It keeps moving while the database is quiet.
- `lag`: against `--max-lag`.
- `WAL retained`: what PostgreSQL keeps for the slot, against
  `--max-retained-wal`, and how far PostgreSQL has written.
- `starts` and `failures`: loads that finished, and failures, since the
  process started.
- `last failure`: when, and the error.
- `sealed`: `no spill`. The shipped binary has no spill configuration.
