# Logical replication, as weir would consume it

Read in `/Users/janbjorge/src/postgres` on 2026-11-08 (tree and tags: README.md). `git diff --stat 0f3b42ce806 HEAD` over `src/backend/replication`, `access/transam`, `storage/ipc`, `heapam.c`, `publicationcmds.c` and `protocol.sgml` empty → all citations upstream code.

## 1. Protocol

- **R** pgoutput options: `proto_version`, `publication_names` required; `binary`, `messages`, `streaming` (off/on/parallel), `two_phase`, `origin` (none/any) optional (`replication/pgoutput/pgoutput.c:290-432`). Unknown option → "unrecognized pgoutput option: %s" (421).
- **R** Versions: MIN=1, STREAM=2, TWOPHASE=3, STREAM_PARALLEL=4, MAX=4 (`src/include/replication/logicalproto.h:40-45`). `streaming=on` needs 2+, `two_phase` 3+, `streaming=parallel` 4 (`pgoutput.c:514-552`).
  - v2: xid on DML, Message, Truncate for streamed txns, plus Stream Start/Stop/Commit/Abort (`protocol.sgml:7251-7253`).
  - v3: Begin Prepare, Prepare, Commit Prepared, Rollback Prepared, Stream Prepare (`protocol.sgml:7428-7430`).
  - v4: abort LSN + timestamp on Stream Abort, only with `streaming=parallel` (`protocol.sgml:7402-7420`; `pgoutput.c:1901`).
  - `binary`, `messages` no version gate. Binary mode: type without typsend falls back to text (`replication/logical/proto.c:834`).
- **R** Message formats:
  - Begin = (final_lsn, commit_time, xid) (`proto.c:49-57`);
  - Commit = (flags, commit_lsn, end_lsn, commit_time) (`proto.c:78-92`);
  - TupleData column kinds n, u, t, b (`protocol.sgml:7796-7876`);
  - BEGIN deferred until change sent; empty txns emit nothing (`pgoutput.c:1593-1601`, 637-650).
- **R** Standby status update `r` carries write, flush, apply, clock, replyRequested (`protocol.sgml:2841-2901`; `walsender.c:2573-2577`). All three positions go to WalSnd, read by `pg_stat_replication` and sync rep (`walsender.c:2632-2643`). Only flush moves slot, via `LogicalConfirmReceivedLocation` (2652-2658).
- **R** `confirmed_flush` only moves forward (`replication/logical/logical.c:1930-1931`, 2019-2020).
- **R** WAL release not directly tied to `confirmed_flush`. `restart_lsn` moves to `candidate_restart_lsn` only once flush >= `candidate_restart_valid` (`logical.c:1956-1965`), then `ReplicationSlotsComputeRequiredLSN` runs (2008-2009). Candidate = restart_decoding_lsn of oldest in-progress txn in reorder buffer, or last serialized snapshot (`snapbuild.c:1252-1269`).
  - **I** One long-open txn anywhere in DB pins WAL even with current acks.
- **R** Reconnect: start_lsn below `confirmed_flush` silently forwarded to it (`logical.c:630-649`). Built-in subscriber acks Commit.end_lsn (`logical/worker.c:2582`).
- **R** Keepalive `k` = (walEnd, clock, replyRequested) (`protocol.sgml:2729-2768`).
  - Carries sentPtr, end of last decoded record (`walsender.c:3745`, 4463), not insert/flush end.
  - Reply-requested ping after `wal_sender_timeout/2` without reply (4498-4502).
  - Unsolicited keepalive when caught up and client flush + write below sentPtr (2025-2028).

**Design implication:** persist state + last applied end_lsn in one atomic write; ack flush only for persisted LSNs. Idle: ack keepalive LSN so slot advances (I). WAL retention set by oldest open txn, not our ack.

## 2. Publications (the spec maps onto these)

- **R** `publish` defaults to all four: insert, update, delete, truncate (`commands/publicationcmds.c:92-98`, 142-154). Other options: `publish_via_partition_root`, `publish_generated_columns` (none/stored) (157-170, 2312-2313).
- **R** Row filters (`publicationcmds.c:589-684`):
  - allowed: columns (not system), constants, built-in operators, immutable built-in functions, Bool/Case/Coalesce/MinMax/NullTest/BooleanTest/Array/Row/ScalarArrayOp nodes;
  - disallowed: user-defined types, operators, functions, collations. Error "invalid publication WHERE expression", detail e.g. "User-defined or built-in mutable functions are not allowed.";
  - why: filters run under historic snapshot (571-585);
  - WHERE on partitioned table needs `publish_via_partition_root` (731-738).
