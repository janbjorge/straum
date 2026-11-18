# straum chart

Runs straum on Kubernetes: a Deployment, the spec in a ConfigMap, a
ClusterIP Service and a ServiceAccount. No volume claims; straum keeps
everything in memory and loads every table again when its pod starts.
The [user guide](https://janbjorge.github.io/straum/deploy.html#kubernetes) has
more.

## Before you install

1. PostgreSQL 17 or newer. 17 and 18 need `wal_level = logical`, which
   takes a restart; on a managed service, set the provider's parameter for
   logical decoding. From 19, `replica`, the default, is enough. Where the
   restart can't be had, `wal: physical` reads the WAL `replica` writes.
2. The setup SQL that `straum plan spec.sql` prints, run by the
   tables' owner: the role, its grants and the publication. With
   `wal: physical`, `straum plan spec.sql --wal physical`: the role
   and its grants.
3. A Secret holding the DSN of the role `straum plan` made:

   ```sh
   kubectl create secret generic straum-dsn \
     --from-literal=dsn='postgres://straum:PASSWORD@HOST:5432/DATABASE'
   ```

   straum checks the server's certificate and name against the roots built
   into it; add `?sslmode=disable` only on a network you trust.
4. For a server whose CA isn't a public one, such as Amazon RDS's or
   Cloud SQL's, a Secret holding its PEM roots, named in `rootCert`.
   The chart mounts them at `/etc/straum-tls/ca.pem`, which the DSN names:

   ```sh
   kubectl create secret generic straum-ca --from-file=ca.pem=global-bundle.pem
   ```

   ```text
   postgres://straum:PASSWORD@HOST:5432/DATABASE?sslrootcert=/etc/straum-tls/ca.pem
   ```

## Install

Each release publishes the chart at its version, pulling its image,
`ghcr.io/janbjorge/straum:X.Y.Z`:

```sh
helm upgrade --install straum oci://ghcr.io/janbjorge/charts/straum \
  --version X.Y.Z \
  --set dsn.existingSecret=straum-dsn \
  --set-file spec=spec.sql
```

The chart in the repository has `appVersion` 0.0.0, which no release
publishes, so installed from a checkout it needs `--set
image.tag=X.Y.Z`. Set `image.repository` for a mirror or an image built
from the repository's `Dockerfile`.

Needs Kubernetes 1.30 or newer: the pod's pause before SIGTERM is a
sleep action, on by default from 1.30.

The pod turns ready once every table is loaded and straum is within
`maxLag` seconds of PostgreSQL. A new spec rolls the pod, and the new pod
loads before the old one stops. Raise `progressDeadlineSeconds`, an hour
by default, if a load takes longer, or Kubernetes calls the rollout
failed. `straum status` says what it has done:
whether it is ready and why not, its lag and the WAL PostgreSQL holds for
it:

```sh
kubectl exec deployment/straum -- /straum status
```

`values.yaml` documents every value, and `values.schema.json` refuses the
values straum can't take.

On an IPv6-only cluster set `listenAddress: "[::]"`, so the probes
reach straum; it listens on `0.0.0.0` otherwise. The chart always
passes `--listen`: on its own straum listens on `127.0.0.1`, which no
probe or Service reaches. straum has no authentication or TLS, so any
pod that reaches the Service can ask; add a NetworkPolicy to narrow
that.

With `wal: physical`, set `maxPageMemory` well under
`resources.limits.memory`: the rows it bounds come on top of the cells,
and at the defaults, both `1Gi`, Kubernetes kills the pod for its memory
before straum's own limit can start it over.

## Checks

`scripts/check-chart.py` runs `helm lint --strict`, the
[helm-unittest](https://github.com/helm-unittest/helm-unittest) suites in
`tests/`, and [kubeconform](https://github.com/yannh/kubeconform) on what
the chart renders with `ci/values.yaml`. CI runs it too.
