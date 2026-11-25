# straum chart

Runs straum on Kubernetes: a Deployment, the spec in a ConfigMap, a
ClusterIP Service and a ServiceAccount. No volume claims; straum keeps
everything in memory and loads every table again when its pod starts.
The [user guide](https://janbjorge.github.io/straum/deploy.html#kubernetes)
says what that means. `values.yaml` documents every value, and
`values.schema.json` refuses what straum can't take.

Each release publishes the chart at that version, pulling
`ghcr.io/janbjorge/straum:X.Y.Z`. The copy in this repository has
`appVersion` 0.0.0, which no release publishes, so installing it from
a checkout needs `image.tag`. `image.repository` names a mirror or an
image you built.

The DSN is a Secret named by `dsn.existingSecret`. The chart never
holds it. For a CA that is not one of the roots built in, such as
Amazon RDS or Cloud SQL, name the Secret in `rootCert.existingSecret`.
The chart mounts it at `/etc/straum-tls/ca.pem`, and the DSN names
that path with `sslrootcert`.

Needs Kubernetes 1.30 or newer. The pod's pause before SIGTERM is a
sleep action, on by default from then. The pod is ready once every
table is loaded and straum is within `maxLag` of PostgreSQL. A new
spec rolls the pod, and the new one loads before the old one stops.
`progressDeadlineSeconds`, an hour, is how long that load may take
before the rollout is called failed.

On an IPv6-only cluster set `listenAddress` to `[::]`. The chart
always passes `--listen`: on its own straum listens on `127.0.0.1`,
which no probe or Service reaches. straum has no authentication or
TLS, so any pod that reaches the Service can ask.

With `wal: physical`, set `maxPageMemory` well under
`resources.limits.memory`. The rows it bounds come on top of the
cells, and at the defaults, both `1Gi`, the pod is killed for its
memory before straum's own limit can stop it with exit 1.

`maxStreamMemory`, `1Gi` by default, bounds the commits straum holds
ahead of the writer, and the rows of one transaction. Past it while the
tables load, or in one transaction, straum exits 1; after the load, a
full queue waits for the writer. The queue and a transaction can each
hold this much, so up to twice `maxStreamMemory` comes on top of the
cells: keep that under `resources.limits.memory`.

`maxMemory`, unset by default, bounds straum's resident memory: past
it straum exits 1 with a line naming the limit, rather than the kernel
killing the pod without one. Set it a little under
`resources.limits.memory`, and above twice `maxStreamMemory` plus the
cells, and `maxPageMemory` too with `wal: physical`.

## Checks

`just chart` runs `helm lint --strict`, the
[helm-unittest](https://github.com/helm-unittest/helm-unittest) suites in
`tests/`, and [kubeconform](https://github.com/yannh/kubeconform) on what
the chart renders with `ci/values.yaml`. CI runs it too.
