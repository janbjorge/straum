# straum chart

Runs straum on Kubernetes: a Deployment, the spec in a ConfigMap, a
ClusterIP Service and a ServiceAccount. No volume claims; straum keeps
everything in memory and loads every table again when its pod starts.
The [user guide](https://janbjorge.github.io/straum/kubernetes.html) has
more.

## Before you install

1. PostgreSQL 18 with `wal_level = logical`, which takes a restart. On a
   managed service, set the provider's parameter for logical decoding.
2. The setup SQL that `straum plan --spec spec.sql` prints, run by the
   tables' owner: the role, its grants and the publication.
3. The image, built from the repository's `Dockerfile` and pushed to your
   registry.
4. A Secret holding the DSN of the role `straum plan` made:

   ```sh
   kubectl create secret generic straum-dsn \
     --from-literal=dsn='postgres://straum:PASSWORD@HOST:5432/DATABASE'
   ```

   straum checks the server's certificate and name against the roots built
   into it; add `?sslmode=disable` only on a network you trust.

## Install

```sh
helm upgrade --install straum deploy/helm/straum \
  --set image.repository=registry.example.com/straum \
  --set image.tag=0.0.0 \
  --set dsn.existingSecret=straum-dsn \
  --set-file spec=spec.sql
```

The pod turns ready once every table is loaded and straum is within
`maxLag` seconds of PostgreSQL. A new spec rolls the pod, and the new pod
loads before the old one stops. `straum status` says what it has done:
whether it is ready and why not, its lag, the WAL PostgreSQL holds for it,
its starts and failures:

```sh
kubectl exec deployment/straum -- /straum status
```

`values.yaml` documents every value, and `values.schema.json` refuses the
values straum can't take.

## Checks

`scripts/check-chart.py` runs `helm lint --strict`, the
[helm-unittest](https://github.com/helm-unittest/helm-unittest) suites in
`tests/`, and [kubeconform](https://github.com/yannh/kubeconform) on what
the chart renders with `ci/values.yaml`. CI runs it too.
