# Deploying Straum

How do you run Straum for real? As one static binary, its image, or the
Helm chart, next to the apps that ask it: on
[Kubernetes](#kubernetes), with [Docker Compose](#docker-compose),
under [systemd](#systemd) or on [Amazon ECS](#amazon-ecs). Set up
[PostgreSQL](postgresql.md) first; every flag is under
[Command line](cli.md).

## Get straum

Each [release](https://github.com/janbjorge/straum/releases) is an
image, `ghcr.io/janbjorge/straum:X.Y.Z`, and the same binary on its own
for Linux on x86-64 and arm64, with no libraries it needs. The image
tagged `latest` is the newest stable release. To install the newest
binary on x86-64:

```sh
base=https://github.com/janbjorge/straum/releases/latest/download
curl -fL -O "$base/straum-x86_64-unknown-linux-musl" -O "$base/SHA256SUMS"
sha256sum -c --ignore-missing SHA256SUMS
sudo install -m 755 straum-x86_64-unknown-linux-musl /usr/local/bin/straum
```

`gh attestation verify straum-x86_64-unknown-linux-musl --repo
janbjorge/straum` checks it was built by this repository's CI, and
`gh attestation verify oci://ghcr.io/janbjorge/straum:X.Y.Z --repo
janbjorge/straum` does the same for an image.

The image holds the static binary and nothing else: no shell, no package
manager. Its entrypoint is `straum`.

## Run it

```sh
export STRAUM_DSN='postgres://straum:PASSWORD@db.example.com:5432/app'
straum run usage.sql
```

Straum loads every table, then follows the stream. `GET /readyz` says
whether it is loaded and within `--max-lag` of PostgreSQL, `GET /livez`
whether the process serves ([endpoints](asks.md#endpoints)). A Straum
that lags still answers asks, each with its `as_of`, as the table below
says.

## Who can ask

Straum has no authentication and no TLS. Anyone who reaches its port
can ask, read its status and read every view's rows. So `straum run`
listens on `127.0.0.1:8080` unless told, which only its own host
reaches. In a container that is the container itself: give `--listen
0.0.0.0:8080` there, as the [quick start](quick-start.md) and the chart
do.

Once it listens on more than loopback, decide who reaches it in front
of Straum: a Kubernetes NetworkPolicy that admits only the apps that
ask, a proxy that authenticates and ends TLS, or a service mesh. Don't
publish the port to a network you don't trust.

`POST /ask` refuses a request that is not sent with `Content-Type:
application/json` ([errors](asks.md#errors)). A browser sends a form
or plain text to any site without asking first, but not JSON, so a page
on another site can't make a visitor's browser run asks.

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

Straum reads the server's WAL position every second. Lag is the time
since Straum last caught up with a position the server had reached. It
can overstate the true delay by up to one second, never understate it.

Straum fails hard. Any failure ends the process with exit 1: a lost
connection, a missing setup, a change Straum can't apply, a table
altered under it, or too much WAL retained for its slot. The last lines
of its log say why. The connections close with it, and PostgreSQL drops
the temporary slot. Straum keeps no state across a failure and never
retries inside the process. Whatever runs it starts it again, with a
wait of its own: Kubernetes restarts the pod with a backoff up to five
minutes, and systemd wants `Restart=on-failure`. The new process loads
every table from a new slot.

```text
straum: PostgreSQL held 5120.0 MiB of WAL for Straum's slot, past the --max-retained-wal limit of 4096.0 MiB; the watchdog ended the slot's session
```

`straum status` says which state it is in and why
([Command line](cli.md#straum-status)).

## Kubernetes

The chart is
[`deploy/helm/straum`](https://github.com/janbjorge/straum/tree/main/deploy/helm/straum).
Each release publishes it at `oci://ghcr.io/janbjorge/charts/straum`,
at that release's version, pulling that release's image. The chart in
the repository names no release: its `appVersion` is 0.0.0. Its
[README](https://github.com/janbjorge/straum/blob/main/deploy/helm/straum/README.md)
says what it requires, and
[`values.yaml`](https://github.com/janbjorge/straum/blob/main/deploy/helm/straum/values.yaml)
documents every value.

It runs one pod. The spec is a ConfigMap. The DSN comes from a Secret,
which the chart never holds. Readiness is `/readyz` and liveness is
`/livez`, so a load or a lag takes the pod out of the Service and
never restarts it. The new pod loads every table before the old one
stops, so asks keep answering, and PostgreSQL holds two slots until
the old pod is gone. Each replica holds its own slot and loads every
table. The rollout waits an hour for that load. A stopping pod leaves
the Service, pauses 5 seconds, then takes SIGTERM and finishes the
asks in flight. The chart needs Kubernetes 1.30 or newer.

It listens on `0.0.0.0:8080`, or `[::]:8080` on an IPv6-only cluster.
With no `--listen`, Straum serves `127.0.0.1`, which no probe or
Service reaches. Any pod that reaches the Service can ask
([Who can ask](#who-can-ask)). For a release named `straum`, that is
`http://straum:8080/ask`. The pod pins no user. On Kubernetes the
image's user, 65532, applies. OpenShift's `restricted-v2` assigns one
from the project's range, and refuses one pinned outside it. No
ServiceAccount token: Straum never calls the Kubernetes API. Size
`resources` by the [cells](how-it-works.md#cells) the spec keeps,
request equal to limit.

A CA Straum does not already trust, such as Amazon RDS or Cloud SQL
([Connect](postgresql.md#2-connect)), is a Secret named by
`rootCert.existingSecret`. The chart mounts it at
`/etc/straum-tls/ca.pem`. The DSN names that path:

```text
postgres://straum:PASSWORD@HOST:5432/DATABASE?sslrootcert=/etc/straum-tls/ca.pem
```

## Docker Compose

[`deploy/compose/compose.yaml`](https://github.com/janbjorge/straum/blob/main/deploy/compose/compose.yaml)
runs a release's image. `STRAUM_VERSION` names the release. The spec
is `spec.sql` beside the file. The DSN is the file `dsn`, passed as a
secret, so it never sits in the environment. The spec and the DSN must
be readable by user 65532, the image's user. Straum listens on
`127.0.0.1:8080` ([Who can ask](#who-can-ask)). The health check is
`straum status`, and a failed check does not restart it: a load or a
lag must not.

## systemd

[`deploy/systemd/straum.service`](https://github.com/janbjorge/straum/blob/main/deploy/systemd/straum.service)
runs the binary from [Get straum](#get-straum) on `127.0.0.1:8080`.
The spec is `/etc/straum/spec.sql`. The DSN is `/etc/straum/dsn`,
passed as a credential, so it never sits in the environment. The
service user can write nowhere. After a failure it starts Straum again
10 seconds later, with no limit, because Straum exits 1 and keeps no
state. `MemoryMax` follows the [cells](how-it-works.md#cells).

## Amazon ECS

[`deploy/ecs/`](https://github.com/janbjorge/straum/tree/main/deploy/ecs)
is a Fargate task. A task mounts no file, so the spec, and the CA
roots when the DSN needs `sslrootcert=/etc/straum-tls/ca.pem`, are
built into an image on top of the release's. The DSN arrives as
`STRAUM_DSN`. Run one task, and let the new one become ready before
the old one stops: PostgreSQL then holds two slots only while both
are up.

ECS has one health check, and it replaces a task that fails it. The
check is `straum status`. A load has about 15 minutes, and a lag about
10, before that happens. A spec whose tables take longer to load
cannot run there. Who can ask is the task's security group
([Who can ask](#who-can-ask)).

Next: [Performance](performance.md), what to expect from a deployment.
