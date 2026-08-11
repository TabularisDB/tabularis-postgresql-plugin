# PostgreSQL Driver Improvements — Feature Gap Audit & Implementation Plan

**Ref:** [#16 — Better PostgreSQL Support](https://github.com/TabularisDB/tabularis/issues/16)
**Related:** [#15 — Schema handling fix (closed)](https://github.com/TabularisDB/tabularis/issues/15),
[PR #342 — Materialized views (merged)](https://github.com/TabularisDB/tabularis/pull/342),
[PR #402 — Multi-database connections (open, in progress)](https://github.com/TabularisDB/tabularis/pull/402)

## Executive Summary

A comprehensive audit of the PostgreSQL driver (`src-tauri/src/drivers/postgres/`)
reveals a **highly mature implementation** — the most complete of the three built-in
drivers. It implements 100% of the `DatabaseDriver` trait, supports 70+ data types
across 14 categories, and handles PostgreSQL-specific complexities (enum CASTs,
composite types, range/multi-range extraction, overloaded routine management, and
schema-qualified identifiers).

However, several PostgreSQL capabilities that are standard in professional database
tools remain unimplemented. Issue 16 explicitly calls out **sequences**, **JSONB
editing**, and **schema handling** — the last of which is resolved. This document
identifies 3 active bugs, 6 feature gaps, and 5 polish items, organized into a
prioritized implementation plan.

---

## Table of Contents

1. [Dependency: PR 402 — Multi-Database Connections](#dependency-pr-402--multi-database-connections)
2. [Audit Methodology](#audit-methodology)
3. [Current State](#current-state)
4. [Findings: Active Bugs](#findings-active-bugs)
5. [Findings: Feature Gaps](#findings-feature-gaps)
6. [Findings: Polish & Enhancements](#findings-polish--enhancements)
7. [Findings: Out of Scope](#findings-out-of-scope)
8. [Feature Comparison Matrix](#feature-comparison-matrix)
9. [Implementation Plan](#implementation-plan)
10. [Testing Strategy](#testing-strategy)
11. [Open Questions](#open-questions)

---

## Dependency: PR 402 — Multi-Database Connections

[PR #402](https://github.com/TabularisDB/tabularis/pull/402) is an in-flight PR
by debba that adds multi-database browsing to PostgreSQL connections. It is the
foundational architecture change that all work in this plan should build on top of.

### What PR 402 Delivers

PR 402 allows a single PostgreSQL connection to browse multiple databases — each
with its own schemas — from the sidebar. Key architectural changes:

- **Per-database connection pools** — Pool key becomes `driver:conn:{id}:{db}`,
  with separate pools for each selected database
- **`database: Option<String>` routing** — Every Tauri command now accepts an
  optional `database` parameter; when set, the backend overrides `params.database`
  to route to the correct pool
- **Editor tabs carry `database`** — Each tab stores its target database alongside
  schema, so DML routes to the correct pool regardless of sidebar state
- **`buildTableRoutingParams` helper** — Frontend utility that builds the
  `{ schema, database }` pair from a tab's context for any backend call
- **`isSchemaBasedMultiDb` helper** — Distinguishes hierarchical PG layout
  (`database → schema → table`) from flat MySQL layout (`database → table`)
- **`SchemaData` nesting** — `databaseDataMap` entries now optionally contain
  `schemas: string[]` and `schemaDataMap: Record<string, SchemaData>` for
  the hierarchical PG model

### PR 402 Status

| Aspect | State |
|--------|-------|
| Branch state | Open, has merge conflicts with `main` |
| Last activity | 2026-07-01 (14 commits, 60+ files changed) |
| Tests | 2730 frontend + 766 Rust passing at last push |
| Verification checklist | 4/69 items checked |
| Formal reviews | None submitted yet |

### PR 402 Remaining Known Limitations

These are gaps that PR 402 explicitly declares as out of scope. Some overlap
with our plan and some are purely routing issues that need follow-up work:

#### Routing Gaps (Still Need Fixing After 402 Merges)

| Gap | Description | Overlap with Our Plan |
|-----|-------------|----------------------|
| **Object-creation DDL not database-aware** | Create Table / View / Trigger / Index / FK from a nested schema node routes to the primary database, not the node's database | Affects our Enhancement 4 (schema/DB management) — any new DDL commands must be database-aware |
| **AI Query Generation not database-aware** | `AiQueryModal` schema context uses primary database only | Not in our scope |
| **Clipboard Import not database-aware** | Import creates table on primary database regardless of context | Not in our scope |
| **SQL autocomplete not database-aware** | `get_columns` for autocomplete runs against primary pool | Not in our scope but good to note |
| **No pool cap or idle eviction** | Each selected database keeps max 10 connections indefinitely | Operational concern for our work (large schemas with many DBs) |

#### Features Explicitly Out of Scope in 402

PR 402's own checklist confirms these are NOT implemented and left for follow-up
(directly aligning with our plan):

| Feature | Our Plan Item |
|---------|---------------|
| Sequences (first-class management) | **Gap 1** — our primary deliverable |
| Custom types / Enums / Domains | **Enhancement 3** |
| Extensions (PostGIS, hstore, …) | **Gap 5** |
| Check / Unique constraints (dedicated listing) | Not in our plan (low priority) |
| CREATE/DROP DATABASE, CREATE/DROP SCHEMA | **Enhancement 4** |
| TRUNCATE / RENAME table | Not in our plan |
| Query cancellation via `pg_cancel_backend` | Not in our plan |
| Materialized views | Already delivered in PR 342 |

### Impact on Our Implementation

#### What 402 Fixes That We Originally Identified

**Bug 516 (Wrong Schema in DML)** — PR 402 directly addresses this class of bug.
The fix is that editor tabs now carry `database` alongside `schema`, and the
`buildTableRoutingParams` helper ensures DML operations route to the tab's stored
context rather than the sidebar's globally-selected schema. The specific commit
"fix: route results-grid operations to the tab's database pool" (86640146) fixed
the exact pattern: Ctrl+S commit was sending the schema name as a database name
on schema-based drivers, routing to the wrong pool.

**Verdict:** Bug 516 should be **verified after PR 402 merges** rather than fixed
independently. If it persists, it would be a residual routing bug in 402's model
(unlikely given the thorough fix commits).

#### What Our Plan Must Do Differently

1. **All new Tauri commands must include `database: Option<String>`** and apply
   the standard routing pattern:

   ```rust
   let mut params = resolve_connection_params_with_id(&expanded_params, &connection_id)?;
   if let Some(db) = database.filter(|d| !d.is_empty()) {
       params.database = crate::models::DatabaseSelection::Single(db);
   }
   ```

2. **All new frontend invocations must pass `database`** from the tab or sidebar
   context using `buildTableRoutingParams` or equivalent.

3. **New sidebar groups (Sequences, Extensions, Types) must propagate `database`**
   down to their child items the same way `SidebarSchemaItem` propagates it to
   tables, views, routines, and triggers.

4. **Rebase on 402 before starting implementation** — our branch should be based
   on the post-402 state of `main` to avoid conflict in `commands.rs` (182
   additions), `DatabaseContext.ts`, `DatabaseProvider.tsx`, and `Editor.tsx`.

---

## Audit Methodology

The audit compared the PostgreSQL driver against:

- The full `DatabaseDriver` trait definition in `src-tauri/src/drivers/driver_trait.rs`
- The MySQL driver (`src-tauri/src/drivers/mysql/mod.rs`) for feature parity baseline
- PostgreSQL's own catalog (`pg_catalog`) and `information_schema` capabilities
- Open GitHub issues tagged with PostgreSQL-related keywords
- Professional database tool standards (pgAdmin, DBeaver, DataGrip)

**Source files reviewed:**

| File | Lines | Purpose |
|------|-------|---------|
| `src-tauri/src/drivers/postgres/mod.rs` | ~2500 | Main driver implementation |
| `src-tauri/src/drivers/postgres/binding.rs` | — | Value binding for parameterized queries |
| `src-tauri/src/drivers/postgres/client.rs` | — | Pool client wrappers |
| `src-tauri/src/drivers/postgres/explain.rs` | — | EXPLAIN plan parsing |
| `src-tauri/src/drivers/postgres/export.rs` | — | Streaming query export |
| `src-tauri/src/drivers/postgres/extract/` | 7 files | Value extraction (simple, array, composite, enum, range, multi_range, advanced) |
| `src-tauri/src/drivers/postgres/helpers.rs` | — | Identifier escaping, enum type handling |
| `src-tauri/src/drivers/postgres/routines.rs` | — | Stored routine SQL builders |
| `src-tauri/src/drivers/postgres/types.rs` | 838 | Data type catalog (70+ types) |
| `src-tauri/src/drivers/postgres/tests.rs` | — | Unit tests |
| `src-tauri/src/drivers/driver_trait.rs` | ~600 | Trait definition and capabilities |
| `src-tauri/src/commands.rs` | — | Tauri command layer |
| `src/types/plugins.ts` | — | Frontend capability types |
| `src/contexts/DatabaseContext.ts` | — | Schema data model |

---

## Current State

### What Works Correctly

The PostgreSQL driver fully implements:

| Category | Features |
|----------|----------|
| **Connection** | Pool-based (`deadpool-postgres`), SSL/TLS, SSH tunneling, connection string import, configurable search_path |
| **Schema Inspection** | Multi-schema browsing, tables, columns (with enum values, max length), FKs (with update/delete rules), indexes |
| **Views** | List, create (CREATE VIEW), alter (CREATE OR REPLACE VIEW), drop, column introspection |
| **Materialized Views** | List, columns, indexes, definition, refresh, read-only grid enforcement |
| **Routines** | List functions/procedures, parameters, full definition (`pg_get_functiondef`), call/create/edit/drop (overload-safe via identity arguments) |
| **Triggers** | List (with event aggregation), definition (`pg_get_triggerdef`), create, drop |
| **CRUD** | Type-aware insert/update/delete with enum CAST, JSON/JSONB, BLOB, DEFAULT VALUES, composite PK binding |
| **DDL** | CREATE TABLE, ADD COLUMN, ALTER COLUMN (with USING clause for incompatible casts), CREATE INDEX, CREATE FK, schema-qualified DROP |
| **Query Execution** | Paginated SELECT (LIMIT+1 pattern), batch on single client (session-safe), cancellation via CancellationToken |
| **EXPLAIN** | FORMAT JSON with ANALYZE and BUFFERS options, parsed plan tree |
| **BLOB** | Save BYTEA to file, preview as data URL |
| **Type Extraction** | Enums, JSON/JSONB, arrays (nested), composites, ranges, multi-ranges, HSTORE (read), network types, geometric types, FTS types, system types |
| **Compatibility** | PG 9.x/10 support (prokind fallback for pre-11 servers) |

### Declared Capabilities

```rust
DriverCapabilities {
    schemas: true,                  // Multi-schema support
    single_database: false,         // Multiple databases
    views: true,                    // Full view lifecycle
    materialized_views: true,       // PG-exclusive
    routines: true,                 // Function/procedure listing
    routine_management: true,       // Full routine CRUD
    file_based: false,              // Network driver
    folder_based: false,
    connection_string: true,        // postgres://user:pass@host:port/db
    identifier_quote: "\"",         // Double-quote identifiers
    alter_primary_key: true,        // ALTER TABLE PK modification
    serial_type: "SERIAL",          // Type-replacement auto-increment
    auto_increment_keyword: "",     // No keyword (uses SERIAL types)
    inline_pk: false,
    alter_column: true,             // ALTER COLUMN support
    create_foreign_keys: true,      // FK creation
    manage_tables: true,            // Full table DDL
    explain: true,                  // EXPLAIN plan visualization
    readonly: false,
    triggers: true,                 // Trigger management
    supports_ssl: true,             // SSL/TLS configuration
    sql_dialect: Postgres,          // PG-specific statement splitting
}
```

---

## Findings: Active Bugs

### Bug 1: Wrong Schema Name in DML Submission

**Issue:** [#516](https://github.com/TabularisDB/tabularis/issues/516)

**Status:** ⚠️ **Likely resolved by PR 402** — verify after merge

**The Problem:**

When a user has tables with the same name in different schemas (e.g.,
`schemaA.app_settings` and `schemaB.app_settings`), editing a row in one schema
may generate DML that targets the wrong schema. The UPDATE/DELETE statement
references `schemaB.app_settings` when the user was editing `schemaA.app_settings`.

**Why PR 402 Likely Fixes This:**

PR 402 introduces per-tab `database` + `schema` routing. The specific commit
(86640146) fixed a regression where `isMultiDatabaseCapable` now including Postgres
caused the flat-driver fallback to send the PostgreSQL schema name as the database
parameter — routing every update/insert/delete to a pool for a database literally
named after the schema. The fix gates this with `!isSchemaBasedConn` and routes
via `buildTableRoutingParams` which uses `activeTab.database` and `activeTab.schema`.

**Action Required:**

After PR 402 merges, reproduce the bug (same table name in two schemas, edit in
schema A, verify DML targets schema A). If it persists, the fix is to ensure the
editor tab captures its schema at open time and never resolves dynamically from
the sidebar.

**Complexity:** None (verify only) or Low (residual fix if needed)

---

### Bug 2: Visual Query Builder Fails on Reserved-Word Table Names

**Issue:** [#335](https://github.com/TabularisDB/tabularis/issues/335)

**The Problem:**

When a table is named with a PostgreSQL reserved word (e.g., `user`, `order`,
`group`), the Visual Query Builder generates unquoted identifiers:

```sql
-- Generated (broken):
SELECT user.name FROM user

-- Correct:
SELECT "user"."name" FROM "user"
```

**Root Cause:**

The Visual Query Builder's SQL generation does not apply identifier quoting. The
`identifier_quote` capability is declared (`"\""`), but the Visual Query Builder
bypasses it.

**Impact:** Visual Query Builder is unusable for any table with a reserved-word name.

**Severity:** MEDIUM

**Fix Direction:**

The Visual Query Builder's SQL generation must quote all identifiers using the
driver's `identifier_quote` value. This applies to table names, column names,
schema names, and aliases. The safest approach is to always quote — this is valid
SQL regardless of whether the name is reserved.

**Complexity:** Low-Medium (Visual Query Builder SQL generation)

---

### Bug 3: Foreign Keys Not Visible with Restricted Privileges

**Issue:** [#96](https://github.com/TabularisDB/tabularis/issues/96)

**The Problem:**

Users with read-only grants (`GRANT SELECT ON ALL TABLES`) cannot see foreign
keys. The `get_foreign_keys` function queries `pg_constraint` which requires
additional privileges beyond SELECT on the user tables.

**Root Cause:**

The FK query uses:

```sql
FROM pg_constraint con
JOIN pg_class cls ON cls.oid = con.conrelid
JOIN pg_namespace ns ON ns.oid = cls.relnamespace
...
WHERE con.contype = 'f'
```

Access to `pg_constraint` requires `USAGE` on the schema AND visibility into
the constraint's owning table in `pg_class`. A strictly read-only user with only
`SELECT` grants may not have the necessary catalog visibility.

**Impact:** FKs appear to not exist for users with limited permissions.

**Severity:** LOW-MEDIUM

**Fix Direction:**

Provide a fallback query using `information_schema.referential_constraints` +
`information_schema.key_column_usage`, which respects standard SQL privilege
rules. Try the `pg_constraint` query first (it returns richer data including
update/delete rules), and fall back to the information_schema approach if the
primary query returns zero results or errors.

```sql
-- Fallback query:
SELECT
    tc.constraint_name,
    kcu.column_name,
    ccu.table_schema AS referenced_schema,
    ccu.table_name AS referenced_table,
    ccu.column_name AS referenced_column,
    rc.update_rule,
    rc.delete_rule
FROM information_schema.table_constraints tc
JOIN information_schema.key_column_usage kcu
    ON tc.constraint_name = kcu.constraint_name
    AND tc.table_schema = kcu.table_schema
JOIN information_schema.constraint_column_usage ccu
    ON ccu.constraint_name = tc.constraint_name
    AND ccu.table_schema = tc.table_schema
JOIN information_schema.referential_constraints rc
    ON rc.constraint_name = tc.constraint_name
    AND rc.constraint_schema = tc.table_schema
WHERE tc.constraint_type = 'FOREIGN KEY'
    AND tc.table_schema = $1
    AND tc.table_name = $2
```

**Complexity:** Medium (fallback logic + testing with restricted users)

---

## Findings: Feature Gaps

### Gap 1: Sequences — Browse, Inspect, Manage

**Priority:** HIGH — Explicitly named in issue 16

**What PostgreSQL Provides:**

Sequences are first-class objects in PostgreSQL. They power `SERIAL`/`BIGSERIAL`
columns, `GENERATED AS IDENTITY`, and can be used standalone for custom ID
generation across tables.

**Catalog Sources:**

- `pg_sequences` view (PG 10+): name, schema, data_type, start, min, max,
  increment, cycle, cache_size, last_value
- `information_schema.sequences` (less detailed)
- `pg_class WHERE relkind = 'S'` (pre-PG10 fallback)

**Proposed Feature Set:**

| Operation | SQL | UI Location |
|-----------|-----|-------------|
| List sequences | `SELECT * FROM pg_sequences WHERE schemaname = $1` | Sidebar → "Sequences" group per schema |
| View properties | `SELECT * FROM pg_sequences WHERE schemaname = $1 AND sequencename = $2` | Properties panel or context menu → "Show Details" |
| View current value | `SELECT last_value FROM schema.sequence_name` | Shown in properties |
| Alter (restart) | `ALTER SEQUENCE schema.seq RESTART WITH n` | Context menu → "Restart…" with value input |
| Alter (properties) | `ALTER SEQUENCE schema.seq INCREMENT BY n MINVALUE m MAXVALUE M CACHE c [NO] CYCLE` | Edit dialog |
| Create | `CREATE SEQUENCE schema.name [AS type] [START WITH n] [INCREMENT BY n] ...` | Context menu on "Sequences" group → "New Sequence" |
| Drop | `DROP SEQUENCE IF EXISTS schema.name [CASCADE]` | Context menu → "Drop Sequence" with confirmation |
| Show DDL | Reconstruct `CREATE SEQUENCE` from metadata | Context menu → "Show Definition" |

**Implementation Requirements:**

1. **Backend (Rust):**
   - New model: `SequenceInfo { name, schema, data_type, start_value, min_value, max_value, increment_by, cycle, cache_size, last_value, owner_table, owner_column }`
   - New functions in `postgres/mod.rs`: `get_sequences`, `get_sequence_details`, `create_sequence`, `alter_sequence`, `drop_sequence`, `restart_sequence`, `get_sequence_ddl`
   - New trait methods with default impls (empty/error) to avoid breaking other drivers
   - New capability flag: `sequences: bool` in `DriverCapabilities`
   - New Tauri commands: `get_sequences`, `get_sequence_details`, `create_sequence`, `alter_sequence`, `drop_sequence`, `restart_sequence`

2. **Frontend (TypeScript/React):**
   - New type: `SequenceInfo` in `src/types/schema.ts`
   - Extend `SchemaData` interface to include `sequences?: SequenceInfo[]`
   - New sidebar group in `SidebarSchemaItem` (between "Tables" and "Views")
   - New component: `SidebarSequenceItem`
   - Context menu actions: Show Details, Restart, Drop
   - Sequence creation dialog
   - Gate on `capabilities.sequences`

3. **Localization:**
   - Add keys to all 8 locale files (en, de, es, fr, it, ja, ru, zh)

**Owner relationship:** Also display which table/column owns a sequence (via
`pg_depend` joining `pg_class` to `pg_attrdef`). This helps users understand
the link between `users.id SERIAL` and `users_id_seq`.

**Complexity:** High (new schema object type end-to-end)

---

### Gap 2: HSTORE Write Support

**Priority:** HIGH — Issue [#395](https://github.com/TabularisDB/tabularis/issues/395) is open

**Current State:**

- **Read:** Works correctly. `extract/simple.rs` line 71 deserializes HSTORE
  to `HashMap<String, Option<String>>` → JSON object via serde.
- **Write:** Not implemented. `binding.rs` has no HSTORE path. Users cannot
  insert or update HSTORE columns through the data grid.

**What PostgreSQL Expects:**

HSTORE values are written as text literals: `'"key1"=>"value1", "key2"=>"value2"'`

Or via the `hstore()` function: `hstore(ARRAY['key1','key2'], ARRAY['val1','val2'])`

**Implementation:**

In `binding.rs`, add a match arm for HSTORE columns:

```rust
// When the incoming value is a JSON object and the column type is hstore:
serde_json::Value::Object(map) => {
    if is_hstore_column {
        // Serialize to PostgreSQL hstore literal format
        let hstore_literal = map.iter()
            .map(|(k, v)| {
                let val = match v {
                    serde_json::Value::Null => "NULL".to_string(),
                    serde_json::Value::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
                    other => format!("\"{}\"", other),
                };
                format!("\"{}\"=>{}", k.replace('\\', "\\\\").replace('"', "\\\""), val)
            })
            .collect::<Vec<_>>()
            .join(", ");
        // Bind as TEXT, let PostgreSQL cast to hstore
        separated.push_bind(hstore_literal);
    }
}
```

**Alternatively:** Use a simple TEXT bind with explicit CAST:

```sql
UPDATE table SET hstore_col = $1::hstore WHERE pk = $2
```

This requires knowing the column is HSTORE at bind time — similar to the existing
enum CAST logic in `get_enum_column_types`.

**Frontend Enhancement (optional but valuable):**

A key/value editor UI for HSTORE cells (similar to the JSON tree editor but
simpler — flat key→value pairs only, both strings). Since HSTORE values are
already extracted as JSON objects, the existing `json-edit-react` component could
be reused with constraints (no nesting, values are always strings or null).

**Complexity:** Medium (binding logic + type detection; optional UI enhancement)

---

### Gap 3: Structured JSONB Editing

**Priority:** MEDIUM — Explicitly named in issue 16

**Current State:**

- **Read:** Perfect. JSON/JSONB values are extracted as structured `serde_json::Value`
  and displayed in a tree editor (`json-edit-react` component in `JsonTreeView.tsx`).
- **Write (full replace):** Works. Users can edit the full JSON text and submit.
- **Write (path-based):** Not supported. Users cannot update a single key within
  a JSONB document without replacing the entire value.

**What PostgreSQL Provides:**

```sql
-- Path-based update (PG 14+):
UPDATE t SET data = jsonb_set(data, '{address,city}', '"Berlin"') WHERE id = 1;

-- Remove a key:
UPDATE t SET data = data - 'deprecated_key' WHERE id = 1;

-- Deep merge (PG 16+):
UPDATE t SET data = data || '{"new_key": "value"}' WHERE id = 1;
```

**Proposed Enhancement:**

This is primarily a **frontend** improvement. When a user edits a single key in
the JSON tree editor:

1. Detect which path was modified (the tree editor already tracks this)
2. Instead of sending the entire document as a replacement, send a
   path-based update command
3. Backend generates `jsonb_set()` for the specific path

**Benefits:**

- Avoids overwriting concurrent changes to other keys in the same document
- More efficient for large JSONB documents
- Matches what users expect from a professional database tool

**Implementation:**

1. **Frontend:** Extend the JSON tree editor to emit path-based change events
   (e.g., `{ path: ["address", "city"], value: "Berlin", operation: "set" }`)
2. **Backend:** New helper that generates `jsonb_set` / `jsonb_delete_path` SQL
   based on the operation type
3. **Fallback:** If the server is < PG 14 or the change is complex (multiple
   paths, restructuring), fall back to full-document replacement

**Complexity:** High (frontend tree editor changes + backend SQL generation + version detection)

---

### Gap 4: Table/Database Size Information

**Priority:** MEDIUM — Universally expected in database tools

**What PostgreSQL Provides:**

```sql
-- Database size:
SELECT pg_size_pretty(pg_database_size(current_database()));

-- Table size (data only):
SELECT pg_size_pretty(pg_table_size('schema.table'));

-- Table size (with indexes):
SELECT pg_size_pretty(pg_total_relation_size('schema.table'));

-- Index size:
SELECT pg_size_pretty(pg_indexes_size('schema.table'));

-- All tables in a schema with sizes:
SELECT
    schemaname,
    relname AS table_name,
    pg_size_pretty(pg_total_relation_size(schemaname || '.' || relname)) AS total_size,
    pg_size_pretty(pg_table_size(schemaname || '.' || relname)) AS data_size,
    pg_size_pretty(pg_indexes_size(schemaname || '.' || relname)) AS index_size,
    n_live_tup AS estimated_rows
FROM pg_stat_user_tables
WHERE schemaname = $1
ORDER BY pg_total_relation_size(schemaname || '.' || relname) DESC;
```

**Proposed Feature Set:**

| Location | Information Shown |
|----------|-------------------|
| Sidebar table item (tooltip or badge) | Total relation size (compact, e.g., "12 MB") |
| Table properties panel | Data size, index size, total size, estimated rows, toast size |
| Database item (tooltip) | Database total size |
| Status bar (when table is open) | Table size + row estimate |

**Implementation:**

1. **Backend:** New function `get_table_sizes(params, schema) -> Vec<TableSizeInfo>`
   that batch-fetches sizes for all tables in a schema (single query).
   Model: `TableSizeInfo { name, data_size, index_size, total_size, toast_size, estimated_rows }`
2. **Backend:** New function `get_database_size(params) -> DatabaseSizeInfo`
3. **Tauri commands:** `get_table_sizes`, `get_database_size`
4. **Frontend:** Display size info in sidebar tooltips and a new properties section
5. **Caching:** Size data should be fetched lazily and cached (refreshed on demand),
   not on every schema load — `pg_total_relation_size` can be slow on schemas
   with thousands of tables.

**Complexity:** Medium (new queries + UI display, but no new object type management)

---

### Gap 5: Extensions List

**Priority:** MEDIUM — Helps users understand available types and features

**What PostgreSQL Provides:**

```sql
-- Installed extensions:
SELECT
    e.extname AS name,
    e.extversion AS version,
    n.nspname AS schema,
    c.description
FROM pg_extension e
JOIN pg_namespace n ON n.oid = e.extnamespace
LEFT JOIN pg_description c ON c.objoid = e.oid AND c.classoid = 'pg_extension'::regclass
ORDER BY e.extname;

-- Available (not yet installed):
SELECT name, default_version, comment
FROM pg_available_extensions
WHERE installed_version IS NULL
ORDER BY name;
```

**Proposed Feature Set:**

| Operation | SQL | UI Location |
|-----------|-----|-------------|
| List installed | `SELECT FROM pg_extension ...` | Sidebar → "Extensions" group (or separate section) |
| View details | Extension name, version, schema, description | Tooltip or details panel |
| Create (install) | `CREATE EXTENSION name [SCHEMA schema] [VERSION version]` | Context menu → "Install Extension" with picker |
| Drop | `DROP EXTENSION name [CASCADE]` | Context menu → "Drop Extension" with cascade warning |

**Implementation:**

1. **Backend:** `get_extensions(params) -> Vec<ExtensionInfo>`,
   `create_extension(params, name, schema, version)`,
   `drop_extension(params, name, cascade)`
2. **Model:** `ExtensionInfo { name, version, schema, description, is_relocatable }`
3. **New capability flag:** `extensions: bool` (only PG sets this to true)
4. **Frontend:** New sidebar section or group within the schema tree
5. **Localization:** Keys for all 8 locales

**Note:** Extension management requires superuser privileges in most configurations.
The UI should gracefully handle permission errors and still allow listing
(which requires less privilege).

**Complexity:** Medium (simpler than sequences — fewer operations, no complex state)

---

### Gap 6: Table Partition Awareness

**Priority:** MEDIUM — Issue [#338](https://github.com/TabularisDB/tabularis/issues/338)

**Current State:**

The `get_tables` query fetches from `information_schema.tables WHERE table_type = 'BASE TABLE'`.
This returns **all** tables including partitions, with no distinction between:

- Regular tables (`relkind = 'r'`)
- Partitioned parent tables (`relkind = 'p'`)
- Child partition tables (`relispartition = true`)

Large production schemas with many partitions show a flat list of hundreds of
tables, making navigation difficult.

**What PostgreSQL Provides:**

```sql
-- Identify partitioned tables and their children:
SELECT
    c.relname AS table_name,
    c.relkind,                          -- 'p' = partitioned, 'r' = regular
    c.relispartition,                   -- true = is a child partition
    pg_get_expr(c.relpartbound, c.oid) AS partition_bound,  -- e.g., "FOR VALUES FROM (1) TO (100)"
    parent.relname AS parent_table
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_inherits i ON i.inhrelid = c.oid
LEFT JOIN pg_class parent ON parent.oid = i.inhparent
WHERE n.nspname = $1
    AND c.relkind IN ('r', 'p')
    AND c.relpersistence != 't'         -- exclude temp tables
ORDER BY c.relname;
```

**Proposed UX:**

```text
📁 Tables
├── 📋 users                         (regular table)
├── 📋 orders                        (regular table)
├── 📋 events [Partitioned]          (relkind = 'p')
│   ├── 📋 events_2024_q1           (partition, collapsed by default)
│   ├── 📋 events_2024_q2
│   ├── 📋 events_2024_q3
│   └── 📋 events_2024_q4
└── 📋 logs [Partitioned]
    ├── 📋 logs_archive
    └── 📋 logs_current
```

**Implementation:**

1. **Backend:** Extend `get_tables` to return partition metadata:
   - New fields on `TableInfo`: `is_partitioned: bool`, `is_partition: bool`,
     `parent_table: Option<String>`, `partition_bound: Option<String>`
   - Query `pg_class` directly instead of `information_schema.tables` (needed
     for `relkind`, `relispartition`)
2. **Frontend:** `SidebarTableItem` nests partition children under their parent
   when `is_partitioned: true`. Partitions are collapsed by default.
3. **Context menu on parent:** "Show Partition Info" → displays partition strategy
   (RANGE, LIST, HASH) and all partition bounds.
4. **Optional:** Filter to hide partitions from the flat list entirely (user preference).

**Compatibility:** The `relispartition` column exists from PG 10+. For PG 9.x (which
supports only inheritance-based partitioning), fall back to showing all tables flat.

**Complexity:** High (modifies the core table-listing model, frontend tree restructuring)

---

## Findings: Polish & Enhancements

These are lower-priority improvements that enhance the professional feel of the
PostgreSQL driver without addressing critical functional gaps.

### Enhancement 1: Column & Table Comments

**What PostgreSQL Provides:**

```sql
-- Table comment:
SELECT obj_description('"schema"."table"'::regclass, 'pg_class');

-- Column comments (batch):
SELECT
    a.attname AS column_name,
    col_description(c.oid, a.attnum) AS comment
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_attribute a ON a.attrelid = c.oid
WHERE n.nspname = $1 AND c.relname = $2
    AND a.attnum > 0 AND NOT a.attisdropped;
```

**Proposed:** Add `comment` field to `TableColumn` model. Display as tooltip in
the sidebar column list and in the column header of the data grid.

**Complexity:** Low (add a field + join in existing get_columns query)

---

### Enhancement 2: Table Statistics (pg_stat_user_tables)

**What PostgreSQL Provides:**

```sql
SELECT
    n_live_tup AS estimated_rows,
    n_dead_tup AS dead_rows,
    last_vacuum,
    last_autovacuum,
    last_analyze,
    last_autoanalyze,
    seq_scan,
    idx_scan
FROM pg_stat_user_tables
WHERE schemaname = $1 AND relname = $2;
```

**Proposed:** Display in a table properties panel (accessible via context menu).
Useful for identifying tables needing VACUUM or ANALYZE.

**Complexity:** Low (read-only query + new UI panel)

---

### Enhancement 3: Custom Type Browser (Enums, Domains, Composites)

**What PostgreSQL Provides:**

```sql
-- All user-defined types:
SELECT
    t.typname AS name,
    n.nspname AS schema,
    CASE t.typtype
        WHEN 'e' THEN 'enum'
        WHEN 'd' THEN 'domain'
        WHEN 'c' THEN 'composite'
        WHEN 'r' THEN 'range'
    END AS kind,
    -- For enums: list values
    -- For domains: base type + constraints
    -- For composites: column definitions
FROM pg_type t
JOIN pg_namespace n ON n.oid = t.typnamespace
WHERE t.typtype IN ('e', 'd', 'c', 'r')
    AND n.nspname = $1
ORDER BY t.typname;
```

**Proposed:** A "Types" group in the sidebar (gated on a new `custom_types: bool`
capability). Allows browsing enum values, domain base types and constraints,
composite field definitions.

**Future:** CREATE TYPE / ALTER TYPE / DROP TYPE operations.

**Complexity:** Medium (new object type, multiple sub-kinds with different display needs)

---

### Enhancement 4: Schema & Database Management

**Current State:** Users can browse schemas and databases, but cannot create, rename,
or drop them from the UI.

**Proposed Operations:**

| Operation | SQL |
|-----------|-----|
| Create schema | `CREATE SCHEMA name [AUTHORIZATION role]` |
| Drop schema | `DROP SCHEMA name [CASCADE\|RESTRICT]` |
| Create database | `CREATE DATABASE name [OWNER role] [TEMPLATE tmpl] [ENCODING enc]` |
| Drop database | `DROP DATABASE name` (must not be connected to it) |

**Note:** `CREATE DATABASE` cannot run inside a transaction and requires the
connection to target a different database (typically `postgres`). This is a
UX challenge — the user would need to be connected to `postgres` or another
database to create a new one.

**Complexity:** Medium (backend is simple; UX for cross-database operations is tricky)

---

### Enhancement 5: Row-Level Security (RLS) Policies

**What PostgreSQL Provides:**

```sql
SELECT
    pol.polname AS policy_name,
    CASE pol.polcmd
        WHEN 'r' THEN 'SELECT'
        WHEN 'a' THEN 'INSERT'
        WHEN 'w' THEN 'UPDATE'
        WHEN 'd' THEN 'DELETE'
        WHEN '*' THEN 'ALL'
    END AS command,
    pg_get_expr(pol.polqual, pol.polrelid) AS using_expression,
    pg_get_expr(pol.polwithcheck, pol.polrelid) AS with_check_expression,
    ARRAY(SELECT rolname FROM pg_roles WHERE oid = ANY(pol.polroles)) AS roles
FROM pg_policy pol
JOIN pg_class cls ON cls.oid = pol.polrelid
JOIN pg_namespace ns ON ns.oid = cls.relnamespace
WHERE ns.nspname = $1 AND cls.relname = $2;
```

**Proposed:** Read-only display of RLS policies in table properties panel.
Increasingly important for modern architectures (Supabase, multi-tenant).

**Complexity:** Low (read-only introspection, new panel section)

---

## Findings: Out of Scope

These PostgreSQL features are deliberately excluded from this plan as they serve
specialized admin/DBA workflows beyond what a database browser/editor targets:

| Feature | Reason |
|---------|--------|
| **Active connections / `pg_stat_activity`** | Admin monitoring tool territory (pgAdmin, pg_top) |
| **VACUUM / ANALYZE / REINDEX** | Maintenance operations; could be added as simple actions later |
| **Publications / Subscriptions** | Logical replication admin — very specialized |
| **Foreign Data Wrappers** | Specialized federated query setup |
| **Event triggers** | Rare; standard triggers cover 99% of use cases |
| **Tablespaces** | Physical storage admin |
| **Roles / Grants management** | Full role admin is complex; read-only role display possible later |
| **pg_hba.conf / Server config** | Server-side config, not accessible via SQL connection |
| **Inheritance (non-partition)** | Legacy feature, rarely used in modern PG |

---

## Feature Comparison Matrix

| Feature | PostgreSQL (current) | PostgreSQL (proposed) | MySQL | Notes |
|---------|---------------------|----------------------|-------|-------|
| **Schema Objects** | | | | |
| Tables | ✅ | ✅ | ✅ | Parity |
| Views | ✅ | ✅ | ✅ | Parity |
| Materialized Views | ✅ | ✅ | N/A | PG-exclusive |
| Sequences | ❌ | ✅ | N/A | **Gap 1** |
| Routines | ✅ | ✅ | ✅ | Parity |
| Triggers | ✅ | ✅ | ✅ | Parity |
| Extensions | ❌ | ✅ | N/A | **Gap 5** |
| Custom Types | ❌ | ✅ | N/A | **Enhancement 3** |
| Partitions (nested display) | ❌ | ✅ | N/A | **Gap 6** |
| **Data Operations** | | | | |
| CRUD (basic types) | ✅ | ✅ | ✅ | Parity |
| HSTORE write | ❌ | ✅ | N/A | **Gap 2** |
| JSONB path-based edit | ❌ | ✅ | N/A | **Gap 3** |
| BLOB read/write | ✅ | ✅ | ✅ | Parity |
| **Metadata** | | | | |
| Table/DB sizes | ❌ | ✅ | ❌ | **Gap 4** |
| Column comments | ❌ | ✅ | ❌ | **Enhancement 1** |
| Table statistics | ❌ | ✅ | ❌ | **Enhancement 2** |
| RLS Policies | ❌ | ✅ | N/A | **Enhancement 5** |
| **Bug Fixes** | | | | |
| Schema in DML | ⚠️ Bug 516 | ✅ | N/A | **Bug 1** |
| Reserved-word quoting | ⚠️ Bug 335 | ✅ | N/A | **Bug 2** |
| FK with restricted user | ⚠️ Bug 96 | ✅ | N/A | **Bug 3** |

---

## Implementation Plan

### Tier 1: Bug Fixes (Critical Path)

These should be addressed first as they affect basic usability.

| Item | Severity | Complexity | Dependencies |
|------|----------|------------|--------------|
| Bug 516 — Schema context in DML | HIGH | Verify only | PR 402 must merge first |
| Bug 335 — Identifier quoting in VQB | MEDIUM | Low-Medium | None |
| Bug 96 — FK fallback for restricted users | LOW-MEDIUM | Medium | None |

**Estimated effort:** 1-2 days (Bug 516 is verification; 335 and 96 are the real work)

---

### Tier 2: Core Feature Gaps (Issue 16 Deliverables)

These directly address the items called out in the issue.

| Item | Priority | Complexity | Dependencies |
|------|----------|------------|--------------|
| Gap 1 — Sequences | HIGH | High | PR 402 merged (database routing pattern) |
| Gap 2 — HSTORE write | HIGH | Medium | Column type detection in binding |
| Gap 3 — JSONB path editing | MEDIUM | High | Frontend tree editor changes |
| Gap 4 — Table/DB sizes | MEDIUM | Medium | PR 402 merged (database routing pattern) |

**Estimated effort:** 1-2 weeks

**Implementation order:** HSTORE write (smaller, unblocks issue 395, no 402
dependency) → Sequences (largest new feature, requires 402 routing pattern) →
Table sizes (independent) → JSONB path editing (highest complexity, can follow
later)

**Critical constraint:** All new Tauri commands for Gaps 1 and 4 must follow the
`database: Option<String>` routing pattern established by PR 402. See the
[Dependency section](#dependency-pr-402--multi-database-connections) for the
exact code pattern.

---

### Tier 3: Schema Object Discovery

New browsable object types in the sidebar.

| Item | Priority | Complexity | Dependencies |
|------|----------|------------|--------------|
| Gap 5 — Extensions | MEDIUM | Medium | PR 402 merged (sidebar propagation) |
| Gap 6 — Partition awareness | MEDIUM | High | PR 402 merged (modifies core TableInfo) |
| Enhancement 3 — Custom Types | LOW-MEDIUM | Medium | PR 402 merged (sidebar propagation) |

**Estimated effort:** 1-2 weeks

**Implementation order:** Extensions (simpler, high visibility) → Partitions
(high value for production users but complex) → Custom Types

**Critical constraint:** New sidebar groups must propagate `database` to their
child items following the same pattern as `SidebarSchemaItem` → `SidebarTableItem`.
PR 402 established this propagation chain; our new groups (Sequences, Extensions,
Types) must participate in it.

---

### Tier 4: Metadata & Polish

Read-only informational additions that enhance the professional feel.

| Item | Priority | Complexity | Dependencies |
|------|----------|------------|--------------|
| Enhancement 1 — Column/table comments | LOW-MEDIUM | Low | None |
| Enhancement 2 — Table statistics | LOW | Low | None |
| Enhancement 4 — Schema/DB management | LOW-MEDIUM | Medium | Cross-DB UX design |
| Enhancement 5 — RLS Policies | LOW | Low | None |

**Estimated effort:** 3-5 days

---

## Testing Strategy

### Unit Tests (Rust)

```text
tests/drivers/postgres/
├── sequences.test.rs
│   ├── get_sequences returns all sequences in schema
│   ├── get_sequence_details returns full metadata
│   ├── create_sequence generates valid DDL
│   ├── alter_sequence modifies properties correctly
│   ├── drop_sequence removes without error
│   ├── restart_sequence resets last_value
│   ├── owned-by relationship resolved correctly
│   └── schema-qualified names handled (non-public schema)
├── hstore_binding.test.rs
│   ├── insert_record with HSTORE JSON object succeeds
│   ├── update_record with HSTORE JSON object succeeds
│   ├── empty HSTORE (empty object) handled
│   ├── HSTORE with NULL values preserved
│   ├── HSTORE with special characters in keys/values
│   ├── HSTORE with unicode content
│   └── round-trip: insert HSTORE → select → compare
├── foreign_key_fallback.test.rs
│   ├── FK query succeeds for superuser (primary path)
│   ├── FK query fallback fires for restricted user
│   ├── Fallback returns same structure as primary
│   ├── FKs across schemas resolved correctly
│   └── Self-referencing FKs handled in both paths
├── extensions.test.rs
│   ├── get_extensions lists installed extensions
│   ├── Extension details include schema and version
│   └── Extensions from non-default schemas included
├── partitions.test.rs
│   ├── Partitioned table identified (relkind = 'p')
│   ├── Child partitions linked to parent
│   ├── Partition bound expression included
│   ├── Regular tables unaffected
│   └── Mixed schemas handled correctly
└── sizes.test.rs
    ├── get_table_sizes returns data for all tables
    ├── Sizes are human-readable
    ├── Empty tables report 0 or minimal size
    └── Schema-qualified tables handled
```

### Frontend Tests

```text
tests/components/layout/sidebar/
├── SidebarSequenceItem.test.tsx
│   ├── Renders sequence with correct icon
│   ├── Context menu shows Restart / Drop options
│   ├── Sequence group hidden when capability is false
│   └── Sequence group shows count badge
├── SidebarSchemaItem.test.tsx (extend)
│   ├── Partitioned tables render with nested partitions
│   ├── Partitions collapsed by default
│   ├── Extensions group rendered when capability is true
│   └── Sequences group rendered when capability is true
└── Editor.test.tsx (extend)
    ├── Schema context retained per tab (not global)
    └── DML uses tab's original schema, not sidebar selection
```

### Integration Tests

- Connect with restricted-privilege user → verify FKs visible via fallback
- Create sequence → restart → verify new value → drop
- Insert HSTORE via grid → SELECT → verify round-trip
- Open table in schemaA → switch sidebar to schemaB → submit edit → verify targets schemaA
- Visual Query Builder with reserved-word table → verify quoted SQL generated
- Schema with 50+ partition tables → verify parent/child nesting in sidebar
- Install extension → verify it appears in list → drop

---

## Open Questions

1. **Sequence sidebar placement** — Should sequences be a peer group to "Tables"
   and "Views" within each schema? Or a separate top-level section? Peer group
   (inside the schema accordion) seems consistent with how other drivers organize
   schema objects.

2. **Partition nesting default** — Should partitions be hidden by default (with a
   toggle to show), or shown nested under their parent (collapsed)? The latter
   matches DBeaver's behavior and is less surprising.

3. **JSONB path editing scope** — Should path-based editing be limited to
   `jsonb_set` on leaf values, or also support structural operations (add key,
   remove key, move key)? The tree editor already supports these operations
   visually — the question is whether to wire them to path-based SQL or always
   fall back to full-document replacement for structural changes.

4. **Table sizes: eager vs. lazy** — Should table sizes be fetched alongside
   `get_tables` (adds latency to schema load) or on-demand (e.g., when user
   hovers or expands a table)? For schemas with thousands of tables,
   `pg_total_relation_size` across all tables can be slow. Recommended: lazy
   fetch with caching.

5. **Extension management privileges** — `CREATE EXTENSION` typically requires
   superuser. Should the UI hide the "Install Extension" action entirely for
   non-superusers, or show it and let the error propagate? Recommended: always
   show; handle the permission error with a clear message ("requires superuser
   privileges").

6. **HSTORE column detection** — Should we detect HSTORE columns proactively
   (like we do for enums in `get_enum_column_types`) to apply proper binding,
   or use a reactive approach (detect from the error when a plain TEXT bind fails)?
   Recommended: proactive detection via `pg_type` — consistent with the enum pattern.

7. **Backward compatibility for `TableInfo`** — Adding `is_partitioned`,
   `is_partition`, `parent_table` fields to `TableInfo` affects all drivers.
   Should these be `Option<T>` fields with `serde(default)` to avoid breaking
   the MySQL/SQLite drivers, or should each driver explicitly return
   `false`/`None`?

8. **PR 402 merge timing** — Our Tier 2 and 3 work depends on PR 402's routing
   pattern being in `main`. If 402 stalls (it has merge conflicts and no formal
   reviews yet), should we proceed with HSTORE write support (which has no 402
   dependency) and defer sequence/extensions work? Or should we resolve 402's
   conflicts and help get it merged first?

9. **PR 402's DDL routing gap** — PR 402 explicitly leaves object-creation DDL
   (Create Table/View/Trigger/Index/FK) as not database-aware on nested schema
   nodes. Should we fix this as part of our Enhancement 4 (Schema/DB management)
   work, or is it a separate follow-up PR that should go in between 402 and our
   work?

---

## Appendix: SQL Reference for Implementation

### Sequence Introspection (PG 10+)

```sql
SELECT
    s.sequencename AS name,
    s.schemaname AS schema,
    s.data_type,
    s.start_value,
    s.min_value,
    s.max_value,
    s.increment_by,
    s.cycle,
    s.cache_size,
    s.last_value,
    -- Owner info (which table.column owns this sequence):
    d.refobjid::regclass AS owner_table,
    a.attname AS owner_column
FROM pg_sequences s
LEFT JOIN pg_depend d
    ON d.objid = (s.schemaname || '.' || s.sequencename)::regclass
    AND d.deptype = 'a'
    AND d.classid = 'pg_class'::regclass
LEFT JOIN pg_attribute a
    ON a.attrelid = d.refobjid
    AND a.attnum = d.refobjsubid
WHERE s.schemaname = $1
ORDER BY s.sequencename;
```

### Partition Hierarchy

```sql
SELECT
    c.relname AS table_name,
    c.relkind,
    c.relispartition,
    CASE
        WHEN pt.partstrat = 'r' THEN 'RANGE'
        WHEN pt.partstrat = 'l' THEN 'LIST'
        WHEN pt.partstrat = 'h' THEN 'HASH'
    END AS partition_strategy,
    pg_get_expr(c.relpartbound, c.oid) AS partition_bound,
    parent.relname AS parent_table
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_inherits i ON i.inhrelid = c.oid
LEFT JOIN pg_class parent ON parent.oid = i.inhparent
LEFT JOIN pg_partitioned_table pt ON pt.partrelid = c.oid
WHERE n.nspname = $1
    AND c.relkind IN ('r', 'p')
    AND c.relpersistence != 't'
ORDER BY
    COALESCE(parent.relname, c.relname),  -- Group children with parent
    c.relispartition,                      -- Parent first
    c.relname;
```

### Extension Details

```sql
SELECT
    e.extname AS name,
    e.extversion AS version,
    n.nspname AS schema,
    e.extrelocatable AS is_relocatable,
    c.description
FROM pg_extension e
JOIN pg_namespace n ON n.oid = e.extnamespace
LEFT JOIN pg_description c
    ON c.objoid = e.oid
    AND c.classoid = 'pg_extension'::regclass
ORDER BY e.extname;
```

### HSTORE Binding Format

```text
-- PostgreSQL HSTORE text representation:
'"key1"=>"value1", "key2"=>"value2", "null_key"=>NULL'

-- Escaping rules:
-- - Keys and values are double-quoted
-- - Backslash and double-quote within values are backslash-escaped
-- - NULL (unquoted) represents a null value
-- - Empty HSTORE is an empty string: ''
```

### Column Comments (batch)

```sql
SELECT
    a.attname AS column_name,
    col_description(c.oid, a.attnum) AS comment
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_attribute a ON a.attrelid = c.oid
WHERE n.nspname = $1
    AND c.relname = $2
    AND a.attnum > 0
    AND NOT a.attisdropped
ORDER BY a.attnum;
```
