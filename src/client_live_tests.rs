//! Live-database integration tests for `client.rs`'s pool-cache eviction and
//! error formatting (#132).
//!
//! These need a reachable PostgreSQL and are `#[ignore]` by default — run with
//! `cargo test -- --include-ignored client_live_tests`. They target the
//! `tabularis-postgres-demo` podman container (postgres:16-alpine on
//! 127.0.0.1:54320, user `postgres`, password `password`, db `testdb`), but any
//! Postgres reachable via the `PG_PLUGIN_TEST_*` env vars works. Set
//! `PG_PLUGIN_TEST_HOST` to opt in; if it's unset the whole module is skipped
//! (not failed) so CI's ordinary `cargo test` stays green without a DB.
//!
//! What these prove that the unit tests in `client_tests.rs` can't: that a
//! failed `test_connection` (wrong password) does not poison the pool cache for
//! a subsequent `test_connection` with the correct password, that no stale
//! cache entry is left behind after a failure, and that the surfaced error no
//! longer carries the full `tokio_postgres::Error` `Debug` dump.

use super::{connection_key, test_connection, POOLS};
use crate::models::ConnectionParams;
use tokio::sync::Mutex;

// Same rationale as `client_tests::POOLS_TEST_LOCK`: `POOLS` is a process-wide
// static and `cleanup_idle_pools` / other concurrent tests sweep every entry,
// not just their own key. Serializes only the live tests below.
static POOLS_TEST_LOCK: Mutex<()> = Mutex::const_new(());

/// Live test DB connection params. Reads `PG_PLUGIN_TEST_*` env vars, falling
/// back to the local podman demo container. Returns `None` when no host is
/// configured so the test harness skips (not fails) these tests in CI.
fn live_params(password: &str) -> Option<ConnectionParams> {
    let host = std::env::var("PG_PLUGIN_TEST_HOST").ok()?;
    let port: u16 = std::env::var("PG_PLUGIN_TEST_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(54320);
    let user = std::env::var("PG_PLUGIN_TEST_USER").unwrap_or_else(|_| "postgres".to_string());
    let database = std::env::var("PG_PLUGIN_TEST_DB").unwrap_or_else(|_| "testdb".to_string());
    Some(ConnectionParams {
        driver: Some("postgresql".to_string()),
        host: Some(host),
        port: Some(port),
        database: Some(database),
        username: Some(user),
        password: Some(password.to_string()),
        ssl_mode: None,
        ssl_ca: None,
        ssl_cert: None,
        ssl_key: None,
        connection_string: None,
        startup_script: None,
        pooler_mode: None,
    })
}

/// The correct password for the live test DB (env override or default
/// `password` for the podman demo container).
fn correct_password() -> String {
    std::env::var("PG_PLUGIN_TEST_PASSWORD").unwrap_or_else(|_| "password".to_string())
}

/// The headline #132 regression: a failed connection attempt (wrong password)
/// must not poison the pool cache for a subsequent attempt with the correct
/// password. This test exercises Fix 2 (password-keying) — the wrong and
/// correct passwords produce distinct cache keys, so the correct-password call
/// builds a fresh pool regardless of eviction. The eviction branch (Fix 1) is
/// exercised by `failed_auth_leaves_no_cached_pool` below and by the
/// CI-runnable `get_pool_client_evicts_on_connection_failure` in
/// `client_tests.rs` (which doesn't need a live DB).
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn failed_auth_then_correct_auth_succeeds() {
    let _guard = POOLS_TEST_LOCK.lock().await;

    let wrong = live_params("this-is-not-the-password")
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");
    let correct = live_params(&correct_password()).expect("PG_PLUGIN_TEST_HOST not set");

    // First attempt: wrong password must fail.
    let first = test_connection(&wrong).await;
    assert!(first.is_err(), "wrong password must fail; got {:?}", first);

    // Second attempt: correct password must succeed. This is the regression —
    // before the fix it reused the poisoned pool and failed.
    let second = test_connection(&correct).await;
    assert!(
        second.is_ok(),
        "correct password must succeed after a wrong-password failure; got: {:?}",
        second.err()
    );

    // Clean up the entry the successful call left behind.
    let key = connection_key(&correct);
    POOLS.lock().unwrap().remove(&key);
}

/// After a failed `test_connection` (wrong password), the pool cache must
/// hold no entry for that connection's key — the poisoned pool is evicted
/// (Fix 1), not left to be reused or to linger until the 600s idle sweep.
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn failed_auth_leaves_no_cached_pool() {
    let _guard = POOLS_TEST_LOCK.lock().await;

    let wrong = live_params("also-not-the-password").expect("PG_PLUGIN_TEST_HOST not set");
    let key = connection_key(&wrong);

    // Ensure we start clean for this key.
    POOLS.lock().unwrap().remove(&key);

    let result = test_connection(&wrong).await;
    assert!(result.is_err(), "wrong password must fail");

    assert!(
        !POOLS.lock().unwrap().contains_key(&key),
        "a failed connection must not leave a poisoned pool in the cache (#132): \
         key {key} is still present"
    );
}

/// The surfaced error for a failed auth must contain the server's
/// `password authentication failed` message and must NOT carry the full
/// `tokio_postgres::Error` `Debug` dump (`Error { kind: Db, ... }`) that
/// Tabularis shows verbatim in its password prompt (#132 secondary).
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn wrong_password_error_message_has_no_debug_dump() {
    let _guard = POOLS_TEST_LOCK.lock().await;

    let wrong = live_params("definitely-wrong").expect("PG_PLUGIN_TEST_HOST not set");

    let err = test_connection(&wrong)
        .await
        .expect_err("wrong password must fail");
    assert!(
        err.contains("password authentication failed"),
        "error should name the auth failure; got: {err}"
    );
    assert!(
        !err.contains("Error { kind:") && !err.contains("DbError"),
        "error must not include the tokio_postgres Debug dump; got: {err}"
    );

    // Clean up any entry (shouldn't be one, but defensive).
    let key = connection_key(&wrong);
    POOLS.lock().unwrap().remove(&key);
}

/// Regression guard for #160 / #163: a query against a table with a
/// non-builtin column type (a custom `enum` here) must return rows, not
/// hang. The failure mode under a transaction-mode pooler (#160/#163) is
/// tokio-postgres's internal cached named `typeinfo_statement` — fired for
/// non-builtin result column OIDs — colliding with stale server-side
/// prepared statements across pooler backend reassignment, or hanging on
/// Supavisor.
///
/// This test runs against a *direct* Postgres (no pooler), so it validates
/// the happy path and that the enum type still decodes correctly through the
/// unnamed `query_typed` binary path. It does NOT reproduce the pooler
/// failure — that needs a transaction-mode pooler fixture (see
/// `pooled_txn_enum_column_returns_rows` below for the pinned-path coverage,
/// and the manual PgBouncer matrix documented in the PR body).
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn query_with_nonbuiltin_enum_column_returns_rows() {
    use crate::client;
    use crate::handlers::query::execute_query;
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");

    // Unique names so repeated runs (and concurrent test DB users) don't
    // collide. `DROP ... IF EXISTS` is idempotent on both setup and teardown.
    let type_name = "plugin_test_160_color";
    let table_name = "plugin_test_160_enum_table";

    // Setup: create a custom enum type and a table with an enum column, then
    // insert a row. `execute_typed` uses the unnamed protocol with no result
    // columns, so setup itself can't hang on the type-resolution interleave.
    for stmt in [
        format!("DROP TABLE IF EXISTS {table_name}"),
        format!("DROP TYPE IF EXISTS {type_name}"),
        format!("CREATE TYPE {type_name} AS ENUM ('red', 'green', 'blue')"),
        format!("CREATE TABLE {table_name} (id int, color {type_name})"),
        format!("INSERT INTO {table_name} (id, color) VALUES (1, 'red')"),
    ] {
        client::execute_typed(&params, &stmt, &[])
            .await
            .unwrap_or_else(|e| panic!("setup statement failed ({stmt}): {e}"));
    }

    // The actual regression check: SELECT * from the enum-typed table through
    // the full `execute_query` handler path. Build the JSON params object the
    // host would send (connection fields at the top level; `inner_params`
    // falls back to the value itself when there's no nested `params` key).
    let request_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "query": format!("SELECT * FROM {table_name}"),
        "limit": 100,
        "page": 1,
    });

    // Fail fast instead of hanging: the bug manifests as a stall past 120s,
    // so a 30s ceiling catches a regression well before the host-style
    // timeout would, while leaving headroom for a slow podman container.
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query(json!(1), &request_params),
    )
    .await
    .expect(
        "execute_query on an enum-typed table hung past 30s — the #160/#163 regression is present",
    );

    // Success: `result.rows` is a non-empty array.
    assert!(
        response.get("error").is_none(),
        "execute_query should succeed for an enum-typed table; got: {response}"
    );
    let result = response
        .get("result")
        .expect("ok_response carries a `result` object");
    let rows = result
        .get("rows")
        .and_then(|r| r.as_array())
        .expect("`result.rows` is an array");
    assert!(
        !rows.is_empty(),
        "the enum-typed table has one row; got empty rows: {result}"
    );

    // Teardown: drop the test objects, then evict the pool entry this test
    // populated so it doesn't linger for the idle sweep or leak into other
    // tests' cache assertions.
    for stmt in [
        format!("DROP TABLE IF EXISTS {table_name}"),
        format!("DROP TYPE IF EXISTS {type_name}"),
    ] {
        let _ = client::execute_typed(&params, &stmt, &[]).await;
    }
    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}

