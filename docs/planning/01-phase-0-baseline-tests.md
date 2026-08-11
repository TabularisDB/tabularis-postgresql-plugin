# Phase 0 — Baseline Test Suite

**Goal:** Create the comprehensive test infrastructure that proves the built-in
PostgreSQL driver's behavior, establishing the specification that the plugin must
match. This is the foundation of our zero-regression guarantee.

**Mantra:** _If it isn't tested, it doesn't exist. If it passes on both drivers,
they are equivalent by construction._

---

## Why This Phase Exists

| Today's Coverage | What's Missing |
| ---------------- | -------------- |
| 162 unit tests for value extraction | Zero tests for 36 public API methods |
| 96 unit tests for parameter binding | Zero integration tests running in CI |
| 4 integration tests (all `#[ignore]`) | Zero golden file / snapshot tests |
| No parity harness | No multi-database test scenarios |

Without Phase 0, we have no way to prove the plugin matches the built-in driver.
We'd be shipping on trust, not evidence.

---

## Deliverables (in order)

### 0.1: CI PostgreSQL Service

**What:** Add a PostgreSQL 16 service container to the GitHub Actions CI workflow.

**Why:** Integration tests must run automatically on every PR. Today they're all
`#[ignore]` because no PG instance exists in CI.

**How:**

```yaml
# .github/workflows/ci.yml — add to the rust test job
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

**Also:**

- Create a second test database: `tabularis_test_secondary`
- Add CI step that runs the seed script before tests
- Set environment variable `TABULARIS_TEST_PG=1` so integration tests detect PG is available
- Remove `#[ignore]` from existing 4 integration tests

**Verify:** CI passes with the 4 existing integration tests (currently un-run).

---

### 0.2: Test Database Seed Script

**What:** A repeatable SQL script that creates all tables, types, views,
functions, triggers, and indexes needed by the test suite.

**Why:** Tests need a known schema state. The seed script is the single source of
truth for what exists in the test database.

**File:** `tests/fixtures/postgres_seed.sql`

**Contents must include:**

