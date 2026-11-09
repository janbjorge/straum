# How the PostgreSQL planner could hand aggregates to Straum

Read in the PostgreSQL tree 2026-11-08 (see README.md for tree, tags). `git diff --stat 0f3b42ce806 HEAD` over `src/backend/optimizer`, `contrib/postgres_fdw`, `src/include/optimizer`, `src/include/foreign`, `src/include/access/tableam.h`, `executor/nodeAgg.c` empty → stock upstream behaviour.

## 1. FDW upper-rel pushdown

- **R** API: `typedef void (*GetForeignUpperPaths_function)(PlannerInfo *root, UpperRelationKind stage, RelOptInfo *input_rel, RelOptInfo *output_rel, void *extra);` (`src/include/foreign/fdwapi.h:62`). Stages: `pathnodes.h:142-154`.
- **R** Call sites in `optimizer/plan/planner.c`: FINAL 2467, GROUP_AGG 4453, WINDOW 4893, DISTINCT 5134, PARTIAL_DISTINCT 5309, ORDERED 5784, PARTIAL_GROUP_AGG 7979.
  - Each gated on `output_rel->fdwroutine`, copied from `input_rel` (4243, 7717).
  - Base rel gets `fdwroutine` only if foreign table (`optimizer/util/plancat.c:544-550`).
  - Join rel gets it only if both sides same server (`relnode.c:733-782`).
  - Docs: "called only if all base relation(s) ... belong to the same FDW" (`fdwhandler.sgml:404-405`).
- **R** postgres_fdw: `postgresGetForeignUpperPaths` (`contrib/postgres_fdw/postgres_fdw.c:8086`).
  - Handles only GROUP_AGG, ORDERED, FINAL (8101-8105); input must be `pushdown_safe` (8096).
  - `add_foreign_grouping_paths` (8139) asserts patype NONE or FULL (8158), adds one ForeignPath via `add_path` (8211-8224).
  - `add_foreign_final_paths` (8373, LIMIT): SELECT only (8395), only with LIMIT or rowMarks (8402), no SRFs (8406).
- **R** Pushdown blockers in `foreign_grouping_ok` (7846):
  - grouping sets (7858);
  - any local_conds on scan rel (7869);
  - unshippable GROUP BY expr (7908) or foreign param (7915);
  - unshippable aggregate (7956).
  - HAVING split into remote + local conds (7986-8012); aggregates in local HAVING must still ship (8045).
- **R** `is_foreign_expr` (`deparse.c:247`):
  - unsafe collation: not pushable (281);
  - `contain_mutable_functions`: not pushable (291) → STABLE functions block too, not just VOLATILE. `date_trunc(text, timestamptz)` STABLE (`src/include/catalog/pg_proc.dat:2609`); `date_trunc(text, timestamptz, text)` and `date_bin(interval, timestamptz, timestamptz)` immutable (2613, 6481).
  - Aggref (962): upper rel only (968); `agg->aggsplit != AGGSPLIT_SIMPLE` not pushable (972); `aggfnoid` must be shippable (976); aggorder sortop, FILTER, input collation checked.
