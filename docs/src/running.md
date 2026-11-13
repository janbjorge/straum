# Running Straum

`straum` is one static binary, run on its own or from the
[Helm chart](#kubernetes):

```text
straum run --spec FILE [--listen ADDRESS] [--publication NAME] [--dsn-file FILE]
           [--max-lag SECONDS] [--max-retained-wal BYTES]
           [--max-transaction-rows ROWS]
straum plan --spec FILE [--publication NAME] [--role NAME] [--dsn-file FILE]
straum status [--url URL]
straum views [--url URL]
straum ask [--url URL] QUERY [PARAMETER...]
straum shell [--url URL]
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
| `--max-transaction-rows ROWS` | Rows one transaction may change in the spec's tables before Straum [starts over](how-it-works.md#following) instead of applying it. |

It logs to stderr, one line per event:

```text
straum: listening on 0.0.0.0:8080
straum: loaded every table; streaming from 0/17B2788
```

SIGTERM or SIGINT stops it: it stops accepting, finishes the asks in
flight and exits, and PostgreSQL drops the slot as the connection closes.

| Endpoint | Answers |
| --- | --- |
| `POST /ask` | An [ask](asks.md), as JSON; as a table with `Accept: text/plain`. |
| `GET /readyz` | 200 `ready`, or 503 `not ready: ` and why. |
| `GET /livez` | 200 while the process serves. Lag never fails it. |
| `GET /status` | What `straum status` prints, in plain text. |
| `GET /views` | What `straum views` prints, in plain text; 503 `not ready: ` and why until every table is loaded. As JSON with `Accept: application/json`. |
| `GET /views/NAME` | The rows PostgreSQL would give for the view, the first 1000, as a table in plain text; 404 for a name no view has. |

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

## straum views

Prints every view of the Straum at `--url` and how much of it Straum
holds, from its `GET /views`.

```text
$ straum views
view trips
  buckets:  3600s, 86400s, 604800s
  holds:    173660 cells and 173660 bound cells, 2003-01-01T03:00:00Z to 2024-02-01T06:00:00Z
  keys:     bucket timestamptz, payment_type integer, pu_location_id integer, borough text
  measures: trips, fare, avg_fare

dim public.taxi_zones
  holds:    265 rows
```

- `buckets`: the view's bucket widths in seconds, narrowest first.
- `holds`: the [cells](how-it-works.md#cells) at the narrowest width,
  and the bound cells of a view that joins. Then the range of its
  buckets: the start of the first and the end of the last, which are
  the bounds an ask takes to read everything.
- `keys`: every name an ask may group or filter by, each with the type
  a parameter for it must have.
- `measures`: the names an ask may select after the keys.
- `dim`: a joined table and the rows Straum holds of it.

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

Quote the query for the shell with single quotes, so `$1` reaches
Straum as written. The query is an [ask](asks.md#the-query), and the
table holds what the JSON answer holds: values print the same, text
without its quotes, a null as `null`.

A command line holds only text, so a parameter's type is read from it:

| Written | Sent as |
| --- | --- |
| `7`, `-7` | An integer. |
| `true`, `false` | A boolean. |
| `'7'`, `'true'` | The string inside the single quotes. |
| Anything else | A string, such as `Queens` or `2024-01-17T05:00:00Z`. |

The shell takes single quotes for itself, so the string 7 is written
`"'7'"`. A parameter must still have its column's type: `straum views`
lists them.

An ask Straum can't answer prints the reply's first line and each
error, and exits with 1:

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
straum> \rows trips
bucket               | payment_type | pu_location_id | borough       | trips | fare    | avg_fare
---------------------+--------------+----------------+---------------+-------+---------+-----------
2003-01-01T03:00:00Z |            3 |            170 | Manhattan     |     2 |    0.00 |          0
2009-01-01T05:00:00Z |            2 |            138 | Airports      |     1 |   50.60 |       50.6
2009-01-02T04:00:00Z |            2 |            137 | Manhattan     |     1 |    4.40 |        4.4
...
2024-01-01T07:00:00Z |            2 |             87 | Manhattan     |     4 |  104.40 |       26.1
1000 of 173660 rows, as of 2026-11-13T07:58:59.965466167Z
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

`\rows` reads every cell of the view, as an ask over all its buckets
does, so on a large view it takes as long as that ask.

At a terminal the line is edited as in a shell, the arrows walk the
lines entered before, and Tab completes by where the cursor is:

| After | Tab offers |
| --- | --- |
| Nothing | `SELECT` and the commands. |
| `SELECT` | The keys and measures not yet listed, then `FROM`. |
| `FROM` | The views, then `WHERE`. |
| `WHERE`, `AND` | A whole condition with the next free parameter: `bucket >= $1 AND bucket < $2`, `borough = $3`. |
| A condition | `AND`, and once both bounds are written `GROUP BY` with the selected keys. |
| `;` | For a bound, the start or the end of the buckets held; for a boolean, `true` and `false`. |
| `\rows` | The views. |

Past the semicolon the line also shows, dimmed, which parameter comes
next and its type. The ask above is `SEL` Tab `pa` Tab `, t` Tab ` FR`
Tab `t` Tab `W` Tab `bu` Tab ` G` Tab Tab Tab. Completion offers what
is likely next, not only what is valid: Straum still checks the ask.

The lines entered are kept between sessions in
`$XDG_STATE_HOME/straum/history`, which is
`~/.local/state/straum/history` unless that variable is set; without a
home directory, as in the image, they are not kept. With `NO_COLOR`
set, or on a terminal that can't, the next parameter is not dimmed.

The prompt is for a terminal at both ends. With its input or its
output anywhere else, the shell prints no prompt and completes
nothing, so a script can drive it:

```sh
printf '%s\n' '\d' '\rows trips' | straum shell
```

Answers go to stdout. A line that fails says why on stderr and the
next line is read, as in psql; if any failed the shell exits with 1
after the last. `straum shell | head` ends quietly when `head` has
its lines, as `straum views` and `straum ask` do.

## Kubernetes

The repository's `Dockerfile` and Helm chart, `deploy/helm/straum`, run
Straum as one pod next to the apps that ask it, with the spec in a
ConfigMap and the DSN in a Secret.

```sh
docker build -t registry.example.com/straum:0.0.0 .
docker push registry.example.com/straum:0.0.0
```

The image holds the static binary and nothing else: no shell, no package
manager. It runs as user 65532 with a read-only root filesystem. Its
entrypoint is `straum`, so `docker run IMAGE help` prints the usage.

[Set up PostgreSQL](postgresql.md) first. Then put the DSN in a Secret,
since the chart never holds it, or have your secret store sync one, such
as the External Secrets Operator or the Secrets Store CSI driver:

```sh
kubectl create secret generic straum-dsn \
  --from-literal=dsn='postgres://straum:PASSWORD@HOST:5432/DATABASE'
helm upgrade --install straum deploy/helm/straum \
  --set image.repository=registry.example.com/straum \
  --set image.tag=0.0.0 \
  --set dsn.existingSecret=straum-dsn \
  --set-file spec=spec.sql
kubectl exec deployment/straum -- /straum status
kubectl exec deployment/straum -- /straum views
kubectl exec -it deployment/straum -- /straum shell
```

The chart makes a Deployment, a ConfigMap with the spec, a ClusterIP
Service on port 8080 and a ServiceAccount. For a release named `straum`,
apps in the cluster ask at `http://straum:8080/ask`.

| Value | Default | Meaning |
| --- | --- | --- |
| `image.repository` | required | Where the image lives. |
| `image.tag` | the chart's `appVersion` | The image's tag. |
| `image.digest` | | Pins the image as `sha256:...`; wins over the tag. |
| `image.pullPolicy` | `IfNotPresent` | |
| `imagePullSecrets` | `[]` | Secrets for a private registry. |
| `spec` | required | The spec; set it with `--set-file spec=spec.sql`. |
| `dsn.existingSecret` | required | The Secret holding the DSN. |
| `dsn.key` | `dsn` | The key in that Secret. |
| `publication` | `straum` | The publication Straum reads. |
| `maxLag` | `30` | Seconds behind PostgreSQL before `/readyz` turns off. |
| `maxRetainedWal` | `4Gi` | WAL retained for the slot before Straum starts over. |
| `maxTransactionRows` | `100000` | Rows one transaction may change before Straum starts over instead of applying it. |
| `replicaCount` | `1` | Pods. Each holds its own slot and loads every table. |
| `service.port` | `8080` | The Service's port. |
| `resources` | 1 GiB memory, request equal to limit | Size it by the [cells](how-it-works.md#cells) the spec keeps. |
| `serviceAccount.create` | `true` | Make a ServiceAccount. |
| `serviceAccount.name` | the release's full name | |
| `serviceAccount.annotations` | `{}` | For workload identity. |
| `podAnnotations`, `podLabels` | `{}` | Added to the pod. |
| `nodeSelector`, `tolerations`, `affinity` | empty | Pod placement. |
| `nameOverride`, `fullnameOverride` | | Resource names. |

`values.yaml` documents each value, and `values.schema.json` refuses
values Straum can't take, such as a `maxLag` of 0.

Readiness is `/readyz` and liveness is `/livez`, so a load or lag takes
the pod out of the Service but never restarts it. A new spec changes
the pod's checksum annotation, which rolls the pod. The new pod loads
every table before the old one stops, so asks keep answering, and
PostgreSQL holds two slots until the old pod is gone.

The pod meets the Restricted Pod Security Standard: non-root, no
capabilities, no privilege escalation, a read-only root filesystem and the
`RuntimeDefault` seccomp profile. It mounts no ServiceAccount token, since
Straum never calls the Kubernetes API, and its only volume is the spec.
The default memory request equals the limit, so the pod never uses more
than it asked for, and a node short of memory evicts pods that do first.
Raise both together.

> [!NOTE]
> The chart mounts no CA bundle, so a server whose certificate needs
> `sslrootcert` can't be reached through the chart yet.
