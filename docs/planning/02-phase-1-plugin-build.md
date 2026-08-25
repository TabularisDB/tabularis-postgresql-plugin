# Phase 1 — Plugin Build (TDD)

**Goal:** Build the `postgres-plugin` executable that passes all 80 parity tests,
proving byte-for-byte parity with the built-in driver. Implementation follows
strict TDD: tests exist first (written RED), code is written to make them pass.

**Mantra:** _80/80 green or it's not done. No exceptions, no "close enough."_

---

## Test Architecture

The test suite has three layers, each serving a distinct purpose:

| Layer | Count | What it proves | Runs against |
|-------|-------|----------------|--------------|
| **Parity tests** | 80 | Plugin output == builtin output (byte-perfect JSON comparison) | Both drivers via `ParityHarness` |
| **Baseline tests** | 72 | Builtin driver behavior hasn't regressed | Builtin only (direct `postgres::*` calls) |
| **Golden tests** | 26 | Builtin output matches committed snapshots (drift detection) | Builtin only |

### Why 80 parity tests (not 102)

The "102 tests" figure included all three layers. Parity tests only cover the
first layer because:

1. **Golden tests (26) don't need parity equivalents.** Golden files compare
   builtin output against static JSON files. `assert_parity()` is strictly
   stronger — it's a live comparison of two running drivers. If the plugin
   matches the builtin, it implicitly matches the golden files too.

2. **Baseline tests (72) are the safety net, not the specification.** They
   call the builtin directly via `postgres::get_tables(...)` — they cannot run
   against the plugin (it speaks JSON-RPC, not Rust function calls). The parity
   tests cover every scenario from the baseline by calling the same methods
   through the `DatabaseDriver` trait.

3. **Parity tests are MORE thorough.** They test additional edge cases beyond
   the baseline (composite PKs, cross-schema FKs, NULL updates, batch session
   state, etc.) — 80 scenarios covering all 72 baseline behaviors plus extras.

### CP-4 Gate

All three layers must pass:

- 80/80 parity tests GREEN → plugin matches builtin byte-perfectly
- 72/72 baseline tests GREEN → builtin hasn't regressed
- 26/26 golden tests GREEN → no snapshot drift

---

## Approach

### The Red → Green Cadence

At the start of Phase 1, point the parity harness at the plugin:

```bash
cargo test --features parity
# Result: 0/55 GREEN, 55/55 RED (plugin binary doesn't exist)
```

Implementation proceeds sprint by sprint. After each sprint:

```bash
cargo build --release
# Install plugin binary to local plugins directory
cargo test --features parity
# Result: N/55 GREEN — N must only increase, never decrease
```

**Rule:** If a previously-GREEN test goes RED, stop everything and fix it before
moving forward. No sprint is "done" with regressions.

---

## Sprint Breakdown

### Sprint 1: Foundation (Scaffold + Connection)

**Build:**

- `main.rs` — tokio runtime, stdin reader, stdout writer, JSON-RPC dispatch loop
- `rpc.rs` — method name → handler routing
- `models.rs` — `ConnectionParams` deserialization from JSON
- `pool.rs` — `deadpool-postgres` pool manager, keyed by `host:port:database:user`

**Implement RPC methods:**

- `initialize` — receive settings, acknowledge
- `ping` — acquire connection from pool, run `SELECT 1`
- `test_connection` — same as ping but with full error reporting
- `shutdown` — drain pools, exit cleanly

**Critical decisions at this point:**

- Pool configuration: max size, connection timeout, idle timeout
- SSL: `tokio-postgres-rustls` integration
- Startup script: `after_connect` hook that executes `params.startup_script`

**Security considerations:**

- `ConnectionParams.password` arrives in plaintext JSON. Store in memory only
  for the duration needed to create the pool. Don't log it.
- `connection_string` may contain credentials embedded in URL. Parse carefully.
- SSL certificate paths (`ssl_ca`, `ssl_cert`, `ssl_key`) should be validated
  (file exists, readable) before attempting connection.

**Tests expected to go GREEN:** 3 (connection-related tests)

---

### Sprint 2: Schema Discovery

**Implement:**