- **R** Column lists:
  - no system columns, no virtual generated columns (`catalog/pg_publication.c:724-730`);
  - not with schemas: "Column lists cannot be specified in publications containing FOR TABLES IN SCHEMA elements." (`publicationcmds.c:807-813`);
  - partitioned table needs pubviaroot (820-828);
  - differing column lists for one table across publications: "cannot use different column lists for table \"%s.%s\" in different publications" (`pgoutput.c:1192`).
- **R Replica identity coverage enforced when UPDATE/DELETE runs on primary, not at CREATE PUBLICATION.** `CheckCmdReplicaIdentity` (`executor/execReplication.c:1057-1157`) raises "cannot update table \"%s\"" (or "cannot delete from table") with one of:
  - "Column used in the publication WHERE expression is not part of the replica identity."
  - "Column list used by the publication does not cover the replica identity."
  - "Replica identity must not contain unpublished generated columns."
  - no RI while publishing updates: "cannot update table \"%s\" because it does not have a replica identity and publishes updates".
  - RI FULL skips row-filter column check (`publicationcmds.c:290-295`).
  - **I** Bad weir spec breaks app writes. Validate spec against replica identities before creating publication.
- **R** UPDATE transformed by row filter (`pgoutput.c:1268-1271`, 1441-1469):
  - old no, new no: dropped;
  - old no, new yes: sent as INSERT;
  - old yes, new no: sent as DELETE;
  - old yes, new yes: sent as UPDATE.
  - Only new tuple (RI unchanged) → only new evaluated (1355-1370). Unchanged toasted RI columns copied old → new before eval (1398-1421).
- **R** Filters across publications OR'ed per action (`pgoutput.c:1051-1053`). Any FOR ALL TABLES or TABLES IN SCHEMA publication → no filter (942-943). Filters and column lists don't apply to TRUNCATE (`doc/src/sgml/logical-replication.sgml:876`, 1460).
- **R** Temp and unlogged tables unpublishable (`pg_publication.c:118-128`). PG19 adds `EXCEPT TABLE` (`pg_publication.c:63-94`) and FOR ALL SEQUENCES (`publicationcmds.c:867`); see README.md, Target versions.
- **R** Row filters run in walsender (`pgoutput.c:1590`) → subset cut on primary.

**Design implication:** UPDATE crossing filter = row entering/leaving set, fits rollups directly. Filter columns must be in RI unless table uses RI FULL.

## 3. Old tuple content

- **R** `ExtractReplicaIdentity` (`access/heap/heapam.c:9357-9443`):
  - NOTHING: no old tuple (9372-9373);
  - FULL: whole old row, toasted values inlined (9375-9386);
  - DEFAULT/INDEX: only when key_required, then only RI columns, rest NULL (9411-9422);
  - UPDATE: key_required = RI column changed, or RI column has external toast (4115-4117); DELETE always true (3014).
- **R** On wire:
  - UPDATE sends `O` under FULL, else `K` if old tuple exists, else nothing (`proto.c:468-476`);
  - DELETE always sends `O` (FULL) or `K` (546-551);
  - `K` TupleData still lists every published column; non-key as `n` (`proto.c:782-812` with `heapam.c:9418-9419`);
  - Relation message flags key columns (`proto.c:949-967`) → client can tell "not sent" from real NULL.
- **R** DELETE without old tuple skipped at DEBUG1 (`pgoutput.c:1533-1536`); if table publishes deletes, that DELETE already errors on primary (`execReplication.c:1152-1157`).
- **R** `u` = unchanged on-disk toasted value, not sent (`proto.c:815-823`). Appears in new tuple; old `O` tuple flattened, always has values (`heapam.c:9381-9385`).

**Design implication (I):**
- DEFAULT RI: UPDATE of non-key columns carries no old values, DELETE only PK. sum/count deltas need PK-keyed store of (group keys, measures) = columnar replica of projected columns. Same store serves ad hoc GROUP BY.
- RI FULL: sum, count, avg (as sum + count) updatable from message alone, cost more WAL (`doc/src/sgml/config.sgml:3297-3301`).
- min/max need per-group multiset or row store either way, to survive deleting extreme.
- `u`: keep prior value. Only varlena above TOAST threshold affected.

