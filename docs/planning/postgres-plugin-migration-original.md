# PostgreSQL Plugin Migration — Phased Implementation Plan

**Ref:** [#16 — Better PostgreSQL Support](https://github.com/TabularisDB/tabularis/issues/16)
**Related:** [PR #402 — Multi-database connections](https://github.com/TabularisDB/tabularis/pull/402)
**Direction:** Per debba — all drivers should eventually become plugins; built-in
drivers will be removed over time.

## Executive Summary

This plan migrates the built-in PostgreSQL driver to a standalone plugin driver,
achieving full feature parity before adding the multi-database capabilities from
PR #402 and the improvements from issue #16. The approach is incremental — each
phase delivers working software that can be tested and shipped independently.

---

## Table of Contents

1. [Architecture Context](#architecture-context)
2. [Critical Constraint: The BUILTIN_DRIVER_IDS Guard](#critical-constraint)
3. [Migration Strategy](#migration-strategy)
4. [RPC Adapter Blockers and Gotchas](#rpc-adapter-blockers-and-gotchas)
5. [Phase 0: Baseline Test Suite](#phase-0-baseline-test-suite-before-any-migration)
6. [Phase 1: Plugin Scaffold with Feature Parity](#phase-1-plugin-scaffold-with-feature-parity)
7. [Phase 2: Multi-Database Support (PR 402)](#phase-2-multi-database-support-pr-402)
8. [Phase 3: Issue 16 Improvements](#phase-3-issue-16-improvements)
9. [Phase 4: Deprecate Built-in Driver](#phase-4-deprecate-built-in-driver-deferred-decision)
10. [Plugin Architecture Reference](#plugin-architecture-reference)
11. [PR 402 Architecture Summary](#pr-402-architecture-summary)
12. [Dependency Sequencing](#dependency-sequencing)
13. [Developer Workflow](#developer-workflow)
14. [Risk Assessment](#risk-assessment)
15. [Open Questions](#open-questions)

---

## Architecture Context

### How Plugin Drivers Work

Tabularis plugin drivers are **standalone executables** that communicate with the
host via **JSON-RPC 2.0 over stdin/stdout**. Each plugin:

- Declares capabilities in a `.tabularium` manifest file
- Is spawned as a child process at startup (or on enable)
- Receives method calls as JSON-RPC requests on stdin
- Returns results as JSON-RPC responses on stdout
- Manages its own connection pooling internally
- Is killed on disable/uninstall (`kill_on_drop: true`)

### Current Built-in PostgreSQL Driver

- Location: `src-tauri/src/drivers/postgres/mod.rs` (2420 lines)
- Uses `sqlx` with `deadpool-postgres` for connection pooling
- 6 extraction submodules (simple, array, range, multi_range, composite, enum, advanced)
- Full typed binding system (473 lines in `binding.rs`)
- Routine management (overloaded function resolution)
- Schema-qualified identifier handling throughout
- 97+ declared data types across 14 categories

---

## Critical Constraint

### The `BUILTIN_DRIVER_IDS` Guard

In `src-tauri/src/plugins/manager.rs` lines 164-169:

```rust
const BUILTIN_DRIVER_IDS: [&str; 3] = ["mysql", "postgres", "sqlite"];
if BUILTIN_DRIVER_IDS.contains(&&plugin_id.as_str()) {
    return Err(format!(
        "Plugin id '{}' collides with a built-in driver and was refused",
        plugin_id
    ));
}
```

**A plugin cannot use the id `"postgres"`.** This means:

| Option | Approach | Impact |
| ------ | -------- | ------ |
| A | Use a different id (e.g., `"postgres-plugin"`) | Existing connections won't auto-migrate; users must reconnect or we need a migration script |
| B | Remove the guard before installing the plugin | Requires a Tabularis core change; allows seamless `driver: "postgres"` swap |
| C | Remove the built-in driver AND the guard simultaneously | Clean swap — plugin takes over the `"postgres"` id slot |

**Recommended: Option C eventually, but deferred.** During development, the plugin
uses the id `"postgres-plugin"`. The question of whether/how to remove the guard
and take over the `"postgres"` id is a decision for later — once feature parity is
proven and the team agrees on a migration path for existing connections.

---

## Migration Strategy

```text
Phase 0: Build baseline test suite + CI infrastructure (PREREQUISITE)
    ↓
Phase 1: Build plugin "postgres-plugin" with full feature parity
    ↓
Phase 2: Integrate PR 402 multi-database support into plugin
    ↓
Phase 3: Add issue #16 improvements (sequences, JSONB editing, etc.)
    ↓
Phase 4: Deprecate built-in driver (decision deferred)
```

Each phase is independently shippable:

- After Phase 0: Confidence in the built-in driver's behavior (test baseline)
- After Phase 1: Users can test the plugin alongside the built-in driver
- After Phase 2: Plugin surpasses built-in in functionality
- After Phase 3: Plugin is the definitive PostgreSQL experience
- After Phase 4: Clean architecture — one plugin, no built-in

---

## RPC Adapter Blockers and Gotchas

Before building the plugin, these limitations in the host's `RpcDriver` adapter
(`src-tauri/src/plugins/driver.rs`) must be understood and addressed. Some require
changes to the Tabularis core; others must be handled plugin-side.

### P0 — Must Fix Before Feature Parity Is Possible

| Issue | Detail | Resolution |
| ----- | ------ | ---------- |
| **BLOB methods not forwarded** | `save_blob_to_file` and `fetch_blob_as_data_url` inherit trait defaults that return "not supported". Built-in PG driver reads bytea data and exports to file or base64 wire format. | Extend the RpcDriver to forward these calls. Plugin returns base64 data over JSON; host writes to file. Requires Tabularis core PR. |
| **Materialized views not forwarded** | `get_materialized_views`, `get_materialized_view_columns`, `get_materialized_view_definition`, `refresh_materialized_view` all inherit empty defaults. | Extend the RpcDriver to forward these 4 methods. Straightforward — same pattern as triggers. Requires Tabularis core PR. |
| **`map_inferred_type` not forwarded** | Synchronous method — cannot issue RPC call. Built-in PG maps `DATETIME`→`TIMESTAMP`, `JSON`→`JSONB`. | Plugin declares mappings in manifest/settings at `initialize` time. Host stores them and applies locally. Requires core change to `RpcDriver`. |

### P1 — Must Handle in Plugin Implementation

| Issue | Detail | Resolution |
| ----- | ------ | ---------- |
| **Query cancellation** | Host aborts the Tokio task but plugin keeps executing. No signal reaches the DB server. | Plugin implements an internal `cancel_query` mechanism using `pg_cancel_backend()` or connection drop. Discuss with team whether a `cancel` RPC method should be added to the protocol. |
| **`execute_query_batch` session state** | If plugin doesn't implement this, fallback uses separate RPC calls (separate connections). Breaks `BEGIN`/`COMMIT`, temp tables, `SET` commands. | Plugin MUST implement `execute_query_batch` using a single connection for the entire batch. Non-negotiable for PG. |
| **Startup script execution** | Host passes `startup_script` in `ConnectionParams` but does NOT execute it. Plugin must detect and run it on every new pooled connection. | Plugin implements `after_connect` hook in its internal pool that executes `params.startup_script`. |
| **120-second hard timeout** | Long queries (VACUUM, migrations, large aggregations) will timeout. | For now: document the limitation. Later: propose configurable timeout per plugin setting. |

### P2 — Acceptable for Initial Release, Fix Later

| Issue | Detail |
| ----- | ------ |
| **No streaming for large results** | Full JSON response in one line. Memory spike for 10K+ row results. Acceptable with pagination (host passes `limit`/`page`). |
| **Batch progress fires post-completion** | UI doesn't show per-statement progress during native batch. Acceptable — same behavior as some existing drivers. |
| **Plaintext password over stdio** | Local pipes only, same user. Acceptable security posture for desktop app. |
| **SSH params still in serialized ConnParams** | Plugin should ignore them (host already tunneled). Document in plugin guide. |
| **Static data_types** | Extension types (PostGIS, pgvector) won't appear in picker. Solve later with dynamic type discovery. |
| **Plugin crash = 120s hang for in-flight calls** | Acceptable for now. Later: fast-fail detection + auto-restart. |

---

## Phase 0: Baseline Test Suite (Before Any Migration)

### Why Phase 0 Exists

The current PostgreSQL driver test coverage has critical gaps:

| Category | Status |
| -------- | ------ |
| Value extraction (wire format parsing) | ✅ 162 unit tests — excellent |
| Parameter binding (type coercion) | ✅ 96 unit tests — excellent |
| Public API functions (36 methods) | ❌ Zero dedicated tests |
| Trait-level interface tests | ❌ Zero tests |
| Integration tests | ⚠️ 4 tests, all `#[ignore]`, never run in CI |
| Cross-driver parity tests | ❌ None |
| EXPLAIN parsing | ❌ Zero tests |
| BLOB handling | ❌ Zero tests |
| DDL generation | ❌ Zero tests |

**We cannot prove feature parity without a baseline.** Phase 0 creates the test
infrastructure that will be used to verify both the built-in driver AND the plugin
produce identical results.

### Phase 0 Deliverables

#### 0.1: CI PostgreSQL Service

Add a PostgreSQL service container to the CI workflow so integration tests run
automatically on every PR:

```yaml
# .github/workflows/ci.yml addition
services:
  postgres:
    image: postgres:16
    ports:
      - 54320:5432
    env:
      POSTGRES_PASSWORD: test
      POSTGRES_DB: tabularis_test
    options: >-
      --health-cmd pg_isready
      --health-interval 10s
      --health-timeout 5s
      --health-retries 5
```

Remove `#[ignore]` from integration tests and gate on `services.postgres`.

#### 0.2: Parity Test Harness

A test framework that runs the same assertions against both the built-in driver
and (later) the plugin, ensuring identical behavior:

```rust
// tests/parity/harness.rs
pub struct ParityTestHarness {
    builtin: Box<dyn DatabaseDriver>,
    plugin: Option<Box<dyn DatabaseDriver>>,  // Added in Phase 1
}

impl ParityTestHarness {
    pub async fn assert_same<T: PartialEq + Debug>(
        &self,
        method: &str,
        builtin_result: Result<T, String>,
        plugin_result: Result<T, String>,
    ) {
        assert_eq!(builtin_result, plugin_result,
            "Parity failure in {}: built-in and plugin returned different results", method);
    }
}
```

#### 0.3: Golden File Tests for API Surface

Capture the exact output of every public method against a known test database
as golden/snapshot files:

```text
tests/parity/golden/
├── get_tables.json              # Expected table list
├── get_columns_users.json       # Expected columns for test table
├── get_indexes_users.json       # Expected indexes
├── get_foreign_keys_orders.json # Expected FKs
├── get_views.json               # Expected views
├── get_routines.json            # Expected functions
├── get_triggers.json            # Expected triggers
├── execute_query_types.json     # Result of SELECT with every PG type
├── explain_simple.json          # EXPLAIN output for simple query
├── explain_analyze.json         # EXPLAIN ANALYZE output
├── get_materialized_views.json  # MV listing
└── ddl/
    ├── create_table.sql         # Generated CREATE TABLE
    ├── add_column.sql           # Generated ALTER TABLE ADD COLUMN
    └── create_index.sql         # Generated CREATE INDEX
```

These golden files become the parity contract. The plugin must produce output
that matches these files exactly (or with documented acceptable differences).

#### 0.4: Integration Test Expansion

Add dedicated integration tests for every public method that currently has zero
test coverage:

```text
tests/integration/postgres/
├── schema_discovery.rs
│   ├── test_get_schemas
│   ├── test_get_databases
│   ├── test_get_tables (with and without schema filter)
│   └── test_get_tables_system_tables_excluded
├── column_metadata.rs
│   ├── test_get_columns_all_types
│   ├── test_get_columns_nullable_detection
│   ├── test_get_columns_pk_detection
│   ├── test_get_columns_auto_increment_serial
│   ├── test_get_columns_default_values
│   └── test_get_columns_character_max_length
├── foreign_keys.rs
│   ├── test_get_foreign_keys_basic
│   ├── test_get_foreign_keys_composite
│   ├── test_get_foreign_keys_cross_schema
│   └── test_get_foreign_keys_on_delete_cascade
├── indexes.rs
│   ├── test_get_indexes_btree
│   ├── test_get_indexes_unique
│   ├── test_get_indexes_composite
│   └── test_get_indexes_partial
├── views.rs
│   ├── test_get_views
│   ├── test_get_view_definition
│   ├── test_get_view_columns
│   ├── test_create_view
│   ├── test_alter_view
│   └── test_drop_view
├── materialized_views.rs
│   ├── test_get_materialized_views
│   ├── test_get_mv_definition
│   ├── test_get_mv_columns
│   └── test_refresh_mv
├── routines.rs
│   ├── test_get_routines_functions
│   ├── test_get_routines_procedures
│   ├── test_get_routine_parameters
│   ├── test_get_routine_definition
│   ├── test_routine_create_template
│   └── test_drop_routine_overloaded
├── triggers.rs
│   ├── test_get_triggers
│   ├── test_get_trigger_definition
│   ├── test_create_trigger
│   └── test_drop_trigger
├── crud.rs
│   ├── test_insert_all_types
│   ├── test_insert_with_enum_cast
│   ├── test_insert_json_object
│   ├── test_insert_array_value
│   ├── test_update_with_pk
│   ├── test_update_composite_pk
│   ├── test_update_uuid_pk
│   ├── test_delete_single_pk
│   └── test_delete_composite_pk
├── ddl_generation.rs
│   ├── test_create_table_sql
│   ├── test_add_column_sql
│   ├── test_alter_column_rename
│   ├── test_alter_column_type
│   ├── test_create_index_sql
│   ├── test_create_foreign_key_sql
│   └── test_drop_index_sql
├── explain.rs
│   ├── test_explain_simple_select
│   ├── test_explain_analyze
│   └── test_explain_with_buffers
├── blob.rs
│   ├── test_save_blob_to_file
│   ├── test_fetch_blob_as_data_url
│   └── test_blob_round_trip
└── query_execution.rs
    ├── test_execute_query_basic
    ├── test_execute_query_with_pagination
    ├── test_execute_query_all_types_roundtrip
    ├── test_execute_batch_transaction
    ├── test_execute_batch_temp_tables
    └── test_execute_batch_set_commands
```

#### 0.5: Test Database Seed Script

A repeatable seed script that creates the test schema used by all tests:

```sql
-- tests/fixtures/postgres_seed.sql
CREATE SCHEMA IF NOT EXISTS test_schema;

CREATE TABLE test_schema.all_types (
    id SERIAL PRIMARY KEY,
    col_text TEXT,
    col_varchar VARCHAR(255),
    col_int INTEGER,
    col_bigint BIGINT,
    col_float REAL,
    col_double DOUBLE PRECISION,
    col_numeric NUMERIC(10,2),
    col_bool BOOLEAN,
    col_date DATE,
    col_time TIME,
    col_timestamp TIMESTAMP,
    col_timestamptz TIMESTAMPTZ,
    col_uuid UUID,
    col_json JSON,
    col_jsonb JSONB,
    col_bytea BYTEA,
    col_inet INET,
    col_cidr CIDR,
    col_macaddr MACADDR,
    col_int_array INTEGER[],
    col_text_array TEXT[],
    col_int4range INT4RANGE,
    col_tsrange TSRANGE
);

CREATE TYPE test_schema.mood AS ENUM ('happy', 'sad', 'neutral');
CREATE TABLE test_schema.with_enum (
    id SERIAL PRIMARY KEY,
    current_mood test_schema.mood
);

-- ... (tables with FKs, indexes, triggers, routines, views, MVs)
```

### Phase 0 Success Criteria

- [ ] All 4 existing integration tests pass in CI (un-ignored, PG service running)
- [ ] 50+ new integration tests covering the full API surface
- [ ] Golden files captured for every public method
- [ ] Parity harness infrastructure ready (built-in driver fills it today)
- [ ] Seed script creates a comprehensive test schema
- [ ] CI runs in < 5 minutes with PG service

---

## Phase 1: Plugin Scaffold with Feature Parity

### Goal

A standalone Rust plugin that implements every method the built-in PostgreSQL
driver currently supports, passing the same test suite.

### Scaffold Structure

```text
plugins/postgres-plugin/
├── .tabularium                    # Plugin manifest
├── Cargo.toml                     # Rust project
├── src/
│   ├── main.rs                    # Stdin/stdout JSON-RPC loop
│   ├── rpc.rs                     # Method dispatch router
│   ├── models.rs                  # ConnectionParams, shared types
│   ├── pool.rs                    # Connection pool management (tokio-postgres)
│   ├── handlers/
│   │   ├── metadata.rs            # get_tables, get_columns, get_views, etc.
│   │   ├── query.rs               # execute_query, execute_query_batch
│   │   ├── crud.rs                # insert_record, update_record, delete_record
│   │   ├── ddl.rs                 # get_create_table_sql, get_add_column_sql, etc.
│   │   ├── routines.rs            # get_routines, build_routine_call_sql, etc.
│   │   ├── explain.rs             # explain_query_plan
│   │   └── blob.rs                # save_blob_to_file, fetch_blob_as_data_url
│   ├── binding.rs                 # Typed parameter binding (enum CAST, etc.)
│   ├── extract/                   # Value extraction from PG rows
│   │   ├── mod.rs
│   │   ├── simple.rs              # Basic types
│   │   ├── array.rs               # PG arrays
│   │   ├── range.rs               # Range types
│   │   ├── multi_range.rs         # Multi-range types
│   │   ├── composite.rs           # Composite/record types
│   │   ├── enum_type.rs           # Enum extraction
│   │   └── advanced.rs            # UUID, JSONB, geometric, etc.
│   └── types.rs                   # Data type declarations (97+ types)
└── tests/
    ├── metadata_test.rs
    ├── query_test.rs
    ├── crud_test.rs
    └── ddl_test.rs
```

### Manifest (`.tabularium`)

```json
{
  "id": "postgres-plugin",
  "name": "PostgreSQL (Next)",
  "version": "0.1.0",
  "description": "Next-generation PostgreSQL driver plugin",
  "executable": "postgres-plugin",
  "default_port": 5432,
  "default_username": "postgres",
  "color": "#336791",
  "icon": "postgres",
  "engine": "PostgreSQL",
  "paradigms": ["relational"],
  "capabilities": {
    "schemas": true,
    "views": true,
    "materialized_views": true,
    "routines": true,
    "routine_management": true,
    "triggers": true,
    "file_based": false,
    "connection_string": true,
    "connection_string_example": "postgresql://user:pass@host:5432/dbname",
    "alter_primary_key": true,
    "alter_column": true,
    "create_foreign_keys": true,
    "explain": true,
    "supports_ssl": true,
    "sql_dialect": "Postgres",
    "identifier_quote": "\"",
    "manage_tables": true,
    "serial_type": "SERIAL",
    "auto_increment_keyword": ""
  },
  "settings": [
    {
      "key": "sslMode",
      "label": "SSL Mode",
      "setting_type": "select",
      "default": "prefer",
      "options": ["disable", "allow", "prefer", "require", "verify-ca", "verify-full"]
    },
    {
      "key": "statementTimeout",
      "label": "Statement Timeout (ms)",
      "setting_type": "number",
      "default": 0,
      "description": "0 = no timeout"
    }
  ],
  "data_types": []
}
```

### RPC Methods to Implement (Full List)

| Category | Methods |
| -------- | ------- |
| Connection | `initialize`, `ping`, `test_connection`, `shutdown` |
| Databases | `get_databases`, `get_schemas` |
| Metadata | `get_tables`, `get_columns`, `get_views`, `get_view_definition`, `get_view_columns`, `get_indexes`, `get_foreign_keys`, `get_triggers`, `get_trigger_definition`, `get_routines`, `get_routine_parameters`, `get_routine_definition` |
| Query | `execute_query`, `execute_query_batch`, `count_query` |
| CRUD | `insert_record`, `update_record`, `delete_record` |
| BLOB | `save_blob_to_file`, `fetch_blob_as_data_url` |
| DDL | `get_create_table_sql`, `get_add_column_sql`, `get_alter_column_sql`, `get_create_index_sql`, `drop_index`, `get_create_foreign_key_sql`, `drop_foreign_key` |
| Views | `create_view`, `alter_view`, `drop_view` |
| Triggers | `create_trigger`, `drop_trigger`, `update_trigger` |
| Routines | `build_routine_call_sql`, `routine_create_template`, `get_routine_edit_script`, `drop_routine` |
| Explain | `explain_query_plan` |
| AI | `get_ai_schema_context` |

### Key Technical Decisions

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| PostgreSQL client library | `tokio-postgres` | Direct async access, full type system control. Matches what sqlx uses internally. |
| Connection pooling | `deadpool-postgres` | Production-grade pool with configurable size, timeouts, and recycling. Key pools by `host:port:database:user`. |
| Typed binding | Port existing `binding.rs` logic | Critical for enum CASTs, UUID handling, array bindings |
| Value extraction | Port existing `extract/` submodules | Needed for proper array, range, composite, enum rendering |
| SSL support | `tokio-postgres-rustls` | Matches existing SSL capability. `rustls` avoids OpenSSL dependency. |
| Binary format | Text protocol initially, binary later | Text is simpler to port; binary can be optimized later |

### Phase 1 Success Criteria — Zero Wiggle Room

Phase 1 is **not done** until:

1. **All Phase 0 golden file tests pass with the plugin driver** — The parity
   harness runs every test against both the built-in driver and the plugin,
   asserting identical results. Zero tolerance for differences.

2. **The full integration test suite passes with the plugin** — Same 50+ tests
   that validate the built-in driver must pass when pointed at the plugin.

3. **Manual smoke test checklist** (all pass):
   - [ ] Connect to PG via host/port
   - [ ] Connect via connection string
   - [ ] Connect via SSL (all modes)
   - [ ] Browse schemas in sidebar
   - [ ] Browse tables, views, materialized views, routines, triggers
   - [ ] Execute SELECT with all PG types (see seed table)
   - [ ] Inline edit: update text, number, boolean, date, enum, json, array
   - [ ] Insert new row with auto-generated serial PK
   - [ ] Delete row by single PK and composite PK
   - [ ] BLOB: save bytea column to file, preview as data URL
   - [ ] EXPLAIN: view query plan, view ANALYZE output
   - [ ] Batch: run multi-statement script with BEGIN/COMMIT
   - [ ] Batch: temp table persists across statements
   - [ ] Batch: SET command persists across statements
   - [ ] Startup script: SET search_path executes on connect
   - [ ] DDL: create table, add column, alter column, create index, create FK
   - [ ] Views: create, alter, drop
   - [ ] Materialized views: list, inspect, refresh
   - [ ] Routines: list, inspect, call function, call procedure
   - [ ] Triggers: list, inspect, create, drop

4. **No regressions in existing frontend tests** — `pnpm test` passes unchanged.

---

## Phase 2: Multi-Database Support (PR 402)

### Phase 2 Goal

Incorporate the multi-database browsing architecture from PR #402 into the plugin.

### What PR 402 Requires from the Driver

1. **Handle `database` parameter on every command** — The host sends `params.database`
   set to the target database. The plugin must route to the correct pool.

2. **Per-database connection pools** — When `params.database` changes between calls,
   the plugin creates/reuses a pool for that specific database.

3. **`get_schemas` per database** — Schema discovery is called separately for each
   database the user expands in the sidebar.

4. **`get_databases` returns all databases** — Used to populate the sidebar tree.

5. **Fall back to `"postgres"` database** — When connecting without an explicit
   database selection, use the maintenance database.

6. **`ref_schema` in ForeignKey results** — Return the schema of the referenced
   table for cross-schema FK navigation.

### Implementation in the Plugin

```rust
// In pool.rs — pool keyed by database
fn pool_key(params: &ConnectionParams) -> String {
    format!("{}:{}:{}:{}", params.host, params.port, params.database, params.user)
}

// In each handler — use params.database to select pool
async fn get_tables(params: &ConnectionParams, schema: Option<&str>) -> Result<...> {
    let pool = get_or_create_pool(params).await?;
    // Query using pool for params.database
}
```

The plugin naturally handles this because every RPC call receives the full
`ConnectionParams` with the correct `database` field already set by the host.

---

## Phase 3: Issue 16 Improvements

### Phase 3 Goal

Add the feature gaps and bug fixes identified in the PostgreSQL audit (issue #16).

### Items (from the audit)

| Priority | Item |
| -------- | ---- |
| High | Sequence management (list, inspect, alter, reset) |
| High | JSONB inline editing (object/array manipulation) |
| High | Extension-aware type system (PostGIS, pgvector, ltree) |
| Medium | Partition table introspection |
| Medium | Row-level security policy display |
| Medium | Publication/subscription visibility |
| Medium | Advisory lock monitoring |
| Low | Query plan cost visualization improvements |
| Low | Table statistics (pg_stat_user_tables) display |

### Advantage of Plugin Architecture

These improvements are easier to ship as a plugin because:

- No Tabularis core release needed — just update the plugin binary
- Can iterate faster (plugin version != app version)
- Users can opt-in to beta plugin versions
- Plugin-specific UI extensions can be bundled (`ui_extensions` in manifest)

---

## Phase 4: Deprecate Built-in Driver (Deferred Decision)

### Phase 4 Goal

Remove the built-in PostgreSQL driver from the Tabularis core and let the plugin
become the sole PostgreSQL driver. **The specifics of this phase are deferred**
until Phases 1-3 are complete and the team can evaluate:

- Whether the plugin id should become `"postgres"` (seamless migration) or remain
  `"postgres-plugin"` (requires connection migration tooling)
- Whether to remove the `BUILTIN_DRIVER_IDS` guard entirely or modify it
- Whether to bundle the plugin with the app distribution or keep it installable

### Possible Steps (to be finalized later)

1. Remove `BUILTIN_DRIVER_IDS` guard (or remove `"postgres"` from the array)
2. Remove `src-tauri/src/drivers/postgres/` directory
3. Remove PostgreSQL pool logic from `pool_manager.rs`
4. Decide on plugin id (`"postgres"` vs keeping `"postgres-plugin"`)
5. If renaming to `"postgres"`: auto-migration for saved connections
6. If keeping `"postgres-plugin"`: connection migration UI or script
7. Update frontend: remove hardcoded PostgreSQL references in `useDrivers.ts`

### Connection Migration (if plugin takes over `"postgres"` id)

Existing saved connections use `driver: "postgres"`. If the plugin takes over
that exact id, connections work without modification:

```text
Before: driver: "postgres" → built-in code path
After:  driver: "postgres" → plugin registered with id "postgres" → same behavior
```

**No user action required** if the plugin uses the same id.

---

## Plugin Architecture Reference

### Communication Protocol

```text
Host (Tauri)                    Plugin (standalone process)
     |                                    |
     |-- JSON-RPC Request (stdin) ------->|
     |   {"jsonrpc":"2.0",                |
     |    "method":"execute_query",       |
     |    "params":{                      |
     |      "params":{...ConnParams...},  |
     |      "query":"SELECT...",          |
     |      "limit":500,                  |
     |      "page":1,                     |
     |      "schema":"public"             |
     |    },                              |
     |    "id":42}                        |
     |                                    |
     |<-- JSON-RPC Response (stdout) -----|
     |   {"jsonrpc":"2.0",                |
     |    "result":{                      |
     |      "columns":["id","name"],      |
     |      "rows":[[1,"Alice"],...],     |
     |      "affected_rows":0,            |
     |      "pagination":{...}            |
     |    },                              |
     |    "id":42}                        |
```

### Key Constraints

| Constraint | Detail |
| ---------- | ------ |
| One process per plugin | All connections for that driver type go through one process |
| 120s call timeout | `PLUGIN_CALL_TIMEOUT` — if exceeded, returns error |
| No streaming | Full result returned in one JSON response |
| Newline-delimited | Each request/response is a single line of JSON |
| `ConnectionParams` on every call | Plugin must parse and route internally |
| `-32601` for unimplemented methods | Host falls back to defaults for optional methods |
| Plugin manages its own pools | Host does not pool connections for plugins |

### ConnectionParams Structure (what the plugin receives)

```json
{
  "host": "localhost",
  "port": 5432,
  "user": "postgres",
  "password": "secret",
  "database": "mydb",
  "ssl": true,
  "ssl_mode": "require",
  "connection_string": null,
  "startup_script": "SET search_path TO myschema",
  "connection_id": "abc-123",
  "driver": "postgres-plugin",
  "settings": {
    "sslMode": "prefer",
    "statementTimeout": 30000
  }
}
```

---

## PR 402 Architecture Summary

### Core Changes

PR #402 adds per-database connection routing for PostgreSQL:

- **Pool key includes database**: `postgres:conn:{id}:{host}:{port}:{dbname}`
- **Every command gains `database: Option<String>`** parameter
- **Frontend tabs carry `tab.database`** alongside `tab.schema`
- **`buildTableRoutingParams()`** utility builds `{ schema, database }` for backend calls
- **`isSchemaBasedMultiDb()`** distinguishes PG hierarchy from MySQL flat layout
- **Lazy schema loading**: Sidebar loads schemas per-database on expand, not all at once
- **`ForeignKey.ref_schema`**: New field for cross-schema FK references

### What This Means for the Plugin

The plugin doesn't need to know about PR 402's frontend changes — the host handles
routing. The plugin just needs to:

1. Use `params.database` to connect to the correct database
2. Pool connections per-database internally
3. Return schema-qualified FK references (`ref_schema`)
4. Support `get_schemas` called per-database

---

## Dependency Sequencing

```text
┌─────────────────────────────────────────────────────────────────────┐
│ PREREQUISITE: 3 Tabularis Core PRs (can be one combined PR)         │
│   • RpcDriver: forward BLOB methods (base64 over JSON)              │
│   • RpcDriver: forward materialized view methods                    │
│   • RpcDriver: resolve map_inferred_type from manifest/settings     │
└──────────────────────────────────┬──────────────────────────────────┘
                                   │ unblocks
                                   ▼
┌──────────────────────────────────────────────────────────────────────┐
│ PHASE 0: Baseline Test Suite                                         │
│   • CI PG service + seed script                                      │
│   • 50+ integration tests against built-in driver                    │
│   • Golden file captures                                             │
│   • Parity harness infrastructure                                    │
│                                                          can overlap │
└──────────────────────────────────┬───────────────────────────────────┘
                                   │ unblocks
                                   ▼
┌──────────────────────────────────────────────────────────────────────┐
│ PHASE 1: Plugin with Feature Parity                                  │
│   • Build postgres-plugin (scaffold + all 30+ RPC methods)           │
│   • Run Phase 0 tests against plugin — must all pass                 │
│   • Golden file comparison — must match built-in output              │
│   • Manual smoke test checklist — all items pass                     │
└──────────────────────────────────┬───────────────────────────────────┘
                                   │ unblocks (+ PR 402 merges)
                                   ▼
┌──────────────────────────────────────────────────────────────────────┐
│ PHASE 2: Multi-Database (PR 402)                                     │
│   • Per-database pool routing in plugin                              │
│   • get_schemas per database                                         │
│   • ref_schema in FK results                                         │
└──────────────────────────────────┬───────────────────────────────────┘
                                   │ unblocks
                                   ▼
┌──────────────────────────────────────────────────────────────────────┐
│ PHASE 3: Issue #16 Improvements                                      │
│   • Sequences, JSONB editing, extensions, partitions, etc.           │
└──────────────────────────────────┬───────────────────────────────────┘
                                   │ team decision
                                   ▼
┌──────────────────────────────────────────────────────────────────────┐
│ PHASE 4: Deprecate Built-in (deferred)                               │
└──────────────────────────────────────────────────────────────────────┘
```

**Parallelization opportunity:** Phase 0 test writing and the Core PRs can happen
simultaneously. Phase 1 plugin scaffold can begin once the Core PRs are merged
(the plugin needs BLOB/MV forwarding to pass parity tests).

---

## Developer Workflow

### Local Development Setup

```bash
# 1. Clone and build the plugin
cd plugins/postgres-plugin
cargo build --release

# 2. Install locally (symlink or copy to plugin directory)
# macOS:
cp target/release/postgres-plugin \
  ~/Library/Application\ Support/tabularis/plugins/postgres-plugin/
cp .tabularium \
  ~/Library/Application\ Support/tabularis/plugins/postgres-plugin/

# 3. Restart Tabularis — plugin auto-loads on startup
# Or use Settings > Plugins > Enable to hot-reload
```

### Testing Locally

```bash
# Run plugin unit tests
cargo test

# Run integration tests against local PG (requires Docker)
docker run -d --name pg-test -p 54320:5432 \
  -e POSTGRES_PASSWORD=test -e POSTGRES_DB=tabularis_test postgres:16
cargo test --features integration

# Run parity tests (compares plugin output vs golden files)
cargo test --features parity

# Interactive REPL for debugging RPC calls
cargo run --bin test_plugin
> {"jsonrpc":"2.0","method":"get_tables","params":{"params":{...},"schema":"public"},"id":1}
```

### Testing in Tabularis

1. Build the plugin binary
2. Install to the plugins directory
3. Launch Tabularis
4. Create a new connection with driver "PostgreSQL (Next)"
5. Run the manual smoke test checklist (see Phase 1 Success Criteria)
6. Compare behavior with a parallel "PostgreSQL" (built-in) connection to the same database

---

## Risk Assessment

| Risk | Likelihood | Mitigation |
| ---- | ---------- | ---------- |
| **Performance regression** (JSON-RPC overhead) | Medium | Benchmark with large result sets. JSON serialization is fast for tabular data. Network latency to PG server dominates. Defer optimization unless measurably slow. |
| **Feature parity gaps missed** | Low (with Phase 0) | Phase 0's golden files and 50+ tests create a comprehensive contract. Gaps caught immediately via automated parity comparison. |
| **Plugin crash isolation** | Low | Plugin crash doesn't crash Tabularis. Host returns error. Can offer "Restart plugin" in UI. |
| **Typed binding fidelity** | High | Existing binding system handles 20+ PG types with CASTs. Port with per-type tests. This is the highest-risk area — must be methodical. |
| **PR 402 integration conflicts** | Medium | Build plugin against `main`. When 402 merges, adapt plugin (isolated codebase — no merge conflicts with Tabularis core). |
| **Core PR rejection** | Low | The 3 required RpcDriver changes are small, non-breaking additions. Same pattern as existing forwarded methods. |
| **Phase 0 scope creep** | Medium | Timebox Phase 0. The 50+ tests are the minimum viable baseline. Don't gold-plate — capture what's needed for parity proof, nothing more. |

---

## Open Questions

1. **Plugin id during development** — Use `"postgres-plugin"` during Phases 1-3.
   Decision on whether to rename to `"postgres"` deferred to Phase 4.

2. **Bundling strategy** — Should the PostgreSQL plugin be bundled with Tabularis
   app distribution (always available) or installed on-demand from the registry?
   Bundling ensures no regression for existing users on Phase 4 cutover.

3. **RPC adapter core PRs** — Phase 1 requires 3 Tabularis core changes to the
   `RpcDriver` (BLOB forwarding, materialized view forwarding, `map_inferred_type`
   resolution). Should these be submitted as a prerequisite PR before plugin
   development, or developed in parallel?

4. **BLOB protocol extension** — The RPC protocol has no binary data support.
   Proposed: base64-encode blob data in JSON responses. Is the size overhead
   (33% increase) acceptable? Alternative: shared temp file path exchange.

5. **Query cancellation protocol** — Should we propose a `cancel_query` RPC method
   to the plugin protocol? Without it, long queries are unkillable from the user's
   perspective (the task is aborted but the server query continues).

6. **PR 402 timing** — Should we wait for PR 402 to merge into main before
   starting Phase 2, or port its changes directly into the plugin from the PR
   branch? The latter avoids waiting but means maintaining a fork of 402's logic.

7. **Existing plugin ecosystem** — Are there any community PostgreSQL plugins
   already? Could we conflict with or build on existing work?

8. **Plugin versioning** — When the plugin ships updates independently of Tabularis,
   how do we ensure compatibility? Should the manifest declare a minimum Tabularis
   version?

9. **Phase 0 scope negotiation** — The 50+ integration tests in Phase 0 represent
   significant work. Can we parallelize Phase 0 and Phase 1 (build plugin scaffold
   while writing tests), or must Phase 0 fully complete first?

10. **Timeout configurability** — The 120s hard timeout will break long-running
    queries. Should we propose a manifest field (`call_timeout_seconds`) or a
    per-call timeout negotiation?