- `get_databases` — `SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate`
- `get_schemas` — `SELECT schema_name FROM information_schema.schemata WHERE ...`
- `get_tables` — query `pg_class` / `information_schema.tables`

**Gotchas:**

- Filter system schemas (`pg_catalog`, `information_schema`, `pg_toast`)
- Handle `schema` param being `None` (return all schemas' tables) vs `Some("public")`
- Table names must include `schema` qualification in responses where expected
- `get_databases` must work when connected to maintenance DB (`"postgres"`)

**Tests expected to go GREEN:** 8 cumulative

---

### Sprint 3: Column & Key Metadata

**Implement:**

- `get_columns` — `information_schema.columns` + PG catalog for extended info
- `get_indexes` — `pg_class` + `pg_index` + `pg_attribute`
- `get_foreign_keys` — `pg_constraint` with JOIN to get column names, ref table, actions

**Port from built-in:**

- `extract/` submodules (needed to correctly identify column types)
- Logic for detecting `SERIAL` → `is_auto_increment: true`
- Logic for parsing `character_maximum_length`

**Gotchas:**

- Enum type detection: must query `pg_type` + `pg_enum` to identify enum columns
- `default_value` for serial columns shows `nextval('seq')` — preserve as-is
- `ref_schema` in FK results — include from day one for multi-database support
- Composite indexes: `seq_in_index` must be correct for multi-column indexes

**Security consideration:**

- Column metadata queries should not expose system catalog internals beyond
  what's needed. Don't return `pg_catalog` tables in `get_tables` responses.

**Tests expected to go GREEN:** 18 cumulative

---

### Sprint 4: Query Execution

**Implement:**

- `execute_query` — run arbitrary SQL, return `QueryResult`
- `execute_query_batch` — run multiple statements on SINGLE connection (session state)
- `count_query` — `SELECT COUNT(*) FROM (user_query) AS q`

**Port from built-in:**

- Full `extract/` subsystem (all PG types → `serde_json::Value` conversion)
- Pagination: `LIMIT {page_size} OFFSET {(page-1) * page_size}`
- `has_more` detection: query `page_size + 1` rows, return `page_size`

**Critical: `execute_query_batch` session semantics**

This is the highest-risk area for behavioral regression:

```rust
// MUST use a SINGLE connection for the entire batch
let conn = pool.get().await?;
let mut results = vec![];
for statement in statements {
    let result = execute_on_conn(&conn, &statement, limit, page).await?;
    results.push(result);
}
```

If each statement gets its own connection, `BEGIN`/`COMMIT`, temp tables, and
`SET` commands will break silently. This is a non-negotiable correctness
requirement.

**Gotchas:**

- DML statements (INSERT/UPDATE/DELETE) return `affected_rows`, not result set
- `SET` statements return empty result with `affected_rows: 0`
- Multiple result sets: PostgreSQL doesn't support this (unlike MySQL). Each
  statement in a batch returns one result.
- Query cancellation: if the host aborts the RPC call mid-batch, the connection
  should be returned to the pool (not leaked)

**Security:**

- Parameterized queries are NOT used here (user provides raw SQL). This is by
  design — the app is a SQL editor. But ensure no metadata queries constructed
  internally are injectable.

**Tests expected to go GREEN:** 26 cumulative

---

### Sprint 5: CRUD Operations

**Implement:**

- `insert_record` — generate `INSERT INTO ... VALUES (...)` with typed bindings
- `update_record` — generate `UPDATE ... SET ... WHERE pk = ...` with typed bindings
- `delete_record` — generate `DELETE FROM ... WHERE pk = ...`

**Port from built-in:**

- `binding.rs` — the most critical and complex piece:
  - Enum column detection → `$N::enum_type` CAST syntax
  - UUID string → UUID type binding
  - JSON object/array → JSONB binding
  - Array values → PostgreSQL array syntax
  - Numeric string → appropriate numeric type
  - Boolean string → PG boolean literals
  - Temporal strings → timestamp/date/time with timezone handling
  - DEFAULT sentinel → `DEFAULT` keyword in SQL
  - NULL handling

**This is the highest-risk sprint.** The binding system has subtle type-specific
behavior that, if wrong, causes silent data corruption. For example:

- Missing enum CAST → PostgreSQL error "column X is of type mood but expression is text"
- Wrong numeric binding → silent precision loss
- Missing UUID detection → type mismatch error

**Verification approach:** After implementing, run CRUD tests that:

1. Insert a row with every type
2. Read it back via `execute_query`
3. Compare round-trip values

**Gotchas:**

- Composite PKs in WHERE clause: must handle multi-column keys correctly
- UUID PKs: must detect UUID format in PK value and bind as UUID type
- NULL in PK: should be rejected (PKs are NOT NULL by definition)
- Schema-qualified table names in generated SQL

**Tests expected to go GREEN:** 35 cumulative

---

### Sprint 6: Views & Materialized Views

**Implement:**

- `get_views` — query `pg_views` / `information_schema.views`
- `get_view_definition` — `pg_get_viewdef(oid)`
- `get_view_columns` — same as `get_columns` but for view
- `create_view` / `alter_view` / `drop_view` — DDL execution
- `get_materialized_views` — query `pg_matviews`
- `get_materialized_view_definition` — from `pg_matviews.definition`
- `get_materialized_view_columns` — from `pg_attribute`
- `refresh_materialized_view` — `REFRESH MATERIALIZED VIEW ...`

**Gotchas:**

- `alter_view` in PG is `CREATE OR REPLACE VIEW` (true ALTER is limited)
- Materialized views have no row count until `ANALYZE` is run
- MV columns query must use `pg_attribute` (not `information_schema`)
- `REFRESH MATERIALIZED VIEW CONCURRENTLY` requires a unique index — don't
  assume concurrency is always possible

**Tests expected to go GREEN:** 41 cumulative

---

### Sprint 7: Routines & Triggers

**Implement:**

- `get_routines` — query `pg_proc` + `pg_namespace`
- `get_routine_parameters` — query `pg_proc.proargnames` + `pg_proc.proargtypes`
- `get_routine_definition` — `pg_get_functiondef(oid)`
- `build_routine_call_sql` — generate `SELECT func(...)` or `CALL proc(...)`
- `routine_create_template` — generate `CREATE OR REPLACE FUNCTION/PROCEDURE`
- `get_routine_edit_script` — same as definition (PG functions are re-runnable)
- `drop_routine` — handle overloaded functions (need argument types in DROP)
- `get_triggers` — query `pg_trigger` + `information_schema.triggers`
- `get_trigger_definition` — `pg_get_triggerdef(oid)`
- `create_trigger` / `drop_trigger` / `update_trigger` — DDL execution

**Gotchas — Routine management is complex in PG:**

- Overloaded functions: same name, different argument types. `DROP FUNCTION`
  requires the argument signature: `DROP FUNCTION add_numbers(integer, integer)`
- Functions vs procedures: different call syntax (`SELECT` vs `CALL`)
- IN/OUT/INOUT parameters: affect call SQL generation
- `SECURITY DEFINER` functions: execute with creator's privileges (security relevant)
- `SET` options on functions: must be preserved in edit scripts

**Gotchas — Triggers:**

- PG triggers can fire FOR EACH ROW or FOR EACH STATEMENT
- Trigger functions are separate objects (function must exist before trigger)
- `update_trigger` = DROP + CREATE (PG has no ALTER TRIGGER for body changes)

**Tests expected to go GREEN:** 48 cumulative

---

### Sprint 8: DDL, EXPLAIN, BLOB

**Implement:**

- `get_create_table_sql` — generate `CREATE TABLE` with all columns, PKs, constraints
- `get_add_column_sql` — `ALTER TABLE ADD COLUMN ...`
- `get_alter_column_sql` — `ALTER TABLE ALTER COLUMN ...` (rename, type, null, default)
- `get_create_index_sql` — `CREATE [UNIQUE] INDEX ...`
- `drop_index` — `DROP INDEX schema."index_name"`
- `get_create_foreign_key_sql` — `ALTER TABLE ADD CONSTRAINT ... FOREIGN KEY ...`
- `drop_foreign_key` — `ALTER TABLE DROP CONSTRAINT ...`
- `explain_query_plan` — `EXPLAIN (FORMAT JSON, ANALYZE, BUFFERS) ...`
- `save_blob_to_file` — query bytea column, write raw bytes to file path
- `fetch_blob_as_data_url` — query bytea column, return as `BLOB:size:mime:base64`
- `get_ai_schema_context` — return schema DDL as context for AI features

**Gotchas — DDL:**

- Schema-qualified identifiers everywhere: `"schema"."table"`
- SERIAL type: `get_create_table_sql` must use `SERIAL` not `INTEGER DEFAULT nextval`
- `ALTER COLUMN TYPE` may require `USING` clause for type casts

**Gotchas — EXPLAIN:**

- Parse JSON format explain output into `ExplainNode` tree
- `ANALYZE` actually executes the query — handle DML carefully
- `BUFFERS` option only available with `ANALYZE`
- Cost units are PG-specific (not milliseconds)

**Gotchas — BLOB:**

- PostgreSQL uses `bytea` (inline) or Large Objects (OID reference)
- Built-in driver uses `bytea` approach: `SELECT col FROM table WHERE pk = val`
- Return value must match the `BLOB:size:mime:base64` wire format exactly
- File write must handle binary data correctly (no UTF-8 assumptions)

**Security — BLOB:**

- `save_blob_to_file` writes to an arbitrary path. The plugin runs locally so
  this is the same trust model as any desktop app file write. But validate the
  path doesn't escape expected directories if possible.

**Tests expected to go GREEN:** 53 cumulative

---

### Sprint 9: Multi-Database & Polish

**Verify:**

- `get_databases` returns both `tabularis_test` and `tabularis_test_secondary`
- Queries with different `params.database` hit different pools
- Schema discovery on secondary database returns `secondary_schema`
- FK results include `ref_schema` for cross-schema references
- Pool cleanup on shutdown drains all per-database pools

**Fix:** Any remaining edge cases or test failures from previous sprints.

**Final verification run:**

```bash
cargo test --features parity
# Result: 55/55 GREEN ✅ (or 70+/70+ if more tests were written)
```

**Tests expected to go GREEN:** 55/55 (ALL)

---

## Security Audit Checklist (End of Phase 1)

Before declaring Phase 1 complete, verify:

- [ ] **No credential logging** — `password`, `ssh_password` never appear in stdout/stderr
- [ ] **Pool credentials in memory only** — not written to temp files, not in stack traces
- [ ] **SSL verification working** — `verify-ca` and `verify-full` modes actually validate certs
- [ ] **SQL injection in internal queries** — all metadata queries use parameterized bindings
  (not string interpolation with user-provided table/column names)
- [ ] **File path validation** — `save_blob_to_file` validates path is writable
- [ ] **Startup script execution** — runs in a try/catch, error doesn't leak connection
- [ ] **Connection string parsing** — malformed URLs don't crash the plugin
- [ ] **Memory cleanup** — pools are properly drained on shutdown (no leaked connections)

---

## Checkpoint: CP-3 (Mid-Phase Progress Check)

**When:** Plugin is at approximately 25/55 tests GREEN (after Sprint 4).

**Purpose:** Early signal to the core team that implementation is on track.

**Communicate:**

- Current test count (objective progress metric)
- Any blockers discovered (unexpected RPC limitations, type handling issues)
- Revised timeline estimate if needed
- Demo: connect to PG via plugin, run a SELECT, show results

**This is NOT a release gate.** It's a progress sync to catch issues early.

---

## Checkpoint: CP-4 (Phase 1 Complete — Beta Release Gate)

**When:** 80/80 parity tests GREEN + baseline tests pass + manual smoke test complete.

**This IS a release gate.** After CP-4:

- The plugin can be published to the Tabularium registry as a **beta**
- Users can install it alongside the built-in driver and test
- Feedback collection begins (does it work with their specific PG setups?)

**After CP-4, before Phase 2:** the plugin bakes on the beta channel
against Tabularis `0.20.0`+ until no new correctness bugs surface, then
ships `1.0.0` stable — independent of Phase 2 scope. See "Versioning" in
`docs/planning/03-phase-2-issue-16.md`.

**Verify at CP-4:**

- [ ] 80/80 parity tests GREEN (byte-perfect dual-driver comparison)
- [ ] 72 baseline tests pass (builtin-only safety net)
- [ ] 26 golden snapshot tests pass (no drift)
- [ ] Manual smoke test: all 24 items pass
- [ ] `pnpm test` (frontend): no regressions
- [ ] Security audit checklist: all items verified
- [ ] Plugin binary builds on all 3 platforms (macOS, Linux, Windows)
- [ ] Plugin installs cleanly via Tabularis Settings > Plugins
- [ ] Built-in driver still works unchanged (no interference)

**Communicate to team:**

- Feature parity achieved and proven
- Ready for beta testing with real users
- Phase 2 (issue #16 improvements) can begin
- Collect feedback on performance, compatibility, edge cases

---

## Repo Extraction — Timing and Open Question

**Access to `TabularisDB/tabularis-postgresql-plugin` was granted during Phase 1
development (2026-08-05).** Decision: stay in-tree through CP-4, then extract.

**Why wait:**

- Phase 1 is mid-TDD with a tight build → test → feedback loop within a single
  CI run (`pg-integration.yml` builds the plugin and runs all 80 parity tests
  against it in one job). Splitting into two repos now means cross-repo CI
  (the host would need to clone/build the plugin repo as a dependency, or pull
  release artifacts) — friction that actively hurts iteration speed while the
  RPC surface and manifest are still shifting commit to commit.
- This matches the plan's original intent: build in-tree through Phase 1,
  extract to a standalone repo at the CP-4 beta gate — consistent with how
  every other Tabularis plugin (DuckDB, ClickHouse, DynamoDB, etc.) is
  structured as an external repo.

**Open question to resolve before/at CP-4 — where do the 80 parity tests live
post-extraction?**

The parity tests currently live in `tabularis`'s own test suite
(`src-tauri/tests/postgres_integration/parity*.rs`). They import
`tabularis_lib` types directly (`DatabaseDriver`, `PostgresDriver`,
`ConnectionParams`, etc.) and spawn the plugin binary in-process via
`RpcDriver::new()`. Two options once the plugin moves to its own repo:

1. **Keep parity tests in `tabularis`.** The host CI would need to build or
   fetch the plugin binary from the new repo (e.g. checkout as a step, or
   download a release artifact) before running the existing test suite
   unchanged. Simpler on the plugin-repo side; adds a cross-repo dependency
   to `tabularis`'s CI.
2. **Move parity tests to the plugin repo.** The plugin repo would need a
   `tabularis_lib` dependency (path or published crate) to get `DatabaseDriver`
   and the builtin `PostgresDriver` for comparison. Keeps the plugin
   self-testing but couples it to the host's internal crate — `tabularis_lib`
   isn't currently published or designed for external consumption.

Revisit this when CP-4 is close — by then the RPC surface should be stable
enough that the decision doesn't need to be made twice.

---

## Potential Gaps & Risks Specific to Phase 1

| Gap/Risk | Impact | Mitigation |
| -------- | ------ | ---------- |
| `tokio-postgres` type handling differs from `sqlx` | Extraction code must be rewritten, not just copied | Port logic, not code. Test each type individually. |
| Binary wire format vs text format | Built-in uses binary (sqlx default); plugin may start with text. Values might format differently (e.g., float precision). | Golden file tests will catch any formatting differences immediately. |
| Pool exhaustion under load | Plugin has one process for all connections. Deadpool defaults may be too conservative. | Configure max_size based on expected concurrent queries. Monitor in beta. |
| Plugin stderr noise | Accidental stdout writes corrupt JSON-RPC stream | Use `tracing` crate with stderr subscriber. Never use `println!`. Add a CI test that verifies no stdout writes outside JSON-RPC. |
| Cross-platform binary build | Plugin must compile for macOS (ARM+Intel), Linux, Windows | Set up cross-compilation in CI. Test on all platforms before CP-4. |

---

## Definition of Done

- [ ] 80/80 parity tests GREEN
- [ ] 72 baseline tests pass (builtin-only safety net)
- [ ] 26 golden snapshot tests pass
- [ ] Manual smoke test: 24/24 items pass
- [ ] Security audit checklist: complete
- [ ] Plugin builds on macOS, Linux, Windows
- [ ] Plugin installs and runs cleanly in Tabularis
- [ ] Built-in driver unaffected (both can coexist)
- [ ] CP-4 sync completed with core team
- [ ] Published to Tabularium registry as beta