## 4. Initial sync

- **R** `CREATE_REPLICATION_SLOT ... LOGICAL pgoutput (SNAPSHOT 'export'|'use'|'nothing')` defaults to export (`protocol.sgml:2411-2423`), returns `consistent_point` and `snapshot_name`. Exported snapshot valid "until a new command is executed on this connection or the replication connection is closed" (2455-2471).
- **R** Restrictions:
  - `export` errors inside txn block;
  - `use` must run in txn, REPEATABLE READ, read-only, first command, outside subtxn (`walsender.c:1332-1373`);
  - both set need_full_snapshot → temporarily pins data xmin plus catalog xmin (`logical.c:491-511`);
  - `SnapBuildInitialSnapshot` first waits for decoded-as-committed txns to leave procarray (`snapbuild.c:479-510`).
- **R** Built-in tablesync per table:
  1. `BEGIN READ ONLY ISOLATION LEVEL REPEATABLE READ`.
  2. Create slot with `CRS_USE_SNAPSHOT`, record LSN as origin start (`logical/tablesync.c:1413-1445`).
  3. COPY under that snapshot (1484-1486): plain `COPY tbl (cols) TO STDOUT`, or `COPY (SELECT cols FROM [ONLY] tbl WHERE f1 OR f2 ...) TO STDOUT` with filters (1109-1184).
  4. States INIT -> DATASYNC -> FINISHEDCOPY -> SYNCWAIT -> CATCHUP -> SYNCDONE -> READY (29-55).
  5. Leader applies table's changes only when READY, or SYNCDONE with statelsn <= txn's finish_lsn (`worker.c:718-721`).

**Design implication (I):** one slot with `SNAPSHOT 'export'`; N parallel sessions `SET TRANSACTION SNAPSHOT` then `COPY (SELECT projected cols WHERE filter) TO STDOUT`; then `START_REPLICATION` from `consistent_point`. Keep replication connection idle until all importers begun. Later-added tables need tablesync pattern: new slot + snapshot, ignore that table's changes with commit LSN below its snapshot LSN.

## 5. Durability and availability

- **R** Slot invalidation causes: WAL removed, catalog rows removed, wal_level insufficient, idle timeout (`src/include/replication/slot.h:58-69`).
  - `max_slot_wal_keep_size` default -1, unlimited (`utils/misc/guc_parameters.dat:2136-2143`);
  - `idle_replication_slot_timeout` default 0, off (1297-1304); only inactive slots, never synced slots on standby (`replication/slot.c:1877-1883`);
  - docs warn of unbounded WAL retention (`doc/src/sgml/logicaldecoding.sgml:390-400`).
- **R** Failover slots:
  - created with `CREATE_REPLICATION_SLOT ... FAILOVER` (`protocol.sgml:2429-2436`; `walsender.c:1253`);
  - standby needs `sync_replication_slots`, `primary_slot_name`, `hot_standby_feedback`, dbname in `primary_conninfo` (`logicaldecoding.sgml:415-427`);
  - slotsync worker copies slots periodically, never moves backward, keeps slot RS_TEMPORARY until safe (`logical/slotsync.c:15-39`);
  - only synced, persistent slots usable after promotion (`logicaldecoding.sgml:486-497`).
- **R** `synchronized_standby_slots`: walsender with failover slot won't send WAL past what listed physical standbys confirmed (`walsender.c:1874-1890`; `logicaldecoding.sgml:428-434`).
- **R** Logical decoding on standby works with `hot_standby_feedback` + physical slot. Slots invalidated if needed catalog rows removed or primary's effective wal_level drops (`logicaldecoding.sgml:361-377`). Cascading walsender decodes only to replay LSN (`walsender.c:3763-3764`).
- **I** Consumer failover:
  - no failover slot: full resync;
  - synced slot: its confirmed_flush trails our real position → applied txns re-sent; dedupe on end_lsn;
  - no `synchronized_standby_slots`: may hold txns new primary lost; only resync fixes. Docs also warn of old primary returning during promotion (`logicaldecoding.sgml:507-515`).

**Design implication:** set `max_slot_wal_keep_size`, treat slot invalidation as resync path. Use `failover=true` with `synchronized_standby_slots`, make apply idempotent by LSN.

## 6. Large transactions

