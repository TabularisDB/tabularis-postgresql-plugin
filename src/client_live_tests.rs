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