/// Exercises the transaction-pooler pinned path (`exec_query_pooled_txn`)
/// against a *direct* Postgres. Setting `pooler_mode: "transaction"` routes
/// result-set queries through `BEGIN; DEALLOCATE ALL; prepare; query_typed;
/// DEALLOCATE ALL; COMMIT` on a one-shot client. On a direct connection this
/// must still succeed — the sequence is valid SQL and the enum column must
/// decode correctly — proving the pinned path doesn't break the happy path.
///
/// The actual pooler hang (#160/#163) needs a transaction-mode pooler
/// fixture; this test covers the direct-Postgres correctness of the new
/// path. The manual PgBouncer matrix in the PR body covers the pooler case.
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn pooled_txn_enum_column_returns_rows() {
    use crate::client;
    use crate::handlers::query::execute_query;
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");

    let type_name = "plugin_test_160b_color";
    let table_name = "plugin_test_160b_enum_table";

    for stmt in [
        format!("DROP TABLE IF EXISTS {table_name}"),
        format!("DROP TYPE IF EXISTS {type_name}"),
        format!("CREATE TYPE {type_name} AS ENUM ('red', 'green', 'blue')"),
        format!("CREATE TABLE {table_name} (id int, color {type_name})"),
        format!("INSERT INTO {table_name} (id, color) VALUES (1, 'red')"),
    ] {
        client::execute_typed(&params, &stmt, &[])
            .await
            .unwrap_or_else(|e| panic!("setup statement failed ({stmt}): {e}"));
    }

    // `pooler_mode: "transaction"` routes the result-set query through the
    // pinned one-shot transaction path even against a direct Postgres.
    let request_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "query": format!("SELECT * FROM {table_name}"),
        "limit": 100,
        "page": 1,
    });

    // Run twice. This proves the pinned path is repeatable on a direct
    // connection — no client-side state (a leaked BEGIN, an uncleared
    // DEALLOCATE, a cached typeinfo handle pointing at a dropped statement)
    // from run 1 breaks run 2. It does NOT prove the one-shot client's
    // *discard* prevents the stale-`typeinfo_statement` collision: that
    // collision is pooler-only (it needs a backend reassignment between the
    // prepare and a later reuse), and on direct PG reusing the client would
    // also succeed. The discard property itself is covered by the manual
    // PgBouncer matrix in the PR body, not by this test. A regression in the
    // direct-Postgres repeatable path would surface here as a 26000/42P05
    // or a hang.
    for run in 1..=2 {
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            execute_query(json!(run), &request_params),
        )
        .await
        .expect("pooled-txn enum query hung past 30s on direct Postgres");

        assert!(
            response.get("error").is_none(),
            "pooled-txn run {run} should succeed; got: {response}"
        );
        let result = response
            .get("result")
            .expect("ok_response carries a `result` object");
        let rows = result
            .get("rows")
            .and_then(|r| r.as_array())
            .expect("`result.rows` is an array");
        assert!(
            !rows.is_empty(),
            "pooled-txn run {run}: expected one row; got empty: {result}"
        );
        // The enum value must decode correctly through the binary path.
        assert_eq!(
            rows[0].as_array().unwrap()[1],
            json!("red"),
            "pooled-txn run {run}: enum column should decode to \"red\"; got: {result}"
        );
    }

    for stmt in [
        format!("DROP TABLE IF EXISTS {table_name}"),
        format!("DROP TYPE IF EXISTS {type_name}"),
    ] {
        let _ = client::execute_typed(&params, &stmt, &[]).await;
    }
    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}

/// Regression guard for the routing decision at `exec_query` (#160/#163):
/// a *non-result-set* statement (DDL) on a `pooler_mode: "transaction"`
/// connection must stay on the shared cached-client path, NOT take the
/// pinned one-shot path. The guard is `is_transaction_pooler() &&
/// returns_result_set(query)`; if the `returns_result_set` half were dropped,
/// this DDL would route to `exec_query_pooled_txn` → `run_pinned_body` →
/// `prepare("CREATE TABLE ...")`, which fails (DDL has no prepared-statement
/// form) — so a successful CREATE here proves the guard's FALSE side holds.
///
/// `execute_query` returns a `result` with `affected_rows` for non-result-set
/// statements (the `execute_typed` branch of `exec_query_on_client`), never
/// rows. Run against direct Postgres so the assertion is about routing, not
/// pooler behavior.
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn pooled_txn_ddl_stays_off_the_pinned_path() {
    use crate::client;
    use crate::handlers::query::execute_query;
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");

    let table_name = "plugin_test_160c_ddl_table";
    // Idempotent setup: drop first so a leftover from a prior run doesn't
    // make the CREATE a no-op. The DROP uses execute_typed directly, not the
    // path under test, so it can't mask a routing regression.
    let _ =
        client::execute_typed(&params, &format!("DROP TABLE IF EXISTS {table_name}"), &[]).await;

    // `pooler_mode: "transaction"` + a DDL query. If the routing guard's
    // `returns_result_set` half is intact, this stays on the shared cached
    // client and the CREATE succeeds. If it were removed, this would take
    // the pinned path and fail at `prepare("CREATE TABLE ...")`.
    let request_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "query": format!("CREATE TABLE {table_name} (id int)"),
        "limit": 100,
        "page": 1,
    });

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query(json!(1), &request_params),
    )
    .await
    .expect("pooled-txn DDL query hung past 30s on direct Postgres");

    assert!(
        response.get("error").is_none(),
        "DDL on a transaction-pooler connection should succeed (routing guard keeps it off the \
         pinned path); got: {response}"
    );
    let result = response
        .get("result")
        .expect("ok_response carries a `result` object");
    // Non-result-set statements return rows: [] and affected_rows (0 for
    // CREATE TABLE, which reports no affected rows in PostgreSQL). The key
    // assertion is no error; assert the shape, not the count.
    assert!(
        result.get("rows").and_then(|r| r.as_array()).is_some(),
        "non-result-set statements return a rows array: {result}"
    );

    // Teardown.
    let _ =
        client::execute_typed(&params, &format!("DROP TABLE IF EXISTS {table_name}"), &[]).await;
    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}

/// Regression guard for #166: a `SELECT *` on a table with a custom `enum`
/// column **and wide rows** (enough total bytes to fill tokio-postgres's
/// bounded `responses` channel + the socket buffer) must return rows on a
/// *direct* connection (no pooler, Pooler Mode not set), not hang.
///
/// The failure mode (#166, the rc.11 gap): tokio-postgres's `query_typed`
/// resolves non-builtin column types (`get_type` → the internal named
/// `typeinfo_statement`) *inside* its `RowDescription` loop, mid-stream over
/// the same channel delivering the query's `DataRow`s. The typeinfo reply is
/// queued behind the pending rows, but `query_typed` won't drain the rows
/// until `get_type` returns → a deadlock once the row data fills the buffer
/// (hence the size threshold — a single tiny row passes on the broken path,
/// which is why `query_with_nonbuiltin_enum_column_returns_rows` above didn't
/// catch the rc.11 regression). The fix pre-resolves column types via a
/// `prepare` on the shared cached client before the data fetch, so the
/// `query_typed`'s `get_type` is a cache hit and never fires
/// `typeinfo_statement` mid-drain.
///
/// This test is the wide-variant: 600 rows × a 500-byte `pad` column + the
/// enum column, fetched at `limit` 50 (≈25 KB, enough to fill the buffer).
/// rc.11 hangs past 120s on this; the fix returns within 30s.
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn wide_enum_table_returns_rows_on_direct_connection() {
    use crate::client;
    use crate::handlers::query::execute_query;
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");

    let type_name = "plugin_test_166_wide_mood";
    let table_name = "plugin_test_166_wide_enum_table";

    // Setup: a custom enum type + a *wide* table (600 rows × 500-byte pad +
    // the enum column). The width is the load-bearing part — it's what makes
    // the response large enough to fill the channel buffer and trigger the
    // mid-stream `get_type` deadlock. A narrow enum table (the existing
    // test) passes on the broken path and can't catch this regression.
    for stmt in [
        format!("DROP TABLE IF EXISTS {table_name}"),
        format!("DROP TYPE IF EXISTS {type_name}"),
        format!("CREATE TYPE {type_name} AS ENUM ('happy', 'sad')"),
        format!(
            "CREATE TABLE {table_name} AS SELECT g AS id, 'happy'::{type_name} AS m, \
             repeat('x', 500) AS pad FROM generate_series(1, 600) g"
        ),
    ] {
        client::execute_typed(&params, &stmt, &[])
            .await
            .unwrap_or_else(|e| panic!("setup statement failed ({stmt}): {e}"));
    }

    // Direct connection (no `pooler_mode`) + `limit` 50 (≈25 KB — the
    // hanging case on rc.11). The fix pre-resolves column types via
    // `prepare` on the shared cached client so `query_typed`'s `get_type` is
    // a cache hit; without the fix this hangs past 120s.
    let request_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "query": format!("SELECT * FROM {table_name}"),
        "limit": 50,
        "page": 1,
    });

    // Fail fast instead of hanging: the bug manifests as a stall past 120s,
    // so a 30s ceiling catches a regression well before the host-style
    // timeout would, with headroom for a slow podman container.
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query(json!(1), &request_params),
    )
    .await
    .expect(
        "execute_query on a wide enum-typed table hung past 30s on a direct connection — \
         the #166 regression is present (the prepare pre-resolve is missing or broken)",
    );

    assert!(
        response.get("error").is_none(),
        "execute_query should succeed for a wide enum-typed table on a direct connection; \
         got: {response}"
    );
    let result = response
        .get("result")
        .expect("ok_response carries a `result` object");
    let rows = result
        .get("rows")
        .and_then(|r| r.as_array())
        .expect("`result.rows` is an array");
    assert!(
        !rows.is_empty(),
        "the wide enum-typed table has 600 rows; got empty rows: {result}"
    );
    // The enum value must decode correctly through the binary path.
    assert_eq!(
        rows[0].as_array().unwrap()[1],
        json!("happy"),
        "enum column should decode to \"happy\"; got: {result}"
    );

    // Teardown.
    for stmt in [
        format!("DROP TABLE IF EXISTS {table_name}"),
        format!("DROP TYPE IF EXISTS {type_name}"),
    ] {
        let _ = client::execute_typed(&params, &stmt, &[]).await;
    }
    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}

