//! Unit tests for `client.rs`. Sibling test file per repo convention
//! (`.rules/rust.md` #4/#5) — loaded via `#[cfg(test)] mod client_tests;`.

use tokio::sync::Mutex;

use super::{
    build_tls_connector, cleanup_idle_pools, connection_key, get_or_create_pool,
    load_client_cert_from_pem, load_roots_from_pem, POOLS,
};
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

#[test]
fn connection_key_differs_by_ssl_mode() {
    let mut a = params("localhost", 5432, "db", "postgres");
    a.ssl_mode = Some("require".to_string());
    let mut b = params("localhost", 5432, "db", "postgres");
    b.ssl_mode = Some("verify-full".to_string());
    assert_ne!(
        connection_key(&a),
        connection_key(&b),
        "different ssl_mode values must not share a cache key"
    );
}

#[test]
fn connection_key_differs_by_ssl_ca() {
    let mut a = params("localhost", 5432, "db", "postgres");
    a.ssl_mode = Some("verify-ca".to_string());
    let mut b = params("localhost", 5432, "db", "postgres");
    b.ssl_mode = Some("verify-ca".to_string());
    b.ssl_ca = Some("/tmp/ca.pem".to_string());
    assert_ne!(
        connection_key(&a),
        connection_key(&b),
        "different ssl_ca values must not share a cache key"
    );
}

#[test]
fn connection_key_differs_by_ssl_cert() {
    let mut a = params("localhost", 5432, "db", "postgres");
    a.ssl_mode = Some("require".to_string());
    let mut b = params("localhost", 5432, "db", "postgres");
    b.ssl_mode = Some("require".to_string());
    b.ssl_cert = Some("/tmp/client-cert.pem".to_string());
    b.ssl_key = Some("/tmp/client-key.pem".to_string());
    assert_ne!(
        connection_key(&a),
        connection_key(&b),
        "different ssl_cert/ssl_key values must not share a cache key"
    );
}

