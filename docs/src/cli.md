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
as written, as in `straum run --wal logical -- -spec.sql`. Each flag
goes on the line at most once.

A flag that takes seconds, rows or bytes takes a whole number above 0.
A byte count may end in `Ki`, `Mi`, `Gi` or `Ti`, powers of 1024.

## Reaching a running Straum

`status`, `views`, `ask` and `shell` take these flags.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--url URL` | `STRAUM_URL`, else `http://127.0.0.1:8080` | Where Straum serves HTTP, as `http://HOST:PORT`. An empty `STRAUM_URL` counts as unset. |
| `--timeout SECONDS` | `10` | How long each request may take, from 1 to 86400. |

The URL reaches Straum directly: plain HTTP, no TLS, and no proxy that
rewrites the reply into chunks, which the client refuses. When nothing
answers, the error ends with `try: --url URL, or set STRAUM_URL`.

Connecting, sending and reading share one `--timeout` wait; a byte
that arrives does not restart it. The shell reads the views before
each prompt, so each prompt may wait that long on a Straum that has
stopped answering.

## Exit status

| Code | Meaning |
| --- | --- |
| 0 | Done. |
| 1 | Failed. |
| 2 | The command line is wrong; `straum` alone prints the help and exits with 2 too. |
| 3 | Straum is not ready yet, or busy. |