/// Regression guard for #166 via the **batch** path: `execute_query_batch`
/// routes through `run_batch_in_session` → `exec_query_on_client` (bypassing
/// `exec_query`), so a wide enum result on a direct connection must return rows
/// through the batch path too, not hang. The single-query test above covers
/// `exec_query` → `exec_query_on_client`; this one covers the batch caller,
/// which is a distinct path with its own connection handling. If the
/// pre-resolve were removed from the batch path specifically (e.g.
/// `run_batch_in_session` started passing `is_transaction_pooler = true` to
/// skip it), this test would hang past 30s.
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn wide_enum_table_returns_rows_via_batch_on_direct_connection() {
    use crate::client;
    use crate::handlers::query::execute_query_batch;
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");

    let type_name = "plugin_test_166b_wide_mood";
    let table_name = "plugin_test_166b_wide_enum_table";

    // Same wide-table setup as the single-query test: 600 rows × 500-byte
    // pad + the enum column — enough to fill the channel buffer and trigger
    // the mid-stream `get_type` deadlock without the pre-resolve.
    for stmt in [
        format!("DROP TABLE IF EXISTS {table_name}"),
        format!("DROP TYPE IF EXISTS {type_name}"),
        format!("CREATE TYPE {type_name} AS ENUM ('happy', 'sad')"),
        format!(
            "CREATE TABLE {table_name} AS SELECT g AS id, 'happy'::{type_name} AS m, \
             repeat('x', 500) AS pad FROM generate_series(1, 600) g"
        ),
    ] {
        client::execute_typed(&params, &stmt, &[])
            .await
            .unwrap_or_else(|e| panic!("setup statement failed ({stmt}): {e}"));
    }

    // `execute_query_batch` with no `session_id` routes through
    // `run_batch_in_session` → `exec_query_on_client` (the batch caller),
    // returning a bare `results` array (not the session-aware shape). The
    // batch contains the wide enum `SELECT` at limit 50 — the hanging case
    // on rc.11.
    let request_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "queries": [format!("SELECT * FROM {table_name}")],
        "limit": 50,
        "page": 1,
    });

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(1), &request_params),
    )
    .await
    .expect(
        "execute_query_batch on a wide enum-typed table hung past 30s on a direct connection — \
         the #166 regression is present on the batch path (the pre-resolve is missing or broken)",
    );

    assert!(
        response.get("error").is_none(),
        "execute_query_batch should succeed for a wide enum-typed table; got: {response}"
    );
    // Non-session batch: `ok_response(id, json!(results))` puts the bare
    // results array under the JSON-RPC `result` field (one entry per query).
    let results = response
        .get("result")
        .and_then(|r| r.as_array())
        .expect("non-session batch returns a bare `result` array");
    assert_eq!(results.len(), 1, "one query in the batch; got {response}");
    let first = &results[0];
    assert!(
        first.get("error").and_then(|e| e.as_str()).is_none(),
        "the batch query should not error; got: {first}"
    );
    let rows = first
        .get("result")
        .and_then(|r| r.get("rows"))
        .and_then(|r| r.as_array())
        .expect("the query's result.rows is an array");
    assert!(
        !rows.is_empty(),
        "the wide enum-typed table has 600 rows; got empty rows: {first}"
    );
    assert_eq!(
        rows[0].as_array().unwrap()[1],
        json!("happy"),
        "enum column should decode to \"happy\"; got: {first}"
    );

    // Teardown.
    for stmt in [
        format!("DROP TABLE IF EXISTS {table_name}"),
        format!("DROP TYPE IF EXISTS {type_name}"),
    ] {
        let _ = client::execute_typed(&params, &stmt, &[]).await;
    }
    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}

/// Regression guard for #169: an interactive transaction **session** on a
/// transaction-mode pooler (`pooler_mode: "transaction"` + `session_id` set)
/// must not hang on a table with a non-builtin column type and a large
/// result. The #166 fix (#168) only pre-resolves for non-session single
/// queries; the #164 pinned one-shot path only covers non-session single
/// queries. The batch/session path (`run_batch_in_session`) bypassed both,
/// so a `SELECT *` on a wide enum table inside an open session transaction
/// still deadlocked.
///
/// This test reproduces the reporter's actual scenario: a two-batch session
/// (`BEGIN` in batch 1, `SELECT *` in batch 2) on a `pooler_mode:
/// "transaction"` connection, so the SELECT runs on the reused/pinned
/// connection. The wide-enum table (600 rows × 500-byte pad + the enum
/// column) triggers the mid-stream `typeinfo_statement` deadlock without
/// the pre-resolve. rc.11/#168 hangs past 120s on this; the fix pre-resolves
/// via `prepare` inside the user's open transaction (the backend is pinned,
/// so the named statement can't be orphaned) → returns cleanly.
///
/// Run against direct Postgres with `pooler_mode: "transaction"` (the
/// `pooler_mode` flag routes through the pooler code paths even without a
/// real pooler; the deadlock is a tokio-postgres interleave, not
/// pooler-specific, so direct PG reproduces it).
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn wide_enum_table_returns_rows_via_session_pinned_batch_on_pooler() {
    use crate::client;
    use crate::handlers::query::{execute_query_batch, release_session};
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");

    let type_name = "plugin_test_169_session_mood";
    let table_name = "plugin_test_169_session_enum_table";
    let session_id = "test-169-session-pinned";

    // Setup: same wide-table fixture as the #166 tests (600 rows × 500-byte
    // pad + the enum column — enough to fill the channel buffer and trigger
    // the mid-stream get_type deadlock without the pre-resolve).
    for stmt in [
        format!("DROP TABLE IF EXISTS {table_name}"),
        format!("DROP TYPE IF EXISTS {type_name}"),
        format!("CREATE TYPE {type_name} AS ENUM ('happy', 'sad')"),
        format!(
            "CREATE TABLE {table_name} AS SELECT g AS id, 'happy'::{type_name} AS m, \
             repeat('x', 500) AS pad FROM generate_series(1, 600) g"
        ),
    ] {
        client::execute_typed(&params, &stmt, &[])
            .await
            .unwrap_or_else(|e| panic!("setup statement failed ({stmt}): {e}"));
    }

    // Batch 1: BEGIN (opens a transaction, pins the connection to the session).
    let begin_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": ["BEGIN"],
        "session_id": session_id,
        "limit": 100,
        "page": 1,
    });
    let begin_resp = execute_query_batch(json!(1), &begin_params).await;
    assert!(
        begin_resp.get("error").is_none(),
        "BEGIN batch should succeed; got: {begin_resp}"
    );

    // Batch 2: SELECT * on the wide enum table, same session (reused/pinned
    // connection). This is the hanging case on rc.11/#168: the batch path
    // passes is_transaction_pooler=true to exec_query_on_client, skipping the
    // pre-resolve → query_typed fires typeinfo_statement mid-stream on the
    // wide result → deadlock. With the fix, in_transaction=true → the
    // pre-resolve prepare fires inside the user's open transaction (backend
    // pinned) → no mid-stream typeinfo_statement → returns cleanly.
    let select_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": [format!("SELECT * FROM {table_name}")],
        "session_id": session_id,
        "limit": 50,
        "page": 1,
    });
    let select_resp = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(2), &select_params),
    )
    .await
    .expect(
        "session-pinned SELECT on a wide enum table hung past 30s on a pooler-mode connection — \
         the #169 regression is present (the session-pinned pre-resolve is missing or broken)",
    );

    assert!(
        select_resp.get("error").is_none(),
        "session-pinned SELECT batch should succeed; got: {select_resp}"
    );
    // Session-aware batch returns {results, in_transaction}.
    let results = select_resp
        .get("result")
        .and_then(|r| r.get("results"))
        .and_then(|r| r.as_array())
        .expect("session-aware batch returns result.results");
    assert_eq!(
        results.len(),
        1,
        "one query in the batch; got {select_resp}"
    );
    let first = &results[0];
    assert!(
        first.get("error").and_then(|e| e.as_str()).is_none(),
        "the SELECT should not error; got: {first}"
    );
    let rows = first
        .get("result")
        .and_then(|r| r.get("rows"))
        .and_then(|r| r.as_array())
        .expect("the query's result.rows is an array");
    assert!(
        !rows.is_empty(),
        "the wide enum-typed table has 600 rows; got empty rows: {first}"
    );
    assert_eq!(
        rows[0].as_array().unwrap()[1],
        json!("happy"),
        "enum column should decode to \"happy\"; got: {first}"
    );

    // Batch 3: ROLLBACK (ends the transaction, releases the pin).
    let rollback_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": ["ROLLBACK"],
        "session_id": session_id,
        "limit": 100,
        "page": 1,
    });
    let _ = execute_query_batch(json!(3), &rollback_params).await;

    // Release the session (frees the pinned connection).
    let _ = release_session(json!(4), &json!({"session_id": session_id})).await;

    // Teardown.
    for stmt in [
        format!("DROP TABLE IF EXISTS {table_name}"),
        format!("DROP TYPE IF EXISTS {type_name}"),
    ] {
        let _ = client::execute_typed(&params, &stmt, &[]).await;
    }
    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}

