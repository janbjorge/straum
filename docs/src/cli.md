# Command line

`straum` is one binary with eight commands. The examples run against
the [quick start](quick-start.md)'s trips.

| Command | Does |
| --- | --- |
| [`run`](#straum-run) | Loads the spec's tables, follows their changes and answers asks over HTTP. |
| [`plan`](#straum-plan) | Prints the setup SQL for a spec, the tables of specs, or scripts that copy them. |
| [`status`](#straum-status) | Prints what a running Straum has done. |
| [`views`](#straum-views) | Prints the views a running Straum holds, and how much of each. |
| [`ask`](#straum-ask) | Asks a running Straum and prints the answer. |
| [`shell`](#straum-shell) | A prompt at a running Straum: its views, their rows, asks. |
| [`version`](#straum-version) | Prints Straum's version. |
| [`help`](#straum-help) | Prints the commands, or one command's flags. |

## Flags

`straum <command> --help` prints a command's flags with their defaults.
`-h` works too, anywhere before a `--`; for `ask`, before the query.

A flag takes its value as the next word or after `=`: `--url
http://straum:8080` or `--url=http://straum:8080`. A word that starts
with `-` is a flag, up to a `--`; after it, a spec or a query is taken
as written, as in `straum run -- -spec.sql`. Each flag goes on the
line at most once.

A flag that takes seconds, rows or bytes takes a whole number above 0.
A byte count may end in `Ki`, `Mi`, `Gi` or `Ti`, powers of 1024 as
Kubernetes writes them: `4Gi` is 4294967296.

## Reaching a running Straum

`status`, `views`, `ask` and `shell` talk to a running Straum. Both
flags below work on all four.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--url URL` | `STRAUM_URL`, else `http://127.0.0.1:8080` | Where Straum serves HTTP, as `http://HOST:PORT`. An empty `STRAUM_URL` counts as unset. |
| `--timeout SECONDS` | `10` | How long each request may take, from 1 to 86400 (a day). |

The URL is where Straum listens, reached directly: plain HTTP, no TLS,
and no proxy that rewrites the reply into chunks, which the client
refuses. When nothing answers there, the error says so and ends with
`try: --url URL, or set STRAUM_URL`.

Connecting, sending and the reply share the one `--timeout` wait. A
byte arriving inside the wait does not start it again. The shell reads
the views before each prompt, so each prompt may wait that long on a
Straum that has stopped answering.

## Exit status

| Code | Meaning |
| --- | --- |
| 0 | Done. |
| 1 | Failed. |
| 2 | The command line is wrong; `straum` alone prints the help and exits with 2 too. |
| 3 | Straum is not ready yet. |

3 comes from `status` while `/readyz` answers 503: Straum is loading,
or further behind PostgreSQL than `--max-lag`. It comes from `views`
and `ask` while Straum is loading; a Straum that lags still answers
asks. A script waits on 3 and asks again; the other failures need a
change first.

`straum shell` on a pipe goes on past a failed command, a not ready one
too, and exits with 1 if any failed. `straum run` exits with 1 on any
failure to load or follow, for whatever runs it to start it again
([States](deploy.md#states)).

## straum run

```text
straum run FILE [flags]
```

Loads every table the [spec](spec.md) in `FILE` names, follows their
changes and answers [asks](asks.md) over HTTP.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--listen ADDRESS` | `127.0.0.1:8080` | Where to serve HTTP. The default reaches this host only ([Who can ask](deploy.md#who-can-ask)). |
| `--wal logical\|physical` | `logical` | Logical decoding through the publication, or the [physical WAL](postgresql.md#physical-wal) that `wal_level = replica` writes. |
| `--publication NAME` | `straum` | The publication `straum plan` made. |
| `--dsn-file FILE` | `STRAUM_DSN`, else libpq's variables (`PGHOST` and the rest) | A file holding the [DSN](postgresql.md#2-connect). |
| `--max-lag SECONDS` | `30` | How far behind PostgreSQL before `/readyz` turns off. |
| `--max-retained-wal BYTES` | `4Gi` | WAL retained for the slot before Straum stops. The [watchdog](postgresql.md#what-straum-costs-the-server) checks it every second, during loads too. |
| `--max-transaction-rows ROWS` | `100000` | Rows one transaction may change in the spec's tables before Straum [stops](how-it-works.md#following) instead of applying it. |
| `--max-page-memory BYTES` | `1Gi` | Only with `--wal physical`, else a command line error: the rows Straum may hold from the WAL, pages and open transactions' changes, before it stops. It counts the rows' bytes, not the reader's buffer or the maps that track pages and transactions. |

It logs to stderr, each line starting `straum: `. A start logs the
address it listens on, the stream it opened, each table as it finishes
loading, with how long it took, and then that it streams. A failure
logs what went wrong, a line per problem, and exits with 1. A failed
connection names the host and port it tried, as does a server that
sends nothing for 10 s while the connection starts.

SIGTERM or SIGINT stops it. It stops accepting, finishes the asks in
flight for at most 10 seconds, and exits; PostgreSQL drops the slot as
the connection closes. A second signal exits at once.

## straum plan

```text
straum plan FILE [flags]
straum plan FILE... --copy DIR
straum plan FILE... --tables
```

Prints the [setup SQL](postgresql.md#1-run-the-setup-sql) for the spec
in `FILE`, or what is wrong with the spec and exits with 1. `--copy`
and `--tables` take several specs. A table two of them read comes once,
with the columns both read.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--wal logical\|physical` | `logical` | The setup for that stream: the physical one needs no publication or `REPLICA IDENTITY FULL`. |
| `--publication NAME` | `straum` | The publication to make; give `straum run` the same name. |
| `--role NAME` | The DSN's user, else `straum` | The role `straum run` connects as. |
| `--dsn-file FILE` | As for `straum run` | With a DSN the role gets its password, as a SCRAM-SHA-256 verifier. |
| `--copy DIR` | | Write `export.sql` and `load.sql` into `DIR` instead of the SQL. No DSN is read. |
| `--tables` | | Print the tables the specs name instead of the SQL: one per line, quoted, in the order the setup lists them, for `pg_dump -t`. No DSN is read. |
| `--out FILE` | stdout | Write the SQL, or the tables, to this file. |

`--copy` goes with neither `--tables` nor `--out`.

### Copying the tables

`--copy DIR` writes two psql scripts into `DIR`, made if missing, and
prints their paths and the psql commands that run them. Straum reads no
database; psql connects, with any sign-in libpq takes.

```sh
straum plan usage.sql --copy copy
psql "$SOURCE" -f copy/export.sql
psql "$DESTINATION" -f copy/load.sql
```

`export.sql` runs in one read-only repeatable-read transaction, so
every table is from the same moment, and writes nothing on the source.
It needs `SELECT` on the columns the specs read, and nothing on their
sequences.

1. It checks every column and stops, naming each one it can't copy: a
   table or column the source lacks, one the role can't read, or a type
   that is neither built in nor an enum. A table a row security policy
   would filter stops it too, rather than lose rows.
2. It writes `schema.sql`: the schemas, the enums and the tables, with
   only those columns. A domain becomes its base type.
3. It writes one CSV per table, `schema.table.csv`, with a header. A
   table locked for more than 5 s stops it.

`load.sql` creates the tables and loads the CSVs in one transaction,
then runs `ANALYZE`. Indexes, keys and defaults are left behind.

Both scripts print each table as they reach it. The first error stops
either script, and psql exits with 3. `-v dir=PATH` points them at
another directory.

## straum status

```text
straum status [--url URL] [--timeout SECONDS] [--json]
```

Prints what the Straum at `--url` has done, from its `GET /status`.

```text
$ straum status
ready:        yes
applied:      0/17B2958, committed 2026-11-09T17:12:57.346371Z
as of:        2026-11-09T17:14:02.118250Z, every commit before it applied
lag:          0.0 s, at most 30 s
WAL retained: 0.0 MiB, at most 4096.0 MiB; PostgreSQL is at 0/17B2958
```

- `ready`: `yes`, or `no` and why, as `/readyz` says.
- `applied`: the last commit applied and when it committed.
- `as of`: the same `as_of` answers carry. It keeps moving while the
  database is quiet.
- `lag` and `WAL retained`: against `--max-lag` and
  `--max-retained-wal`.

A failure ends the process (ADR-0009), so the status keeps no count of
starts or failures: whatever runs Straum counts its restarts, and the
log of the run that stopped says why.

It exits with 3 while `ready` is no, as `/readyz` answers 503, so a
script can wait for Straum:

```sh
until straum status > /dev/null; do sleep 1; done
```

`--json` prints the same facts as one JSON object, null where unknown:
the body `GET /status` sends with `Accept: application/json`
([endpoints](asks.md#endpoints)). A script reads it rather than the
lines, whose layout may change:

```sh
straum status --json | jq -e .ready
```

## straum views

```text
straum views [--url URL] [--timeout SECONDS] [--json]
```

Prints every view of the Straum at `--url` and how much of it Straum
holds, from its `GET /views`.

```text
$ straum views
view trips
  buckets:  1 hour, 1 day, 7 days
  holds:    173660 cells, 2003-01-01T03:00:00Z to 2024-02-01T06:00:00Z
  keys:     bucket timestamptz, payment_type integer, pu_location_id integer, borough text
  measures: trips, fare, avg_fare

dim public.taxi_zones
  holds:    265 rows
  asked as: trips.borough
```

- `buckets`: the bucket widths the view keeps.
- `holds`: the [cells](how-it-works.md#cells) at the narrowest width,
  and the bound cells of a view that keeps them. Then the range of its
  buckets, which an ask without bounds reads. A few trips in the month
  carry a wrong year, hence the range from 2003.
- `keys`: every name an ask may group or filter by, each with the type
  a parameter for it must have.
- `measures`: the names an ask may select after the keys.
- `asked as`: the keys of the views that read a dim's columns. An ask
  names a view, never a dim, so it reads the dim through these.

Until every table is loaded it prints `not ready: ` and why, and exits
with 3.

`--json` prints each view's keys with their types, the name of its
time, its measures and the range of buckets it holds: the body `GET
/views` sends with `Accept: application/json`. It leaves out the cells
and the dims.

## straum ask

```text
straum ask [--url URL] [--timeout SECONDS] [--json] QUERY [PARAMETER...]
```

Sends `QUERY` to the Straum at `--url` and prints the answer as a
table. The words after the query are the values of `$1`, `$2` and on.
Flags go before the query; every word after it is a parameter, `-h`
and negative numbers included. A query that starts with `-` follows a
`--`.

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
values print the same, text without its quotes, a null as `null`. In
text, a backslash and each control character print as JSON writes them,
`\\`, `\n` or `\u001b`, so data can't break a line of the table or
reach the terminal as an escape sequence. The text `null` prints as
`\null`, so it differs from a null.

`--json` prints that JSON answer itself, for a script. Errors go to
stderr either way: a line naming the URL and the reply's status, then
the reply as Straum sent it, `{"errors": [...]}` with `--json`.
stdout stays empty.

A command line holds only text, so a parameter's type is read from it:

| Written | Sent as |
| --- | --- |
| `7`, `-7` | An integer. |
| `20.5`, `-0.05` | A number, exactly as written: digits on both sides of the point. |
| `true`, `false` | A boolean. |
| `'7'`, `'20.5'`, `'true'` | The string inside the single quotes. |
| Anything else | A string, such as `Queens`, `2024-01-17T05:00:00Z` or `.5`. |

The shell takes single quotes for itself, so the string 7 is written
`"'7'"`. An ask Straum can't answer prints the reply's first line and
each error, and exits with 1:

```text
$ straum ask 'SELECT trips FROM trips WHERE payment_type = $1
    AND bucket >= $2 AND bucket < $3' card 0 3600
straum: http://127.0.0.1:8080 answered "HTTP/1.1 400 Bad Request"
straum: payment_type is an integer, so it can't be the text "card"
```

## straum shell

```text
straum shell [--url URL] [--timeout SECONDS]
```

A prompt at the Straum at `--url`. It takes these commands:

| Command | Prints |
| --- | --- |
| `\d` | Every view, as `straum views` does. |
| `\d VIEW` | The view's `CREATE VIEW` statement as the spec wrote it, with the comments right above it. |
| `\rows VIEW [START [COUNT]]` | The rows PostgreSQL would give for the view: every measure by the bucket and every key, over the buckets held. `COUNT` rows, 1000 at most and by default, from row `START`, 0 by default. |
| `QUERY; PARAMETER...` | The answer to an ask, as `straum ask` does: the query, a semicolon, then the values of `$1`, `$2` and on. A value with spaces goes in single quotes: `'Staten Island'`. |
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
Time: 1.728 ms (in Straum: parse 0.370, answer 0.208, text 0.133, total 0.763)
```

### Entering a command

A command that starts with `\` is one line and runs at Enter. An ask
may take several lines, so a query pasted as it was written works. It
ends at the line that holds its semicolon, so its parameters go on that
line. Without a semicolon, Enter starts another line of the ask, and a
blank line sends it with no parameters. The semicolon is the shell's:
it never reaches Straum, which refuses one at the end of an ask.

```text
straum> SELECT payment_type, trips
FROM trips
WHERE bucket >= $1 AND bucket < $2
GROUP BY payment_type; 2003-01-01T03:00:00Z 2024-02-01T06:00:00Z
```

Ctrl-C at the prompt drops the lines typed so far. While a command
runs, Ctrl-C ends the shell; the commands entered are still kept.

### Paging with \rows

`\rows` pages through a view as a Python slice does: rows count from 0,
and a negative `START` counts back from the end. `\rows trips 1000 50`
prints rows 1000 to 1049, `\rows trips -20` the last 20. A `START` past
either end is held at it. The last line says how many rows the view
has and, past the first, from which row they print.

`\rows` reads every cell of the view, whichever rows it prints, so on a
large view it takes as long as an ask over all its buckets.

### Timing

At a terminal an ask and `\rows` end in a line that says how long they
took, in milliseconds. `Time` is the whole request as the shell
measured it, from connecting to the last byte of the reply. In the
parentheses is the reply's [`Server-Timing`](asks.md#the-answer)
header, the time in Straum: reading the ask, answering it, writing the
table, and `total` for all of it. `\rows` sends no ask to read, so its
`parse` is 0. What `Time` holds beyond `total` is the network and the
shell. A command that fails is not timed.

### Completion and history

At a terminal, Tab completes by where the cursor is:

- after `SELECT`: keys and measures;
- after `FROM`, `\d` or `\rows`: views;
- after `WHERE` or `AND`: a whole condition with the next free
  parameter;
- after `HAVING`: a selected measure, then a comparison;
- past the semicolon: the start or end of the buckets held.

Tab and the dimmed hint read every line of the ask up to the cursor,
so `FROM` on one line still offers views at the start of the next.
Completion offers what is likely next; Straum still checks the ask. A
completed word ends in a space, so the next is typed straight on.
Where the candidates differ, the first Tab lists them.

The commands entered are kept, each as it is entered, in
`$XDG_STATE_HOME/straum/history` when that variable holds an absolute
path, else in `~/.local/state/straum/history`. Without a home
directory, as in the image, they are not kept. The last command entered
that starts as the one being typed shows dimmed past the cursor, and
the right arrow takes it; with none, a dimmed hint says what the next
parameter is. Ctrl-R searches the commands entered. `NO_COLOR` turns
off the dimming.

### On a pipe

With its input or its output anywhere but a terminal, the shell prints
no prompt, completes nothing and times nothing, so a script can drive
it and reads the same output each run:

```sh
printf '%s\n' '\d' '\rows trips' | straum shell
```

An ask takes lines there as at a terminal, and the end of input ends
one too. So a file of asks written over several lines, a blank line or
a semicolon after each, reads as it would typed.

Answers go to stdout. A command that fails says why on stderr and the
next is read, as in psql. If any failed, the shell exits with 1 after
the last, saying how many commands failed.

## straum version

```text
straum version
```

Prints `straum` and its version. `--version` and `-V` do the same.

## straum help

```text
straum help [COMMAND]
```

Prints every command, a line each, with the exit statuses. With a
command, prints that command's flags, as `straum COMMAND --help` does.
`--help` and `-h` alone do the same as `help`.
