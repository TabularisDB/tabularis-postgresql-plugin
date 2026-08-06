# Phase 2 — Issue #16 Improvements

**Goal:** Add the PostgreSQL-specific features identified in issue #16 that go
beyond what the built-in driver supports. This is where the plugin exceeds the
built-in driver and becomes the definitively better PostgreSQL experience.

**Prerequisite:** Phase 1 complete (55/55 parity tests GREEN, beta published).

---

## Approach

### TDD Continues

Each new feature follows the same discipline:

1. Write the test (RED)
2. Implement the feature (GREEN)
3. Verify no regressions (all previous tests still GREEN)

### Plugin-Only Development

Phase 2 features go into the plugin only — they do NOT exist in the built-in
driver. This is the first divergence point: the plugin becomes strictly superior.

### UI Extensions

Some features may need frontend UI. The plugin manifest supports `ui_extensions`
for injecting custom panels/tabs. However, for Phase 2, most features expose
through existing UI patterns (sidebar tree nodes, query results, context menus).

### Check for Existing In-Flight Work

**Before implementing any feature, check for open PRs that already address it.**
Duplicating community work wastes effort and creates merge conflicts.

**Known in-flight PRs relevant to Phase 2 (as of this writing):**

| PR | Feature | Author | Status |
| -- | ------- | ------ | ------ |
| [#427](https://github.com/TabularisDB/tabularis/pull/427) | HStore column editing | arturbent0 | Open |
| [#402](https://github.com/TabularisDB/tabularis/pull/402) | Multi-database connections | debba | Draft |
| [#222](https://github.com/TabularisDB/tabularis/pull/222) | Composite PK end-to-end | saurabh500 | Draft |

**Process for each Phase 2 feature:**

1. Search open PRs: `gh pr list --repo TabularisDB/tabularis --search "<feature>"`
2. If a PR exists and is active → coordinate with the author, don't duplicate
3. If a PR exists but is stale (>2 months inactive) → comment asking if still active;
   if no response in 1 week, proceed with your own implementation
4. If no PR exists → proceed

**For PR #427 (HStore) specifically:** This work already exists. When Phase 2
reaches hstore support, either:

- The PR has merged → we port its logic into the plugin (or the plugin
  inherits it via the existing driver trait behavior)
- The PR hasn't merged → coordinate with `arturbent0` to align with the plugin
  architecture (their work may target the built-in driver and need adaptation)

---

## Features (Priority Order)

### 2.1: Sequence Management

**What:** List, inspect, create, alter, reset, and drop PostgreSQL sequences.

**Why:** Sequences are fundamental to PG (every SERIAL/BIGSERIAL creates one).
Currently invisible in Tabularis — users must write raw SQL to manage them.

**Implementation:**

| Method | SQL |
| ------ | --- |
| List sequences | `SELECT * FROM pg_sequences WHERE schemaname = $1` |
| Get sequence details | `SELECT * FROM pg_sequences WHERE sequencename = $1` |
| Get current value | `SELECT currval('schema.seq')` or `last_value` from pg_sequences |
| Reset sequence | `ALTER SEQUENCE schema.seq RESTART WITH $1` |
| Set sequence value | `SELECT setval('schema.seq', $1)` |
| Create sequence | `CREATE SEQUENCE schema.seq [INCREMENT BY ...] [START WITH ...]` |
| Drop sequence | `DROP SEQUENCE schema.seq` |

**Frontend integration:** Sequences appear in the sidebar under a "Sequences"
node (same level as Tables, Views, Routines). Double-click opens a detail panel.

**Tests:**

- `test_get_sequences` — lists all sequences in schema
- `test_get_sequence_details` — returns increment, min, max, start, current
- `test_reset_sequence` — verify value changes
- `test_create_and_drop_sequence` — lifecycle

---

### 2.2: JSONB Inline Editing

**What:** Edit JSONB column values with structured awareness — add/remove keys,
modify nested values, toggle between raw JSON text and structured editor.

**Why:** Currently JSONB is edited as a raw text string. This is error-prone for
complex nested objects. A structured editor prevents syntax errors.

**Implementation approach:**

This is primarily a **frontend feature** (UI extension). The plugin's role is:

1. Detect JSONB columns and flag them in `get_columns` response (already done — `data_type: "jsonb"`)
2. Validate JSON on `update_record` — return clear error if invalid JSON is submitted
3. Optionally: expose `jsonb_set`, `jsonb_insert`, `jsonb_delete_path` as helper operations

**Plugin-side additions:**

- New RPC method: `validate_jsonb(value)` → returns ok or parse error with position
- New RPC method: `jsonb_patch(params, table, pk, path, operation, value)` → applies a
  targeted JSONB modification without overwriting the entire value

**Frontend UI extension:**

- JSON tree editor component (expand/collapse nodes, edit values inline)
- Add/remove key buttons
- Path breadcrumb showing current location in the JSON tree
- Raw mode toggle (switch between tree and text editor)

**Tests:**

- `test_insert_complex_jsonb` — nested objects, arrays, mixed types
- `test_update_jsonb_full_replace` — overwrite entire value
- `test_jsonb_patch_add_key` — add key to existing object
- `test_jsonb_patch_remove_key` — remove key from object
- `test_jsonb_patch_nested_update` — modify deeply nested value
- `test_invalid_jsonb_rejected` — malformed JSON returns clear error

---

### 2.3: Extension-Aware Type System

**What:** Detect installed PostgreSQL extensions and expose their types in the
type picker and column handling.

**Extensions to support initially:**

- **PostGIS** — geometry, geography, raster types
- **pgvector** — vector(N) type for embeddings
- **ltree** — label tree type
- **hstore** — key-value store (legacy, still common) — **see PR #427 (in-flight)**
- **citext** — case-insensitive text

**Implementation:**

```sql
-- Detect installed extensions
SELECT extname, extversion FROM pg_extension WHERE extname IN (
    'postgis', 'vector', 'ltree', 'hstore', 'citext'
);
```

For each detected extension, add its types to the runtime type list. The plugin
can dynamically extend `data_types` after `initialize` by checking what's installed.

**Gotcha:** The `data_types` in the manifest are static. Dynamic type discovery
requires either:

- A new RPC method: `get_dynamic_data_types(params)` → returns additional types
  based on what's installed
- Or: the plugin returns a comprehensive superset and the UI filters by what's
  actually usable

**Tests:**

- `test_detect_postgis_extension` (requires PG with PostGIS — optional CI extension)
- `test_vector_type_handling` (requires pgvector)
- `test_ltree_insert_and_query`

**Note:** These tests may need to be `#[ignore]` in CI unless extensions are
installed in the test container. Consider a separate "extended type" test profile.

---

### 2.4: Partition Table Introspection

**What:** Show partition hierarchy in the sidebar — parent table with child
partitions listed underneath. Show partition key and bounds.

**Implementation:**

```sql
-- Find partitioned tables
SELECT c.relname, pg_get_partkeydef(c.oid) as partition_key
FROM pg_class c
JOIN pg_namespace n ON c.relnamespace = n.oid
WHERE c.relkind = 'p' AND n.nspname = $1;

-- Find partitions of a parent table
SELECT c.relname, pg_get_expr(c.relpartbound, c.oid) as partition_bound
FROM pg_inherits i
JOIN pg_class c ON i.inhrelid = c.oid
WHERE i.inhparent = (SELECT oid FROM pg_class WHERE relname = $1);
```

**Frontend integration:**

- Partitioned tables show with a special icon in sidebar
- Expanding shows child partitions with their bounds
- Context menu: "Create Partition", "Detach Partition"

**Tests:**

- `test_get_partitioned_tables` — identifies partition parents
- `test_get_partitions` — lists children with bounds
- `test_partition_range_bounds` — range partition display
- `test_partition_list_bounds` — list partition display

---

### 2.5: Row-Level Security Policies

**What:** Display RLS policies on tables. Show which roles they apply to, the
USING and WITH CHECK expressions.

**Implementation:**

```sql
SELECT polname, polcmd, polroles, pg_get_expr(polqual, polrelid) as using_expr,
       pg_get_expr(polwithcheck, polrelid) as check_expr
FROM pg_policy WHERE polrelid = $1::regclass;
```

**Frontend integration:**

- In the table detail panel, show a "Security Policies" section
- Each policy shows: name, command (SELECT/INSERT/UPDATE/DELETE/ALL), roles, expressions

**Tests:**

- `test_get_policies` — lists policies on a table with RLS enabled
- `test_policy_per_command` — distinguishes SELECT vs UPDATE policies

---

### 2.6: Publication/Subscription Visibility

**What:** Show logical replication publications and subscriptions for monitoring.

**Implementation:**

```sql
-- Publications
SELECT pubname, puballtables, pubinsert, pubupdate, pubdelete
FROM pg_publication;

-- Subscription status
SELECT subname, subenabled, subslotname, subpublications
FROM pg_subscription;
```

**Frontend:** New sidebar section "Replication" with Publications and Subscriptions.

---

### 2.7: Advisory Lock Monitoring

**What:** Show currently held advisory locks for debugging lock contention.

```sql
SELECT locktype, objid, mode, granted, pid,
       (SELECT usename FROM pg_stat_activity WHERE pid = l.pid) as held_by
FROM pg_locks l WHERE locktype = 'advisory';
```

---

## Implementation Order

Prioritized by user impact and implementation complexity:

```text
Sprint 1: Sequence management (high demand, straightforward)
Sprint 2: JSONB inline editing (high demand, more complex — UI extension)
Sprint 3: Extension type system (high demand for PostGIS/pgvector users)
Sprint 4: Partition introspection (medium demand)
Sprint 5: RLS policies (medium demand, straightforward)
Sprint 6: Pub/Sub + Advisory locks (lower priority, quick wins)
```

---

## Checkpoint: CP-5 (Phase 2 Complete — Stable Release Gate)

**When:** Core Phase 2 features complete (at minimum: sequences + JSONB + extensions).

**This IS a major release gate.** The plugin now exceeds the built-in driver.

**Verify:**

- [ ] All Phase 1 parity tests still GREEN (no regressions)
- [ ] New Phase 2 features have dedicated tests (all GREEN)
- [ ] Sequence management works end-to-end
- [ ] JSONB editing works with nested objects
- [ ] At least one extension type (PostGIS or pgvector) is handled
- [ ] Plugin published as **stable** (not beta) to Tabularium registry

**Communicate to team:**

- Plugin is now the recommended PostgreSQL driver for power users
- Built-in driver still works but is feature-frozen
- Begin planning Phase 3 (deprecation decision)

---

## Ship / Release Points

| After | What to ship | Channel |
| ----- | ------------ | ------- |
| Sequences done | Plugin update (minor version bump) | Beta → early adopters |
| JSONB editing done | Plugin update | Beta |
| All Phase 2 core done | Plugin promoted to **stable** | Public registry |
| Extensions done | Plugin update | Stable |

The plugin architecture enables shipping each feature independently without
waiting for a Tabularis core release. This is a key advantage.

---

## Security Considerations

| Feature | Security Concern | Mitigation |
| ------- | ---------------- | ---------- |
| Sequence reset | Could disrupt application logic (PK collisions) | Confirmation dialog before reset |
| JSONB patch | Could corrupt data if path is wrong | Validate path exists before patching; show preview |
| RLS policies | Exposing USING expressions might reveal security rules | Only show to connection owner / superuser |
| Advisory locks | Revealing lock holders exposes active sessions | Same visibility as `pg_stat_activity` (requires `pg_monitor` role) |

---

## Definition of Done

- [ ] Sequences: list, inspect, reset, create, drop — all tested
- [ ] JSONB: structured editing, validation, patch operations — all tested
- [ ] Extensions: at least PostGIS + pgvector types detected and handled
- [ ] Partitions: hierarchy displayed, bounds shown
- [ ] All Phase 1 tests still GREEN (no regressions)
- [ ] Plugin published as stable release
- [ ] CP-5 sync completed with core team