/// Regression guard for #169 (non-session sub-case): a non-session batch
/// (`session_id` absent) on a transaction-mode pooler must route result-set
/// queries to the one-shot pinned path (`exec_query_pooled_txn`), not the
/// shared cached client (where a plain `prepare` would collide across
/// backend reassignment). This is the batch-path equivalent of the existing
/// `pooled_txn_enum_column_returns_rows` test (which covers the single-query
/// `exec_query` routing).
///
/// Without the fix, a non-session pooler batch's result-set query would run
/// `query_typed` on the shared cached client with no pre-resolve → the same
/// mid-stream `typeinfo_statement` deadlock on wide non-builtin results. With
/// the fix, `in_transaction=false` + `is_transaction_pooler()` +
/// `returns_result_set` routes to `exec_query_pooled_txn` (one-shot client,
/// BEGIN/prepare/query_typed/COMMIT) → returns cleanly.
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn wide_enum_table_returns_rows_via_non_session_batch_on_pooler() {
    use crate::client;
    use crate::handlers::query::execute_query_batch;
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");

    let type_name = "plugin_test_169_nosession_mood";
    let table_name = "plugin_test_169_nosession_enum_table";

    // Setup: same wide-table fixture (600 rows × 500-byte pad + enum column).
    for stmt in [
        format!("DROP TABLE IF EXISTS {table_name}"),
        format!("DROP TYPE IF EXISTS {type_name}"),
        format!("CREATE TYPE {type_name} AS ENUM ('happy', 'sad')"),
        format!(
            "CREATE TABLE {table_name} AS SELECT g AS id, 'happy'::{type_name} AS m, \
             repeat('x', 500) AS pad FROM generate_series(1, 600) g"
        ),
    ] {
        client::execute_typed(&params, &stmt, &[])
            .await
            .unwrap_or_else(|e| panic!("setup statement failed ({stmt}): {e}"));
    }

    // Non-session batch (no `session_id`): `in_transaction=false` +
    // `is_transaction_pooler()` + `returns_result_set` → routes to
    // `exec_query_pooled_txn` (one-shot client + BEGIN/prepare/query_typed/
    // COMMIT). The wide enum table at limit 50 is the hanging case without
    // the routing.
    let request_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": [format!("SELECT * FROM {table_name}")],
        "limit": 50,
        "page": 1,
    });

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(1), &request_params),
    )
    .await
    .expect(
        "non-session batch SELECT on a wide enum table hung past 30s on a pooler-mode connection — \
         the #169 regression is present (the non-session batch routing is missing or broken)",
    );

    assert!(
        response.get("error").is_none(),
        "non-session batch should succeed; got: {response}"
    );
    // Non-session batch: `ok_response(id, json!(results))` puts the bare
    // results array under the JSON-RPC `result` field.
    let results = response
        .get("result")
        .and_then(|r| r.as_array())
        .expect("non-session batch returns a bare `result` array");
    assert_eq!(results.len(), 1, "one query in the batch; got {response}");
    let first = &results[0];
    assert!(
        first.get("error").and_then(|e| e.as_str()).is_none(),
        "the batch query should not error; got: {first}"
    );
    let rows = first
        .get("result")
        .and_then(|r| r.get("rows"))
        .and_then(|r| r.as_array())
        .expect("the query's result.rows is an array");
    assert!(
        !rows.is_empty(),
        "the wide enum-typed table has 600 rows; got empty rows: {first}"
    );
    assert_eq!(
        rows[0].as_array().unwrap()[1],
        json!("happy"),
        "enum column should decode to \"happy\"; got: {first}"
    );

    // Teardown.
    for stmt in [
        format!("DROP TABLE IF EXISTS {table_name}"),
        format!("DROP TYPE IF EXISTS {type_name}"),
    ] {
        let _ = client::execute_typed(&params, &stmt, &[]).await;
    }
    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}

/// Regression guard for the empty-rows column-discovery branch on the #169
/// session-pinned pooler batch path. `exec_query_on_client`'s empty-rows
/// block falls back to `prepare` for column discovery when
/// `pre_resolve_types` is true (query.rs:849) — a path that became reachable
/// for session-pinned pooler batches only after #169 passed
/// `pre_resolve_types=true` on the `in_transaction` branch. Before #169 a
/// pooler (`is_transaction_pooler=true`) always skipped `prepare`, so an
/// empty result returned `columns: []`; after #169 it returns the real
/// column names. Neither #169 test exercises this (both use 600-row tables),
/// so this test guards the branch: a `SELECT ... WHERE 1=0` in a
/// session-pinned transaction on a `pooler_mode: "transaction"` connection
/// must return the table's column names, not `[]`.
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn empty_result_returns_columns_via_session_pinned_batch_on_pooler() {
    use crate::client;
    use crate::handlers::query::{execute_query_batch, release_session};
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");

    let type_name = "plugin_test_169_empty_mood";
    let table_name = "plugin_test_169_empty_enum_table";
    let session_id = "test-169-empty-columns";

    // Setup: the same enum-typed table, but the query below selects zero rows.
    for stmt in [
        format!("DROP TABLE IF EXISTS {table_name}"),
        format!("DROP TYPE IF EXISTS {type_name}"),
        format!("CREATE TYPE {type_name} AS ENUM ('happy', 'sad')"),
        format!(
            "CREATE TABLE {table_name} AS SELECT g AS id, 'happy'::{type_name} AS m, \
             repeat('x', 500) AS pad FROM generate_series(1, 600) g"
        ),
    ] {
        client::execute_typed(&params, &stmt, &[])
            .await
            .unwrap_or_else(|e| panic!("setup statement failed ({stmt}): {e}"));
    }

    // Batch 1: BEGIN (pins the connection; sets in_transaction=true for the
    // next batch, so the SELECT routes to the pre-resolve-on-shared-client
    // path with pre_resolve_types=true).
    let begin_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": ["BEGIN"],
        "session_id": session_id,
        "limit": 100,
        "page": 1,
    });
    let begin_resp = execute_query_batch(json!(1), &begin_params).await;
    assert!(
        begin_resp.get("error").is_none(),
        "BEGIN batch should succeed; got: {begin_resp}"
    );

    // Batch 2: a SELECT that returns ZERO rows. With the #169 fix this runs
    // pre_resolve_types=true on the shared (session-pinned) client, so
    // `prepare` succeeds and the empty-rows branch reuses `.columns()` →
    // real column names. If the empty-rows fallback condition is reverted to
    // `!is_transaction_pooler` (or the pre-resolve is skipped), this returns
    // `columns: []` and the assertion fails.
    let select_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": [format!("SELECT id, m, pad FROM {table_name} WHERE 1 = 0")],
        "session_id": session_id,
        "limit": 50,
        "page": 1,
    });
    let select_resp = execute_query_batch(json!(2), &select_params).await;
    assert!(
        select_resp.get("error").is_none(),
        "empty SELECT batch should succeed; got: {select_resp}"
    );
    let results = select_resp
        .get("result")
        .and_then(|r| r.get("results"))
        .and_then(|r| r.as_array())
        .expect("session-aware batch returns result.results");
    let first = &results[0];
    assert!(
        first.get("error").and_then(|e| e.as_str()).is_none(),
        "the SELECT should not error; got: {first}"
    );
    let columns = first
        .get("result")
        .and_then(|r| r.get("columns"))
        .and_then(|r| r.as_array())
        .expect("the query's result.columns is an array");
    assert_eq!(
        columns,
        &["id", "m", "pad"]
            .iter()
            .map(|s| json!(s))
            .collect::<Vec<_>>(),
        "an empty result on a session-pinned pooler batch must return the \
         real column names (the pre-resolve ran); got `columns: {columns:?}` — \
         if this is `[]`, the empty-rows column-discovery fallback regressed",
    );

    // Batch 3: ROLLBACK (ends the transaction, releases the pin).
    let rollback_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": ["ROLLBACK"],
        "session_id": session_id,
        "limit": 100,
        "page": 1,
    });
    let _ = execute_query_batch(json!(3), &rollback_params).await;
    let _ = release_session(json!(4), &json!({"session_id": session_id})).await;

    // Teardown.
    for stmt in [
        format!("DROP TABLE IF EXISTS {table_name}"),
        format!("DROP TYPE IF EXISTS {type_name}"),
    ] {
        let _ = client::execute_typed(&params, &stmt, &[]).await;
    }
    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}

/// Regression guard for #173 (temp-table case): a bare non-session batch on
/// a `pooler_mode: "transaction"` connection that creates a temp table in an
/// earlier statement then selects from it must keep the `SELECT` on the
/// shared batch connection — not the #172 one-shot path, which runs the
/// `SELECT` on a fresh connection that never saw the temp table.
///
/// On rc.11+#172 this errors `ERROR: relation "t" does not exist` (the
/// `SELECT` runs on a one-shot connection where `CREATE TEMP TABLE` never
/// ran). The #173 fix detects the `CREATE TEMP TABLE` as session-local state
/// and routes the `SELECT` to the shared client with a column-type
/// pre-resolve, where the temp table is visible.
///
/// Run against direct Postgres with `pooler_mode: "transaction"` (the
/// clearest regression config — one TCP connection = one backend, held —
/// where `main` preserved within-batch state and #172 broke it). A real
/// PgBouncer reassigns the backend per autocommit transaction, so the
/// temp-table-in-a-bare-batch case needs `BEGIN`/`COMMIT` there regardless;
/// that path is covered by `wide_enum_table_returns_rows_via_session_pinned_
/// batch_on_pooler`.
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn temp_table_visible_in_bare_batch_on_pooler() {
    use crate::client;
    use crate::handlers::query::execute_query_batch;
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");

    // A unique temp-table name so a leftover from a prior run can't mask the
    // assertion. Temp tables are connection-scoped and dropped at session end,
    // but the bare batch uses a cached pool client that may survive the test.
    let table_name = "plugin_test_173_temp_tbl";

    // Ensure no permanent leftover (defensive — temp tables don't persist, but
    // a prior crashed run could have left a permanent table of the same name).
    let _ =
        client::execute_typed(&params, &format!("DROP TABLE IF EXISTS {table_name}"), &[]).await;

    // Bare non-session batch (no `session_id`): `CREATE TEMP TABLE` (a
    // non-result-set statement → `else` branch on the shared client, sets
    // `session_state_set`) then `SELECT * FROM t` (result-set → with the fix,
    // `!in_transaction` + `session_state_set` routes to the shared client,
    // not the one-shot). On rc.11+#172 the SELECT takes the one-shot path and
    // errors `relation "t" does not exist`.
    let request_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": [
            format!("CREATE TEMP TABLE {table_name} AS SELECT 1 AS id"),
            format!("SELECT * FROM {table_name}"),
        ],
        "limit": 100,
        "page": 1,
    });

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(1), &request_params),
    )
    .await
    .expect(
        "bare [CREATE TEMP TABLE, SELECT] batch hung past 30s on a pooler-mode connection — \
         the #173 regression is present (the temp-table SELECT misroutes to the one-shot path)",
    );

    assert!(
        response.get("error").is_none(),
        "bare batch should succeed; got: {response}"
    );
    // Non-session batch returns a bare `result` array.
    let results = response
        .get("result")
        .and_then(|r| r.as_array())
        .expect("non-session batch returns a bare `result` array");
    assert_eq!(results.len(), 2, "two queries in the batch; got {response}");

    // The CREATE TEMP TABLE statement (non-result-set) returns affected_rows,
    // no error.
    let create_result = &results[0];
    assert!(
        create_result
            .get("error")
            .and_then(|e| e.as_str())
            .is_none(),
        "CREATE TEMP TABLE should not error; got: {create_result}"
    );

    // The SELECT must succeed and see the temp table on the shared
    // connection. On rc.11+#172 this is the failing statement (one-shot path
    // → `relation "t" does not exist`).
    let select_result = &results[1];
    assert!(
        select_result
            .get("error")
            .and_then(|e| e.as_str())
            .is_none(),
        "SELECT after CREATE TEMP TABLE should see the temp table (not error \
         `relation \"t\" does not exist`); got: {select_result}"
    );
    let rows = select_result
        .get("result")
        .and_then(|r| r.get("rows"))
        .and_then(|r| r.as_array())
        .expect("the SELECT's result.rows is an array");
    assert_eq!(
        rows.len(),
        1,
        "the temp table has one row; got empty: {select_result}"
    );
    assert_eq!(
        rows[0].as_array().unwrap()[0],
        json!(1),
        "the temp table's id column is 1; got: {select_result}"
    );

    // Teardown. The temp table is connection-scoped; the cached pool client
    // holds it, so drop it explicitly before clearing the pool entry (a
    // recycled connection would otherwise carry the temp table to the next
    // borrower on direct PG). Then evict the pool so no state leaks to other
    // tests.
    let _ =
        client::execute_typed(&params, &format!("DROP TABLE IF EXISTS {table_name}"), &[]).await;
    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}

