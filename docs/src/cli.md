# Command line

`straum` is one binary with eight commands: `run`, `plan`, `status`,
`views`, `ask`, `shell`, `version` and `help`. `straum <command> --help`
prints a command's flags with their defaults. The examples run against
the [quick start](quick-start.md)'s trips.

## straum run

```text
straum run FILE [flags]
```

Loads every table the [spec](spec.md) in `FILE` names, follows their
changes and answers [asks](asks.md) over HTTP.

| Flag | Meaning |
| --- | --- |
| `--listen ADDRESS` | Where to serve HTTP. |
| `--wal logical\|physical` | Logical decoding through the publication, the default, or the [physical WAL](postgresql.md#without-logical-decoding) `wal_level = replica` writes. |
| `--publication NAME` | The publication `straum plan` made. |
| `--dsn-file FILE` | A file holding the [DSN](postgresql.md#3-connect); without it, the `STRAUM_DSN` environment variable. |
| `--max-lag SECONDS` | How far behind PostgreSQL before `/readyz` turns off. |
| `--max-retained-wal BYTES` | WAL retained for the slot before Straum starts over. Takes `Ki`, `Mi`, `Gi` or `Ti`. |
| `--max-transaction-rows ROWS` | Rows one transaction may change in the spec's tables before Straum [starts over](how-it-works.md#following) instead of applying it. |
| `--max-page-memory BYTES` | With `--wal physical`: the rows Straum may hold from the WAL, pages and open transactions' changes, before it starts over. It counts the rows' bytes, not the reader's buffer or the maps that track pages and transactions. Takes `Ki`, `Mi`, `Gi` or `Ti`. |

It logs to stderr, one line per event. SIGTERM or SIGINT stops it: it
stops accepting, finishes the asks in flight and exits, and PostgreSQL
drops the slot as the connection closes.

## straum plan

```text
straum plan FILE [flags]
```

Prints the [setup SQL](postgresql.md#2-run-the-setup-sql) for the spec
in `FILE`, or what is wrong with the spec and exits with 1.

| Flag | Meaning |
| --- | --- |
| `--wal logical\|physical` | The setup for that stream: the physical one needs no publication or `REPLICA IDENTITY FULL`. |
| `--publication NAME` | The publication to make; give `straum run` the same name. |
| `--role NAME` | The role `straum run` connects as; without it, the DSN's user, else `straum`. |
| `--dsn-file FILE` | As for `straum run`. With a DSN the role gets its password, as a SCRAM-SHA-256 verifier. |

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
```

- `ready`: `yes`, or `no` and why, as `/readyz` says.
- `applied`: the last commit applied and when it committed.
- `as of`: the same `as_of` answers carry. It keeps moving while the
  database is quiet.
- `lag` and `WAL retained`: against `--max-lag` and
  `--max-retained-wal`.
- `starts` and `failures`: loads that finished, and failures, since the
  process started.

## straum views

Prints every view of the Straum at `--url` and how much of it Straum
holds, from its `GET /views`.

```text
$ straum views
view trips
  buckets:  3600s, 86400s, 604800s
  holds:    173660 cells, 2003-01-01T03:00:00Z to 2024-02-01T06:00:00Z
  keys:     bucket timestamptz, payment_type integer, pu_location_id integer, borough text
  measures: trips, fare, avg_fare

dim public.taxi_zones
  holds:    265 rows
```

- `holds`: the [cells](how-it-works.md#cells) at the narrowest width,
  and the bound cells of a view that keeps them. Then the range of its
  buckets, which are the bounds an ask takes to read everything. A few
  trips in the month carry a wrong year, hence the range from 2003.
- `keys`: every name an ask may group or filter by, each with the type
  a parameter for it must have.
- `measures`: the names an ask may select after the keys.

Until every table is loaded it prints `not ready: ` and why, and exits
with 1.

## straum ask

Sends a query to the Straum at `--url` and prints the answer as a
table. The words after the query are the values of `$1`, `$2` and on:

```text
$ straum ask 'SELECT borough, trips, fare, avg_fare
    FROM trips WHERE bucket >= $1 AND bucket < $2
    GROUP BY borough' 2024-01-17T05:00:00Z 2024-01-18T05:00:00Z
borough       | trips | fare       | avg_fare
--------------+-------+------------+----------
Bronx         |   220 |    7196.07 | 32.709409
Brooklyn      |   924 |   27585.30 | 29.854221
EWR           |    10 |    1001.50 |    100.15
Manhattan     | 99225 | 1502063.48 | 15.137954
N/A           |    68 |    6436.59 | 94.655735
Queens        |  9388 |  454949.34 | 48.460731
Staten Island |     3 |     107.40 |      35.8
Unknown       |   527 |    9738.86 |  18.47981
8 rows, as of 2026-11-13T07:52:57.7760555Z
```

Quote the query with single quotes, so `$1` reaches Straum as written.
The table holds what the [JSON answer](asks.md#the-answer) holds:
values print the same, text without its quotes, a null as `null`.

A command line holds only text, so a parameter's type is read from it:

| Written | Sent as |
| --- | --- |
| `7`, `-7` | An integer. |
| `20.5`, `-0.05` | A number, exactly as written. |
| `true`, `false` | A boolean. |
| `'7'`, `'20.5'`, `'true'` | The string inside the single quotes. |
| Anything else | A string, such as `Queens` or `2024-01-17T05:00:00Z`. |

The shell takes single quotes for itself, so the string 7 is written
`"'7'"`. An ask Straum can't answer prints the reply's first line and
each error, and exits with 1:

```text
$ straum ask 'SELECT trips FROM trips WHERE payment_type = $1
    AND bucket >= $2 AND bucket < $3' card 0 3600
straum: http://127.0.0.1:8080 answered "HTTP/1.1 400 Bad Request"
payment_type is an integer, so it can't be the text "card"
```

## straum shell

A prompt at the Straum at `--url`. Each line is a command:

| Line | Prints |
| --- | --- |
| `\d` | Every view, as `straum views` does. |
| `\rows VIEW` | The rows PostgreSQL would give for the view: every measure by the bucket and every key, over the buckets held. The first 1000. |
| `QUERY; PARAMETER...` | The answer to an ask, as `straum ask` does: the query, a semicolon, then the values of `$1`, `$2` and on. |
| `\?` | These commands. |
| `\q` | Nothing; it leaves, as the end of input does. |

```text
$ straum shell
Straum at http://127.0.0.1:8080. Tab completes; \? lists the commands, \q leaves.
straum> SELECT payment_type, trips FROM trips WHERE bucket >= $1 AND bucket < $2 GROUP BY payment_type; 2003-01-01T03:00:00Z 2024-02-01T06:00:00Z
payment_type | trips
-------------+--------
           0 |  140162
           1 | 2319047
           2 |  439191
           3 |   19597
           4 |   46628
5 rows, as of 2026-11-13T07:59:07.988040041Z
```

`\rows` reads every cell of the view, so on a large view it takes as
long as an ask over all its buckets.

At a terminal, Tab completes by where the cursor is: keys and measures
after `SELECT`, views after `FROM`, a whole condition with the next
free parameter after `WHERE` or `AND`, a selected measure and then a
comparison after `HAVING`, and past the semicolon the start or end of
the buckets held. It offers what is likely next; Straum still checks
the ask.

The lines entered are kept in `$XDG_STATE_HOME/straum/history`,
`~/.local/state/straum/history` unless that variable is set; without a
home directory, as in the image, they are not kept. `NO_COLOR` turns
off the dimmed hint of the next parameter.

With its input or its output anywhere but a terminal, the shell prints
no prompt and completes nothing, so a script can drive it:

```sh
printf '%s\n' '\d' '\rows trips' | straum shell
```

Answers go to stdout. A line that fails says why on stderr and the
next line is read, as in psql; if any failed the shell exits with 1
after the last.
