# Command line

`straum` is one binary with eight commands: `run`, `plan`, `status`,
`views`, `ask`, `shell`, `version` and `help`.
`straum <command> --help` prints a command's flags with their defaults.
A flag takes its value as the next word or after `=`:
`--url http://straum:8080` or `--url=http://straum:8080`. The examples
run against the [quick start](quick-start.md)'s trips.

A word that starts with `-` is a flag, up to a `--`: after it, a spec
or a query is taken as written, as in `straum run -- -spec.sql`.

`status`, `views`, `ask` and `shell` talk to a running Straum. They find
it at `--url`, else at the URL in `STRAUM_URL`, else at
`http://127.0.0.1:8080`. When nothing answers there, the error says so
and ends with `try: --url URL, or set STRAUM_URL`. The URL is where
Straum listens, reached directly: plain HTTP, no TLS, and no proxy
that rewrites the reply into chunks, which the client refuses. Each
request waits at most `--timeout` seconds for the whole exchange.
Connecting, sending and the reply share that one wait: 10 unless told.
A byte arriving inside the wait does not start it again. The shell
reads the views before each prompt, so each prompt may wait that long
on a Straum that has stopped answering.

Every command exits with 0 when done, 1 when it failed, 2 when its
command line is wrong, and 3 when Straum is not ready yet: loading, or
further behind PostgreSQL than `--max-lag`. A script waits on 3 and
asks again; the other failures need a change first. `straum shell` on
a pipe is the one exception: it goes on past a failed command, a not
ready one too, and exits with 1 if any failed. `straum run` exits with
1 on any failure to load or follow, for whatever runs it to start it
again ([States](deploy.md#states)).

## straum run

```text
straum run FILE [flags]
```

Loads every table the [spec](spec.md) in `FILE` names, follows their
changes and answers [asks](asks.md) over HTTP.

| Flag | Meaning |
| --- | --- |
| `--listen ADDRESS` | Where to serve HTTP: `127.0.0.1:8080`, this host only, unless told ([Who can ask](deploy.md#who-can-ask)). |
| `--wal logical\|physical` | Logical decoding through the publication, the default, or the [physical WAL](postgresql.md#physical-wal) `wal_level = replica` writes. |
| `--publication NAME` | The publication `straum plan` made. |
| `--dsn-file FILE` | [DSN](postgresql.md#2-connect) file; else `STRAUM_DSN`, else `PGHOST`. |
| `--max-lag SECONDS` | How far behind PostgreSQL before `/readyz` turns off. |
| `--max-retained-wal BYTES` | WAL retained for the slot before Straum stops. A watchdog checks it every second, during loads too, and ends the slot's session past it. Takes `Ki`, `Mi`, `Gi` or `Ti`. |
| `--max-transaction-rows ROWS` | Rows one transaction may change in the spec's tables before Straum [stops](how-it-works.md#following) instead of applying it. |
| `--max-page-memory BYTES` | With `--wal physical`: the rows Straum may hold from the WAL, pages and open transactions' changes, before it stops. It counts the rows' bytes, not the reader's buffer or the maps that track pages and transactions. Takes `Ki`, `Mi`, `Gi` or `Ti`. |

It logs to stderr, each line starting `straum: `. A start logs the
stream it opened, each table as it finishes loading, with how long it
took, and then that it streams. A failure logs what went wrong, a line
per problem, and exits with 1. A failed connection names the host and
port it tried, as does a server that sends nothing for 10 s while the
connection starts. SIGTERM or
SIGINT stops it: it
stops accepting, finishes the asks in flight, for at most 10 seconds,
and exits, and PostgreSQL drops the slot as the connection closes. A
second signal exits at once.

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

`--copy DIR` writes two psql scripts into `DIR`, made if missing.
Straum reads no database; psql connects, with any sign-in libpq takes.

```sh
straum plan usage.sql --copy copy
psql "$SOURCE" -f copy/export.sql
psql "$DESTINATION" -f copy/load.sql
```

`export.sql` runs in one read-only repeatable-read transaction, so
every table is from the same moment, and writes nothing on the source.
It needs `SELECT` on the columns the specs read, and nothing on their
sequences. First it checks every column and stops, naming each one it
can't copy: a table or column the source lacks, one the role can't
read, or a type that is neither built in nor an enum. A table a row
security policy would filter stops it too, rather than lose rows. Then
it writes
`schema.sql` (the schemas, the enums and the tables, with only those
columns; a domain becomes its base type) and one CSV per table,
`schema.table.csv`, with a header. A table locked for more than 5 s
stops it. `load.sql` creates the tables and loads the CSVs in one
transaction, then runs `ANALYZE`. The first error stops either script
and exits with 3. Indexes, keys and defaults are left behind. Both
print each table as they reach it, and `-v dir=PATH` points them at
another directory.

| Flag | Meaning |
| --- | --- |
| `--wal logical\|physical` | The setup for that stream: the physical one needs no publication or `REPLICA IDENTITY FULL`. |
| `--publication NAME` | The publication to make; give `straum run` the same name. |
| `--role NAME` | The role `straum run` connects as; without it, the DSN's user, else `straum`. |
| `--dsn-file FILE` | As for `straum run`. With a DSN the role gets its password, as a SCRAM-SHA-256 verifier. |
| `--copy DIR` | Write `export.sql` and `load.sql` into `DIR` instead of the SQL. No DSN is read. |
| `--tables` | Print the tables the specs name, one per line, quoted, in the order the setup lists them, instead of the SQL. For `pg_dump -t`. No DSN is read. |
| `--out FILE` | Write the SQL (or the tables) to this file instead of stdout. |

## straum status

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

`--json` prints the same facts as one JSON object, the body `GET
/status` sends with `Accept: application/json`
([endpoints](asks.md#endpoints)). A script reads it rather than the
lines, whose layout may change:

```sh
straum status --json | jq -e .ready
```

## straum views

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

- `holds`: the [cells](how-it-works.md#cells) at the narrowest width,
  and the bound cells of a view that keeps them. Then the range of its
  buckets, which an ask without bounds reads. A few
  trips in the month carry a wrong year, hence the range from 2003.
- `keys`: every name an ask may group or filter by, each with the type
  a parameter for it must have.
- `measures`: the names an ask may select after the keys.
- `asked as`: the keys of the views that read a dim's columns. An ask
  names a view, never a dim, so it reads the dim through these.

Until every table is loaded it prints `not ready: ` and why, and exits
with 3.

`--json` prints each view's keys with their types, the name of its
time, its measures and the range of buckets it holds, as JSON, the body `GET /views` sends with
`Accept: application/json`. It leaves out the cells and the dims.

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
values print the same, text without its quotes, a null as `null`. In
text, a backslash and each control character print as JSON writes them,
`\\`, `\n` or `\u001b`, so data can't break a line of the table or
reach the terminal as an escape sequence, and the text `null` prints as
`\null`, so it differs from a null.
`--json`, before the query, prints that JSON answer itself, for a
script. Errors go to stderr, as without it: a line naming the URL and
the reply's status, then the reply, `{"errors": [...]}`, as Straum sent
it; stdout stays empty.

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
straum: payment_type is an integer, so it can't be the text "card"
```

## straum shell

A prompt at the Straum at `--url`. It takes these commands:

| Command | Prints |
| --- | --- |
| `\d` | Every view, as `straum views` does. |
| `\d VIEW` | The view's `CREATE VIEW` statement as the spec wrote it, with the comments right above it. |
| `\rows VIEW [START [COUNT]]` | The rows PostgreSQL would give for the view: every measure by the bucket and every key, over the buckets held. COUNT of them, at most and else 1000, from row START, else 0. A negative START counts back from the end. |
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

A command that starts with `\` is one line and runs at Enter. An ask
may take several lines, so a query pasted as it was written works. It
ends at the line that holds its semicolon, so its parameters go on that
line. Without a semicolon, Enter starts another line of the ask, and a
blank line sends it with no parameters. The semicolon is the shell's:
it never reaches Straum, which refuses one at the end of an ask. Ctrl-C
drops the lines typed so far.

```text
straum> SELECT payment_type, trips
FROM trips
WHERE bucket >= $1 AND bucket < $2
GROUP BY payment_type; 2003-01-01T03:00:00Z 2024-02-01T06:00:00Z
```

Tab and the dimmed hint read every line of the ask up to the cursor,
so `FROM` on one line still offers views at the start of the next.

`\rows` pages through a view as a Python slice does: rows count from 0,
and a negative START counts back from the end. `\rows trips 1000 50`
prints rows 1000 to 1049, `\rows trips -20` the last 20. A START past
either end is held at it. The last line says how many rows the view
has and, past the first, from which row they print.

`\rows` reads every cell of the view, whichever rows it prints, so on a
large view it takes as long as an ask over all its buckets.

At a terminal an ask and `\rows` end in a line that says how long they
took, in milliseconds. `Time` is the whole request as the shell
measured it, from connecting to the last byte of the reply. In the
parentheses is the reply's [`Server-Timing`](asks.md#the-answer)
header, the time in Straum: reading the ask, answering it, writing the
table, and `total` for all of it. `\rows` sends no ask to read, so its
`parse` is 0. What `Time` holds beyond `total` is the network and the
shell. A command that fails is not timed.

At a terminal, Tab completes by where the cursor is: keys and measures
after `SELECT`, views after `FROM`, `\d` or `\rows`, a whole condition with the next
free parameter after `WHERE` or `AND`, a selected measure and then a
comparison after `HAVING`, and past the semicolon the start or end of
the buckets held. It offers what is likely next; Straum still checks
the ask. A completed word ends in a space, so the next is typed straight
on, and where the candidates differ the first Tab lists them.

The commands entered are kept in `$XDG_STATE_HOME/straum/history`,
`~/.local/state/straum/history` unless that variable is set; without a
home directory, as in the image, they are not kept. The last command
entered that starts as the one being typed shows dimmed past the
cursor, and the right arrow takes it; with none, a dimmed hint says
what the next parameter is. Ctrl-R searches the commands entered.
`NO_COLOR` turns off the dimming.

With its input or its output anywhere but a terminal, the shell prints
no prompt, completes nothing and times nothing, so a script can drive
it and reads the same output each run:

```sh
printf '%s\n' '\d' '\rows trips' | straum shell
```

An ask takes lines there as at a terminal, and the end of input ends
one too, so a file of asks written over several lines, a blank line or
a semicolon after each, reads as it would typed.

Answers go to stdout. A command that fails says why on stderr and the
next is read, as in psql; if any failed the shell exits with 1 after
the last, saying how many commands failed.