`status` exits with 3 while `/status` says not ready, as `/readyz`
does with a 503: Straum is loading, or further behind PostgreSQL than
`--max-lag`. `views` and `ask` exit
with 3 only while it loads, or while every core answers another ask
([Errors](asks.md#errors)); a Straum that lags still answers. A script
waits on 3 and tries again; the other failures need a change first.

On a pipe, `straum shell` goes on past a failed command, one that met
a Straum not ready included, and exits with 1 if any failed. `straum
run` exits with 1 on any failure to load or follow, for whatever runs
it to start it again ([States](deploy.md#states)).

## straum run

```text
straum run FILE --wal logical|physical [flags]
```

Loads every table the [spec](spec.md) in `FILE` names, follows their
changes and answers [asks](asks.md) over HTTP.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--listen ADDRESS` | `127.0.0.1:8080` | Where to serve HTTP. The default reaches this host only ([Who can ask](deploy.md#who-can-ask)). |
| `--health-listen ADDRESS` | Off | Where to serve `/livez` and `/readyz` alone, with connections of their own. `--listen` serves them too ([Connections](deploy.md#connections)). |
| `--allow-hosts NAME:PORT,...` | On loopback its names, else any | The only `Host` names a request may name, each with the port the client writes, `localhost:8080` say; a name without one means port 80. Any other gets 421 ([Who can ask](deploy.md#who-can-ask)). |
| `--wal logical\|physical` | Required | Logical decoding through the publication, or the [physical write-ahead log (WAL)](postgresql.md#physical-wal) that `wal_level = replica` writes. |
| `--publication NAME` | `straum` | The publication `straum plan` made. |
| `--dsn-file FILE` | `STRAUM_DSN`, else libpq's variables, when any of `PGHOST`, `PGUSER`, `PGPASSWORD` and the rest is set | A file holding the [data source name (DSN)](postgresql.md#2-connect). |
| `--max-lag SECONDS` | `30` | How far behind PostgreSQL before `/readyz` turns off. |
| `--max-retained-wal BYTES` | `4Gi` | WAL retained for the slot before Straum stops. The [watchdog](postgresql.md#what-straum-costs-the-server) checks it every second, during loads too. |
| `--max-transaction-rows ROWS` | `100000` | Rows one transaction may change in the spec's tables before Straum [stops](how-it-works.md#following) instead of applying it. A row counts once, however many views and dims read its table ([Large transactions](deploy.md#large-transactions)). |
| `--max-answer-rows ROWS` | `100000` | Rows an ask may get. An answer is written whole in memory, so an ask whose answer has more is [refused with 400](asks.md#errors). |
| `--max-page-memory BYTES` | `1Gi` | Only with `--wal physical`, else a command line error: the rows Straum may hold from the WAL, pages and open transactions' changes, before it stops. It counts the rows' bytes, not the reader's buffer or the maps that track pages and transactions. |
| `--max-stream-memory BYTES` | `1Gi` | Commits Straum may hold, read from the stream ahead of the writer. Past it while the tables load, Straum [stops](how-it-works.md#following). After the load, a full queue waits for the writer. A transaction whose rows hold more bytes than this stops Straum too, on both streams ([Large transactions](deploy.md#large-transactions)). The queue and the transaction the stream reads can each hold this much, so the memory ahead of the writer can be up to twice the limit. It counts the rows' bytes, text by its length, not `Vec` slack. |
| `--max-memory BYTES` | Off | Resident memory Straum may hold before it [stops](deploy.md#states). Straum reads resident memory about once a second from the start on, loads included, set or not, for [`straum status`](#straum-status): `VmRSS` in `/proc/self/status` on Linux, `ps -o rss=` where that file is missing, as on macOS. Where neither can be read, a set limit fails the start. Set it under the memory limit of the pod or host ([Kubernetes](deploy.md#kubernetes)). |

It logs to stderr, each line starting `straum: `: the address it
listens on, the stream it opened with each table to load, its
estimated rows and size and the columns read, then each dim and each
fact table's views as they finish loading with how long it took, the
rows read and the rows per second, then that it streams:

```text
straum: opened a stream at 0/B81BDF18; loading 2 tables, estimated 20331249 rows and 1698.1 MiB: public.taxi_zones (dim taxi_zones) 265 rows, 0.0 MiB, columns location_id, borough; public.yellow_trips (views trips, dropoffs) 20330984 rows, 1698.1 MiB, columns pickup_at, payment_type, pu_location_id, fare_amount, do_location_id, tip_amount, trip_distance
straum: loaded dim taxi_zones in 0.0 s: 265 rows, 0.0 MiB on disk, 287471 rows/s
straum: loaded views trips, dropoffs in 22.4 s: 20332093 rows, 1698.1 MiB on disk, 908779 rows/s
```

The estimates come from PostgreSQL's catalog: rows as the last
`ANALYZE` or `VACUUM` counted them, `unknown rows` for a table neither
has read, and the size of the table's main file. A failure logs a line per
problem and exits with 1. A failed connection names the host and port
it tried, as does a server that sends nothing for 10 s while the
connection starts.

On SIGTERM or SIGINT it stops accepting, finishes the asks in flight
for at most 10 seconds and exits; PostgreSQL drops the slot as the
connection closes. A second signal exits at once.

## straum plan

```text
straum plan FILE --wal logical|physical [flags]
straum plan FILE... --copy DIR
straum plan FILE... --tables
```

Prints the [setup SQL](postgresql.md#1-run-the-setup-sql) for the spec
in `FILE`, or what is wrong with it and exits with 1. `--copy` and
`--tables` take several specs; a table two of them read comes once,
with the columns both read.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--wal logical\|physical` | Required for the SQL | The setup for that stream: the physical one needs no publication or `REPLICA IDENTITY FULL`. |
| `--publication NAME` | `straum` | The publication to make; give `straum run` the same name. |
| `--role NAME` | The DSN's user, else `straum` | The role `straum run` connects as. |
| `--dsn-file FILE` | As for `straum run` | With a DSN the role gets its password, as a SCRAM-SHA-256 verifier. |
| `--copy DIR` | | Write `export.sql` and `load.sql` into `DIR` instead of the SQL. No DSN is read. |
| `--tables` | | Print the tables the specs name instead of the SQL: one per line, quoted, in the order the setup lists them, for `pg_dump -t`. No DSN is read. |
| `--out FILE` | stdout | Write the SQL, or the tables, to this file. |

`--copy` goes with neither `--tables` nor `--out`.

### Copying the tables

`--copy DIR` writes two psql scripts into `DIR`, made if missing, and
prints their paths and the commands that run them. Straum reads no
database; psql connects, with any sign-in libpq takes.

```sh
straum plan usage.sql --copy copy
psql "$SOURCE" -f copy/export.sql
psql "$DESTINATION" -f copy/load.sql
```

`export.sql` runs in one read-only repeatable-read transaction, writes
nothing on the source, and needs `SELECT` on the columns the specs
read, nothing on their sequences.

1. It checks every column and stops, naming each one it can't copy: a
   table or column the source lacks, one the role can't read, or a type
   that is neither built in nor an enum. A table a row security policy
   would filter stops it too, since its copy would miss rows.
2. It writes `schema.sql`: the schemas, the enums and the tables, with
   only those columns. A domain becomes its base type.
3. It writes one CSV per table, `schema.table.csv`, with a header. A
   table locked for more than 5 s stops it.

`load.sql` creates the tables and loads the CSVs in one transaction,
then runs `ANALYZE`. Indexes, keys and defaults are left behind.

Both print each table as they reach it. The first error stops either,
and psql exits with 3. `-v dir=PATH` points them at another directory.

## straum status

```text
straum status [--url URL] [--timeout SECONDS] [--json]
```

Prints what the Straum at `--url` has done, from its `GET /status`.

```text
$ straum status
ready:        yes
applied:      0/B81BDEB0, where the stream started
as of:        2026-11-24T19:04:42.083217042Z, every commit before it applied
lag:          0.8 s, at most 30 s; caught up to 0/B81BDF88
applying:     0 commits and 0 changes a second
WAL retained: 0.0 MiB, at most 4096.0 MiB; PostgreSQL is at 0/B81BDF88
memory:       145.0 MiB resident, no --max-memory set
version:      0.18.1-dirty
```

| Line | Shows |
| --- | --- |
| `ready` | `yes`, or `no` and why, as `/readyz` says. |
| `applied` | The last commit applied and when it committed. |
| `as of` | The `as_of` answers carry. It keeps moving while the database is quiet. |
| `lag`, `WAL retained` | Against `--max-lag` and `--max-retained-wal`. The lag counts to the position Straum has caught up to: every commit up to there is applied, WAL that holds nothing for Straum included. On a quiet server it runs ahead of `applied`, which only a commit to the spec's tables moves. |
| `applying` | Commits and changes applied each second between the last two readings of PostgreSQL's WAL, about a second apart. With `lag`, it tells a Straum catching up from one falling further behind. Unknown in the stream's first second. |
| `memory` | Resident memory at the last reading, about a second old, against `--max-memory` when it is set. Where neither `/proc/self/status` nor `ps` gives it, the line says it is unknown. |
| `version` | The version of the Straum serving, as [`straum version`](#straum-version) prints it; a build `just taxi` makes names its commit as `git describe` does, and any other build outside a release says `0.0.0`. |

A failure ends the process (architecture decision record ADR-0009), so
the status counts no starts or failures: whatever runs Straum counts
its restarts, and the log of the run that stopped says why.

It exits with 3 while `ready` is no, so a script can wait for Straum:

```sh
until straum status > /dev/null; do sleep 1; done
```

`--json` prints the same facts as one JSON object, null where unknown,
as `GET /status` sends with `Accept: application/json`
([endpoints](asks.md#endpoints)). The lines' layout may change, so a
script reads the JSON:

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
  buckets:  1 hour (1157581 cells), 1 day (129115 cells), 7 days (25803 cells)
  holds:    1157581 cells, 2002-12-31T21:00:00Z to 2026-06-27T04:00:00Z
  keys:     bucket timestamptz, payment_type integer, pu_location_id integer, borough text
  measures: trips, fare, avg_fare, fare_variance, fare_deviation, median_fare, p99_fare, common_fare, distinct_fares, distinct_fare_sum, distinct_fare_avg

view dropoffs
  buckets:  1 hour (713457 cells), 1 day (45626 cells), 7 days (7004 cells)
  holds:    713457 cells, 2002-12-31T21:00:00Z to 2026-06-27T04:00:00Z
  keys:     bucket timestamptz, do_location_id integer, borough text
  measures: trips, tips, avg_distance, fare

dim public.taxi_zones
  holds:    265 rows
  asked as: trips.borough, dropoffs.borough
```

| Line | Shows |
| --- | --- |
| `buckets` | The bucket widths the view keeps, each with the [cells](how-it-works.md#cells) it holds, so the counts show which width holds the most. A width [kept for a time](spec.md#keeping-a-width-for-a-time) says for how long and where its buckets start, its horizon, as in `1 hour (N cells) kept for 14 days from 2026-11-10T00:00:00Z`; before Straum first reads PostgreSQL's time it has no `from`. |
| `holds` | The [cells](how-it-works.md#cells) at the narrowest width, and the bound cells of a view that keeps them; then the range of every bucket it holds at any width, which an ask without bounds reads. A few trips carry a wrong year, hence 2002. |
| `keys` | Every name an ask may group or filter by, with the type a parameter for it must have. |
| `measures` | The names an ask may select after the keys. |
| `asked as` | The keys of the views that read a dim's columns. An ask names a view, never a dim, so it reads the dim through these. |

Until every table is loaded it prints `not ready: ` and why, and exits
with 3.

`--json` prints each view's keys with their types, the name of its
time, its measures, the range of buckets it holds and each width kept
for a time, under `kept` with its `width`, `for` and `from`, as `GET
/views` sends with `Accept: application/json`. It leaves out the cells
and the dims.

## straum ask

```text
straum ask [--url URL] [--timeout SECONDS] [--json] QUERY [PARAMETER...]
```

Sends `QUERY` to the Straum at `--url` and prints the answer as a
table. Flags go before the query. Every word after it is a parameter,
the value of `$1`, `$2` and on, `-h` and negative numbers included. A
query that starts with `-` follows a `--`.

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

The table holds what the [JSON answer](asks.md#the-answer) holds, text
without its quotes and a null as `null`. In text, a backslash and each
control character print as JSON writes them, `\\`, `\n` or `\u001b`, so
data can't break a table line or reach the terminal as an escape
sequence. The text `null` prints as `\null`.

`--json` prints the JSON answer itself. Errors go to stderr either
way, and stdout stays empty: a line naming the URL and the reply's
status, then the reply as Straum sent it, `{"errors": [...]}` with
`--json`.

A parameter's type is read from how it is written:

| Written | Sent as |
| --- | --- |
| `7`, `-7`, `9223372036854775808` | A number, exactly as written, however many digits. |
| `20.5`, `-0.05` | A number, exactly as written: digits on both sides of the point. |
| `true`, `false` | A boolean. |
| `'7'`, `'20.5'`, `'true'` | The string inside the single quotes. |
| Anything else | A string, such as `Queens`, `2024-01-17T05:00:00Z`, `.5` or `007`. |

The string 7 is written `"'7'"`. An ask Straum can't answer prints the
reply's first line and each error, and exits with 1:

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
may take several lines, so a pasted query works. It ends at the line
holding its semicolon, so its parameters go on that line. Without
one, Enter starts another line, and a blank line sends the ask with no
parameters. The semicolon is the shell's: it never reaches Straum,
which refuses one at the end of an ask.

```text
straum> SELECT payment_type, trips
FROM trips
WHERE bucket >= $1 AND bucket < $2
GROUP BY payment_type; 2003-01-01T03:00:00Z 2024-02-01T06:00:00Z
```

Ctrl-C at the prompt drops the lines typed so far. While a command
runs, Ctrl-C ends the shell; the commands entered are still kept.

### Paging with \rows

Rows count from 0, and a negative `START` counts back from the end:
`\rows trips 1000 50` prints rows 1000 to 1049, `\rows trips -20` the
last 20. A `START` past either end is held at it. The last line says
how many rows the view has and, past the first, from which row they
print. `\rows` reads every cell of the view whichever rows it prints,
so on a large view it takes as long as an ask over all its buckets.

### Timing

At a terminal an ask and `\rows` end in a line of milliseconds. `Time`
is the whole request as the shell measured it, from connecting to the
reply's last byte. The parentheses hold the reply's
[`Server-Timing`](asks.md#the-answer) header, the time in Straum:
reading the ask, answering it, writing the table, and `total`. `\rows`
sends no ask to read, so its `parse` is 0. A failed command is not
timed.

### Completion and history

At a terminal, Tab completes by where the cursor is:

| After | Completes |
| --- | --- |
| `SELECT` | Keys and measures. |
| `FROM`, `\d`, `\rows` | Views. |
| `WHERE`, `AND` | A whole condition with the next free parameter. |
| `HAVING` | A selected measure, then a comparison. |
| The semicolon | The start or end of the buckets held. |

Tab and the dimmed hint read every line of the ask up to the cursor,
so `FROM` on one line still offers views on the next. Completion
offers what is likely next; Straum still checks the ask. A completed
word ends in a space. Where the candidates differ, the first Tab lists
them.

Each command is kept as it is entered, in
`$XDG_STATE_HOME/straum/history` when that variable holds an absolute
path, else in `~/.local/state/straum/history`. Without a home
directory, as in the image, none is kept. The last command that starts
as the one being typed shows dimmed past the cursor, and the right
arrow takes it; with none, a dimmed hint names the next parameter.
Ctrl-R searches the history. `NO_COLOR` turns off the dimming.

### On a pipe

With its input or output anywhere but a terminal, the shell prints no
prompt, completes nothing and times nothing, so a script gets the same
output each run:

```sh
printf '%s\n' '\d' '\rows trips' | straum shell
```

An ask takes lines as at a terminal, and the end of input ends one
too, so a file of asks with a blank line or a semicolon after each
reads as typed. Answers go to stdout. A failed command says why on
stderr and the next is read, as in psql. If any failed, the shell
exits with 1 after the last, saying how many.

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