/// Regression guard for #173 (session-SET case): a bare non-session batch on
/// a `pooler_mode: "transaction"` connection that sets a session GUC in an
/// earlier statement then reads it back must keep the `SELECT` on the shared
/// batch connection so it sees the `SET`.
///
/// `SET extra_float_digits = 3` (default is `1`) is read back via
/// `SELECT current_setting('extra_float_digits')`, which returns the GUC's
/// *current* value as `text`. On rc.11+#172 the `SELECT` runs on a one-shot
/// connection that never saw the `SET`, so it returns the default `"1"`
/// (the SET is lost). The #173 fix detects the `SET` as session-local state
/// and routes the `SELECT` to the shared client where the GUC took effect,
/// returning `"3"`. The probe is a direct `current_setting` read (not a
/// value whose rendering the GUC influences), so the assertion is independent
/// of how the plugin decodes/formats `float8`/`numeric` — it proves the
/// SELECT ran on the connection that saw the SET, unambiguously.
///
/// Run against direct Postgres with `pooler_mode: "transaction"` (the
/// clearest regression config). A real PgBouncer loses the GUC to backend
/// reassignment between autocommit statements regardless (pre-existing on
/// `main`), so the GUC case is not asserted on a real pooler — only the
/// direct+flag config where `main` preserved it.
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn session_set_visible_in_bare_batch_on_pooler() {
    use crate::handlers::query::execute_query_batch;
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");

    // Bare non-session batch: `SET extra_float_digits = 3` (non-result-set →
    // shared client, sets `session_state_set`) then
    // `SELECT current_setting('extra_float_digits')` (result-set → with the
    // fix, routes to the shared client, not the one-shot). On rc.11+#172 the
    // SELECT takes the one-shot path, loses the SET, and returns the default
    // "1" instead of "3".
    let request_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": [
            "SET extra_float_digits = 3",
            "SELECT current_setting('extra_float_digits') AS digits",
        ],
        "limit": 100,
        "page": 1,
    });

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(1), &request_params),
    )
    .await
    .expect(
        "bare [SET, SELECT] batch hung past 30s on a pooler-mode connection — \
         the #173 regression is present",
    );

    assert!(
        response.get("error").is_none(),
        "bare batch should succeed; got: {response}"
    );
    let results = response
        .get("result")
        .and_then(|r| r.as_array())
        .expect("non-session batch returns a bare `result` array");
    assert_eq!(results.len(), 2, "two queries in the batch; got {response}");

    let set_result = &results[0];
    assert!(
        set_result.get("error").and_then(|e| e.as_str()).is_none(),
        "SET should not error; got: {set_result}"
    );

    let select_result = &results[1];
    assert!(
        select_result
            .get("error")
            .and_then(|e| e.as_str())
            .is_none(),
        "SELECT after SET should not error; got: {select_result}"
    );
    let rows = select_result
        .get("result")
        .and_then(|r| r.get("rows"))
        .and_then(|r| r.as_array())
        .expect("the SELECT's result.rows is an array");
    assert_eq!(rows.len(), 1, "one row; got: {select_result}");

    // `current_setting('extra_float_digits')` returns the GUC's current value
    // as text: "3" if the SELECT ran on the connection that saw the SET, "1"
    // (the default) if it ran on a one-shot that lost the SET. Decoded as a
    // JSON string (text column).
    let val = &rows[0].as_array().unwrap()[0];
    assert_eq!(
        val,
        &json!("3"),
        "current_setting('extra_float_digits') should be \"3\" (the SET took \
         effect on the SELECT's connection); got: {val} — if this is \"1\", the \
         SELECT ran on a one-shot connection that lost the SET (#173 regression)"
    );

    // Teardown: evict the pool entry so the SET GUC doesn't leak to other
    // tests on the cached direct-PG connection.
    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}

/// Regression test for #170 (the 25P02 transaction-abort cascade, the second
/// consequence of the `.ok()` prepare swallow found during #172's review).
///
/// On a transaction-mode pooler, a session-pinned batch inside an open
/// transaction (`in_transaction=true`) pre-resolves column types via
/// `prepare` (PR #172). If the user's query has a Parse-time error (e.g.
/// `SELECT * FROM nonexistent_table` → SQLSTATE 42P01), the swallowed `prepare`
/// *aborts the transaction server-side*, then `query_typed` fails with 25P02
/// ("current transaction is aborted") instead of the real 42P01. Worse:
/// `transaction_effect("SELECT")=None` keeps `in_transaction=true`, the
/// aborted connection is re-pinned, and the *next* batch also 25P02s until an
/// explicit ROLLBACK — a confusing cascade that hides the root cause.
///
/// The fix (#170): when `prepare` fails *inside an open transaction*,
/// short-circuit — ROLLBACK and surface the real prepare error (42P01),
/// never running `query_typed` into the guaranteed 25P02. The caller then
/// treats the transaction as closed, so the connection is not re-pinned in
/// aborted state. Batch 3 (`SELECT 1`) proves that: it must succeed, not
/// 25P02.
///
/// Outside an open transaction (`in_transaction=false`) the swallow is
/// preserved — a 42P05 on a real PgBouncer bare batch is recoverable because
/// `query_typed` (unnamed) can still succeed. This test only covers the
/// `in_transaction=true` case; the recoverable-swallow case is exercised by
/// the #174 `temp_table`/`session_set` tests.
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn prepare_error_in_open_transaction_surfaces_real_sqlstate_not_25p02() {
    use crate::handlers::query::{execute_query_batch, release_session};
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");
    let session_id = "test-170-25p02-cascade";

    // Batch 1: BEGIN (opens a transaction, pins the connection to the session,
    // sets in_transaction=true for the next batch so the SELECT routes to the
    // pre-resolve-on-shared-client path with pre_resolve_types=true).
    let begin_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": ["BEGIN"],
        "session_id": session_id,
        "limit": 100,
        "page": 1,
    });
    let begin_resp = execute_query_batch(json!(1), &begin_params).await;
    assert!(
        begin_resp.get("error").is_none(),
        "BEGIN batch should succeed; got: {begin_resp}"
    );

    // Batch 2: a SELECT against a nonexistent table inside the open
    // transaction. The prepare (Parse+Describe+Sync) fails with 42P01
    // ("relation \"...\" does not exist"). On current code the `.ok()` swallow
    // discards that error, the transaction is now aborted server-side, and
    // `query_typed` fails with 25P02 — the user sees "current transaction is
    // aborted", not the real "does not exist". With the fix, the prepare's
    // 42P01 is surfaced directly.
    let select_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": ["SELECT * FROM plugin_test_170_nonexistent_table"],
        "session_id": session_id,
        "limit": 50,
        "page": 1,
    });
    let select_resp = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(2), &select_params),
    )
    .await
    .expect("the 42P01 batch hung past 30s — unexpected; got: ?");
    assert!(
        select_resp.get("error").is_none(),
        "batch should not top-level error; got: {select_resp}"
    );
    let results = select_resp
        .get("result")
        .and_then(|r| r.get("results"))
        .and_then(|r| r.as_array())
        .expect("session-aware batch returns result.results");
    assert_eq!(
        results.len(),
        1,
        "one query in the batch; got: {select_resp}"
    );
    let first = &results[0];
    let err_msg = first
        .get("error")
        .and_then(|e| e.as_str())
        .expect("the SELECT should error (nonexistent table); got: {first}");

    // The headline assertion: the real 42P01 surfaces, NOT the 25P02 cascade.
    assert!(
        err_msg.contains("does not exist"),
        "the error should be the real 42P01 (\"does not exist\"), not the 25P02 \
         transaction-abort cascade; got: {err_msg:?}"
    );
    assert!(
        !err_msg.contains("current transaction is aborted"),
        "the error must not be the 25P02 cascade (\"current transaction is \
         aborted\"); got: {err_msg:?}"
    );
    // The fix ROLLBACKs the aborted transaction and must tell the user so the
    // state change isn't silent (the user's earlier writes in the transaction
    // were discarded). Without this, a multi-statement batch's user wouldn't
    // know the transaction was ended.
    assert!(
        err_msg.contains("rolled back"),
        "the error should tell the user the transaction was rolled back; got: {err_msg:?}"
    );

    // Batch 3: a plain `SELECT 1` on the same session. On current code the
    // aborted connection was re-pinned (in_transaction stayed true), so this
    // also 25P02s — the cascade. With the fix, the transaction was closed by
    // the short-circuit's ROLLBACK and in_transaction was forced false, so
    // this SELECT runs cleanly.
    let followup_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": ["SELECT 1 AS one"],
        "session_id": session_id,
        "limit": 50,
        "page": 1,
    });
    let followup_resp = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(3), &followup_params),
    )
    .await
    .expect("the follow-up SELECT hung past 30s — the connection was likely re-pinned in aborted state (#170 cascade)");
    assert!(
        followup_resp.get("error").is_none(),
        "follow-up batch should not top-level error; got: {followup_resp}"
    );
    let followup_results = followup_resp
        .get("result")
        .and_then(|r| r.get("results"))
        .and_then(|r| r.as_array())
        .expect("follow-up batch returns result.results");
    let followup_first = &followup_results[0];
    assert!(
        followup_first
            .get("error")
            .and_then(|e| e.as_str())
            .is_none(),
        "the follow-up SELECT must succeed (the connection was NOT re-pinned in \
         aborted state); got: {followup_first}"
    );
    let followup_rows = followup_first
        .get("result")
        .and_then(|r| r.get("rows"))
        .and_then(|r| r.as_array())
        .expect("follow-up result.rows is an array");
    assert_eq!(
        followup_rows[0].as_array().unwrap()[0],
        json!(1),
        "follow-up SELECT 1 returns 1; got: {followup_first}"
    );

    // Release the session (frees the pinned connection). The fix already
    // ROLLBACK'd the aborted transaction, so release's own ROLLBACK is a
    // harmless no-op.
    let _ = release_session(json!(4), &json!({"session_id": session_id})).await;

    // Teardown: evict the pool entry so no state leaks to other tests.
    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}

