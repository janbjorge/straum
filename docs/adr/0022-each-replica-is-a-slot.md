# ADR-0022: Each replica is a slot, so Straum doesn't scale out

Status: Accepted (retroactive), 2026-11-10

## Context

A Kubernetes chart usually brings an HPA, a PDB and an Ingress. Each
Straum replica makes its own slot and walsender and loads the whole
spec, so two replicas cost PostgreSQL twice and share nothing.

## Decision

The image is the static musl binary built in `rust:1.99.1-alpine`,
copied into `gcr.io/distroless/static-debian13:nonroot`, both pinned by
digest; it runs as user 65532 with a read-only root filesystem. The
chart, `deploy/helm/straum` (Helm 4, chart API v2), is a Deployment,
spec ConfigMap, DSN Secret, ClusterIP Service and ServiceAccount under
the Restricted Pod Security Standard. No PDB, HPA, Ingress or
NetworkPolicy. `scripts/check-chart.py` runs `helm lint --strict`,
helm-unittest and kubeconform on Kubernetes 1.33 and 1.36.

## Consequences

- One replica. More throughput is a bigger pod, not more pods.
- The image has no shell or curl, so the binary is its own client:
  `kubectl exec deployment/straum -- /straum status` (ADR-0023).
- A CA bundle mount is missing (ADR-0011).
- The AKS test cluster is never used to try the chart.
