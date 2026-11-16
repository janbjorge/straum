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
whether it answers asks, `GET /livez` whether the process serves
([endpoints](asks.md#endpoints)).

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

`straum status` says which state it is in and why
([Command line](cli.md#straum-status)).

## Kubernetes

The Helm chart, `deploy/helm/straum`, runs Straum as one pod with the
spec in a ConfigMap and the DSN in a Secret. Build the image from the
repository's `Dockerfile` and push it, then put the DSN in a Secret,
since the chart never holds it, or have your secret store sync one, such
as the External Secrets Operator or the Secrets Store CSI driver:

```sh
docker build -t registry.example.com/straum:0.0.0 .
docker push registry.example.com/straum:0.0.0
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
apps in the cluster ask at `http://straum:8080/ask`.
[`values.yaml`](https://github.com/janbjorge/straum/blob/main/deploy/helm/straum/values.yaml)
documents every value, and `values.schema.json` refuses values Straum
can't take, such as a `maxLag` of 0. Size `resources` by the
[cells](how-it-works.md#cells) the spec keeps, request equal to limit.

Readiness is `/readyz` and liveness is `/livez`, so a load or lag takes
the pod out of the Service but never restarts it. A new spec rolls the
pod. The new pod loads every table before the old one stops, so asks
keep answering, and PostgreSQL holds two slots until the old pod is
gone. Each replica holds its own slot and loads every table.

The pod meets the Restricted Pod Security Standard: user 65532, no
capabilities, no privilege escalation, a read-only root filesystem and
the `RuntimeDefault` seccomp profile. It mounts no ServiceAccount
token, since Straum never calls the Kubernetes API, and its only
volume is the spec.

> [!NOTE]
> The chart mounts no CA bundle, so a server whose certificate needs
> `sslrootcert` can't be reached through the chart yet.

Next: [Performance](benchmark.md), what to expect from a deployment.
