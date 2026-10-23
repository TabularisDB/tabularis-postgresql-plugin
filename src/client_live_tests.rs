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