/// Regression test for #170 hole 2: the empty-rows column-discovery branch's
/// second `prepare` (the second swallow at `exec_query_on_client`) must be
/// bounded by `POOLED_TXN_TIMEOUT` when the *first* pre-resolve `prepare`
/// failed, so a cold type cache can't hang the worker past the host's 120s.
///
/// This reproduces a bare batch (`session_state_set`, no open transaction) on
/// a transaction-mode pooler where the SELECT must stay on the shared client
/// (to see a prior `SET`/temp table) but targets a nonexistent table: the
/// first `prepare` 42P01s, the swallow preserves it (the recoverable case —
/// `in_transaction=false`), and `query_typed` runs. Because the table doesn't
/// exist `query_typed` also 42P01s (returning rows fails), so we land in the
/// empty-rows branch's column-discovery `else if pre_resolve_types` arm where
/// the *second* `prepare` runs — now bounded by the timeout per #170, so a
/// wedged pooler can't pin the worker. The assertion is simply that this
/// completes well within the 30s fail-fast ceiling (no hang) and surfaces the
/// real 42P01 (not a 25P02 cascade — there's no open transaction to abort).
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn empty_result_column_discovery_after_prepare_failure_does_not_hang() {
    use crate::handlers::query::execute_query_batch;
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");

    // A bare batch (no `session_id`, so no open transaction) on a
    // transaction-mode pooler. The `SET` makes `session_state_set=true`, so
    // the SELECT stays on the shared client (`in_transaction=false`,
    // `pre_resolve_types=true`) — the recoverable-swallow path. The SELECT
    // targets a nonexistent table, so both prepares 42P01.
    let batch_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": [
            "SET extra_float_digits = 3",
            "SELECT * FROM plugin_test_170_hole2_nonexistent_table",
        ],
        "limit": 50,
        "page": 1,
    });
    // The headline assertion: this completes within 30s (no hang). On the
    // unfixed code the second prepare is unbounded; on a wedged pooler it
    // could pin the worker to the host's 120s. Against the live direct-PG
    // container the prepare fails fast (42P01), so this is a guard that the
    // bounded path stays bounded and surfaces the real error, not a
    // 25P02 (there's no open transaction to abort).
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(1), &batch_params),
    )
    .await
    .expect("the bare-batch SELECT against a nonexistent table hung past 30s — the #170 hole-2 bound is missing");

    // A bare batch returns a bare `result` array (no session wrapper).
    let results = resp
        .get("result")
        .and_then(|r| r.as_array())
        .expect("bare batch returns a result array");
    // The SET succeeds; the SELECT fails with 42P01.
    let select_outcome = &results[1];
    let err_msg = select_outcome
        .get("error")
        .and_then(|e| e.as_str())
        .expect("the SELECT should error (nonexistent table); got: {select_outcome}");
    assert!(
        err_msg.contains("does not exist"),
        "the error should be the real 42P01 (\"does not exist\"); got: {err_msg:?}"
    );
    assert!(
        !err_msg.contains("current transaction is aborted"),
        "no open transaction here — must not be a 25P02 cascade; got: {err_msg:?}"
    );

    // Teardown: evict the pool entry.
    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}

/// Regression test for #170 on the **direct-connection** batch path inside an
/// open transaction (the pre-existing surface from #168 the plan said was "also
/// fixed" but the test-coverage review found untested at the callsite level).
///
/// The headline `prepare_error_in_open_transaction_surfaces_real_sqlstate_not_25p02`
/// test uses `pooler_mode: "transaction"`, which routes through `run_batch_in_session`'s
/// `if shared_client` arm (`query.rs:244`). A *direct* connection (no `pooler_mode`)
/// inside a user's `BEGIN` routes through the `else` arm (`query.rs:293`) with
/// `pre_resolve_types = !is_transaction_pooler() = true` and `in_transaction`
/// threaded from the batch loop. A prepare failure there aborts the transaction
/// the same way — this test proves the short-circuit fires on that callsite too,
/// surfacing the real 42P01 (not 25P02) and not re-pinning the aborted connection.
///
/// Uses a bare batch (no `session_id`) so `run_batch_in_session` releases the
/// connection at batch end (`in_transaction=false` after the short-circuit),
/// rather than re-pinning it. The follow-up `SELECT 1` would 25P02 on a re-pinned
/// aborted connection; on a fresh pool connection (the released path) it succeeds.
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn prepare_error_in_open_transaction_on_direct_connection_surfaces_real_sqlstate() {
    use crate::handlers::query::execute_query_batch;
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");

    // Bare batch (no `session_id`, no `pooler_mode` → direct connection) with a
    // `BEGIN` followed by a SELECT against a nonexistent table. The BEGIN opens
    // a transaction on the shared direct client, so the SELECT routes to the
    // `else` arm of `run_batch_in_session` with `in_transaction=true` and
    // `pre_resolve_types=true`. The prepare 42P01s; without the fix the swallow
    // would cascade to 25P02.
    let batch_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        // No `pooler_mode` → direct connection, NOT a transaction pooler.
        "queries": [
            "BEGIN",
            "SELECT * FROM plugin_test_170_direct_nonexistent_table",
        ],
        "limit": 50,
        "page": 1,
    });
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(1), &batch_params),
    )
    .await
    .expect("the bare direct-connection batch hung past 30s — unexpected");
    assert!(
        resp.get("error").is_none(),
        "batch should not top-level error; got: {resp}"
    );
    // A bare batch returns a bare `result` array (no session wrapper).
    let results = resp
        .get("result")
        .and_then(|r| r.as_array())
        .expect("bare batch returns a result array");
    assert_eq!(results.len(), 2, "two queries in the batch; got: {resp}");

    // Statement 1: BEGIN succeeds.
    let begin_outcome = &results[0];
    assert!(
        begin_outcome
            .get("error")
            .and_then(|e| e.as_str())
            .is_none(),
        "BEGIN should succeed; got: {begin_outcome}"
    );

    // Statement 2: the SELECT against a nonexistent table. The short-circuit
    // must fire (in_transaction=true, prepare 42P01) and surface the real 42P01,
    // not the 25P02 cascade.
    let select_outcome = &results[1];
    let err_msg = select_outcome
        .get("error")
        .and_then(|e| e.as_str())
        .expect("the SELECT should error (nonexistent table); got: {select_outcome}");
    assert!(
        err_msg.contains("does not exist"),
        "the error should be the real 42P01 (\"does not exist\"), not the 25P02 \
         transaction-abort cascade; got: {err_msg:?}"
    );
    assert!(
        !err_msg.contains("current transaction is aborted"),
        "the error must not be the 25P02 cascade (\"current transaction is \
         aborted\"); got: {err_msg:?}"
    );
    // The fix ROLLBACKs the aborted transaction and must tell the user so the
    // state change isn't silent (the user's earlier writes in the transaction
    // were discarded). Without this, a multi-statement batch's user wouldn't
    // know the transaction was ended.
    assert!(
        err_msg.contains("rolled back"),
        "the error should tell the user the transaction was rolled back; got: {err_msg:?}"
    );

    // Teardown: evict the pool entry so the aborted-then-ROLLBACK'd connection's
    // state doesn't leak to other tests on the cached direct-PG connection.
    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}

