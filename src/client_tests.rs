//! Unit tests for `client.rs`. Sibling test file per repo convention
//! (`.rules/rust.md` #4/#5) — loaded via `#[cfg(test)] mod client_tests;`.

use tokio::sync::Mutex;

use super::{cleanup_idle_pools, connection_key, get_or_create_pool, load_roots_from_pem, POOLS};
use crate::models::ConnectionParams;

// `POOLS` is a single process-wide static, and Rust's test harness runs
// `#[tokio::test]` fns concurrently on separate threads — without this,
// `cleanup_idle_pools`'s sweep (which iterates every entry, not just its
// own key) can race with and evict another concurrently-running test's
// freshly-inserted pool. Serializes only the tests below that touch the
// shared map; pure `connection_key` tests above are unaffected. An async
// mutex (not std::sync::Mutex) since the guard must span `.await` points.
static POOLS_TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn params(host: &str, port: u16, db: &str, user: &str) -> ConnectionParams {
    ConnectionParams {
        driver: Some("postgres-plugin".to_string()),
        host: Some(host.to_string()),
        port: Some(port),
        database: Some(db.to_string()),
        username: Some(user.to_string()),
        password: None,
        ssl_mode: None,
        ssl_ca: None,
        ssl_cert: None,
        ssl_key: None,
        connection_string: None,
        startup_script: None,
    }
}

#[test]
fn connection_key_differs_by_database() {
    let a = connection_key(&params("localhost", 5432, "db1", "postgres"));
    let b = connection_key(&params("localhost", 5432, "db2", "postgres"));
    assert_ne!(a, b, "different databases must not share a cache key");
}

#[test]
fn connection_key_differs_by_host() {
    let a = connection_key(&params("host1", 5432, "db", "postgres"));
    let b = connection_key(&params("host2", 5432, "db", "postgres"));
    assert_ne!(a, b);
}

#[test]
fn connection_key_differs_by_port() {
    let a = connection_key(&params("localhost", 5432, "db", "postgres"));
    let b = connection_key(&params("localhost", 5433, "db", "postgres"));
    assert_ne!(a, b);
}

#[test]
fn connection_key_differs_by_user() {
    let a = connection_key(&params("localhost", 5432, "db", "alice"));
    let b = connection_key(&params("localhost", 5432, "db", "bob"));
    assert_ne!(a, b);
}

#[test]
fn connection_key_is_stable_for_identical_params() {
    let a = connection_key(&params("localhost", 5432, "db", "postgres"));
    let b = connection_key(&params("localhost", 5432, "db", "postgres"));
    assert_eq!(a, b);
}

#[tokio::test]
async fn get_or_create_pool_reuses_cached_entry_for_identical_params() {
    // deadpool's Pool::new is lazy (no connection attempt at creation
    // time) as long as no startup script is set — this test's `params()`
    // helper leaves startup_script as None, so this exercises only the
    // cache bookkeeping, not real connectivity. Use a key unlikely to
    // collide with other tests running in the same process.
    let _guard = POOLS_TEST_LOCK.lock().await;
    let p = params("cache-test-host-unique", 5432, "db", "user");
    let key = connection_key(&p);

    let before = POOLS.lock().unwrap().len();
    get_or_create_pool(&p)
        .await
        .expect("first call creates and caches a pool");
    let after_first = POOLS.lock().unwrap().len();
    assert_eq!(
        after_first,
        before + 1,
        "first call should insert one entry"
    );
    assert!(POOLS.lock().unwrap().contains_key(&key));

    get_or_create_pool(&p)
        .await
        .expect("second call should hit the cache");
    let after_second = POOLS.lock().unwrap().len();
    assert_eq!(
        after_second, after_first,
        "second call with identical params must not create a new entry"
    );
}

#[tokio::test]
async fn cleanup_idle_pools_evicts_pools_with_no_checked_out_connections() {
    // A freshly-built, never-connected pool has status().size ==
    // status().available == 0 (deadpool's Pool::new is lazy) — no
    // checked-out connections, so it must be evicted as idle/unused.
    let _guard = POOLS_TEST_LOCK.lock().await;
    let p = params("cleanup-test-host-unique", 5432, "db", "user");
    let key = connection_key(&p);

    get_or_create_pool(&p)
        .await
        .expect("pool should be created and cached");
    assert!(POOLS.lock().unwrap().contains_key(&key));

    cleanup_idle_pools();

    assert!(
        !POOLS.lock().unwrap().contains_key(&key),
        "an idle pool with no checked-out connections must be evicted"
    );
}

