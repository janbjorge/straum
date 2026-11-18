# Deploying Straum

How do you run Straum for real? As one static binary, its image, or the
Helm chart, next to the apps that ask it. Set up
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

The Helm chart, `deploy/helm/straum`, runs Straum as one pod with the
spec in a ConfigMap and the DSN in a Secret. Build the image with the
repository's `compose.yaml` and push it, then put the DSN in a Secret,
since the chart never holds it, or have your secret store sync one, such
as the External Secrets Operator or the Secrets Store CSI driver:

```sh
export STRAUM_IMAGE=registry.example.com/straum:0.0.0
docker compose build straum
docker compose push straum
kubectl create secret generic straum-dsn \
  --from-literal=dsn='postgres://straum:PASSWORD@HOST:5432/DATABASE'
helm upgrade --install straum deploy/helm/straum \
  --set image.repository=registry.example.com/straum \
  --set image.tag=0.0.0 \
  --set dsn.existingSecret=straum-dsn \
  --set-file spec=usage.sql
kubectl exec deployment/straum -- /straum status
```

The chart makes a Deployment, a ConfigMap with the spec, a ClusterIP
Service on port 8080 and a ServiceAccount. For a release named `straum`,
apps in the cluster ask at `http://straum:8080/ask`. The chart passes
`--listen 0.0.0.0:8080`, or `[::]:8080` on an IPv6-only cluster, so any
pod in the cluster reaches it; a NetworkPolicy narrows that
([Who can ask](#who-can-ask)).
[`values.yaml`](https://github.com/janbjorge/straum/blob/main/deploy/helm/straum/values.yaml)
documents every value, and `values.schema.json` refuses values Straum
can't take, such as a `maxLag` of 0. Size `resources` by the
[cells](how-it-works.md#cells) the spec keeps, request equal to limit.

Readiness is `/readyz` and liveness is `/livez`, so a load or lag takes
the pod out of the Service but never restarts it. A new spec rolls the
pod. The new pod loads every table before the old one stops, so asks
keep answering, and PostgreSQL holds two slots until the old pod is
gone. Each replica holds its own slot and loads every table.
Kubernetes calls a rollout failed, and `helm --wait` with it, if the
new pod isn't ready within `progressDeadlineSeconds`, an hour by
default; raise it if the spec's tables take longer to load. A stopping
pod keeps serving for 5 seconds after SIGTERM, while the Service drops
it, then finishes the asks in flight.

The pod meets the Restricted Pod Security Standard: a non-root user,
no capabilities, no privilege escalation, a read-only root filesystem
and the `RuntimeDefault` seccomp profile. The chart pins no user, so
the image's, 65532, applies, and OpenShift's `restricted-v2` admits
the pod with a user from the namespace's range. It mounts no ServiceAccount
token, since Straum never calls the Kubernetes API, and its only
volumes are the spec and, if named, the roots below.

A server whose CA isn't one of Straum's built-in roots, such as Amazon
RDS's or Cloud SQL's ([Connect](postgresql.md#2-connect)), needs its PEM roots in
a Secret. Name it in `rootCert.existingSecret`, and the key in
`rootCert.key` (`ca.pem` by default). The chart mounts the roots at
`/etc/straum-tls/ca.pem`, and the DSN names them:

```sh
kubectl create secret generic straum-ca --from-file=ca.pem=global-bundle.pem
helm upgrade --install straum deploy/helm/straum \
  --reuse-values --set rootCert.existingSecret=straum-ca
```

```text
postgres://straum:PASSWORD@HOST:5432/DATABASE?sslrootcert=/etc/straum-tls/ca.pem
```

Next: [Performance](performance.md), what to expect from a deployment.
