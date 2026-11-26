# Decisions

One record per subject; each says what holds today (ADR-0001). Numbers
are never reused, so a dropped record leaves a gap.

| Record | Subject |
| --- | --- |
| [0001](0001-decisions-are-recorded-by-subject.md) | How decisions are recorded |
| [0002](0002-crates-depend-inward-around-a-synchronous-core.md) | Crates, the synchronous core, the store lock |
| [0003](0003-straum-holds-cells-and-nothing-else.md) | Cells, cell state and its bytes |
| [0004](0004-specs-are-a-strict-postgresql-create-view-subset.md) | Specs: Straum SQL, buckets, types |
| [0005](0005-asks-are-straum-sql-with-parameters.md) | Asks, answers, the 1 ms target |
| [0006](0006-refs-are-bound-at-ask-time.md) | Joins and bound cells |
| [0007](0007-postgresql-is-the-only-source.md) | The source: replication, load, versions |
| [0008](0008-straum-reads-physical-wal-where-logical-decoding-is-off.md) | The physical stream |
| [0009](0009-a-change-that-cannot-apply-stops-straum.md) | Failing hard, lag |
| [0010](0010-every-bucket-stays-in-memory.md) | Every bucket in memory unless kept for a time, no spill |
| [0011](0011-operators-read-plain-text-through-the-binary.md) | DSN, help, status, views, shell |
| [0012](0012-straum-ships-as-one-image-from-a-release.md) | TLS, image, chart, name, releases |
| [0013](0013-code-reads-plainly-and-clippy-holds-it-there.md) | Code style, lints, the one `unsafe` |
| [0014](0014-the-guide-is-one-path-of-fixed-pages.md) | The guide's pages, order and what each owns |
| [0015](0015-straum-writes-the-copy-and-psql-runs-it.md) | Copying a spec's tables for a try |
| [0016](0016-a-mysql-source-is-the-test-of-every-source-boundary.md) | The MySQL test for the source port |
| [0018](0018-triggers-write-changes-to-an-outbox.md) | Trigger capture, its outbox, one Straum per outbox |
| [0019](0019-orchestrators-control-straum-through-flags-environment-probes-and-signals.md) | Flags and variables, stop, probes, what orchestrators get |