/// Regression test for #181 — the 25P02 cascade from a `query_typed` *runtime*
/// error inside an open transaction (the path #170 deliberately left
/// unchanged; the same cascade class from a different source).
///
/// #170 closed the cascade for the **prepare-failure** path: a prepare
/// Parse-error inside an open transaction aborts it, and #170's short-circuit
/// surfaces the real error + ROLLBACKs. But a **runtime** error at execute
/// time (division by zero, a constraint violation surfacing at fetch, a 42P01
/// on a relation dropped between prepare and fetch) also aborts the
/// transaction — and #170's `txn_aborted: false` on the `query_typed` error
/// arm means the caller keeps `in_transaction=true` and re-pins the aborted
/// connection. The next batch's first statement 25P02s.
///
/// This test uses `SELECT 1/0` — syntactically valid, so the pre-resolve
/// `prepare` *succeeds* (warm cache, `prepare_err=None`), and the 22012
/// division-by-zero fires at `query_typed` execute. On current code (pre-#181)
/// the follow-up `SELECT 1` 25P02-cascades; with #181 it succeeds.
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn query_typed_runtime_error_in_open_transaction_does_not_cascade() {
    use crate::handlers::query::{execute_query_batch, release_session};
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");
    let session_id = "test-181-runtime-cascade";

    // Batch 1: BEGIN (opens a transaction, pins the connection, sets
    // in_transaction=true for the next batch).
    let begin_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": ["BEGIN"],
        "session_id": session_id,
        "limit": 100,
        "page": 1,
    });
    let begin_resp = execute_query_batch(json!(1), &begin_params).await;
    assert!(
        begin_resp.get("error").is_none(),
        "BEGIN batch should succeed; got: {begin_resp}"
    );

    // Batch 2: SELECT 1/0. The prepare succeeds (syntactically valid →
    // prepare_err=None → the `else` branch of the `if prepare_err.is_some()`
    // block in `exec_query_on_client`); the 22012
    // division-by-zero is a runtime error at execute. Inside the open
    // transaction, this aborts it server-side. On current code the error
    // surfaces (the real 22012 — no 25P02 masks it on the *failing*
    // statement), but `txn_aborted: false` means the connection is re-pinned
    // in the aborted state. With #181, `txn_aborted: true` forces
    // in_transaction=false and the connection is released + ROLLBACK'd.
    let select_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": ["SELECT 1/0 AS zero"],
        "session_id": session_id,
        "limit": 50,
        "page": 1,
    });
    let select_resp = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(2), &select_params),
    )
    .await
    .expect("the division-by-zero batch hung past 30s — unexpected");
    assert!(
        select_resp.get("error").is_none(),
        "batch should not top-level error; got: {select_resp}"
    );
    let results = select_resp
        .get("result")
        .and_then(|r| r.get("results"))
        .and_then(|r| r.as_array())
        .expect("session-aware batch returns result.results");
    assert_eq!(
        results.len(),
        1,
        "one query in the batch; got: {select_resp}"
    );
    let first = &results[0];
    let err_msg = first
        .get("error")
        .and_then(|e| e.as_str())
        .expect("the SELECT 1/0 should error (division by zero); got: {first}");

    // The failing statement surfaces the real 22012 (no 25P02 masks it on
    // this statement — the cascade is only on the *next* statement).
    assert!(
        err_msg.contains("division by zero"),
        "the error should be the real 22012 (\"division by zero\"); got: {err_msg:?}"
    );

    // Batch 3: a plain `SELECT 1` on the same session. On current code the
    // aborted connection was re-pinned (in_transaction stayed true), so this
    // 25P02-cascades — the user sees "current transaction is aborted" instead
    // of `1`. With #181, the transaction was closed and the connection
    // released, so this runs cleanly.
    let followup_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": ["SELECT 1 AS one"],
        "session_id": session_id,
        "limit": 50,
        "page": 1,
    });
    let followup_resp = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(3), &followup_params),
    )
    .await
    .expect("the follow-up SELECT hung past 30s — the connection was likely re-pinned in aborted state (#181 cascade)");
    assert!(
        followup_resp.get("error").is_none(),
        "follow-up batch should not top-level error; got: {followup_resp}"
    );
    let followup_results = followup_resp
        .get("result")
        .and_then(|r| r.get("results"))
        .and_then(|r| r.as_array())
        .expect("follow-up batch returns result.results");
    let followup_first = &followup_results[0];
    let followup_err = followup_first.get("error").and_then(|e| e.as_str());
    assert!(
        followup_err.is_none(),
        "the follow-up SELECT must succeed (the connection was NOT re-pinned in \
         aborted state — the #181 cascade); got: {followup_err:?}"
    );
    let followup_rows = followup_first
        .get("result")
        .and_then(|r| r.get("rows"))
        .and_then(|r| r.as_array())
        .expect("follow-up result.rows is an array");
    assert_eq!(
        followup_rows[0].as_array().unwrap()[0],
        json!(1),
        "follow-up SELECT 1 returns 1; got: {followup_first}"
    );

    // Release the session (frees the pinned connection). #181 already
    // ROLLBACK'd the aborted transaction.
    let _ = release_session(json!(4), &json!({"session_id": session_id})).await;

    // Teardown: evict the pool entry so no state leaks to other tests.
    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}

/// Regression test for #181 on the `execute_typed` (non-result-set) path —
/// the second of the two error arms #170 left as `txn_aborted: false`. A
/// failed DDL/write inside an open transaction aborts it server-side; #181
/// ROLLBACKs and surfaces the real error so the follow-up statement doesn't
/// 25P02-cascade.
///
/// Uses `INSERT INTO ...nonexistent_table` — a non-result-set statement that
/// 42P01s at execute (the `execute_typed` arm — the `if !returns_result_set`
/// early-return in `exec_query_on_client`), distinct
/// from the headline test's `SELECT 1/0` which is a result-set statement
/// (the `query_typed` arm). Together the two tests cover both #181 arms.
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn execute_typed_failure_in_open_transaction_does_not_cascade() {
    use crate::handlers::query::{execute_query_batch, release_session};
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");
    let session_id = "test-181-execute-typed-cascade";

    // Batch 1: BEGIN.
    let begin_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": ["BEGIN"],
        "session_id": session_id,
        "limit": 100,
        "page": 1,
    });
    let begin_resp = execute_query_batch(json!(1), &begin_params).await;
    assert!(
        begin_resp.get("error").is_none(),
        "BEGIN batch should succeed; got: {begin_resp}"
    );

    // Batch 2: INSERT into a nonexistent table — a non-result-set statement
    // that 42P01s at execute (the `execute_typed` arm, not `query_typed`).
    // Inside the open transaction this aborts it; #181 ROLLBACKs + surfaces
    // the real 42P01.
    let insert_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": ["INSERT INTO plugin_test_181_nonexistent_table VALUES (1)"],
        "session_id": session_id,
        "limit": 50,
        "page": 1,
    });
    let insert_resp = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(2), &insert_params),
    )
    .await
    .expect("the INSERT batch hung past 30s — unexpected");
    assert!(
        insert_resp.get("error").is_none(),
        "batch should not top-level error; got: {insert_resp}"
    );
    let results = insert_resp
        .get("result")
        .and_then(|r| r.get("results"))
        .and_then(|r| r.as_array())
        .expect("session-aware batch returns result.results");
    assert_eq!(
        results.len(),
        1,
        "one query in the batch; got: {insert_resp}"
    );
    let first = &results[0];
    let err_msg = first
        .get("error")
        .and_then(|e| e.as_str())
        .expect("the INSERT should error (nonexistent table); got: {first}");
    assert!(
        err_msg.contains("does not exist"),
        "the error should be the real 42P01 (\"does not exist\"); got: {err_msg:?}"
    );
    assert!(
        !err_msg.contains("current transaction is aborted"),
        "the error must not be the 25P02 cascade; got: {err_msg:?}"
    );
    assert!(
        err_msg.contains("rolled back"),
        "the error should tell the user the transaction was rolled back; got: {err_msg:?}"
    );

    // Batch 3: SELECT 1 — must succeed (no 25P02 cascade).
    let followup_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": ["SELECT 1 AS one"],
        "session_id": session_id,
        "limit": 50,
        "page": 1,
    });
    let followup_resp = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(3), &followup_params),
    )
    .await
    .expect("the follow-up SELECT hung past 30s — the #181 cascade on the execute_typed path");
    assert!(
        followup_resp.get("error").is_none(),
        "follow-up batch should not top-level error; got: {followup_resp}"
    );
    let followup_results = followup_resp
        .get("result")
        .and_then(|r| r.get("results"))
        .and_then(|r| r.as_array())
        .expect("follow-up batch returns result.results");
    let followup_first = &followup_results[0];
    assert!(
        followup_first
            .get("error")
            .and_then(|e| e.as_str())
            .is_none(),
        "the follow-up SELECT must succeed (the connection was NOT re-pinned in \
         aborted state — the #181 cascade); got: {followup_first}"
    );

    let _ = release_session(json!(4), &json!({"session_id": session_id})).await;
    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}

/// Regression test for #181 on the **direct-connection** batch path inside an
/// open transaction (mirroring #170's direct-connection variant). The
/// headline test uses `pooler_mode: "transaction"`; this one has no
/// `pooler_mode` (direct connection) but still opens a transaction via a
/// bare-batch `BEGIN`, exercising the `else` arm of `run_batch_in_session`
/// with `in_transaction=true` and `pre_resolve_types=true`. A runtime error
/// (`SELECT 1/0`) must short-circuit the cascade here too.
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn query_typed_runtime_error_on_direct_connection_in_txn_does_not_cascade() {
    use crate::handlers::query::execute_query_batch;
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");

    // Bare batch (no `session_id`, no `pooler_mode` → direct connection) with
    // BEGIN then SELECT 1/0. The BEGIN opens a transaction on the shared
    // direct client, so the SELECT routes to the `else` arm with
    // `in_transaction=true` and `pre_resolve_types=true`. The 22012
    // division-by-zero fires at execute; #181 ROLLBACKs + forces
    // `in_transaction=false`, so the connection is released (not re-pinned
    // aborted) and a follow-up `SELECT 1` succeeds.
    let batch_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        // No `pooler_mode` → direct connection.
        "queries": ["BEGIN", "SELECT 1/0 AS zero"],
        "limit": 50,
        "page": 1,
    });
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(1), &batch_params),
    )
    .await
    .expect("the bare direct-connection batch hung past 30s — unexpected");
    assert!(
        resp.get("error").is_none(),
        "batch should not top-level error; got: {resp}"
    );
    let results = resp
        .get("result")
        .and_then(|r| r.as_array())
        .expect("bare batch returns a result array");
    assert_eq!(results.len(), 2, "two queries in the batch; got: {resp}");

    // Statement 1: BEGIN succeeds.
    let begin_outcome = &results[0];
    assert!(
        begin_outcome
            .get("error")
            .and_then(|e| e.as_str())
            .is_none(),
        "BEGIN should succeed; got: {begin_outcome}"
    );

    // Statement 2: SELECT 1/0 — the real 22012 surfaces, the transaction is
    // rolled back.
    let select_outcome = &results[1];
    let err_msg = select_outcome
        .get("error")
        .and_then(|e| e.as_str())
        .expect("the SELECT 1/0 should error (division by zero); got: {select_outcome}");
    assert!(
        err_msg.contains("division by zero"),
        "the error should be the real 22012 (\"division by zero\"); got: {err_msg:?}"
    );
    assert!(
        err_msg.contains("rolled back"),
        "the error should tell the user the transaction was rolled back; got: {err_msg:?}"
    );

    // Follow-up batch: SELECT 1 on the same (now-released) connection. Must
    // succeed — the aborted transaction was ROLLBACK'd, not re-pinned.
    let followup_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "queries": ["SELECT 1 AS one"],
        "limit": 50,
        "page": 1,
    });
    let followup_resp = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(2), &followup_params),
    )
    .await
    .expect("the follow-up SELECT hung past 30s — the #181 cascade on the direct path");
    let followup_results = followup_resp
        .get("result")
        .and_then(|r| r.as_array())
        .expect("bare batch returns a result array");
    let followup_first = &followup_results[0];
    assert!(
        followup_first
            .get("error")
            .and_then(|e| e.as_str())
            .is_none(),
        "the follow-up SELECT must succeed (no 25P02 cascade); got: {followup_first}"
    );

    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}

