# PostgreSQL Plugin Migration — Alternative: Multi-Database From Day One

**Ref:** [#16 — Better PostgreSQL Support](https://github.com/TabularisDB/tabularis/issues/16)
**Related:** [PR #402 — Multi-database connections](https://github.com/TabularisDB/tabularis/pull/402)
**Context:** Feedback suggesting multi-database support should be built in from the
start rather than added as a later phase.

## Executive Summary

This document explores the alternative approach of building the PostgreSQL plugin
with multi-database support from day one. After analysis, the conclusion is that
**the two approaches are architecturally equivalent** — a correctly-built plugin
inherently supports multi-database because the RPC protocol routes `params.database`
on every call. The plugin cannot function without reading this field.

However, the feedback raises a valid point about **test coverage and verification
confidence**. This alternative plan consolidates Phases 1 and 2 into a single
phase that tests multi-database from the beginning, eliminating any theoretical
risk of overlooking it.

The Phase 0 baseline test suite and zero-regression guarantee remain unchanged.

---

## Table of Contents

1. [Why Multi-Database Is Not a Separate Concern](#why-multi-database-is-not-a-separate-concern)
2. [What Changes vs. the Phased Plan](#what-changes-vs-the-phased-plan)
3. [Revised Phase Structure](#revised-phase-structure)
4. [Phase 0: Baseline Test Suite](#phase-0-baseline-test-suite)
5. [Phase 1: Plugin with Full Parity + Multi-Database (TDD)](#phase-1-plugin-with-full-parity--multi-database-tdd)
6. [Phase 2: Issue 16 Improvements](#phase-2-issue-16-improvements)
7. [Phase 3: Deprecate Built-in Driver](#phase-3-deprecate-built-in-driver-deferred)
8. [Why This Is Safe — Zero Regression Guarantee](#why-this-is-safe--zero-regression-guarantee)
9. [RPC Adapter Blockers](#rpc-adapter-blockers)
10. [Open Questions](#open-questions)

---

## Why Multi-Database Is Not a Separate Concern

The RPC protocol makes multi-database support **emergent from correct implementation**:

1. **Every RPC call includes `params.database`** — The host sets this to the target
   database before calling the plugin. The plugin must read it to connect at all.

2. **PostgreSQL requires per-database connections** — You cannot `USE other_db`
   mid-session. Each database needs its own TCP connection. This means the pool
   key MUST include the database name regardless of whether "multi-database" is a
   stated goal.

3. **The plugin is stateless between calls** — There is no "current database"
   concept in the plugin. Each call receives full connection parameters including
   the database to target.

4. **The host does all routing** — The frontend (PR 402) handles sidebar tree
   expansion, tab database tracking, and routing params construction. The plugin
   just connects to whatever it's told.

### What a Correctly-Built Plugin Pool Looks Like

```rust
// This is the ONLY correct implementation — it naturally supports multi-database
fn pool_key(params: &ConnectionParams) -> String {
    format!("{}:{}:{}:{}", params.host, params.port, params.database, params.user)
}

async fn get_or_create_pool(params: &ConnectionParams) -> Result<Pool, Error> {
    let key = pool_key(params);
    // Return existing pool for this database, or create a new one
    // ...
}
```

A developer building this plugin would write this code on day one because it's
the only way to connect to PostgreSQL. You cannot accidentally build a
single-database-only plugin — the protocol doesn't allow it.

### The Only Multi-Database-Specific Items

| Item | Effort | Why it's trivial |
| ---- | ------ | ---------------- |
| `get_databases` returns all databases | One SQL query | `SELECT datname FROM pg_database WHERE datallowconn` |
| Fall back to `"postgres"` maintenance DB | One-line default | `let db = params.database.or("postgres")` |
| `ref_schema` in ForeignKey results | One field in FK query | Add `nsp2.nspname AS ref_schema` to existing JOIN |

These are not architectural decisions — they're checklist completeness items that
belong alongside all other method implementations.

---

## What Changes vs. the Phased Plan

| Aspect | Original (Phases 1+2 separate) | This Alternative (Combined) |
| ------ | ------------------------------ | --------------------------- |
| Plugin build phases | Phase 1 (parity) → Phase 2 (multi-db) | Single Phase 1 (parity + multi-db) |
| Testing approach | Phase 0 tests single-db, Phase 2 adds multi-db tests | Phase 0 tests BOTH from the start |
| Pool implementation | Same code either way | Same code either way |
| Phase 0 scope | 50+ tests, single database | 55+ tests, includes multi-database scenarios |
| Total phases | 5 (0-4) | 4 (0-3) |
| Risk | Theoretical: could build single-db pools accidentally | Eliminated: tests catch it immediately |
| Phase 0 seed script | Single database | Two databases (test primary + test secondary) |

**The actual plugin code is identical.** The difference is purely in **test scope**
and **verification confidence** — which aligns exactly with the requirement for
zero-regression proof.

---

## Revised Phase Structure

```text
PREREQUISITE: 3 Tabularis Core PRs (RpcDriver fixes)
    ↓
Phase 0: Baseline test suite (includes multi-database scenarios)
    ↓
Phase 1: Build plugin "postgres-plugin" — full parity including multi-database
    ↓
Phase 2: Issue #16 improvements (sequences, JSONB editing, etc.)
    ↓
Phase 3: Deprecate built-in driver (deferred decision)
```

---

## Phase 0: Baseline Test Suite

Phase 0 is identical to the original plan with one key addition: the test seed
creates **two databases** and the test suite includes multi-database scenarios.

### Seed Script Addition

```sql
-- tests/fixtures/postgres_seed.sql

-- Primary test database (tabularis_test) — same as before
CREATE SCHEMA IF NOT EXISTS test_schema;
CREATE TABLE test_schema.all_types ( ... );
-- ... all existing seed tables ...

-- SECOND database for multi-database testing
-- (created via separate connection to maintenance DB)
CREATE DATABASE tabularis_test_secondary;

-- In tabularis_test_secondary:
CREATE SCHEMA IF NOT EXISTS secondary_schema;
CREATE TABLE secondary_schema.remote_lookup (
    id SERIAL PRIMARY KEY,
    code TEXT UNIQUE
);
```

### Additional Multi-Database Tests (Added to Phase 0)

```text
tests/integration/postgres/
└── multi_database.rs
    ├── test_get_databases_lists_both
    ├── test_get_schemas_on_secondary_database
    ├── test_get_tables_on_secondary_database
    ├── test_execute_query_on_secondary_database
    ├── test_pool_reuse_same_database
    ├── test_pool_isolation_different_databases
    └── test_fallback_to_postgres_maintenance_db
```

### Phase 0 Success Criteria (Updated)

- [ ] All existing integration tests pass in CI (un-ignored, PG service running)
- [ ] 55+ new integration tests covering full API surface + multi-database
- [ ] Golden files captured for every public method
- [ ] Multi-database golden files (schemas/tables from secondary database)
- [ ] Parity harness infrastructure ready
- [ ] Seed script creates TWO databases with comprehensive test schemas
- [ ] CI runs in < 5 minutes with PG service

---

## Phase 1: Plugin with Full Parity + Multi-Database (TDD)

### Phase 1 Goal

A standalone Rust plugin that implements every method the built-in PostgreSQL
driver supports — including multi-database routing — passing the same test suite
that validates the built-in driver. Built iteratively using Test-Driven Development:
one method at a time, watching tests go from red to green.

### TDD Workflow

Phase 0 produces a test suite that passes against the built-in driver. At the
start of Phase 1, the same suite is pointed at the plugin. Every test is RED
because the plugin doesn't exist yet. Implementation proceeds method by method:

```text
START: 0/55 tests GREEN (plugin binary doesn't exist)

Sprint 1 — Foundation (scaffold + connection)
─────────────────────────────────────────────
  cargo init → main.rs with JSON-RPC loop → rpc.rs router
  Implement: initialize, ping, test_connection, shutdown
  Run tests → 3/55 GREEN (connection tests pass)

Sprint 2 — Schema Discovery
────────────────────────────
  Implement: get_databases, get_schemas, get_tables
  Run tests → 8/55 GREEN

Sprint 3 — Column & Key Metadata
──────────────────────────────────
  Implement: get_columns, get_indexes, get_foreign_keys
  Port: extract/ submodules (needed for type-aware column reading)
  Run tests → 18/55 GREEN

Sprint 4 — Query Execution
───────────────────────────
  Implement: execute_query, execute_query_batch, count_query
  Port: extract/ for result value extraction (all PG types)
  Run tests → 26/55 GREEN

Sprint 5 — CRUD Operations
───────────────────────────
  Implement: insert_record, update_record, delete_record
  Port: binding.rs (enum CASTs, UUID handling, array bindings)
  Run tests → 35/55 GREEN

Sprint 6 — Views & Materialized Views
───────────────────────────────────────
  Implement: get_views, get_view_definition, get_view_columns,
             create_view, alter_view, drop_view,
             get_materialized_views, get_mv_definition,
             get_mv_columns, refresh_materialized_view
  Run tests → 41/55 GREEN

Sprint 7 — Routines & Triggers
───────────────────────────────
  Implement: get_routines, get_routine_parameters,
             get_routine_definition, build_routine_call_sql,
             routine_create_template, get_routine_edit_script,
             drop_routine, get_triggers, get_trigger_definition,
             create_trigger, drop_trigger, update_trigger
  Run tests → 48/55 GREEN

Sprint 8 — DDL, EXPLAIN, BLOB
──────────────────────────────
  Implement: get_create_table_sql, get_add_column_sql,
             get_alter_column_sql, get_create_index_sql,
             drop_index, get_create_foreign_key_sql, drop_foreign_key,
             explain_query_plan, save_blob_to_file,
             fetch_blob_as_data_url, get_ai_schema_context
  Run tests → 53/55 GREEN

Sprint 9 — Multi-Database & Polish
────────────────────────────────────
  Verify: get_databases returns both test DBs
  Verify: queries route to correct database
  Verify: ref_schema populated in FK results
  Fix: any remaining failures, edge cases
  Run tests → 55/55 GREEN ✅

DONE: All tests green. Run golden file comparison. Run manual smoke test.
```

### The Red → Green Discipline

At each sprint:

1. **Run the full parity suite** — see exactly which tests are RED
2. **Pick the next batch of related methods** — implement them
3. **Run again** — confirm new tests are GREEN, nothing regressed
4. **Commit** — each commit message references which tests it turns green

```bash
# Developer workflow at each sprint
cargo build --release
cp target/release/postgres-plugin ~/Library/Application\ Support/tabularis/plugins/postgres-plugin/

# Run parity suite against plugin
cargo test --features parity -- --nocapture
# Output: 26/55 passed, 29 failed (EXPECTED — haven't built those yet)

# After implementing next batch:
cargo test --features parity -- --nocapture
# Output: 35/55 passed, 20 failed (PROGRESS — 9 new tests green)

# Verify no regressions:
# Previously-green tests must stay green. If one goes RED, fix before moving on.
```

### What This Guarantees

| Guarantee | Mechanism |
| --------- | --------- |
| No method is forgotten | Every method has a test from Phase 0. If the test is still RED, the method isn't done. |
| No silent regressions | The full suite runs at every sprint. A previously-GREEN test going RED is immediately visible. |
| Progress is measurable | "35/55 green" is an objective, unambiguous progress metric. |
| Parity is proven, not claimed | The same test produces the same assertion against both drivers. If it passes on both, they are equivalent by construction. |
| Implementation order is flexible | Sprints above are a suggested order. If a different order is easier, the tests don't care — they just need to all be GREEN eventually. |

### What's Different From Original Phase 1

| Original Phase 1 | This Phase 1 |
| ----------------- | ------------ |
| Build plugin, then run tests | Tests exist first, guide implementation |
| `get_databases` not required | `get_databases` implemented and tested |
| No multi-db tests in parity suite | Multi-db tests included in parity suite |
| `ref_schema` not in FK results | `ref_schema` included from the start |
| Pool tested with one database | Pool tested with multiple databases |
| Progress measured by checklist | Progress measured by test count (objective) |

### Plugin Structure

```text
plugins/postgres-plugin/
├── .tabularium
├── Cargo.toml
├── src/
│   ├── main.rs                    # JSON-RPC stdin/stdout loop
│   ├── rpc.rs                     # Method dispatch router
│   ├── models.rs                  # ConnectionParams, shared types
│   ├── pool.rs                    # deadpool-postgres, keyed by host:port:db:user
│   ├── handlers/
│   │   ├── metadata.rs            # get_tables, get_columns, get_databases, etc.
│   │   ├── query.rs               # execute_query, execute_query_batch
│   │   ├── crud.rs                # insert_record, update_record, delete_record
│   │   ├── ddl.rs                 # get_create_table_sql, get_add_column_sql, etc.
│   │   ├── routines.rs            # get_routines, build_routine_call_sql, etc.
│   │   ├── explain.rs             # explain_query_plan
│   │   └── blob.rs                # save_blob_to_file, fetch_blob_as_data_url
│   ├── binding.rs                 # Typed parameter binding (enum CAST, etc.)
│   ├── extract/                   # Value extraction from PG rows
│   │   ├── mod.rs
│   │   ├── simple.rs
│   │   ├── array.rs
│   │   ├── range.rs
│   │   ├── multi_range.rs
│   │   ├── composite.rs
│   │   ├── enum_type.rs
│   │   └── advanced.rs
│   └── types.rs                   # 97+ data type declarations
└── tests/
    ├── metadata_test.rs
    ├── query_test.rs
    ├── crud_test.rs
    ├── ddl_test.rs
    └── multi_database_test.rs
```

### Phase 1 Success Criteria — Zero Wiggle Room

Phase 1 is **not done** until:

1. **55/55 parity tests GREEN** — Including multi-database tests. Zero RED.
   This is binary: either all pass or it's not done.

2. **Golden file comparison passes** — Plugin output matches built-in output
   byte-for-byte for every captured method response.

3. **Manual smoke test checklist** (all pass):
   - [ ] Connect to PG via host/port
   - [ ] Connect via connection string
   - [ ] Connect via SSL (all modes)
   - [ ] Browse schemas in sidebar
   - [ ] Browse tables, views, materialized views, routines, triggers
   - [ ] Execute SELECT with all PG types
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
   - [ ] Multi-db: browse second database in sidebar
   - [ ] Multi-db: execute query against second database
   - [ ] Multi-db: get_schemas returns schemas from correct database
   - [ ] Multi-db: FK with ref_schema navigates cross-schema

4. **No regressions in existing frontend tests** — `pnpm test` passes unchanged.

---

## Phase 2: Issue 16 Improvements

Identical to original plan's Phase 3. Now Phase 2 since multi-db is absorbed
into Phase 1.

**Important:** Before implementing any feature, check for existing open PRs that
already address it. Known in-flight: PR #427 (hstore editing), PR #222 (composite
PK). See `03-phase-2-issue-16.md` for the full coordination process.

| Priority | Item |
| -------- | ---- |
| High | Sequence management (list, inspect, alter, reset) |
| High | JSONB inline editing (object/array manipulation) |
| High | Extension-aware type system (PostGIS, pgvector, ltree, hstore — **see PR #427**) |
| Medium | Partition table introspection |
| Medium | Row-level security policy display |
| Medium | Publication/subscription visibility |
| Medium | Advisory lock monitoring |
| Low | Query plan cost visualization improvements |
| Low | Table statistics (pg_stat_user_tables) display |

---

## Phase 3: Deprecate Built-in Driver (Deferred)

Identical to original plan's Phase 4. Decision deferred until Phase 1 parity is
proven.

---

## Why This Is Safe — Zero Regression Guarantee

The safety model has three layers:

### Layer 1: Golden File Parity (Automated)

Every public method's output is captured as a golden file against the built-in
driver. The plugin must produce byte-for-byte identical output. This runs in CI
on every commit.

```text
Built-in: get_columns("all_types", "test_schema") → golden/get_columns_all_types.json
Plugin:   get_columns("all_types", "test_schema") → must match exactly
```

### Layer 2: Integration Test Suite (Automated)

55+ tests exercise every API method with real PostgreSQL. Parameterized to run
against both built-in and plugin. Any difference = test failure = CI red.

```rust
#[test_case("postgres"; "built-in driver")]
#[test_case("postgres-plugin"; "plugin driver")]
async fn test_insert_with_enum_cast(driver: &str) {
    // Same test, same assertions, both drivers must produce identical results
}
```

### Layer 3: Manual Smoke Test (Human Verification)

24-item checklist performed manually before any release. Covers UX flows that
automated tests can't fully validate (sidebar navigation, inline editing feel,
error message quality).

### What This Catches

| Failure Mode | Caught By |
| ------------ | --------- |
| Missing method (returns -32601) | Golden file test fails (no output vs expected) |
| Wrong result shape | Golden file byte comparison fails |
| Type extraction bug (e.g., array renders differently) | Integration test + golden file |
| Pool keying error (wrong database) | Multi-database integration tests |
| Session state lost in batch | Batch integration tests (temp tables, SET) |
| Startup script not executed | Dedicated integration test |
| BLOB not working | BLOB round-trip integration test |
| Enum CAST missing (silent data corruption) | CRUD integration test with enum type |
| SSL connection failure | SSL integration test |
| Performance regression | Benchmark suite (separate, optional) |

---

## RPC Adapter Blockers

Identical to the original plan. These 3 Tabularis core PRs are prerequisites:

| Issue | Resolution |
| ----- | ---------- |
| BLOB methods not forwarded | Extend RpcDriver to forward `save_blob_to_file` / `fetch_blob_as_data_url` (base64 over JSON) |
| Materialized views not forwarded | Extend RpcDriver to forward 4 MV methods |
| `map_inferred_type` not forwarded | Plugin declares mappings at `initialize`; host applies locally |

Additionally, the plugin must handle these internally:

| Issue | Plugin-Side Resolution |
| ----- | --------------------- |
| Query cancellation | Implement `pg_cancel_backend()` or connection drop internally |
| `execute_query_batch` session state | Use single connection for entire batch |
| Startup script execution | `after_connect` hook in internal pool |
| 120s hard timeout | Document limitation; propose configurable timeout later |

---

## Open Questions

1. **Core PRs timing** — Should the 3 RpcDriver fixes be submitted before or
   during Phase 0 development? They can be parallelized.

2. **PR 402 merge dependency** — The multi-database frontend routing lives in
   PR 402. If it hasn't merged by the time Phase 1 is ready, multi-database
   testing can only be done at the RPC level (calling the plugin directly), not
   through the full Tabularis UI. Is RPC-level verification sufficient for the
   multi-db smoke tests?

3. **Bundling strategy** — Should the plugin be bundled with Tabularis distribution
   or installed from registry?

4. **BLOB protocol** — Base64 over JSON (33% overhead) vs shared temp files?

5. **Query cancellation** — Add a `cancel_query` RPC method to the protocol?

6. **Plugin versioning** — Manifest field for minimum compatible Tabularis version?

7. **Phase 0 parallelization** — Can Phase 0 test writing and Core PRs happen
   simultaneously? (Yes — they touch different code.)

---

## Comparison: This Plan vs. Original Phased Plan

| Dimension | Original (5 phases) | This Alternative (4 phases, TDD) |
| --------- | ------------------- | -------------------------------- |
| Methodology | Build first, test after | Tests first, build to pass them (TDD) |
| Plugin code | Identical | Identical |
| Pool architecture | Same | Same |
| Test coverage | Multi-db added in Phase 2 | Multi-db tested from Phase 0 |
| Confidence in multi-db | Proven in Phase 2 | Proven in Phase 1 |
| Progress tracking | Checklist-based (subjective) | Test count (0/55 → 55/55, objective) |
| Regression detection | End-of-phase verification | Every sprint (previously-green must stay green) |
| Implementation order | Implicit (build everything, then test) | Explicit sprints, flexible ordering |
| Total effort | Same | Same (7 extra tests in Phase 0) |
| Risk of parity gap | Detected at end of Phase 1 | Detected immediately at each sprint |
| Simpler to explain | 5 phases with small Phase 2 | 4 phases, TDD-driven, each substantive |

**Bottom line:** This plan is better because it gives continuous, objective proof
of progress and catches regressions at every step — not just at the end. The test
suite IS the specification. Implementation is done when all tests are green.