- **R** `is_shippable` (`shippable.c:163`): built-ins (OID < `FirstGenbkiObjectId`, 153-155) always ship. Others ship only if in extension listed in server option `extensions` (`option.c:255`; `postgres_fdw.c:7597`; `getExtensionOfObject` at `shippable.c:125`).
- **R Partial aggregates not pushed.** `UPPERREL_PARTIAL_GROUP_AGG` ignored (8101), split Aggrefs rejected (`deparse.c:972`). `rg -i "PARTIAL_AGGREGATE|aggpartialfn|partial_aggregate_support"` over src, contrib, doc finds nothing.
  - Regression proof (`postgres_fdw.out:11416-11434`): GROUP BY non-partition key → Finalize GroupAggregate -> Merge Append -> Partial GroupAggregate -> Foreign Scan; raw rows shipped.
  - GROUP BY partition key → full per-partition pushdown (11347-11361). Needs `enable_partitionwise_aggregate`, default false (`utils/misc/guc_parameters.dat:992-996`).
  - History: "Partial aggregates push down" returned with feedback ([CF #4019](https://commitfest.postgresql.org/patch/4019/)). Committers objected serial/deserial functions not designed portable across versions (Tomas Vondra, [2024-08-21](https://postgresql.org/message-id/982a903e-cb59-4b60-911a-bdbb88da594e%40vondra.me); Tom Lane, [2024-03-19](https://www.postgresql.org/message-id/4020145.1710883747%40sss.pgh.pa.us)).

**Design implication:** postgres_fdw pushes whole aggregates only, only when every base rel foreign on one server. Local table never reaches `GetForeignUpperPaths`. Custom FDW (not postgres_fdw) can serve `UPPERREL_PARTIAL_GROUP_AGG`; core calls it at `planner.c:7974-7983`.

## 2. What a fake server needs to be a postgres_fdw remote

- **R** Connect: non-superuser must really password-auth unless user mapping sets `password_required=false` (`connection.c:460-500`). `PQserverVersion` from `server_version` ParameterStatus (libpq `fe-exec.c:1158`). Report ≥150000, else rescan uses `MOVE BACKWARD` (`postgres_fdw.c:1912`). TABLESAMPLE needs ≥90500 (5510).
- **R** Session setup over simple-query protocol (`PQsendQuery`, `connection.c:875`), `configure_remote_session` (`connection.c:828-860`):
  ```
  SET search_path = pg_catalog
  SET timezone = 'GMT'
  SET datestyle = ISO
  SET intervalstyle = postgres
  SET extra_float_digits = 3
  ```
- **R** Transactions, `begin_remote_xact` (`connection.c:914`):
  - start: `START TRANSACTION ISOLATION LEVEL {SERIALIZABLE|REPEATABLE READ}[ READ ONLY][ DEFERRABLE]` (951-959); never READ COMMITTED;
  - also `SET TRANSACTION READ ONLY` (988), `SAVEPOINT sN[; SET TRANSACTION READ ONLY]` (1019);
  - commit: `COMMIT TRANSACTION`, or multi-statement `SET TRANSACTION READ ONLY; COMMIT TRANSACTION` (124-126);
  - abort: `ABORT TRANSACTION` or `ROLLBACK TO SAVEPOINT sN; RELEASE SAVEPOINT sN` (134-137);
  - also `RELEASE SAVEPOINT sN` (1414), `DEALLOCATE ALL` (1282), cancel requests (1611).
- **R** Scans:
  - `DECLARE cN CURSOR FOR\n<sql>` over extended protocol (`PQsendQueryParams`, paramTypes NULL, text results) at `postgres_fdw.c:4082-4093`;
  - `FETCH <n> FROM cN` over simple protocol (4158); `fetch_size` default 100 (780);
  - `CLOSE cN` (4266, 1907);
  - values parsed with type input functions (9012) → text must be exact PostgreSQL output format. E.g. `avg(int)` returns numeric.
- **R** DML uses `PQsendPrepare`/`PQsendQueryPrepared` (4539/4470). Not needed if read-only.
- **R** `use_remote_estimate` (default off, 776) sends `EXPLAIN <select>` (3470), parses `(cost=%lf..%lf rows=%lf width=%d)` (3950).
- **R** ANALYZE (optional):
  - page count: `SELECT pg_catalog.pg_relation_size('<rel>'::pg_catalog.regclass) / 8192` (`deparse.c:2643`);
  - stats: `SELECT relpages, reltuples, relkind, relhassubclass FROM pg_catalog.pg_class WHERE oid = '<rel>'::pg_catalog.regclass` (`deparse.c:2668`);
  - sample cursor with `TABLESAMPLE BERNOULLI(p)`, `SYSTEM(p)` or `WHERE pg_catalog.random() < f` (`deparse.c:2763-2771`); auto picks BERNOULLI (5510-5513);
  - new PG19: option `import_stats` + `ImportForeignStatistics` (`fdwapi.h:160`; `postgres_fdw.c:5696`), reads `pg_catalog.pg_stats` (6054).
- **R** IMPORT FOREIGN SCHEMA (optional; skip via manual CREATE FOREIGN TABLE): `SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = ...` (6570), then pg_class/pg_attribute/pg_attrdef/pg_collation join (6604-6684).
- **R** Remote SQL from `postgres_fdw.out`:
  - 3622: `SELECT count(c6), sum("C 1"), avg("C 1"), min(c2), max("C 1"), stddev(c2), c2 FROM "S 1"."T 1" WHERE ((c2 < 5)) GROUP BY 7 ORDER BY count(c6) ASC NULLS LAST, sum("C 1") ASC NULLS LAST`
  - 3642: same + `LIMIT 1::bigint`
  - 3835: `SELECT c2, sum("C 1") FROM "S 1"."T 1" GROUP BY 1 HAVING ((avg("C 1") < 500::numeric)) AND ((sum("C 1") < 49800)) ORDER BY c2 ASC NULLS LAST`
  - 4026: `SELECT sum("C 1") FILTER (WHERE (("C 1" < 100) AND (c2 > 5))), c2 FROM "S 1"."T 1" GROUP BY 2 ORDER BY ...`
  - not pushed: `random()` inside aggregate (3651-3661); unshippable GROUP BY (3765-3778).
- **R** Greenplum once failed as postgres_fdw remote on exactly these statements ([mail](https://www.postgresql.org/message-id/tencent_3C49A923328D53C12A46570C@qq.com)).

**Design implication:** server needs wire protocol with extended-protocol cursors, multi-statement simple queries, PostgreSQL-dialect SQL subset (ordinal GROUP BY, `::` casts, NULLS LAST, FILTER). Output must use PostgreSQL text formats + result types.

## 3. Transparent interception

- **R** Hooks (typedef, call site, what extension sees):
  - `planner_hook`: `planner.h:28`, called `planner.c:344`. Gets (Query*, query_string, cursorOptions, boundParams, ExplainState*). Can rewrite Query or return own PlannedStmt.
  - `planner_setup_hook` / `planner_shutdown_hook` (PG19): `planner.h:36/44`, called `planner.c:538/735`.
  - `post_parse_analyze_hook`: `analyze.h:22`, gets (ParseState*, Query*, const JumbleState*). Called `analyze.c:142/184/222`, plus explain.c, createas.c, portalcmds.c. Can mutate Query, not cost-based (I).
  - `get_relation_info_hook` **gone**. Commit `91f33a2ae92` "Replace get_relation_info_hook with build_simple_rel_hook". `build_simple_rel_hook` at `pathnode.h:21`, called `relnode.c:412` with (root, rel, rte). PG19+; PG18 still has `get_relation_info_hook`. Code building on both should use neither.
  - `set_rel_pathlist_hook`: `paths.h:39`, called `allpaths.c:589`. Base rels only; may add/delete paths (comment 583-587).
  - Join-level: `join_path_setup_hook` `joinpath.c:177` (PG19); `set_join_pathlist_hook` `joinpath.c:382`; `joinrel_setup_hook` `relnode.c:1018/1179` (PG19).
  - `create_upper_paths_hook`: `planner.h:50`, same signature as `GetForeignUpperPaths`, fires every stage, FDW or not: SETOP `prepunion.c:1477`; PARTIAL_GROUP_AGG `planner.c:7986`; GROUP_AGG 4458; WINDOW 4898; PARTIAL_DISTINCT 5316; DISTINCT 5141; ORDERED 5789; FINAL 2472.
  - `ExecutorStart/Run/Finish/End`: `executor.h:77-92`, called `execMain.c:136/311/419/479`. See finished plan; too late for cost-based choice.
  - `ProcessUtility_hook`: `utility.h:71`, called `utility.c:520`. Utility statements only.
- **R** GROUP_AGG flow in `create_grouping_paths` (`planner.c:4087`):
  1. `make_grouping_rel`. Top grouped_rel has relids NULL (4219), fdwroutine NULL for local table.
  2. Native Agg paths built.
  3. ERROR "could not implement GROUP BY" if none (4441-4445). Hook can't rescue.
  4. FDW callback (4451).
  5. `create_upper_paths_hook(UPPERREL_GROUP_AGG, input_rel, grouped_rel, GroupPathExtraData*)` (4458). Re-read by hand 2026-11-08.
  6. `set_cheapest` (4187).
  Single-table query: input_rel = base RelOptInfo with baserestrictinfo.
- **R** `add_path` (`pathnode.c:459`) and `compare_path_costs_fuzzily` (181): disabled_nodes compared first (186-193), then total cost with 1% fuzz (`STD_FUZZ_FACTOR` 1.01, :47). Nothing restricts grouped_rel paths to Agg nodes → cheaper CustomPath wins.
- **R** Plan creation:
  - CustomPath goes `create_scan_plan` (`createplan.c:419-421`) -> `create_customscan_plan` 4123 -> `PlanCustomPath(root, rel, best_path, tlist, clauses, custom_plans)` (`extensible.h:99`); `custom_relids` = parent relids (4167);
  - upper/join form: `scanrelid = 0` + `custom_scan_tlist` holding Aggref exprs. `setrefs.c:1735-1765` resolves tlist to INDEX_VAR against it; `nodeCustom.c:58-81` builds scan tupdesc from `custom_scan_tlist`;
  - postgres_fdw uses same trick with `fdw_scan_tlist = grouped_tlist` (`postgres_fdw.c:8054`).
  - `custom-scan.sgml` documents only base-rel and join custom paths (90-131, 203-220). Upper-rel use undocumented but not blocked (R: no checks; I: that it works end to end).
- **R** In-tree examples: `src/test/modules/test_extensible/test_extensible.c` uses `set_rel_pathlist_hook` with base-rel CustomScan (345, 367-433, 587). `contrib/pg_plan_advice/pgpa_planner.c:185-194` uses setup/shutdown/build_simple_rel/joinrel/join_path hooks. `delay_execution` uses `planner_hook`. **No in-tree user of `create_upper_paths_hook`.** Out of tree, ParadeDB pg_search builds aggregate CustomScan through it (RUST.md §5).
- **R** Base rel still planned; `ExecCheckPermissions` covers all RTEs (`execMain.c:862`) → table ACLs still apply.
- **I** RLS quals live in baserestrictinfo; extension must apply them or bail. External store not visible under local MVCC snapshot (POSTGRES-REPLICATION.md §8).

**Design implication:** `create_upper_paths_hook` + CustomPath(`scanrelid = 0`) at `UPPERREL_GROUP_AGG` = transparent route: user queries local table, cost model picks external path. Freshness, MVCC, RLS, quals entirely extension's job.

## 4. Matview rewrite, IVM, eager aggregation

- **R** No summary-table matching. `rg -i "matview|materialized view|summary table|RELKIND_MATVIEW"` in `src/backend/optimizer` hits only `planner.c:411` (parallel comment) and `appendinfo.c:969`.
- **R** No IVM. `matview.c:949` mentions "incremental maintenance", but `MatViewIncrementalMaintenanceIsEnabled` only gates REFRESH CONCURRENTLY DML.
- **R** Prior objections to planner substitution = staleness: Tom Lane 2015, "unlikely that matviews would ever be so transparent that it would be okay for the planner to automatically substitute them" ([mail](https://www.postgresql.org/message-id/10883.1424541848%40sss.pgh.pa.us)). Extension doesn't need that blessing.
- **R** Eager aggregation exists from PG19, `enable_eager_aggregate` default true (`guc_parameters.dat:879-884`). `setup_eager_aggregation` (`initsplan.c:633`) requires GROUP BY, no grouping sets, no ordered aggs, no non-partial/non-serial aggs (665), multiple base rels (680).
  - Builds `rel->grouped_rel` (`allpaths.c:350-373`), filled by `generate_grouped_paths` with `AGGSPLIT_INITIAL_SERIAL` (`allpaths.c:3508/3536`) after `set_rel_pathlist_hook` (589 vs 620). Example plan: `postgres_fdw.out:4413-4434`. No dedicated hook for grouped base rels.
  - **I** `set_rel_pathlist_hook` could `add_path` into `rel->grouped_rel`, which already exists then (`allpaths.c:201`). Untested.

**Design implication:** summary matching must live in extension. Eager aggregation = second partial-aggregate injection point, join queries only.

## 5. Async execution

- **R** Callbacks: `fdwapi.h:190-196` (`IsForeignPathAsyncCapable`, `ForeignAsyncRequest`, `ForeignAsyncConfigureWait`, `ForeignAsyncNotify`).
- **R** `execAsync.c` dispatches only `T_ForeignScanState` (38/71/97); else elog ERROR. **CustomScan can't be async.**
- **R** Async only under Append: `createplan.c:1297-1299` requires `enable_async_append` (default true, guc 858-862), no pathkeys, not parallel_safe, >1 subpath. `mark_async_capable_plan` accepts ForeignPath incl. upper rels, unless gating Result (1164-1180).
- **R** postgres_fdw `async_capable` default false (781); server or table option (7602/7625).
- **I** No in-tree test of async upper ForeignScan.

**Design implication:** Append(async ForeignScan returning external partial states, local Partial Agg over tail) would overlap remote latency with local scan, only if external side is ForeignScan.

## 6. Table AM and index AM

- **R** No aggregate callback in `tableam.h` or `amapi.h`. `tableam.h` tuple-at-a-time (`scan_getnextslot` 384, tidrange 405-413, `relation_estimate_size` 808). `amapi.h` has `amgettuple`/`amgetbitmap`/`amcanreturn` (321/322/311).
- **R** Only scan-level aggregate shortcut: `planagg.c` `preprocess_minmax_aggregates` (74): MIN/MAX → ordered index scan with LIMIT 1. Skipped when query has GROUP BY (96-102).

**Design implication:** access method can't answer GROUP BY. Must go through upper-rel path (hook or FDW).

## 7. Partial aggregation

- **R** `AggSplit` (`nodes.h:370-386`): SIMPLE; INITIAL_SERIAL = SKIPFINAL|SERIALIZE; FINAL_DESERIAL = COMBINE|DESERIALIZE.
- **R** `can_partial_agg` (`planner.c:8143`) rejects grouping sets and `hasNonPartialAggs`/`hasNonSerialAggs`. `prepagg.c` sets flags for:
  - ORDER BY or DISTINCT aggs: non-partial (245-249);
  - no combinefn: non-partial (~298);
  - INTERNAL transtype without serialfn + deserialfn: non-serial (305-312);
  - array_agg special case (325-329).
- **R** Partial Aggref type: transtype, or BYTEA if transtype INTERNAL (`mark_partial_aggref`, `planner.c:6046-6068`).
- **R** Transition states (`pg_aggregate.dat`):
  - count: int8, combine int8pl (255-262);
  - sum(int4): int8 (62);
  - avg(int4): `_int8` {N, sum} (22-26);
  - float8 aggs: `_float8` / float8 (43-45, 72);
  - min/max: plain value type (95-220);
  - avg/sum(int8): INTERNAL, `int8_avg_serialize` (16-21, 56-61);
  - avg/sum(numeric): INTERNAL, `numeric_avg_serialize` (32-39, 85).
- **R** `int8_avg_serialize` (`utils/adt/numeric.c:5804`) writes int64 N, then int128 sum as hi int64 + lo uint64 (5635-5639). Requires `AggCheckCallContext` (5811) → not callable from SQL.
- **R** Finalize over arbitrary input: `add_paths_to_grouping_rel` wraps partially_grouped_rel paths in `create_agg_path(..., AGGSPLIT_FINAL_DESERIAL)` (`planner.c:7540-7550`, 7600-7612). `convert_combining_aggrefs` (`setrefs.c:2732-2783`) makes Finalize Aggref take child's INITIAL_SERIAL Aggref as arg. Append in between proven by `postgres_fdw.out:11421-11434`.
- **R Gate:** partially_grouped_rel created only when parallel partial paths, eager-aggregation paths or partial partitionwise aggregation exist. Else returns NULL (`planner.c:7697-7701`, re-read by hand) → PARTIAL_GROUP_AGG hook never fires for plain serial single-table query.
- **I** Workaround: at GROUP_AGG, build own partial path + `create_agg_path(AGGSPLIT_FINAL_DESERIAL)`. `make_partial_grouping_target` static (`planner.c:242`) → must copy. Exported helpers: `mark_partial_aggref` (`planner.h:75`), `create_agg_path` (`pathnode.h:262`), `create_append_path` (`pathnode.h:102`).

Append(external partial states, Partial Agg over live tail) under one Finalize Agg = legal plan shape. Blockers:
- **R/I** INTERNAL states need byte-exact serialfn output, not stable API across majors. Prefer non-internal states, or server returns (N, sum) columns and build bytea in C.
- **I** Tail split vs snapshot: logical replication applies by commit LSN, local visibility by xid snapshot. UPDATE/DELETE of old rows also hit external side, not tail.
- **R** Native alternative: partitioned table with custom-FDW partition + `enable_partitionwise_aggregate`; core calls FDW at child PARTIAL_GROUP_AGG stage (`planner.c:7974-7983`). Requires restructuring table.

**Design implication:** plan machinery supports merging partial states. Hard parts: state formats for INTERNAL aggs, snapshot-exact handoff. With freshness barrier (POSTGRES-REPLICATION.md §8) tail merge not needed.