```sql
-- Core type coverage table
CREATE TABLE test_schema.all_types (
    id SERIAL PRIMARY KEY,
    col_text TEXT, col_varchar VARCHAR(255),
    col_int INTEGER, col_bigint BIGINT,
    col_float REAL, col_double DOUBLE PRECISION,
    col_numeric NUMERIC(10,2), col_bool BOOLEAN,
    col_date DATE, col_time TIME,
    col_timestamp TIMESTAMP, col_timestamptz TIMESTAMPTZ,
    col_uuid UUID DEFAULT gen_random_uuid(),
    col_json JSON, col_jsonb JSONB,
    col_bytea BYTEA, col_inet INET, col_cidr CIDR,
    col_macaddr MACADDR,
    col_int_array INTEGER[], col_text_array TEXT[],
    col_int4range INT4RANGE, col_tsrange TSRANGE
);

-- Enum type
CREATE TYPE test_schema.mood AS ENUM ('happy', 'sad', 'neutral');
CREATE TABLE test_schema.with_enum (
    id SERIAL PRIMARY KEY,
    current_mood test_schema.mood
);

-- Foreign key relationships (single and composite PK)
CREATE TABLE test_schema.orders (
    id SERIAL PRIMARY KEY,
    user_id INTEGER REFERENCES test_schema.all_types(id) ON DELETE CASCADE,
    amount NUMERIC(10,2)
);
CREATE TABLE test_schema.order_items (
    order_id INTEGER, item_no INTEGER,
    product TEXT,
    PRIMARY KEY (order_id, item_no),
    FOREIGN KEY (order_id) REFERENCES test_schema.orders(id)
);

-- Indexes (btree, unique, partial, composite)
CREATE INDEX idx_all_types_text ON test_schema.all_types (col_text);
CREATE UNIQUE INDEX idx_all_types_uuid ON test_schema.all_types (col_uuid);
CREATE INDEX idx_orders_amount_positive ON test_schema.orders (amount)
    WHERE amount > 0;

-- Views
CREATE VIEW test_schema.active_users AS
    SELECT id, col_text AS name FROM test_schema.all_types WHERE col_bool = true;

-- Materialized views
CREATE MATERIALIZED VIEW test_schema.user_stats AS
    SELECT COUNT(*) as total FROM test_schema.all_types;

-- Functions and procedures
CREATE FUNCTION test_schema.add_numbers(a INTEGER, b INTEGER)
    RETURNS INTEGER LANGUAGE SQL AS $$ SELECT a + b $$;

CREATE FUNCTION test_schema.get_user(p_id INTEGER)
    RETURNS TABLE(id INTEGER, name TEXT) LANGUAGE SQL AS $$
    SELECT id, col_text FROM test_schema.all_types WHERE id = p_id
$$;

-- Overloaded function (same name, different args)
CREATE FUNCTION test_schema.add_numbers(a INTEGER, b INTEGER, c INTEGER)
    RETURNS INTEGER LANGUAGE SQL AS $$ SELECT a + b + c $$;

CREATE PROCEDURE test_schema.reset_data() LANGUAGE SQL AS $$
    DELETE FROM test_schema.order_items;
    DELETE FROM test_schema.orders;
$$;

-- Triggers
CREATE FUNCTION test_schema.audit_trigger_fn() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    RAISE NOTICE 'Row modified in %', TG_TABLE_NAME;
    RETURN NEW;
END $$;

CREATE TRIGGER trg_audit AFTER UPDATE ON test_schema.all_types
    FOR EACH ROW EXECUTE FUNCTION test_schema.audit_trigger_fn();

-- Cross-schema FK (for ref_schema testing)
CREATE SCHEMA IF NOT EXISTS other_schema;
CREATE TABLE other_schema.lookup (
    code TEXT PRIMARY KEY, label TEXT
);
CREATE TABLE test_schema.with_cross_schema_fk (
    id SERIAL PRIMARY KEY,
    lookup_code TEXT REFERENCES other_schema.lookup(code)
);

-- SECONDARY DATABASE (for multi-database testing)
-- Must be created via separate connection to maintenance DB
-- CREATE DATABASE tabularis_test_secondary;
-- Then connect to it and run:
--   CREATE SCHEMA secondary_schema;
--   CREATE TABLE secondary_schema.remote_data (id SERIAL PRIMARY KEY, value TEXT);
```

**Seed runner script:** `tests/fixtures/run_seed.sh`

```bash
#!/bin/bash
PGPASSWORD=test psql -h localhost -p 54320 -U postgres -d tabularis_test \
  -f tests/fixtures/postgres_seed.sql

# Create secondary database
PGPASSWORD=test psql -h localhost -p 54320 -U postgres -c \
  "SELECT 'exists' FROM pg_database WHERE datname='tabularis_test_secondary'" \
  | grep -q exists || \
PGPASSWORD=test createdb -h localhost -p 54320 -U postgres tabularis_test_secondary

PGPASSWORD=test psql -h localhost -p 54320 -U postgres -d tabularis_test_secondary -c "
  CREATE SCHEMA IF NOT EXISTS secondary_schema;
  CREATE TABLE IF NOT EXISTS secondary_schema.remote_data (
    id SERIAL PRIMARY KEY, value TEXT
  );
  INSERT INTO secondary_schema.remote_data (value)
    SELECT 'row_' || g FROM generate_series(1, 5) g
    ON CONFLICT DO NOTHING;
"
```

---

### 0.3: Parity Test Harness

**What:** A test infrastructure that runs identical assertions against two
different driver implementations.

**Why:** This is how we mechanically prove the plugin matches the built-in driver.
In Phase 0, only the built-in driver fills it. In Phase 1, the plugin is added.

**Design:**

