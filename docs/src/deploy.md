# Deploying Straum

Straum runs as one static binary, its image, or the Helm chart, next to
the apps that ask it: on
[Kubernetes](#kubernetes), with [Docker Compose](#docker-compose) or on
[Amazon ECS](#amazon-ecs). Set up [PostgreSQL](postgresql.md) first;
every flag is under [Command line](cli.md).

## Get straum

Each [release](https://github.com/janbjorge/straum/releases) is an
image, `ghcr.io/janbjorge/straum:X.Y.Z`, and the same statically linked
binary for Linux, `straum-x86_64-unknown-linux-musl` and
`straum-aarch64-unknown-linux-musl`. The image is also tagged `X.Y`,
the newest release of that line, and `latest`, the newest stable
release. To install the newest binary on x86-64:

```sh
base=https://github.com/janbjorge/straum/releases/latest/download
curl -fL -O "$base/straum-x86_64-unknown-linux-musl" -O "$base/SHA256SUMS"
sha256sum -c --ignore-missing SHA256SUMS
sudo install -m 755 straum-x86_64-unknown-linux-musl /usr/local/bin/straum
```

`gh attestation verify straum-x86_64-unknown-linux-musl --repo
janbjorge/straum` checks it was built by this repository's CI;
`gh attestation verify oci://ghcr.io/janbjorge/straum:X.Y.Z --repo
janbjorge/straum` does the same for an image.

The image is the binary at `/straum`, its entrypoint, on a distroless
base with no shell and no package manager. It runs as user 65532.

## Run it

```sh
export STRAUM_DSN='postgres://straum:PASSWORD@db.example.com:5432/app'
straum run usage.sql --wal logical
```

Straum loads every table, then follows the stream. `GET /readyz` says
whether it is loaded and within `--max-lag` of PostgreSQL, `GET /livez`
whether the process serves ([endpoints](asks.md#endpoints)). A Straum
that lags still answers asks, each with its `as_of` ([States](#states)).

## Who can ask

Straum has no authentication and no TLS: anyone who reaches its port
can ask, read its status and read every view's rows. So `straum run`
listens on `127.0.0.1:8080` unless told otherwise. In a container,
give `--listen 0.0.0.0:8080`, as the [quick start](quick-start.md) and
the chart do. Then decide in front of Straum who reaches it: a
Kubernetes NetworkPolicy that admits only the apps that ask, a proxy
that authenticates and ends TLS, or a service mesh.

`POST /ask` refuses a request without `Content-Type: application/json`
([errors](asks.md#errors)), so a page on another site can't make a
visitor's browser run asks.

A page can still reach a Straum on a laptop by DNS rebinding: its own
name, rebound to 127.0.0.1, makes the visitor's browser read and ask
as if the page were Straum's own. The `Host` the browser sends then
names the page's site, so Straum checks it. A request whose `Host` it
doesn't answer gets 421 Misdirected Request on every endpoint.

- With `--allow-hosts`, Straum answers only the names it lists, each
  with the port the client writes: `localhost:8080,127.0.0.1:8080`.
- Without it, on loopback, Straum answers `localhost`, `127.0.0.1`,
  `[::1]` or the address it listens on, with its port.
- Without it, on any other address, `0.0.0.0` too, every `Host` is
  answered, since whoever reaches the port can ask by any name that
  points at it.

A port published or forwarded to the host's loopback is reached as
loopback is, but Straum listens on `0.0.0.0` behind it and can't tell.
So give `--allow-hosts` there, as the Compose files do for their
`127.0.0.1:8080:8080`. The chart gives none, since pods reach Straum
by whatever name the Service has; while a `kubectl port-forward` to it
runs, a rebound page in a browser on that machine can read it.

## States

![The states of a running Straum](diagrams/lifecycle.svg)

*Loading turns Ready once every table is in. Ready turns Lagging past
`--max-lag` and back when it catches up. Any failure ends the process
with exit 1, and the new process whatever runs Straum starts loads from
a new slot. SIGTERM leads to Stopping.*

| State | `/readyz` | Asks |
| --- | --- | --- |
| Loading | 503 `not ready: loading` | 503 |
| Ready | 200 `ready` | Answered |
| Lagging | 503 `not ready: 45 s behind PostgreSQL` | Answered, each with its `as_of` |
| Exited | No answer: the port is closed | Refused |
| Stopping | No answer: the port is closed | Those in flight finish, for at most 10 s; new ones refused |

Stopping ends with exit 0 ([straum run](cli.md#straum-run)).

Straum reads the server's write-ahead log (WAL) position every second.
Lag is the time since Straum last caught up with a position the server
had reached, so it can overstate the true delay by up to one second but
never understate it.

A lost connection, a missing setup, a change Straum can't apply, a
table altered under it, too much WAL retained for its slot, or more
commits held while the tables load than `--max-load-memory` allows ends
the process with exit 1, and the last lines of its log say why. The
connections close with it, and PostgreSQL drops the temporary slot.
Straum keeps no state across a failure and never retries inside the
process; whatever runs it starts it again, with a wait of its own.

```text
straum: PostgreSQL held 5120.0 MiB of WAL for Straum's slot, past the --max-retained-wal limit of 4096.0 MiB; the watchdog ended the slot's session
```

With `--max-memory` set, resident memory past it stops Straum the same
way, loads included, so the log says why instead of the kernel killing
the process without a word. A spec too big for the limit fails each
start.

```text
straum: Straum holds 1843.2 MiB of resident memory, past the --max-memory limit of 1800.0 MiB
```

`straum status` says which state it is in and why
([Command line](cli.md#straum-status)).

### Large transactions

A transaction that changes more rows of the spec's tables than
`--max-transaction-rows` ([straum run](cli.md#straum-run)) stops
Straum, and the next start loads those rows through the bulk `COPY`.
Every replica reads the same transaction, so every one stops and loads
again at once. Batch a backfill or bulk update into transactions under
the limit, or raise the limit and pay what
[Transactions](performance.md#transactions) measures.

## Kubernetes

The chart is
[`deploy/helm/straum`](https://github.com/janbjorge/straum/tree/main/deploy/helm/straum),
and needs Kubernetes 1.30 or newer. Each release publishes it at
`oci://ghcr.io/janbjorge/charts/straum`, at that release's version,
pulling that release's image. The chart in the repository names no
release: its `appVersion` is 0.0.0, so an install from a checkout needs
`image.tag`. Its
[README](https://github.com/janbjorge/straum/blob/main/deploy/helm/straum/README.md)
says what it requires, and
[`values.yaml`](https://github.com/janbjorge/straum/blob/main/deploy/helm/straum/values.yaml)
documents every value.

It runs one pod. The spec is a ConfigMap; the data source name (DSN)
comes from a Secret the chart never holds. Readiness is `/readyz` and
liveness `/livez`, so a load or a lag takes the pod out of the Service
and never restarts it. Each replica holds its own slot and loads every
table.

A new spec rolls the pod. The new pod loads every table before the old
one stops, so asks keep answering, and PostgreSQL holds two slots until
the old pod is gone. Kubernetes gives that load an hour before it calls
the rollout failed. A stopping pod leaves the Service, pauses 5
seconds, then takes SIGTERM and finishes the asks in flight, within its
30 second grace period.

The chart passes `--listen 0.0.0.0:8080`, or `[::]:8080` on an
IPv6-only cluster, so any pod that reaches the Service can ask
([Who can ask](#who-can-ask)); for a release named `straum`, that is
`http://straum:8080/ask`. The pod pins no user, so the image's user,
65532, applies. OpenShift's `restricted-v2` assigns one from the
project's range, and refuses one pinned outside it. The pod mounts no
ServiceAccount token, since Straum never calls the Kubernetes API.

Size `resources` by the cells the spec keeps
([Memory](performance.md#memory)), request equal to limit. With
`--wal physical`, the page memory comes on top: keep `maxPageMemory`
well under the memory limit, or the pod is killed before Straum's own
limit stops it.

`maxMemory` (`--max-memory`, unset by default) bounds the whole
process, cells and page memory alike. Set it a little under the memory
limit, so Straum stops with a line naming it before the kernel kills
the pod, or, on a shared host, something else. With `--wal physical`
keep it above `maxPageMemory` plus the cells, or it stops Straum
before the page memory limit can.

A certificate authority (CA) Straum does not already trust, such as
Amazon RDS's or Cloud SQL's ([Connect](postgresql.md#2-connect)), is a
Secret named by `rootCert.existingSecret`, mounted at
`/etc/straum-tls/ca.pem`. The DSN names that path:

```text
postgres://straum:PASSWORD@HOST:5432/DATABASE?sslrootcert=/etc/straum-tls/ca.pem
```

## Docker Compose

[`deploy/compose/compose.yaml`](https://github.com/janbjorge/straum/blob/main/deploy/compose/compose.yaml)
runs a release's image, named by `STRAUM_VERSION`. The spec is
`spec.sql` beside the file. The DSN is the file `dsn`, passed as a
secret so it never sits in the environment. User 65532, the image's
user, must be able to read both. Straum listens on `0.0.0.0:8080`
inside the container, and Compose publishes it on the host's
`127.0.0.1:8080` only, so `--allow-hosts` names `localhost:8080` and
`127.0.0.1:8080` ([Who can ask](#who-can-ask)). Compose restarts
it unless it was stopped. The health check is `straum status`; a
failed check does not restart it, since a load or a lag must not.

## Amazon ECS

[`deploy/ecs/`](https://github.com/janbjorge/straum/tree/main/deploy/ecs)
is a Fargate task. A task mounts no file, so the spec, and the CA
roots when the DSN needs `sslrootcert=/etc/straum-tls/ca.pem`, are
built into an image on top of the release's. The DSN arrives as
`STRAUM_DSN`. Run one task, and let the new one become ready before
the old one stops: PostgreSQL then holds two slots only while both
are up.

ECS has one health check, `straum status`, and replaces a task that
fails it. A load has about 15 minutes, and a lag about 10, before that
happens, so a spec whose tables take longer to load cannot run there.
The task's security group decides who can ask ([Who can
ask](#who-can-ask)).

Next: [Performance](performance.md), what to expect from a deployment.
