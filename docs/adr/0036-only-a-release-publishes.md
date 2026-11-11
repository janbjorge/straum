# ADR-0036: Only a release publishes

Status: Accepted, 2026-11-11

## Context

ADR-0032 published `latest` from every commit to `main` and versioned
images from releases. With releases now made, an image per commit is a
second, unversioned stream that users pull by default, ahead of what
was released. Straum ships as an image and, outside containers, as a
Linux binary; both must be what CI built and checked.

## Decision

The maintainer publishes a release on the release page with a new tag
`vX.Y.Z` on `main`, marked pre-release exactly when the tag has a
suffix such as `-rc.1`. Publishing starts CI; a draft starts nothing.
A push to `main` runs every check and publishes nothing. The tag is
the version: `Cargo.toml` stays `0.0.0`, CI passes the version to the
build, and `straum version` prints it.

CI runs every check on the tagged commit and fails the release unless
the tag is well formed, on `main` and its release has no assets yet.
It then publishes the image as `X.Y.Z` and `X.Y`, and as `latest` when
the tag is the highest without a suffix. It copies the amd64 and arm64
binaries out of that image, checks the amd64 one prints the version,
attests the binaries and the image, and attaches the binaries and
`SHA256SUMS` to the release.

## Consequences

- `latest` is the newest stable release: a fix to an older line or a
  pre-release leaves it.
- A change on `main` reaches an image only through a release; trying
  it before then means building it.
- A binary is the bytes its image runs: one build, not two.
- `sha256sum -c SHA256SUMS` checks a download, and `gh attestation
  verify` checks it came from this repository's CI.
- GitHub's immutable releases can't be on: they refuse assets after
  publishing, and drafts trigger no workflow. A ruleset on `v*` tags
  stops a tag moving instead; it is a setting, not code.
- A release publishes once: with assets attached, a re-run fails, as a
  new build would move `X.Y.Z` off the digest the notes name.