/// Regression guard for the `Closes`/`Chains` gate on #181's two error arms
/// (`execute_typed` and `query_typed` in `exec_query_on_client`). The gate's
/// *negative* behavior — a failed `COMMIT`/`ROLLBACK`/`END`/`COMMIT AND CHAIN`
/// issued via the **batch** path while `in_transaction=true` must NOT receive
/// the spurious ROLLBACK + "rolled back" notice — is uncovered by any other
/// test. The existing `commit_that_fails_on_a_deferred_constraint_releases_
/// the_session` test in `tests/live_db.rs` uses the single-statement
/// `execute_query` RPC, which passes `in_transaction=false` to
/// `exec_query_on_client` — so the `Closes`/`Chains` gate is never evaluated
/// there, and its assertion (`commit["error"].is_string()` only) would still
/// pass if #181 wrongly appended "rolled back" to a failed COMMIT's error.
///
/// This test exercises the gate via the batch path: `["BEGIN", "INSERT
/// (violating)", "COMMIT"]` as a session-pinned batch on a transaction
/// pooler, so the COMMIT fails with the deferred-FK violation while
/// `in_transaction=true` (the `BEGIN` set it). The gate must keep
/// `txn_aborted=false` for the failed `COMMIT` (a `Closes` verb — already
/// excluded), so:
///   - the COMMIT's error carries the real SQLSTATE (foreign key violation),
///     NOT "current transaction is aborted" (no 25P02 cascade — the `Closes`
///     path in `in_transaction_after_outcome` already forces
///     `in_transaction=false`), AND
///   - the COMMIT's error does NOT contain "rolled back" (the gate excluded
///     `Closes`, so no spurious ROLLBACK + notice fires for a transaction
///     the server already rolled back), AND
///   - the follow-up `SELECT 1` succeeds (the connection was released, not
///     re-pinned aborted).
///
/// If someone deleted the `&& !matches!(transaction_effect(query), Closes |
/// Chains)` conjunct from either arm, the COMMIT's error here would gain a
/// spurious "rolled back" suffix and the `!contains("rolled back")` assertion
/// would fail — catching the regression.
#[tokio::test]
#[ignore = "needs a live PostgreSQL (PG_PLUGIN_TEST_HOST=127.0.0.1 ... podman 54320)"]
async fn failed_commit_in_batch_does_not_get_spurious_rolled_back_notice() {
    use crate::client;
    use crate::handlers::query::{execute_query_batch, release_session};
    use serde_json::json;

    let _guard = POOLS_TEST_LOCK.lock().await;

    let params = live_params(&correct_password())
        .expect("PG_PLUGIN_TEST_HOST not set — set PG_PLUGIN_TEST_HOST=127.0.0.1 (and optionally PG_PLUGIN_TEST_PORT/USER/DB) to run live tests");

    // Setup: a table with a DEFERRABLE INITIALLY DEFERRED self-FK so the
    // INSERT succeeds but COMMIT fails (the FK check fires at commit).
    let table = "plugin_test_181_gate_deferred_fk";
    for stmt in [
        format!("DROP TABLE IF EXISTS {table}"),
        format!(
            "CREATE TABLE {table} (id INT PRIMARY KEY, other_id INT \
             REFERENCES {table}(id) DEFERRABLE INITIALLY DEFERRED)"
        ),
    ] {
        client::execute_typed(&params, &stmt, &[])
            .await
            .unwrap_or_else(|e| panic!("setup statement failed ({stmt}): {e}"));
    }

    let session_id = "test-181-closes-gate";

    // The batch: BEGIN opens a transaction (sets in_transaction=true for the
    // subsequent statements in this session-pinned batch), the INSERT
    // succeeds (FK is deferred), then COMMIT fails with the FK violation.
    // The COMMIT is a `Closes`-effect statement, so the Closes/Chains gate
    // must keep `txn_aborted=false` for it — no spurious ROLLBACK, no
    // "rolled back" suffix. The `Closes` path in `in_transaction_after_
    // outcome` already forces `in_transaction=false` (the server ended the
    // transaction when COMMIT failed), so there's no 25P02 cascade either.
    let batch_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": [
            "BEGIN",
            format!("INSERT INTO {table} VALUES (1, 999)"),
            "COMMIT",
        ],
        "session_id": session_id,
        "limit": 100,
        "page": 1,
    });
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(1), &batch_params),
    )
    .await
    .expect("the BEGIN/INSERT/COMMIT batch hung past 30s — unexpected");
    assert!(
        resp.get("error").is_none(),
        "batch should not top-level error; got: {resp}"
    );
    let results = resp
        .get("result")
        .and_then(|r| r.get("results"))
        .and_then(|r| r.as_array())
        .expect("session-aware batch returns result.results");
    assert_eq!(results.len(), 3, "three queries in the batch; got: {resp}");

    // Statement 1 (BEGIN) and 2 (INSERT) succeed.
    assert!(
        results[0].get("error").and_then(|e| e.as_str()).is_none(),
        "BEGIN should succeed; got: {}",
        results[0]
    );
    assert!(
        results[1].get("error").and_then(|e| e.as_str()).is_none(),
        "the deferred-FK INSERT should succeed (FK check fires at COMMIT); got: {}",
        results[1]
    );

    // Statement 3 (COMMIT) fails with the real FK violation — the gate's
    // negative behavior is what we're guarding.
    let commit_outcome = &results[2];
    let err_msg = commit_outcome
        .get("error")
        .and_then(|e| e.as_str())
        .expect("the COMMIT should error (deferred FK violation); got: {commit_outcome}");

    // The COMMIT's error surfaces the real SQLSTATE (foreign key violation),
    // NOT a 25P02 cascade ("current transaction is aborted"). The `Closes`
    // path in `in_transaction_after_outcome` already forces
    // `in_transaction=false`, so the failed COMMIT's reply reports
    // `in_transaction: false` and the connection is released — no re-pin of
    // an aborted connection.
    assert!(
        err_msg.contains("foreign key") || err_msg.contains("violates"),
        "the error should be the real FK violation (\"foreign key\" / \"violates\"), \
         not a 25P02 cascade; got: {err_msg:?}"
    );
    assert!(
        !err_msg.contains("current transaction is aborted"),
        "the error must not be the 25P02 cascade; got: {err_msg:?}"
    );

    // The load-bearing negative assertion: the failed COMMIT's error must
    // NOT contain "rolled back". The Closes/Chains gate excludes `COMMIT`
    // (a `Closes` verb), so #181's spurious ROLLBACK + "rolled back" notice
    // must NOT fire — the server already rolled back the transaction when
    // COMMIT failed; our ROLLBACK would be a no-op and our notice would be
    // factually wrong (the failing statement WAS the commit, not "a statement
    // in it"). If the gate's `Closes` conjunct is removed, this assertion
    // fails.
    assert!(
        !err_msg.contains("rolled back"),
        "the failed COMMIT must NOT get the 'rolled back' notice (the Closes/Chains \
         gate excludes it — the server already rolled back); got: {err_msg:?}"
    );

    // Follow-up batch on the same session: SELECT 1 must succeed (the
    // connection was released, not re-pinned aborted). The `Closes` path set
    // `in_transaction=false`, so the session slot is empty and a fresh
    // connection runs the follow-up.
    let followup_params = json!({
        "driver": "postgresql",
        "host": params.host,
        "port": params.port,
        "database": params.database,
        "username": params.username,
        "password": params.password,
        "pooler_mode": "transaction",
        "queries": ["SELECT 1 AS one"],
        "session_id": session_id,
        "limit": 50,
        "page": 1,
    });
    let followup_resp = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        execute_query_batch(json!(2), &followup_params),
    )
    .await
    .expect("the follow-up SELECT hung past 30s — the connection was re-pinned in aborted state");
    assert!(
        followup_resp.get("error").is_none(),
        "follow-up batch should not top-level error; got: {followup_resp}"
    );
    let followup_results = followup_resp
        .get("result")
        .and_then(|r| r.get("results"))
        .and_then(|r| r.as_array())
        .expect("follow-up batch returns result.results");
    let followup_first = &followup_results[0];
    assert!(
        followup_first
            .get("error")
            .and_then(|e| e.as_str())
            .is_none(),
        "the follow-up SELECT must succeed (no 25P02 cascade — the Closes path \
         released the connection); got: {followup_first}"
    );

    // Release the session and teardown.
    let _ = release_session(json!(3), &json!({"session_id": session_id})).await;
    let _ = client::execute_typed(&params, &format!("DROP TABLE IF EXISTS {table}"), &[]).await;
    let key = connection_key(&params);
    POOLS.lock().unwrap().remove(&key);
}