- **R** `logical_decoding_work_mem` default 64MB (`config.sgml:2160-2169`; boot value 65536 kB at `guc_parameters.dat:1938-1943`).
- **R** Over limit: largest top-level txn streamed if streaming on; else largest (sub)txn spilled to `pg_replslot/<slot>/xid-*-lsn-*.spill` (`logical/reorderbuffer.c:4033-4107`, 5067). Streaming also needs consistent snapshot and no re-decode in progress (4453-4471).
- **R** `streaming=off`: txn sent only at commit. Decoding one sequential loop (`walsender.c:3729-3745`) → **I** big commit delays every later commit on slot. `streaming=on`: chunks arrive pre-commit; consumer buffers until Stream Commit or discards on Stream Abort (`protocol.sgml:3689-3695`).

**Design implication:** `streaming=on` with local staging, apply at Stream Commit. Big txn freshness bounded by own decode + transfer time either way.

## 7. Schema changes

- **R** DDL not replicated (`logical-replication.sgml:2351-2361`).
- **R** Relation message re-sent after any relcache invalidation of table (schema_sent reset at `pgoutput.c:2128`). Diff it, don't assume schema changed. Type messages only for type OIDs >= FirstGenbkiObjectId (816-830).
- **R** Table rewrites not decoded as DML: rewrite inserts use `HEAP_INSERT_NO_LOGICAL` (`access/heap/rewriteheap.c:628`; `replication/logical/decode.c:936-938`).
- **R** TRUNCATE has own `T` message with CASCADE and RESTART IDENTITY bits (`protocol.sgml:7195-7248`), gated by pubtruncate (`pgoutput.c:1688`).
- **R** Sequences: incremental changes not replicated (`logical-replication.sgml:2366-2368`). PG19 adds FOR ALL SEQUENCES + sequence-sync worker started by REFRESH SEQUENCES (1772-1811), pulls current values via `pg_get_sequence_data` (`logical/sequencesync.c:326-341`), never applies stream changes (`worker.c:723-725`).

**Design implication:** each Relation message: compare columns + types; incompatible change → stop, resync table. TRUNCATE = reset table state.

## 8. Consistency and freshness

### 8a. Commit order is not visibility order

- **R** `RecordTransactionCommit` inserts commit record (`access/transam/xact.c:1488`), flushes (1548), sets clog (1560), waits sync rep (1609). `ProcArrayEndTransaction` runs later in `CommitTransaction` (2446). Re-read by hand 2026-11-08.
- **R** `ProcArrayEndTransaction` takes ProcArrayLock conditionally or uses group clearing (`storage/ipc/procarray.c:680-686`). No lock spans WAL insert and procarray removal.
- **R** `snapbuild.c:478-484`: transactions "appear in snap->xip as soon as their commit WAL records are decoded, but that doesn't imply they can be considered committed in a regular MVCC snapshot: that happens only when they remove themselves from procarray." Also `snapbuild.c:1384-1387`: txns listed running "might already have inserted their commit record". `access/transam/README:246-257` only guarantees consistency within a snapshot.
- **I** Stream prefix up to LSN L not always equal to some primary snapshot: snapshot can see T2 (commit LSN 200) but not T1 (commit LSN 100). Usable bound holds: any txn visible to snapshot has commit record inserted before any WAL record inserted after snapshot taken.

### 8b. Asking "has weir applied up to LSN X?"

- **R** `pg_current_wal_lsn` returns write pointer, `pg_current_wal_insert_lsn` insert pointer, `pg_current_wal_flush_lsn` flush pointer (`access/transam/xlogfuncs.c:304-356`).
- **R** `XactLastCommitEnd` backend-local C variable (`xact.c:1612`; `src/include/access/xlog.h:35`), not exposed to SQL.
- **R** WAIT FOR LSN from PG19 (`parser/gram.y:16720-16727`), modes standby_replay, standby_write, standby_flush, primary_flush (`commands/wait.c:77-84`; `src/include/access/xlogwait.h:38-44`). Waiters woken only by walreceiver, startup process, local WAL flush (`walreceiver.c:1074,1119`; `xlog.c:682,3009,3198`; `xlogrecovery.c:1804`). **Walsender never wakes them → WAIT FOR LSN can't track logical consumer.**
- **R** Logical walsender decodes only WAL at/below flush pointer (`walsender.c:3763-3771`). **I** `synchronous_commit=off` → decoding lags until WAL writer flushes, every `wal_writer_delay` (default 200ms, `guc_parameters.dat:3627`).

