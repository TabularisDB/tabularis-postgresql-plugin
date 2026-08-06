# Prerequisites — Tabularis Core PRs

**Must be merged before Phase 0 testing or Phase 1 building can begin.**

## Overview

Three changes to the Tabularis host's `RpcDriver` adapter are required to enable
full feature parity for any PostgreSQL plugin. Without these, certain tests in
Phase 0 will always fail when pointed at a plugin driver, making parity
verification impossible.

These are small, non-breaking additions to existing code. They follow the same
patterns already used for other forwarded methods (triggers, views, etc.).

---

## PR 1: Forward BLOB Methods

### What

Extend `RpcDriver` in `src-tauri/src/plugins/driver.rs` to forward:

- `save_blob_to_file(params, table, column, pk_column, pk_value, file_path, schema)`
- `fetch_blob_as_data_url(params, table, column, pk_column, pk_value, schema)`

### Current Behavior

These methods inherit the trait default which returns:

```rust
Err("BLOB file export not supported by this driver".into())
```

### Proposed Implementation

```rust
async fn save_blob_to_file(&self, params: &ConnectionParams, table: &str,
    column: &str, pk_column: &str, pk_value: &str,
    file_path: &str, schema: Option<&str>) -> Result<(), String>
{
    // Plugin returns base64-encoded blob data
    let res = self.process.call("save_blob_to_file", json!({
        "params": params, "table": table, "column": column,
        "pk_column": pk_column, "pk_value": pk_value,
        "file_path": file_path, "schema": schema
    })).await?;
    Ok(())  // Plugin writes to file_path directly (local process)
}

async fn fetch_blob_as_data_url(&self, params: &ConnectionParams, table: &str,
    column: &str, pk_column: &str, pk_value: &str,
    schema: Option<&str>) -> Result<String, String>
{
    let res = self.process.call("fetch_blob_as_data_url", json!({
        "params": params, "table": table, "column": column,
        "pk_column": pk_column, "pk_value": pk_value, "schema": schema
    })).await?;
    serde_json::from_value(res).map_err(|e| e.to_string())
}
```

### Testing

- Verify existing BLOB tests pass with built-in driver (unchanged behavior)
- Verify a plugin returning base64 data works end-to-end

### Risk

None — purely additive. Existing plugins that don't implement these methods
will return `-32601` and the host falls back to the existing "not supported" error.

---

## PR 2: Forward Materialized View Methods

### What

Extend `RpcDriver` to forward:

- `get_materialized_views(params, schema)`
- `get_materialized_view_columns(params, view_name, schema)`
- `get_materialized_view_definition(params, view_name, schema)`
- `refresh_materialized_view(params, view_name, schema)`

### Current Behavior

These inherit defaults returning `Ok(vec![])` or
`Err("Materialized views are not supported...")`.

### Proposed Implementation

Same pattern as `get_views`, `get_triggers`, etc. — straightforward JSON-RPC
forwarding with `serde_json::from_value` deserialization.

### Risk

None — same pattern as existing forwarded methods.

---

## PR 3: Resolve `map_inferred_type` from Plugin Manifest

### What

The `map_inferred_type` method is **synchronous** (`fn`, not `async fn`) so it
cannot issue an RPC call. Currently returns the input unchanged for plugin drivers.

The built-in PG driver maps: `DATETIME` → `TIMESTAMP`, `JSON` → `JSONB`.

### Proposed Solution

Add an optional `type_mappings` field to `PluginManifest`:

```rust
// In driver_trait.rs, add to PluginManifest:
pub type_mappings: Option<HashMap<String, String>>,
```

The `RpcDriver` stores these at construction time and applies them in
`map_inferred_type`:

```rust
fn map_inferred_type(&self, kind: &str) -> String {
    if let Some(mappings) = &self.manifest.type_mappings {
        if let Some(mapped) = mappings.get(&kind.to_uppercase()) {
            return mapped.clone();
        }
    }
    kind.to_string()
}
```

Plugin manifest declares:

```json
{
  "type_mappings": {
    "DATETIME": "TIMESTAMP",
    "JSON": "JSONB"
  }
}
```

### Risk

Low — new optional field. Existing plugins without it behave unchanged.

---

## Approach

### Option A: One Combined PR

Submit all three changes in a single PR titled:
"feat(plugins): extend RpcDriver for BLOB, materialized views, and type mappings"

**Pros:** One review cycle, atomic merge, single CI run.
**Cons:** Larger diff, harder to review.

### Option B: Three Separate PRs

Submit sequentially, each small and focused.

**Pros:** Easy to review, bisectable, can merge independently.
**Cons:** Three review cycles.

### Recommendation

**Option A** — These are all small, non-breaking additions with zero risk of
conflict. A single PR with clear commit separation (one commit per feature)
gives the reviewer full context of why these are needed (PostgreSQL plugin
migration) without the overhead of three separate review cycles.

---

## Checkpoint: CP-1

**When:** After the prerequisites PR is merged into `main`.

**Verify:**

- [ ] `cargo test` passes (no regressions in existing drivers)
- [ ] Existing plugin drivers (DuckDB, D1) still work (methods return -32601 gracefully)
- [ ] No changes to MySQL or SQLite drivers
- [ ] New trait fields are `Option` / backward-compatible

**Communicate to team:**

- Prerequisites are in place
- Phase 0 can begin (test suite development)
- No user-facing changes yet

---

## Definition of Done

- [ ] PR merged to `main`
- [ ] CI green
- [ ] No existing test regressions
- [ ] CHANGELOG entry added (under "Plugin System" section)
- [ ] Core team acknowledged at CP-1
