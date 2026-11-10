# Kubernetes

The repository's `Dockerfile` and Helm chart, `deploy/helm/straum`, run
Straum as one pod next to the apps that ask it, with the spec in a
ConfigMap and the DSN in a Secret.

## The image

```sh
docker build -t registry.example.com/straum:0.0.0 .
docker push registry.example.com/straum:0.0.0
```

The image holds the static binary and nothing else: no shell, no package
manager. It runs as user 65532 with a read-only root filesystem. Its
entrypoint is `straum`, so `docker run IMAGE help` prints the usage.

## Install

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
| `maxRetainedWal` | `4Gi` | WAL held for the slot before Straum starts over. |
| `maxTransactionRows` | `100000` | Rows one transaction may change before Straum starts over instead of applying it. |
| `replicaCount` | `1` | Pods. Each holds its own slot and loads every table. |
| `service.port` | `8080` | The Service's port. |
| `resources` | 1 GiB memory, request equal to limit | Size it to the [cells](spec.md#memory). |
| `serviceAccount.create` | `true` | Make a ServiceAccount. |
| `serviceAccount.name` | the release's full name | |
| `serviceAccount.annotations` | `{}` | For workload identity and the like. |
| `podAnnotations`, `podLabels` | `{}` | Added to the pod. |
| `nodeSelector`, `tolerations`, `affinity` | empty | Pod placement. |
| `nameOverride`, `fullnameOverride` | | Resource names. |

`values.yaml` documents each value, and `values.schema.json` refuses
values Straum can't take, such as a `maxLag` of 0.

## The pod

Readiness is `/readyz` and liveness is `/livez`, so a load or lag takes
the pod out of the Service but never restarts it.

A new spec changes the pod's checksum annotation, which rolls the pod. The
new pod loads every table before the old one stops, so asks keep
answering, and PostgreSQL holds two slots until the old pod is gone.

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