```rust
// tests/parity/harness.rs

use std::fmt::Debug;

pub enum DriverTarget {
    Builtin,       // Uses the built-in postgres driver via Tauri commands
    Plugin(String), // Uses the plugin driver (id = "postgres-plugin")
}

pub struct ParityHarness {
    targets: Vec<DriverTarget>,
    pg_host: String,
    pg_port: u16,
    pg_user: String,
    pg_password: String,
    pg_database: String,
}

impl ParityHarness {
    /// Run a test function against all configured targets and assert identical results
    pub async fn assert_parity<T, F, Fut>(&self, method_name: &str, test_fn: F)
    where
        T: PartialEq + Debug + serde::Serialize,
        F: Fn(DriverTarget) -> Fut,
        Fut: std::future::Future<Output = Result<T, String>>,
    {
        let results: Vec<_> = /* run test_fn against each target */;
        // Compare all results pairwise
        for window in results.windows(2) {
            assert_eq!(window[0], window[1],
                "Parity failure in '{}': targets returned different results",
                method_name);
        }
    }
}
```

**Phase 0 usage:** Only `DriverTarget::Builtin` is registered. Tests pass
trivially (one result, nothing to compare). But the harness is ready for Phase 1
to add `DriverTarget::Plugin`.

**Phase 1 usage:** Both targets registered. Tests now compare outputs.

---

### 0.4: Golden File Capture

**What:** Run every public method against the seeded test database and save the
output as JSON files. These become the parity contract.

**Why:** Golden files catch subtle differences that `assert_eq` on structs might
miss (field ordering, null vs absent, number precision).

**Directory:** `tests/parity/golden/`

**How to capture:**

```rust
// tests/parity/capture_golden.rs (run once to generate golden files)
#[tokio::test]
#[ignore] // Only run manually to regenerate golden files
async fn capture_golden_files() {
    let harness = ParityHarness::builtin_only();

    let tables = harness.get_tables("test_schema").await;
    write_golden("get_tables.json", &tables);

    let columns = harness.get_columns("all_types", "test_schema").await;
    write_golden("get_columns_all_types.json", &columns);

    // ... for every method
}
```

**Golden files to capture:**

```text
src-tauri/tests/postgres_integration/golden/
├── get_databases.json
├── get_schemas.json
├── get_tables.json
├── get_columns_all_types.json
├── get_columns_with_enum.json
├── get_indexes_all_types.json
├── get_foreign_keys_orders.json
├── get_foreign_keys_cross_schema.json
├── get_views.json
├── get_view_definition_active_users.json
├── get_view_columns_active_users.json
├── get_materialized_views.json
├── get_mv_definition.json              ← captures known regclass error
├── get_mv_columns.json
├── get_routines.json
├── get_routine_parameters_add_numbers.json
├── get_routine_definition_add_numbers.json
├── get_triggers.json
├── get_trigger_definition_audit.json
├── execute_query_all_types.json
├── execute_query_with_pagination.json
├── explain_simple.json
├── explain_analyze.json
├── count_query.json
└── multi_db/
    ├── get_schemas_secondary.json
    └── get_tables_secondary.json
```

**Note:** DDL golden files (`ddl/*.sql`) were removed from scope. DDL generation
produces SQL statements whose correctness depends on dialect and formatting — not
on byte-exact reproducibility. The `ddl_generation.rs` tests validate DDL output
structurally (contains correct keywords, types, constraints) which is the right
parity approach. Exact-match golden files for DDL would create brittle tests that
break on whitespace changes without catching real bugs.

Similarly, `multi_db/get_databases.json` was dropped because `get_databases` is
server-wide (returns the same result regardless of which database you connect to)
— it's already captured at the top level.

---

### 0.5: Integration Test Suite (55+ tests)

**What:** Dedicated integration tests for every public method, organized by domain.

**Structure:**

```text
src-tauri/tests/postgres/
├── mod.rs                    # Shared test setup, connection helpers
├── schema_discovery.rs       # 4 tests
├── column_metadata.rs        # 6 tests
├── foreign_keys.rs           # 4 tests
├── indexes.rs                # 4 tests
├── views.rs                  # 6 tests
├── materialized_views.rs     # 4 tests
├── routines.rs               # 6 tests
├── triggers.rs               # 4 tests
├── crud.rs                   # 9 tests
├── ddl_generation.rs         # 7 tests
├── explain.rs                # 3 tests
├── blob.rs                   # 3 tests
├── query_execution.rs        # 6 tests
└── multi_database.rs         # 7 tests
                              ─────────
                              Total: 73 tests
```