// Self-signed, 10-year-validity fixture cert (CN=test) — not a real trust
// anchor, just PEM content shaped like what a user's ssl_ca file holds.
const FIXTURE_CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIICmjCCAYICCQDx4lJdeLVc1TANBgkqhkiG9w0BAQsFADAPMQ0wCwYDVQQDDAR0
ZXN0MB4XDTI2MDgxNzE1MTMwN1oXDTM2MDgxNDE1MTMwN1owDzENMAsGA1UEAwwE
dGVzdDCCASIwDQYJKoZIhvcNAQEBBQADggEPADCCAQoCggEBANMxVfIixruhs3HK
5n0jDQtZIAUS8utHmhZNCKm7+k78MT14F28d+vRQCmncgmqqE940FG16EQNpw51U
359dB8Hv7K5aTYPoO2JAFaxkeJYi7u0VkV2Fi/c8OKIBqr9OAYrnvlSfzSLVRKEx
EamaHo2RX7nbmPYQf3P8djJco/pkehGuVMtR01GT94HVBorJD/nYlI/Ino2uvaJ7
McUFJI2Zc2vZ5Zl9KEjWqYZ29m1aJ8QMlEb0pQzV5Gy7zHa54t5RerQCtrrP71/W
GAhcmzBIozsE1p4R7pVYSbHxaM8wQWSOUtbC3pnFlUdg+zuInJDHaxhXRpTFsJRF
aTdVW7cCAwEAATANBgkqhkiG9w0BAQsFAAOCAQEAhOZaDEUUI8zy1RJpeacpaCkK
S+Y1SlbPi3XNFuLrmTjDRfjO9a8tby1kcAl5iKnkeVj08rRtB/gXggrjzaVnKHs+
17TcN0Px2kd/VpPFTPmxZecWqjU2PlqM9groKG0ojzVD24mkh908mJ4YvhW24T1i
5c4ITfLX0pUzj9vAyHLc9UIPfkCx3bKV0orRMn3GQNq5hYVtcaW4YyHJZCJAwB/2
U9921xiaw8MsvsxFeonvfUPdPK3cwTShdimQXB8TODJG81+AjF63/LikEirYfpNM
LmZVutFvYpIWVkZezGSamTOSepBusgSeR+M0ve18yzjHYGNes+1S8nrZQPBO9Q==
-----END CERTIFICATE-----
";

fn write_temp_file(contents: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "load_roots_from_pem_test_{}_{n}.pem",
        std::process::id()
    ));
    std::fs::write(&path, contents).expect("write temp fixture file");
    path
}

#[test]
fn load_roots_from_pem_accepts_a_valid_certificate() {
    let path = write_temp_file(FIXTURE_CERT_PEM);
    let result = load_roots_from_pem(path.to_str().unwrap());
    std::fs::remove_file(&path).ok();

    let roots = result.expect("valid PEM certificate should load successfully");
    assert!(
        !roots.is_empty(),
        "root store should contain the loaded cert"
    );
}

#[test]
fn load_roots_from_pem_rejects_a_file_with_no_certificate_blocks() {
    let path = write_temp_file("not a real certificate\n");
    let result = load_roots_from_pem(path.to_str().unwrap());
    std::fs::remove_file(&path).ok();

    let err = result.expect_err("non-PEM content should be rejected");
    assert!(
        err.contains("contained no PEM CERTIFICATE blocks"),
        "unexpected error message: {err}"
    );
}

#[test]
fn load_roots_from_pem_reports_a_clear_error_for_a_missing_file() {
    let result = load_roots_from_pem("/nonexistent/path/does-not-exist.pem");
    let err = result.expect_err("a missing file should be rejected");
    assert!(
        err.contains("Failed to read ssl_ca file"),
        "unexpected error message: {err}"
    );
}
