# ADR-0012: Straum ships as one image, from a release

Status: Accepted 2026-11-10; revised 2026-11-25

## Context

Managed PostgreSQL requires TLS. OpenSSL and `ring` need a C compiler,
which the static musl image avoids, and libpq's `sslmode` has modes
that silently skip verification. A chart usually brings an HPA, a PDB
and an Ingress, but each Straum replica makes its own slot and loads
the whole spec, sharing nothing. Image and binary must be what CI
built and checked, traceable to one commit, and an image per commit
to `main` was an unversioned stream users pulled by default.

## Decision

TLS is `rustls` 0.23 on `rustls-graviola`, pure Rust. `sslmode=disable`
is plain TCP; `require`, `verify-ca`, `verify-full` and no `sslmode`
all verify the chain and host name; `allow` and `prefer` are refused.
Roots are `webpki-roots` or the PEM `sslrootcert` names, read at each
connect.

The image is the static musl binary from `rust:1.100.0-alpine` in
`gcr.io/distroless/static-debian13:nonroot`, both pinned by digest,
running as user 65532 on a read-only root. The chart,
`deploy/helm/straum` (Helm 4, API v2), is a Deployment, spec ConfigMap,
DSN Secret, optional CA bundle Secret, ClusterIP Service and
ServiceAccount under the Restricted Pod Security Standard, pinning no
user so OpenShift's `restricted-v2` admits it; no PDB, HPA, Ingress or
NetworkPolicy. `scripts/check-chart.py` runs `helm lint --strict`,
helm-unittest and kubeconform on the oldest Kubernetes the chart
takes, read from its `kubeVersion`, and on 1.36. The chart requires
1.30 or newer: its preStop pause is a sleep action, on by default from
1.30 and stable from 1.34.

Besides the chart, `deploy/` holds two more ways to run the same
image, chosen to cover where long-running services run: Docker
Compose on one host, and an Amazon ECS Fargate task definition with
an image on top of the release's for the spec and CA roots. They are
files to copy, like the chart, and Straum runs no tool over them.
Cloud Run and Azure Container Apps get none: they throttle or stop
an instance between requests, which stalls the stream.

The name is Straum, Nynorsk for a stream, everywhere: crates, binary,
`STRAUM_DSN`, default role and publication, slot prefix,
`application_name`, chart.

The maintainer publishes a release on the release page with a new tag
`vX.Y.Z` on `main`, a pre-release exactly when it has a suffix such as
`-rc.1`. A draft or a push to `main` publishes nothing. The tag is the
version: `Cargo.toml` and the chart's `version` and `appVersion` stay
`0.0.0`, and `straum version` prints what CI passes. CI checks the
tagged commit, fails unless the tag is well formed, on `main` and its
release has no assets, then publishes the image as `X.Y.Z`, `X.Y` and,
for the highest unsuffixed tag, `latest`. It copies the amd64 and arm64
binaries out of that image, checks the amd64 one prints the version,
attests both and attaches them with `SHA256SUMS`. It pushes the chart,
both versions `X.Y.Z`, to `oci://ghcr.io/janbjorge/charts/straum` and
attests it. The chart's image defaults to `ghcr.io/janbjorge/straum`;
installed from the repository, with `appVersion` 0.0.0, it refuses to
render without a tag.

## Consequences

- graviola needs AES, AVX, AVX2, ADX, BMI1/2 and PCLMULQDQ on x86_64,
  or AES, NEON, PMULL and SHA2 on aarch64, else it panics and aborts.
- No DSN is weaker than libpq's `verify-full`. The chart mounts a CA
  bundle from a Secret for a chain needing `sslrootcert`, as Amazon
  RDS's and Cloud SQL's do.
- One replica: more throughput is a bigger pod. The binary is its own
  client in the pod (ADR-0011). The AKS test cluster never runs it.
- `rg -i straum` finds every rename site. Trademarks are unchecked, so
  no crate is published.
- `latest` is the newest stable release; `main` reaches an image only
  through one. A binary is the bytes its image runs, checked with
  `sha256sum -c` and `gh attestation verify`.
- GHCR makes a new package private, so the maintainer makes
  `charts/straum` public once, after the first release that pushes it.
- Immutable releases can't be on (they refuse assets, and drafts start
  no workflow); a ruleset on `v*` tags stops a tag moving. A release
  publishes once; a re-run with assets fails.
- Binaries are Linux musl only. Agents don't make releases.
- `bench/demo.py` runs the image tagged `latest`, the newest release,
  which the README's demo runs; `just demo --tag X.Y.Z` runs another.
  No CI job runs it, so each release runs `just demo` once.
