# Decisions

One record per subject; each says what holds today (ADR-0001).

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
| [0010](0010-every-bucket-stays-in-memory.md) | Every bucket in memory, no spill |
| [0011](0011-operators-read-plain-text-through-the-binary.md) | DSN, help, status, views, shell |
| [0012](0012-straum-ships-as-one-image-from-a-release.md) | TLS, image, chart, name, releases |
| [0013](0013-code-reads-plainly-and-clippy-holds-it-there.md) | Code style, lints, the one `unsafe` |
| [0014](0014-the-guide-is-one-path-of-fixed-pages.md) | The guide's pages, order and what each owns |
| [0015](0015-straum-writes-the-copy-and-psql-runs-it.md) | Copying a spec's tables for a try |
| [0016](0016-a-mysql-source-is-the-test-of-every-source-boundary.md) | The MySQL test for the source port |
| [0017](0017-triggers-send-changes-through-notify.md) | Trigger capture, its SQL, delivery through NOTIFY |
| [0018](0018-triggers-write-changes-to-an-outbox.md) | Delivery through an outbox table |

## Numbers before 2026-11-16

Until 2026-11-16 each decision had its own record, 0001 to 0057. Old
commit messages cite those numbers; this table maps them to the record
that now holds the decision. `git show 8d41e60:docs/adr/` lists the
old files.

| Old | Now | Old | Now | Old | Now | Old | Now |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 0001 | 0001 | 0002 | 0003 | 0003 | 0002 | 0004 | 0004 |
| 0005 | 0004 | 0006 | 0005 | 0007 | 0005 | 0008 | 0002 |
| 0009 | 0002 | 0010 | 0007 | 0011 | 0012 | 0012 | 0007 |
| 0013 | 0006 | 0014 | 0007 | 0015 | 0009 | 0016 | 0003 |
| 0017 | 0004 | 0018 | 0004 | 0019 | 0004 | 0020 | 0009 |
| 0021 | 0011 | 0022 | 0012 | 0023 | 0011 | 0024 | 0007 |
| 0025 | 0011 | 0026 | 0012 | 0027 | 0013 | 0028 | 0004 |
| 0029 | 0004 | 0030 | 0006 | 0031 | 0007 | 0032 | 0012 |
| 0033 | 0010 | 0034 | 0010 | 0035 | 0010 | 0036 | 0012 |
| 0037 | 0004 | 0038 | 0006 | 0039 | 0011 | 0040 | 0011 |
| 0041 | 0003 | 0042 | 0006 | 0043 | 0003 | 0044 | 0003 |
| 0045 | 0007 | 0046 | 0007 | 0047 | 0007 | 0048 | 0003 |
| 0049 | 0008 | 0050 | 0005 | 0051 | 0007 | 0052 | 0008 |
| 0053 | 0013 | 0054 | 0013 | 0055 | 0005 | 0056 | 0006 |
| 0057 | 0008 |  |  |  |  |  |  |