### 8c. pg_logical_emit_message as a barrier

- **R** Signature (transactional, prefix, message, flush default false), returns pg_lsn (`src/include/catalog/pg_proc.dat:11827-11836`). Non-transactional message delivered as soon as decoder reads it (`logical/message.c:17-23`). `flush=true` forces `XLogFlush` (`message.c:78-79`, re-read by hand).
- **R** Sent only if `messages` option on; not filtered by publication, only by database and origin (`pgoutput.c:1732-1733`; `decode.c:628-638`).
- **I** Barrier protocol: querying backend (or weir over own connection) runs `SELECT pg_logical_emit_message(false, 'weir-barrier', nonce, true)`, gets L. weir waits until applied up to marker, then answers. Commits arrive in commit-LSN order → everything visible to backend's snapshot applied. weir may also hold txns in WAL but not yet visible on primary.
- **I** Not covered by barrier → fall back to native plan: txn's own uncommitted writes; REPEATABLE READ and SERIALIZABLE txns (snapshot older than barrier).
- Prior art: Materialize real-time recency, TiFlash read index (COMPETITORS.md).

### 8d. weir as a synchronous standby

- **R** Supported. Docs: "A logical replication subscription can be a standby for synchronous replication" (`logical-replication.sgml:242-246`); clients must send standby status updates (`logicaldecoding.sgml:1443-1453`).
- **R** `StartLogicalReplication` calls `SyncRepInitConfig` (`walsender.c:1604`, re-read by hand). Priority from matching application_name (`replication/syncrep.c:886-887`). Candidate must be streaming with valid flush position (796-810).
- **R** Commit waits after clog set, while txn still running in procarray (`xact.c:1605-1609`). Txns with nothing to publish get keepalive so released (`walsender.c:1816-1820`).
- **R** Caveats:
  - wait only when `synchronous_commit` > local (`src/include/replication/syncrep.h:18-19`), changeable per session;
  - cancelled wait leaves txn committed locally: "The transaction has already committed locally, but might not have been replicated to the standby." (`syncrep.c:331-332`);
  - docs: works properly only with single database (`logicaldecoding.sgml:1457-1461`);
  - deadlock possible if txn locks catalog tables exclusively (1470-1476).
- **I** `synchronous_commit=remote_apply` + weir reporting apply only once queryable → every txn visible on primary already queryable in weir. Costs: commit latency + decode + network + apply; no streaming → big txn stalls own commit; weir outage blocks all writes unless `synchronous_standby_names` uses ANY quorum with other standbys; unrelated txns also wait for walsender round trip.
- **R** `synchronized_standby_slots` doesn't affect commit waits; only gates failover-slot walsenders (§5).

**Design implication:** default async + emit_message(flush=true) barrier for read-your-writes reads, one round trip per consistent read. Sync rep with `remote_apply` = opt-in strict mode, ANY quorum so weir outage doesn't stop writes.

## 9. Overhead on the primary

- **R** `wal_level=logical` logs everything replica does + decoding info; WAL grows most with RI FULL (`config.sgml:3295-3301`). Specific costs:
  - prefix/suffix compression for same-page UPDATEs off for logically logged tables (`heapam.c:9079-9084`);
  - new tuple data kept even with full-page image (`REGBUF_KEEP_DATA`, `heapam.c:2147`, 9162);
  - catalog changes emit extra NEW_CID records (`heapam.c:2115-2116`).
- **R** PG19: `wal_level=replica` auto-raised to logical while any logical slot exists, lowered lazily by checkpointer (`logical/logicalctl.c:5-38`; `config.sgml:12423-12429`).
- **R** Each walsender reads + decodes WAL itself, own reorder buffer (`walsender.c:3729-3745`). Each slot pins own restart_lsn and catalog_xmin; server keeps minimum (`logical.c:2001-2009`).
- **I** N slots ≈ N× decode CPU and WAL reads.
- **R** Data point: TimescaleDB tried WAL-based invalidation for continuous aggregates in 2.22, retired in 2.23 because it "did not meet our IOPS reduction goals", removed in 2.25.0 ([release](https://github.com/timescale/timescaledb/releases/tag/2.25.0)).

**Design implication:** one slot covering all spec'd tables, RI FULL only where needed, cap `max_slot_wal_keep_size`, monitor `pg_replication_slots`.
