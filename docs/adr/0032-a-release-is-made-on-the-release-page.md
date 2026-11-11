# ADR-0032: A release is made on the release page

Status: Superseded by ADR-0036

## Context

Straum ships as an image, and people who run it outside containers need
a Linux binary. Both must be what CI built and checked, and traceable
to one commit. The maintainer wants to cut a release from GitHub's
release page, tag and notes in one place, without a commit to bump a
version.

## Decision

The maintainer publishes a release on the release page with a new tag
`vX.Y.Z` on `main`, marked pre-release exactly when the tag has a
suffix such as `-rc.1`. Publishing starts CI; a draft starts nothing.
The tag is the version: `Cargo.toml` stays `0.0.0`, CI passes the
version to the build, and `straum version` prints it. A build from
`main` prints `git describe`, such as `0.1.0-3-gabc1234`.

CI runs every check on the tagged commit and fails the release unless
the tag is well formed, on `main` and its release has no assets yet.
It then publishes the image as `X.Y.Z` and `X.Y`, copies the amd64 and
arm64 binaries out of that image, checks the amd64 one prints the
version, attests the binaries and the image, and attaches the binaries
and `SHA256SUMS` to the release. `latest` stays the newest `main`.

## Consequences

- A binary is the bytes its image runs: one build, not two.
- `sha256sum -c SHA256SUMS` checks a download, and `gh attestation
  verify` checks it came from this repository's CI.
- GitHub's immutable releases can't be on: they refuse assets after
  publishing, and drafts trigger no workflow. A ruleset on `v*` tags
  stops a tag moving instead; it is a setting, not code.
- A release is out for minutes before its binaries; one that fails CI
  stays without them, to delete with its tag.
- A release publishes once: with assets attached, a re-run fails, as a
  new build would move `X.Y.Z` off the digest the notes name.
- The binaries are Linux musl only; other systems use the image.
- AGENTS.md no longer bans tags; agents still don't make releases.