#[test]
fn connection_key_differs_by_ssl_key_alone() {
    let mut a = params("localhost", 5432, "db", "postgres");
    a.ssl_mode = Some("require".to_string());
    a.ssl_cert = Some("/tmp/client-cert.pem".to_string());
    a.ssl_key = Some("/tmp/client-key-a.pem".to_string());
    let mut b = params("localhost", 5432, "db", "postgres");
    b.ssl_mode = Some("require".to_string());
    b.ssl_cert = Some("/tmp/client-cert.pem".to_string());
    b.ssl_key = Some("/tmp/client-key-b.pem".to_string());
    assert_ne!(
        connection_key(&a),
        connection_key(&b),
        "different ssl_key values must not share a cache key even with an identical ssl_cert"
    );
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

// Self-signed RSA client cert + matching PKCS#8 private key (CN=test-client)
// — a real X.509v3 cert/key pair (openssl req -x509 -newkey rsa:2048 -nodes
// -addext basicConstraints=CA:FALSE -addext keyUsage=digitalSignature), not
// a real trust anchor, just shaped like what a user's ssl_cert/ssl_key
// files hold for mTLS. rustls's client-auth path requires v3 (basic
// constraints present) — a v1 cert is rejected with UnsupportedCertVersion.
const FIXTURE_CLIENT_CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIICyTCCAbGgAwIBAgIJAL2rJBvvf1YfMA0GCSqGSIb3DQEBCwUAMBYxFDASBgNV
BAMMC3Rlc3QtY2xpZW50MB4XDTI2MDgxOTEyMDU1MVoXDTM2MDgxNjEyMDU1MVow
FjEUMBIGA1UEAwwLdGVzdC1jbGllbnQwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAw
ggEKAoIBAQDYG5QxpH4lT6J+dmSZKn905KrDi++om1OV8K3cPIG5sni3phLVWcX/
I2MOH8DkLtQgR3gnjSOHFk6RKE4ezMfCMMQ+6nXIRP/B3lt06Ub3uTvGmRApk3hh
5JE6ae8+xhowh4IXdC2wYEi81PIh/RGyyylsitmUyAt/4j3q9Kt/StPmLbrXMl02
mYSC3Z8QabSnAh+Yd9MFRfaJDXRYpoUtOror9S4u1JU6+FLyvjIeUWbCFZU6EvDP
MleiG3pbiZX/EPK3t3gwYg40AAS+LIijhJ+1T2LlOE+6wPJjEYiMPzNdnSnh6MzT
tXCX1a5AqRRajb5jdZv20Eqf1E/7HRFpAgMBAAGjGjAYMAkGA1UdEwQCMAAwCwYD
VR0PBAQDAgeAMA0GCSqGSIb3DQEBCwUAA4IBAQBPCAEFChrzv2oY1KSS5/Z2qc9D
0PDjquvTLyDOcxQywBpLxEhWPTQVMoryZlSyqoKn2n1aj1+CASsuBcemoL3714IJ
hcU0GLDcRmJnWvU8JZfJgI4tdFvpFiBfn9hixpgTSnS8J/9/3CrE/c/tqakrYncd
+PwDBIyo36f78sToa853LmWabC/KelfzhFpJFsTygu3KtAeyAvm/0S5tkqh9GOkg
U3d2kk7Mb7fmDTzT83A4vIGVbTG4wP4HDr4AkapAmFb9BvQBjtU1nywhqkM2PQ8z
1T+5CI0B8rJ2pTrZo25nc7EaPWzojE9hce6FGuIPOexbFzFBcxLt/yP3h8eJ
-----END CERTIFICATE-----
";

const FIXTURE_CLIENT_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQDYG5QxpH4lT6J+
dmSZKn905KrDi++om1OV8K3cPIG5sni3phLVWcX/I2MOH8DkLtQgR3gnjSOHFk6R
KE4ezMfCMMQ+6nXIRP/B3lt06Ub3uTvGmRApk3hh5JE6ae8+xhowh4IXdC2wYEi8
1PIh/RGyyylsitmUyAt/4j3q9Kt/StPmLbrXMl02mYSC3Z8QabSnAh+Yd9MFRfaJ
DXRYpoUtOror9S4u1JU6+FLyvjIeUWbCFZU6EvDPMleiG3pbiZX/EPK3t3gwYg40
AAS+LIijhJ+1T2LlOE+6wPJjEYiMPzNdnSnh6MzTtXCX1a5AqRRajb5jdZv20Eqf
1E/7HRFpAgMBAAECggEADVC1stFVzGq5sl0NGbram5MzSlUm8RaQ8d4geD9qJszu
TzJ2Wprrbir6AXbHZcfU3iBJMParR7mCIcN//LnVXQuwK8g6dZp6v7E5pVxyOPU6
z6PCsS0a770rjZPVX+LI3lCbHWLVJdbo5GmieaGkI4YNCVkMyvQAcWK5Oe7VWkRf
BXeZVXswocBzTOTjGRK1ZtoINWuLKZjb6J8QkB46SwcQrvW6MUP+YocxMDPt7tyk
YBGuRoWcFYxnNzxrlubQJshHIK0vYD/zidvxA/7z1lG3torpgpsVVyjZqEL4vKEw
5wy5VbUWLUPNMTGeRuerqz3XjYUD9s+PaWEll4j2GQKBgQDzdcBxhVBDPZPG2myg
1w4XnofqOCP523aUc7lvUBovMmeMTqso+2jk609Gx+UjUx6TvcmK1xmqOSQ4N3z0
IKoDGWAkxmtArSTU8NIGGf69QcLiRwGMvT9m/a01loHoq+wia6ua18OLHNJwCh8v
1Vy5VIaFPycmNvjkXQzEIaus6wKBgQDjPSmG37vtyckxVkQbpKOaZfYlsDlwGrhW
c3m6yHdjd6Hfwb+dCfJeHVlwHtu2PYgURXdh/UqDX+HzD5pqH9cjeD0sN8ayqrcW
Z9tEw19hzrPKWPZ4nJYjjmd/XA25Ac2UiyLbmMOBN3VsA1Phacew7tsMS/UaFliX
BHzNXAHV+wKBgAZe93lBBterndlfT+ZpmknN8TqU24QnVRQPbzPVgcnoZMNML7hz
08vhyIJOqtVg0HUHS2XhuR82PZdnBFMTI7/PAzATLS1VGpij8KsONRdYyDPJreWz
8hvM2aKEXMPs89H2xVfY+5oBWBRsf2JuD+4doyOLgofCeoLnWHUteGOfAoGAIaWk
yHvIb+U5DT0gyJcQQoRmdh4p4xeRw/tFQwr74paMOX2Oycn3QUhHPfrTvaBOzfGb
Q78lkV5ZLoxY6O3eBTqAlFON8Fam1YJ7TStArFLW/Fc/54wIDyu+13Th80r5Dc2s
U6fDCxcTI/M6MF5hWymC9ccpe7tjUrkvYZkGDJECgYBKg1inw1umpsMLr+CbOdEz
c2YEH/DT1A4OSvsksNTGR9pySU5xIKQx0hOVA8eLQ6m97yxFwhU8mCnyAKqahsMz
5dY86Z3aBuAfLt3FUk2jnAx0pEA/Lf7HeR/EPCdgUWG05ZymXUnnU5OgQ0qow6wm
yORfscWKlsDf+tv4Zb2jYQ==
-----END PRIVATE KEY-----
";

fn params_with_ssl(ssl_mode: &str) -> ConnectionParams {
    ConnectionParams {
        driver: Some("postgres-plugin".to_string()),
        host: Some("localhost".to_string()),
        port: Some(5432),
        database: Some("db".to_string()),
        username: Some("user".to_string()),
        password: None,
        ssl_mode: Some(ssl_mode.to_string()),
        ssl_ca: None,
        ssl_cert: None,
        ssl_key: None,
        connection_string: None,
        startup_script: None,
    }
}

#[test]
fn load_client_cert_from_pem_accepts_a_valid_cert_and_key() {
    let cert_path = write_temp_file(FIXTURE_CLIENT_CERT_PEM);
    let key_path = write_temp_file(FIXTURE_CLIENT_KEY_PEM);
    let result = load_client_cert_from_pem(cert_path.to_str().unwrap(), key_path.to_str().unwrap());
    std::fs::remove_file(&cert_path).ok();
    std::fs::remove_file(&key_path).ok();

    let (certs, _key) = result.expect("valid client cert/key PEM should load successfully");
    assert_eq!(
        certs.len(),
        1,
        "cert chain should contain the one leaf cert"
    );
}

#[test]
fn load_client_cert_from_pem_rejects_a_cert_file_with_no_certificate_blocks() {
    let cert_path = write_temp_file("not a real certificate\n");
    let key_path = write_temp_file(FIXTURE_CLIENT_KEY_PEM);
    let result = load_client_cert_from_pem(cert_path.to_str().unwrap(), key_path.to_str().unwrap());
    std::fs::remove_file(&cert_path).ok();
    std::fs::remove_file(&key_path).ok();

    let err = result.expect_err("non-PEM cert content should be rejected");
    assert!(
        err.contains("contained no PEM CERTIFICATE blocks"),
        "unexpected error message: {err}"
    );
}

#[test]
fn load_client_cert_from_pem_rejects_a_key_file_with_no_private_key() {
    let cert_path = write_temp_file(FIXTURE_CLIENT_CERT_PEM);
    let key_path = write_temp_file("not a real private key\n");
    let result = load_client_cert_from_pem(cert_path.to_str().unwrap(), key_path.to_str().unwrap());
    std::fs::remove_file(&cert_path).ok();
    std::fs::remove_file(&key_path).ok();

    let err = result.expect_err("non-PEM key content should be rejected");
    assert!(
        err.contains("Failed to parse ssl_key"),
        "unexpected error message: {err}"
    );
}

#[test]
fn load_client_cert_from_pem_reports_a_clear_error_for_a_missing_cert_file() {
    let key_path = write_temp_file(FIXTURE_CLIENT_KEY_PEM);
    let result = load_client_cert_from_pem(
        "/nonexistent/path/does-not-exist.pem",
        key_path.to_str().unwrap(),
    );
    std::fs::remove_file(&key_path).ok();

    let err = result.expect_err("a missing cert file should be rejected");
    assert!(
        err.contains("Failed to read ssl_cert file"),
        "unexpected error message: {err}"
    );
}

#[test]
fn load_client_cert_from_pem_reports_a_clear_error_for_a_missing_key_file() {
    let cert_path = write_temp_file(FIXTURE_CLIENT_CERT_PEM);
    let result = load_client_cert_from_pem(
        cert_path.to_str().unwrap(),
        "/nonexistent/path/does-not-exist.pem",
    );
    std::fs::remove_file(&cert_path).ok();

    let err = result.expect_err("a missing key file should be rejected");
    assert!(
        err.contains("Failed to read ssl_key file"),
        "unexpected error message: {err}"
    );
}

#[test]
fn build_tls_connector_succeeds_with_valid_client_cert_and_key() {
    let cert_path = write_temp_file(FIXTURE_CLIENT_CERT_PEM);
    let key_path = write_temp_file(FIXTURE_CLIENT_KEY_PEM);

    let mut params = params_with_ssl("require");
    params.ssl_cert = Some(cert_path.to_str().unwrap().to_string());
    params.ssl_key = Some(key_path.to_str().unwrap().to_string());
    let result = build_tls_connector(&params);
    std::fs::remove_file(&cert_path).ok();
    std::fs::remove_file(&key_path).ok();

    result.expect("connector should build successfully with a valid client cert/key pair");
}

#[test]
fn build_tls_connector_errors_when_ssl_cert_is_set_without_ssl_key() {
    let mut params = params_with_ssl("require");
    params.ssl_cert = Some("/path/to/cert.pem".to_string());
    params.ssl_key = None;

    let err = build_tls_connector(&params)
        .expect_err("ssl_cert without ssl_key must be rejected as a config error");
    assert!(
        err.contains("ssl_cert") && err.contains("ssl_key"),
        "unexpected error message: {err}"
    );
}

#[test]
fn build_tls_connector_errors_when_ssl_key_is_set_without_ssl_cert() {
    let mut params = params_with_ssl("require");
    params.ssl_cert = None;
    params.ssl_key = Some("/path/to/key.pem".to_string());

    let err = build_tls_connector(&params)
        .expect_err("ssl_key without ssl_cert must be rejected as a config error");
    assert!(
        err.contains("ssl_cert") && err.contains("ssl_key"),
        "unexpected error message: {err}"
    );
}
