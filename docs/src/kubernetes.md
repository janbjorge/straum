# Kubernetes

The repository has a `Dockerfile` for the image and a Helm chart in
`deploy/helm/straum`. Together they run Straum as one pod next to the apps
that ask it, with the spec in a ConfigMap and the DSN in a Secret.

## The image

```sh
docker build -t registry.example.com/straum:0.0.0 .
docker push registry.example.com/straum:0.0.0
```

The image holds the static binary and nothing else: no shell, no package
manager. It runs as user 65532 and works with a read-only root
filesystem. Its entrypoint is `straum`, so `docker run IMAGE help` prints
the usage.

## Before you install

1. [Set up PostgreSQL](postgresql.md): logical decoding on, and the SQL
   from `straum plan` run by the tables' owner.
2. Push the image to a registry your cluster pulls from.
3. Put the DSN in a Secret. The chart never holds it:

   ```sh
   kubectl create secret generic straum-dsn \
     --from-literal=dsn='postgres://straum:PASSWORD@HOST:5432/DATABASE'
   ```

   Or have your secret store sync one, such as the External Secrets
   Operator or the Secrets Store CSI driver.

## Install

```sh
helm upgrade --install straum deploy/helm/straum \
  --set image.repository=registry.example.com/straum \
  --set image.tag=0.0.0 \
  --set dsn.existingSecret=straum-dsn \
  --set-file spec=spec.sql
```

The chart makes a Deployment, a ConfigMap with the spec, a ClusterIP
Service on port 8080 and a ServiceAccount. For a release named `straum`,
apps in the cluster ask at `http://straum:8080/ask`.

Check on it with `straum status`, which is in the image:

```sh
kubectl exec deployment/straum -- /straum status
```

## Values

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
| `maxRetainedWal` | `4Gi` | WAL held for the slot before Straum starts over. |
| `replicaCount` | `1` | Pods. Each holds its own slot and loads every table. |
| `service.port` | `8080` | The Service's port. |
| `resources` | 1 GiB memory, request equal to limit | Size it to the rollups. |
| `serviceAccount.create` | `true` | Make a ServiceAccount. |
| `serviceAccount.name` | the release's full name | |
| `serviceAccount.annotations` | `{}` | For workload identity and the like. |
| `podAnnotations`, `podLabels` | `{}` | Added to the pod. |
| `nodeSelector`, `tolerations`, `affinity` | empty | Pod placement. |
| `nameOverride`, `fullnameOverride` | | Resource names. |

`values.yaml` documents each value, and `values.schema.json` refuses
values Straum can't take, such as a `maxLag` of 0.

## How the pod runs

Readiness is `/readyz`, which is off while the tables load and while
Straum lags past `maxLag`. Liveness is `/livez`, which checks the process
only, so a slow load or lag never restarts the pod.

A new spec changes the pod's checksum annotation, which rolls the pod. The
new pod loads every table before the old one stops, so asks keep
answering, and PostgreSQL holds two slots until the old pod is gone.

The pod meets the Restricted Pod Security Standard: it runs as non-root
with no capabilities, no privilege escalation, a read-only root filesystem
and the `RuntimeDefault` seccomp profile. It mounts no ServiceAccount
token, since Straum never calls the Kubernetes API. Its only volume is the
spec.

Straum holds everything in memory. The default memory request equals the
limit, so the pod never uses more memory than it asked for, and a node
short of memory evicts pods that do first. Raise both together.

> [!NOTE]
> The chart reads the DSN from a Secret only, and mounts no CA bundle. A
> server whose certificate needs `sslrootcert` can't be reached through
> the chart yet.