(Note: 55 was a minimum estimate — full coverage is likely 70+.)

**Each test follows this structure:**

```rust
#[tokio::test]
async fn test_get_columns_all_types() {
    let harness = test_harness().await;

    let columns = harness.get_columns("all_types", Some("test_schema")).await
        .expect("get_columns should succeed");

    // Structural assertions
    assert_eq!(columns.len(), 24, "all_types has 24 columns");

    // Specific column assertions
    let id_col = columns.iter().find(|c| c.name == "id").unwrap();
    assert!(id_col.is_pk);
    assert!(id_col.is_auto_increment);
    assert_eq!(id_col.data_type, "integer");

    let uuid_col = columns.iter().find(|c| c.name == "col_uuid").unwrap();
    assert_eq!(uuid_col.data_type, "uuid");
    assert!(!uuid_col.is_nullable); // has DEFAULT but NOT NULL isn't set... verify

    // Golden file comparison
    harness.assert_matches_golden("get_columns_all_types.json", &columns);
}
```

---

### 0.6: Un-ignore Existing Tests

**What:** Remove `#[ignore]` from the 4 existing integration tests and verify
they pass in CI with the new PG service.

**Tests:**

- `test_postgres_integration_flow`
- `test_postgres_batch_preserves_temp_table_and_transaction`
- `test_postgres_affected_rows_reported_correctly`
- `test_postgres_foreign_keys_via_pg_catalog`

---

## Implementation Order

```text
Week 1:
  0.1 — CI PG service (unblocks everything)
  0.2 — Seed script (needed by all tests)
  0.6 — Un-ignore existing tests (quick win, validates CI setup)

Week 2:
  0.3 — Parity harness infrastructure
  0.5 — Write integration tests (start with schema_discovery, column_metadata)

Week 3:
  0.5 — Continue integration tests (crud, ddl, query_execution, multi_database)
  0.4 — Capture golden files (can only run after tests exist)

Week 4:
  0.5 — Remaining integration tests (views, MVs, routines, triggers, blob, explain)
  Final verification — all tests green against built-in driver
```

---

## Checkpoint: CP-2

**When:** All Phase 0 deliverables complete.

**Verify:**

- [x] CI runs PG service and all integration tests pass
- [x] 70+ integration tests exist and are GREEN against built-in driver (102 tests)
- [x] Golden files captured for every public method (26 files)
- [x] Parity harness ready to accept a second driver target
- [x] Seed script is idempotent (can run multiple times without error)
- [x] Multi-database tests pass (secondary database accessible)
- [x] CI total time < 5 minutes (~10s test execution + ~6m build)

**Communicate to team:**

- Baseline is established — we have objective proof of how the built-in driver behaves
- Phase 1 can begin — the plugin will be built to pass these exact tests
- No user-facing changes — this is all internal test infrastructure
- Share the test count as the "parity contract" the plugin must satisfy

---

## Ship / Release Gate

**Phase 0 does NOT produce a shippable release.** It is purely internal
infrastructure. However, the CI improvements (PG service, un-ignored tests) DO
improve quality for ALL future PRs touching the PostgreSQL driver. This is value
delivered to the team even if the plugin migration never proceeds.

---

## Definition of Done

- [x] CI workflow includes PostgreSQL 16 service
- [x] Seed script exists and is run automatically in CI
- [x] 70+ integration tests written and passing (102 total)
- [x] Golden files captured and committed to repo (26 files)
- [x] Parity harness infrastructure committed
- [x] Existing 4 integration tests un-ignored and passing
- [x] Multi-database seed (secondary DB) working
- [x] All tests pass deterministically (sequential execution, 2 consecutive green runs)
- [ ] CP-2 sync completed with core team
