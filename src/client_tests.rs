//! Unit tests for `client.rs`. Sibling test file per repo convention
//! (`.rules/rust.md` #4/#5) — loaded via `#[cfg(test)] mod client_tests;`.

use tokio::sync::Mutex;

use super::{
    build_pool_pub, build_tls_connector, cleanup_idle_pools, connection_key, get_or_create_pool,
    get_pool_client, load_client_cert_from_pem, load_roots_from_pem, pooler_error_lead,
    resolve_ssl_mode, split_postgres_socket_path, uses_tls, with_text_types, NoCertVerifier,
    VerifyCaCertVerifier, POOLS,
};
use crate::models::ConnectionParams;
use crate::settings;
use deadpool_postgres::SslMode;
use tokio_postgres::types::{ToSql, Type};

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
        unix_socket_path: None,
        pooler_mode: None,
        read_only: false,
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

// Coverage for #132: the pool cache key must distinguish connections that differ
// only by password, so a failed connection built with a wrong password can't be
// reused for a later request carrying the correct one (and a rotated password
// gets its own pool instead of sharing the stale one). The password is folded in
// as a SHA-256 hex digest — never as plaintext — mirroring how the builtin
// folds the startup script (`pool_manager.rs`'s `Sha256::digest`) and how TLS
// params were added in #36.

#[test]
fn connection_key_differs_by_password() {
    let mut a = params("localhost", 5432, "db", "postgres");
    a.password = Some("hunter2".to_string());
    let mut b = params("localhost", 5432, "db", "postgres");
    b.password = Some("correct-horse-battery-staple".to_string());
    assert_ne!(
        connection_key(&a),
        connection_key(&b),
        "different passwords must not share a cache key — otherwise a failed \
         connection built with a wrong password poisons the pool for the \
         correct one (#132)"
    );
}

#[test]
fn connection_key_is_stable_for_identical_passwords() {
    let mut a = params("localhost", 5432, "db", "postgres");
    a.password = Some("s3cret".to_string());
    let mut b = params("localhost", 5432, "db", "postgres");
    b.password = Some("s3cret".to_string());
    assert_eq!(
        connection_key(&a),
        connection_key(&b),
        "identical passwords must produce identical keys (no salt/randomness)"
    );
}

#[test]
fn connection_key_omits_password_segment_when_absent() {
    // A passwordless connection (None) must keep the legacy key shape with no
    // password segment — guards stability for passwordless/trust connections
    // and keeps the existing connection_key_* tests above unchanged.
    let none_pw = params("localhost", 5432, "db", "postgres");
    let key = connection_key(&none_pw);
    assert!(
        !key.contains(":pw:"),
        "absent password must not add a pw segment to the key: {key}"
    );
}

#[test]
fn connection_key_omits_password_segment_when_empty() {
    // An empty-string password is treated as absent (no segment) — matches
    // build_pool, which sets cfg.password from params.password verbatim and
    // would otherwise hash "" into the key for no benefit.
    let mut empty_pw = params("localhost", 5432, "db", "postgres");
    empty_pw.password = Some(String::new());
    let key = connection_key(&empty_pw);
    assert!(
        !key.contains(":pw:"),
        "empty password must not add a pw segment to the key: {key}"
    );
}

#[test]
fn connection_key_differs_by_surrounding_whitespace() {
    // The raw (untrimmed) password is hashed — matching build_pool's
    // params.password.clone() verbatim — so passwords differing only in
    // surrounding whitespace get distinct keys, not a silent collision that
    // would serve B's request on A's authenticated pool (#132 pre-handoff
    // review finding: trimming here while build_pool doesn't is a bug).
    let mut a = params("localhost", 5432, "db", "postgres");
    a.password = Some("secret".to_string());
    let mut b = params("localhost", 5432, "db", "postgres");
    b.password = Some(" secret ".to_string());
    assert_ne!(
        connection_key(&a),
        connection_key(&b),
        "passwords differing only in surrounding whitespace must not share a \
         cache key — build_pool connects with the raw (untrimmed) password, so \
         the key must hash the same raw bytes to avoid serving B on A's pool"
    );
}

#[test]
fn connection_key_whitespace_only_password_differs_from_absent() {
    // A whitespace-only password ("   ") is NOT the same as None — build_pool
    // sends "   " to PostgreSQL, so the key must include a :pw: segment to
    // avoid sharing a pool with a passwordless (None) connection. Trimming
    // would collapse these together (the bug the pre-handoff review found);
    // hashing the raw value keeps them distinct.
    let mut ws_only = params("localhost", 5432, "db", "postgres");
    ws_only.password = Some("   ".to_string());
    let absent = params("localhost", 5432, "db", "postgres");
    assert_ne!(
        connection_key(&ws_only),
        connection_key(&absent),
        "a whitespace-only password must not share a key with an absent \
         password — build_pool sends the whitespace bytes to PostgreSQL"
    );
}

// Coverage for #134: when connection_string is set, build_pool parses the
// password (and everything else) from the string, not from the discrete
// fields. The cache key must fold in the connection string (hashed) so two
// strings differing only by password — or by any other field — get distinct
// pools, not a silent collision that serves B's request on A's authenticated
// pool. Mirrors how the password is folded (#132) and how the builtin folds
// the startup script (pool_manager.rs's Sha256::digest).

#[test]
fn connection_key_differs_by_connection_string() {
    // Two connection strings that differ only by password must not share a
    // cache key — build_pool parses the password from the string, so a
    // collision would let the wrong-password pool serve the correct-password
    // request (#134, same bug class as #132).
    let mut a = params("localhost", 5432, "db", "postgres");
    a.connection_string =
        Some("postgresql://postgres:wrongpass@127.0.0.1:54320/testdb".to_string());
    let mut b = params("localhost", 5432, "db", "postgres");
    b.connection_string =
        Some("postgresql://postgres:correctpass@127.0.0.1:54320/testdb".to_string());
    assert_ne!(
        connection_key(&a),
        connection_key(&b),
        "connection strings differing only by password must not share a cache key"
    );
}

#[test]
fn connection_key_differs_by_connection_string_host() {
    // Two connection strings differing by host must not share a key.
    let mut a = params("localhost", 5432, "db", "postgres");
    a.connection_string = Some("postgresql://postgres:pw@host-a:5432/db".to_string());
    let mut b = params("localhost", 5432, "db", "postgres");
    b.connection_string = Some("postgresql://postgres:pw@host-b:5432/db".to_string());
    assert_ne!(
        connection_key(&a),
        connection_key(&b),
        "connection strings differing by host must not share a cache key"
    );
}

#[test]
fn connection_key_is_stable_for_identical_connection_strings() {
    let mut a = params("localhost", 5432, "db", "postgres");
    a.connection_string = Some("postgresql://postgres:pw@127.0.0.1:54320/db".to_string());
    let mut b = params("localhost", 5432, "db", "postgres");
    b.connection_string = Some("postgresql://postgres:pw@127.0.0.1:54320/db".to_string());
    assert_eq!(
        connection_key(&a),
        connection_key(&b),
        "identical connection strings must produce identical keys"
    );
}

#[test]
fn connection_key_omits_connection_string_segment_when_absent() {
    // A connection without a connection_string must keep the legacy key shape
    // with no cs: prefix — guards stability for discrete-field connections.
    let p = params("localhost", 5432, "db", "postgres");
    let key = connection_key(&p);
    assert!(
        !key.starts_with("cs:"),
        "absent connection_string must not produce a cs: key: {key}"
    );
}

#[test]
fn connection_key_omits_connection_string_segment_when_empty() {
    // An empty/whitespace connection_string is treated as absent (no cs: prefix)
    // — matches build_pool, which filters empty/whitespace strings.
    let mut p = params("localhost", 5432, "db", "postgres");
    p.connection_string = Some("   ".to_string());
    let key = connection_key(&p);
    assert!(
        !key.starts_with("cs:"),
        "empty/whitespace connection_string must not produce a cs: key: {key}"
    );
}

#[test]
fn connection_key_does_not_contain_plaintext_connection_string() {
    // The connection string contains the password in plaintext — it must
    // appear in the key only as a hex digest, never as the raw string.
    let secret = "postgresql://postgres:super-secret-do-not-leak@127.0.0.1:54320/db";
    let mut p = params("localhost", 5432, "db", "postgres");
    p.connection_string = Some(secret.to_string());
    let key = connection_key(&p);
    assert!(
        !key.contains(secret),
        "plaintext connection string must not appear in the cache key: {key}"
    );
    assert!(
        !key.contains("super-secret-do-not-leak"),
        "the password embedded in the connection string must not appear in the key: {key}"
    );
    assert!(
        key.starts_with("cs:"),
        "a present connection_string must produce a key starting with 'cs:': {key}"
    );
}

#[test]
fn connection_key_connection_string_takes_precedence_over_discrete_fields() {
    // When connection_string is set, build_pool ignores the discrete
    // host/port/db/user fields (parses them from the string). The key mirrors
    // that: same string + same TLS/startup_script → same key regardless of
    // host/port/db/user. But TLS params and startup_script are STILL in the
    // key (build_pool reads them unconditionally), so different TLS or
    // startup_script still produce distinct keys.
    let mut with_cs = params("localhost", 5432, "db", "postgres");
    with_cs.connection_string = Some("postgresql://postgres:pw@127.0.0.1:54320/db".to_string());

    let same_cs_diff_host_port_db_user = {
        let mut p = params("other-host", 9999, "other-db", "other-user");
        p.connection_string = Some("postgresql://postgres:pw@127.0.0.1:54320/db".to_string());
        p
    };
    assert_eq!(
        connection_key(&with_cs),
        connection_key(&same_cs_diff_host_port_db_user),
        "when connection_string is set, host/port/db/user are irrelevant — \
         same string + same TLS/startup_script must produce the same key"
    );
}

#[test]
fn connection_key_connection_string_differs_by_ssl_mode() {
    // build_pool reads ssl_mode from the discrete field even when
    // connection_string is set, so the key must include it — otherwise
    // a require-mode pool would be silently served to a disable-mode
    // request (or vice versa). This is the critical fix for the pre-handoff
    // review finding: TLS params must stay in the key.
    let mut a = params("localhost", 5432, "db", "postgres");
    a.connection_string = Some("postgresql://postgres:pw@127.0.0.1:54320/db".to_string());

    let mut b = params("localhost", 5432, "db", "postgres");
    b.connection_string = Some("postgresql://postgres:pw@127.0.0.1:54320/db".to_string());
    b.ssl_mode = Some("require".to_string());

    assert_ne!(
        connection_key(&a),
        connection_key(&b),
        "same connection_string but different ssl_mode must not share a key — \
         build_pool reads ssl_mode from the discrete field unconditionally"
    );
}

#[test]
fn connection_key_connection_string_differs_by_startup_script() {
    // build_pool reads startup_script from the discrete field even when
    // connection_string is set (applies it via post_create hook), so the
    // key must include it — otherwise a pool with one search_path would be
    // silently served to a request expecting a different one.
    let mut a = params("localhost", 5432, "db", "postgres");
    a.connection_string = Some("postgresql://postgres:pw@127.0.0.1:54320/db".to_string());

    let mut b = params("localhost", 5432, "db", "postgres");
    b.connection_string = Some("postgresql://postgres:pw@127.0.0.1:54320/db".to_string());
    b.startup_script = Some("SET search_path TO schema_b".to_string());

    assert_ne!(
        connection_key(&a),
        connection_key(&b),
        "same connection_string but different startup_script must not share \
         a key — build_pool reads startup_script from the discrete field"
    );
}

#[test]
fn connection_key_connection_string_differs_by_ssl_ca() {
    // build_pool reads ssl_ca from the discrete field (via build_tls_connector)
    // even when connection_string is set, so the key must include it.
    let mut a = params("localhost", 5432, "db", "postgres");
    a.connection_string = Some("postgresql://postgres:pw@127.0.0.1:54320/db".to_string());

    let mut b = params("localhost", 5432, "db", "postgres");
    b.connection_string = Some("postgresql://postgres:pw@127.0.0.1:54320/db".to_string());
    b.ssl_ca = Some("/path/to/ca-b.pem".to_string());

    assert_ne!(
        connection_key(&a),
        connection_key(&b),
        "same connection_string but different ssl_ca must not share a key"
    );
}

#[test]
fn connection_key_connection_string_differs_by_ssl_cert() {
    // build_pool reads ssl_cert from the discrete field (via build_tls_connector)
    // even when connection_string is set, so the key must include it.
    let mut a = params("localhost", 5432, "db", "postgres");
    a.connection_string = Some("postgresql://postgres:pw@127.0.0.1:54320/db".to_string());

    let mut b = params("localhost", 5432, "db", "postgres");
    b.connection_string = Some("postgresql://postgres:pw@127.0.0.1:54320/db".to_string());
    b.ssl_cert = Some("/path/to/client-cert.pem".to_string());
    b.ssl_key = Some("/path/to/client-key.pem".to_string());

    assert_ne!(
        connection_key(&a),
        connection_key(&b),
        "same connection_string but different ssl_cert/ssl_key must not share a key"
    );
}

#[test]
fn connection_key_connection_string_differs_by_ssl_key() {
    // build_pool reads ssl_key from the discrete field (via build_tls_connector)
    // even when connection_string is set, so the key must include it.
    let mut a = params("localhost", 5432, "db", "postgres");
    a.connection_string = Some("postgresql://postgres:pw@127.0.0.1:54320/db".to_string());
    a.ssl_cert = Some("/path/to/client-cert.pem".to_string());
    a.ssl_key = Some("/path/to/client-key-a.pem".to_string());

    let mut b = params("localhost", 5432, "db", "postgres");
    b.connection_string = Some("postgresql://postgres:pw@127.0.0.1:54320/db".to_string());
    b.ssl_cert = Some("/path/to/client-cert.pem".to_string());
    b.ssl_key = Some("/path/to/client-key-b.pem".to_string());

    assert_ne!(
        connection_key(&a),
        connection_key(&b),
        "same connection_string but different ssl_key must not share a key"
    );
}

#[test]
fn connection_key_connection_string_key_has_no_host_port_db_user() {
    // When connection_string is set, the key starts with "cs:{hash}" and
    // includes TLS params + startup_script, but NOT host/port/db/user (which
    // build_pool ignores — it parses them from the string).
    let mut p = params("distinct-host-unique", 5432, "distinct-db", "distinct-user");
    p.connection_string = Some("postgresql://postgres:pw@127.0.0.1:54320/db".to_string());
    let key = connection_key(&p);
    assert!(
        !key.contains("distinct-host-unique"),
        "discrete host must not appear in the key when connection_string is set: {key}"
    );
    assert!(
        !key.contains("distinct-db"),
        "discrete database must not appear in the key when connection_string is set: {key}"
    );
    assert!(
        !key.contains("distinct-user"),
        "discrete user must not appear in the key when connection_string is set: {key}"
    );
    assert!(
        key.starts_with("cs:"),
        "key must start with 'cs:{{hash}}' when connection_string is set: {key}"
    );
}

#[test]
fn connection_key_does_not_contain_plaintext_password() {
    // The password must appear in the key only as a hex digest, never as the
    // raw string — a plaintext password sitting in a HashMap key is a secret
    // leak via any dump of the cache (#132's suggested fix explicitly calls
    // for hashing).
    let secret = "super-secret-do-not-leak";
    let mut p = params("localhost", 5432, "db", "postgres");
    p.password = Some(secret.to_string());
    let key = connection_key(&p);
    assert!(
        !key.contains(secret),
        "plaintext password must not appear in the cache key: {key}"
    );
    assert!(
        key.contains(":pw:"),
        "a present password must add a hashed pw segment: {key}"
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

// Coverage for #132 Fix 1 (eviction-on-failure): the eviction branch of
// get_pool_client — the `Err` arm of `pool.get()` that removes the cache entry —
// is the primary fix. The live tests in client_live_tests.rs exercise it against
// a real database, but this test runs in CI without one: deadpool's `Pool::new`
// is lazy (no connection at creation time when no startup script is set), so
// get_pool_client builds+caches a pool, then `pool.get()` fails because the host
// is non-existent (connection refused), hitting the eviction arm. Asserting the
// cache no longer contains the key proves eviction ran. This test would FAIL if
// the eviction code (`pools.remove(&key)`) were removed — the lazy pool would
// stay in the cache despite the failed `get()`.

#[tokio::test]
async fn get_pool_client_evicts_on_connection_failure() {
    let _guard = POOLS_TEST_LOCK.lock().await;
    // A non-existent host: port 1 on the loopback is almost certainly not
    // running a PostgreSQL, so pool.get() will fail with a connection error.
    let p = params("127.0.0.1", 1, "db", "user");
    let key = connection_key(&p);

    // Ensure we start clean for this key.
    POOLS.lock().unwrap().remove(&key);

    let result = get_pool_client(&p).await;
    assert!(
        result.is_err(),
        "connecting to a non-existent host must fail; got {:?}",
        result
    );

    assert!(
        !POOLS.lock().unwrap().contains_key(&key),
        "a failed pool.get() must evict the cache entry so the next request \
         rebuilds a fresh pool (#132); key {key} is still present"
    );
}

#[tokio::test]
async fn get_pool_client_eviction_branch_is_not_vacuous() {
    // Proves the test above is non-vacuous: if eviction were removed, the
    // cache entry would persist. We verify that get_or_create_pool (which
    // does NOT evict) leaves the entry, while get_pool_client (which does)
    // removes it — so the eviction is load-bearing, not a no-op.
    let _guard = POOLS_TEST_LOCK.lock().await;
    let p = params("127.0.0.1", 1, "db", "user");
    let key = connection_key(&p);

    POOLS.lock().unwrap().remove(&key);

    // get_or_create_pool builds and caches a lazy pool (no connection attempt).
    get_or_create_pool(&p)
        .await
        .expect("lazy pool builds without a live DB when no startup script is set");
    assert!(
        POOLS.lock().unwrap().contains_key(&key),
        "get_or_create_pool should cache the lazy pool"
    );

    // Clean up — get_pool_client would also evict, but we want to isolate the
    // "get_or_create_pool caches" assertion from the eviction test above.
    POOLS.lock().unwrap().remove(&key);
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

// Self-signed CA + a server leaf cert it issued for CN/SAN
// `cert-hostname.example` (100-year validity, generated via `openssl req
// -x509`/`openssl x509 -req`) — used to prove `verify-ca` validates the
// chain but must NOT reject a hostname mismatch (that's what distinguishes
// it from `verify-full`; see issue #38).
const FIXTURE_CA_CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIICyjCCAbKgAwIBAgIJAMUP8ld3x16kMA0GCSqGSIb3DQEBCwUAMBkxFzAVBgNV
BAMMDlRlc3QgUGFyaXR5IENBMCAXDTI2MDgxOTEzMDMxMVoYDzIxMjYwNzI2MTMw
MzExWjAZMRcwFQYDVQQDDA5UZXN0IFBhcml0eSBDQTCCASIwDQYJKoZIhvcNAQEB
BQADggEPADCCAQoCggEBANO7mxf1C4m0QXiWFez1OIwX0SLM35IRrpIEki4yIMlA
3c1SGzuXchNROu0QEz1A0WJosZHZNR1LRwxYpk6x9G7h/ojlzlpEYkyEE1y/i0VV
q5m+VxzARr54YcEny18tAFmKV73HeIRDrrM2k+L5GAObtCGJpsmkJQV87OMXrRWy
WsASq4hZS1/rFd+mwyUeVp5U7o8LkFMp+Kxc4UOUtXu5EAda7x26H3ARXtpYgUdX
l07k8SZ7jNR1immup/FVUpuCualtguEH5dLfmi4d+nAakIktGz61TSUQtSrPTFJH
c7c8UYG0ogqk1RZiNxMnSFjhDCgKblrPXq4EUloAiy8CAwEAAaMTMBEwDwYDVR0T
AQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOCAQEAOB2iKUnYvx6BhczwsULe8ASU
wJeN2u6uj0EwxPpcAacDb7ccz0/muD5GyPFs2f7DkiYOg7aI6XVmWIIb0hN4gvwy
+480mQj4+y51ZqtPDykXkhyUerrU/ZRrP/AgpjtTw1tJ2I2xQWFLHtyb55zKguxW
r8sWnR/o6ZqaMQk6mcAQUkmOAUoTONPhrZD6LyKgxEC4/SN4TullHzPyaA0Grn1d
NdpSUD6Q5w8D7iXzeWS14pGG9JBKoa/f3/RBSXIqO3HsMttltNbUXpQE/uT+LDZe
yYrwHa9ZAVl3uV5ToMwfbGRK58IILIEDmmSkq97YoYAunqUcC/nAu9SmVQMteQ==
-----END CERTIFICATE-----
";

const FIXTURE_SERVER_CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIC4jCCAcqgAwIBAgIJAPgkrGMQ7ApvMA0GCSqGSIb3DQEBCwUAMBkxFzAVBgNV
BAMMDlRlc3QgUGFyaXR5IENBMCAXDTI2MDgxOTEzMDMxMVoYDzIxMjYwNzI2MTMw
MzExWjAgMR4wHAYDVQQDDBVjZXJ0LWhvc3RuYW1lLmV4YW1wbGUwggEiMA0GCSqG
SIb3DQEBAQUAA4IBDwAwggEKAoIBAQDClp6Zk39KW/t8Pg2EzKn+OEOxL7WlxMCS
T1Je/FWZLHxV2EoyBLBQDvLsbREJnqxoAZ2bYd2Lq8YJWEP/SqnjWFpyPrLxIIb4
HJzI7sqENF6fZpcLLo7hCnVHzWkWOPB1p3QOFYrot1Pnf51Qdw7QkqOp4J2GR3Zf
TbwCSZt2Kkn30IisFzp6Gl/FHPqKEnm6PK9RYtXOCtDjY4tECrSIpQIhkFE/CytI
sq71boNUBL5bpCIQkqVIvMT1pd5b2VRe1p15mOE3QsBBMu9SFk5FSljS/n3NrIQK
AimBqRY5Hx+Se/FpGFADVjzEF4YaalLRHGjVjzaoNVhgcCsWsjwVAgMBAAGjJDAi
MCAGA1UdEQQZMBeCFWNlcnQtaG9zdG5hbWUuZXhhbXBsZTANBgkqhkiG9w0BAQsF
AAOCAQEAKJseOSTJEdEq+pGXPNHsXrizteuF7WlFYBr3Cup/MO4b+nMkJarzmbqp
MFZk3l/3nssmCSlJ4lXDToWadkTkvFF6+IbSENS25OE5v01BNDYpx3P0eKrJ45G2
/FWtAQr/yqVuCdPJSlaeTnn9RsPwZuH/RjWcH7tASPtAy1QaBclTZlX4jz2LzvoJ
QSukREcPVQohSKwAM4pU+YNa7ktLFa0APCQcQ8pks9rY98vxxib0FQNr5kh3ZcCT
GEWslhtuCliQjxnE4A1E8PxtF6I6huONjlJ0Z/uQrZc1B5uJf0dsPlAb0Y3Op5R+
NV3jgQrSEPlHBW/Z+aJ152TvH0IG4w==
-----END CERTIFICATE-----
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
        unix_socket_path: None,
        pooler_mode: None,
        read_only: false,
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

/// Constructs a `VerifyCaCertVerifier` from `FIXTURE_CA_CERT_PEM` and runs
/// it against a leaf cert issued by that CA (`FIXTURE_SERVER_CERT_PEM`),
/// checked against a hostname that deliberately does not match the leaf's
/// CN/SAN (`cert-hostname.example`). Returns the verifier's
/// `verify_server_cert` result so tests can assert accept-vs-reject
/// without a live TLS server.
fn probe_verify_ca_verifier_against_mismatched_hostname() -> Result<(), rustls::Error> {
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName, UnixTime};

    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(FIXTURE_CA_CERT_PEM.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let verifier = VerifyCaCertVerifier::new(roots).expect("verifier should build from valid CA");

    let end_entity: CertificateDer =
        CertificateDer::pem_slice_iter(FIXTURE_SERVER_CERT_PEM.as_bytes())
            .next()
            .unwrap()
            .unwrap();
    let server_name = ServerName::try_from("totally-unrelated-hostname.internal").unwrap();

    verifier
        .verify_server_cert(&end_entity, &[], &server_name, &[], UnixTime::now())
        .map(|_| ())
}

#[test]
fn verify_ca_cert_verifier_accepts_a_chain_valid_cert_with_mismatched_hostname() {
    probe_verify_ca_verifier_against_mismatched_hostname().expect(
        "verify-ca must validate the chain but skip hostname verification — \
         that's the entire distinction from verify-full (matches libpq semantics)",
    );
}

#[test]
fn verify_ca_cert_verifier_still_rejects_a_cert_from_an_untrusted_ca() {
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName, UnixTime};

    // Empty root store: the fixture server cert's issuer is not in it, so
    // chain validation itself (not hostname) must reject this.
    let untrusted_ca_pem = FIXTURE_CERT_PEM; // unrelated CA from load_roots_from_pem's fixtures above
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(untrusted_ca_pem.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let verifier = VerifyCaCertVerifier::new(roots).expect("verifier should build");

    let end_entity: CertificateDer =
        CertificateDer::pem_slice_iter(FIXTURE_SERVER_CERT_PEM.as_bytes())
            .next()
            .unwrap()
            .unwrap();
    let server_name = ServerName::try_from("cert-hostname.example").unwrap();

    let result = verifier.verify_server_cert(&end_entity, &[], &server_name, &[], UnixTime::now());
    assert!(
        result.is_err(),
        "a cert signed by a CA not in the root store must still be rejected"
    );
}

#[test]
fn build_tls_connector_verify_ca_uses_the_hostname_skipping_verifier() {
    let ca_path = write_temp_file(FIXTURE_CA_CERT_PEM);
    let mut params = params_with_ssl("verify-ca");
    params.ssl_ca = Some(ca_path.to_str().unwrap().to_string());

    let result = build_tls_connector(&params);
    std::fs::remove_file(&ca_path).ok();

    result.expect("verify-ca with a valid ssl_ca file should build a connector successfully");
}

// Regression coverage for a merge-time bug (#38's VerifyCaCertVerifier
// branch initially dropped #35's client_auth entirely, always calling
// .with_no_client_auth() regardless of ssl_cert/ssl_key): verify-ca must
// actually attach the client cert, not silently ignore it. Checking
// `has_certs()` distinguishes "attached" from merely "built successfully"
// — the buggy version also built successfully, just with no client cert.
#[test]
fn build_tls_connector_verify_ca_attaches_a_configured_client_cert() {
    let ca_path = write_temp_file(FIXTURE_CA_CERT_PEM);
    let cert_path = write_temp_file(FIXTURE_CLIENT_CERT_PEM);
    let key_path = write_temp_file(FIXTURE_CLIENT_KEY_PEM);

    let mut params = params_with_ssl("verify-ca");
    params.ssl_ca = Some(ca_path.to_str().unwrap().to_string());
    params.ssl_cert = Some(cert_path.to_str().unwrap().to_string());
    params.ssl_key = Some(key_path.to_str().unwrap().to_string());

    let config = build_tls_connector(&params).expect("connector should build");
    std::fs::remove_file(&ca_path).ok();
    std::fs::remove_file(&cert_path).ok();
    std::fs::remove_file(&key_path).ok();

    assert!(
        config.client_auth_cert_resolver.has_certs(),
        "verify-ca must attach the configured client cert, not silently drop it"
    );
}

// Coverage for #43: build_pool never called cfg.ssl_mode(...), so
// tokio_postgres's own default (SslMode::Prefer) applied regardless of
// ssl_mode=require/verify-ca/verify-full, letting connections silently
// fall back to plaintext instead of enforcing TLS at the protocol level.
// resolve_ssl_mode maps this plugin's ssl_mode strings to
// tokio_postgres::config::SslMode, matching the builtin driver's
// build_postgres_configurations mapping exactly.

#[test]
fn resolve_ssl_mode_maps_disable() {
    assert_eq!(resolve_ssl_mode(Some("disable")), Some(SslMode::Disable));
}

#[test]
fn resolve_ssl_mode_maps_allow_and_prefer_to_prefer() {
    assert_eq!(resolve_ssl_mode(Some("allow")), Some(SslMode::Prefer));
    assert_eq!(resolve_ssl_mode(Some("prefer")), Some(SslMode::Prefer));
}

#[test]
fn resolve_ssl_mode_maps_require_verify_ca_and_verify_full_to_require() {
    assert_eq!(resolve_ssl_mode(Some("require")), Some(SslMode::Require));
    assert_eq!(resolve_ssl_mode(Some("verify-ca")), Some(SslMode::Require));
    assert_eq!(
        resolve_ssl_mode(Some("verify-full")),
        Some(SslMode::Require)
    );
}

#[test]
fn resolve_ssl_mode_leaves_unset_or_unknown_values_unmapped() {
    assert_eq!(resolve_ssl_mode(None), None);
    assert_eq!(resolve_ssl_mode(Some("bogus")), None);
}

// Coverage for #192: `uses_tls` returned false for `prefer`/`allow`/unset,
// so `build_pool_impl` wired `NoTls` for them. With `cfg.ssl_mode = Prefer`
// (the unset/default, and the explicit `prefer`/`allow` mapping) plus a
// `NoTls` connector, tokio_postgres's `connect_tls` took its
// `Prefer if !tls.can_connect` arm — and `NoTls::can_connect` is always false
// — so it skipped the SSL request entirely: pure plaintext, never asking the
// server for TLS. Servers that require SSL (`hostssl` / cloud-managed
// Postgres / k8s port-forwards) reject that with
// `no pg_hba.conf entry for host ... no encryption`. The fix inverts the
// predicate to `uses_tls`: only `disable` is plaintext; every other mode
// (including unset, which the builtin normalizes to `prefer`) wires a
// TLS-capable connector so the SSL request is actually sent.

#[test]
fn uses_tls_is_false_only_for_disable() {
    assert!(!uses_tls(&params_with_ssl("disable")));
}

#[test]
fn uses_tls_is_true_for_require_verify_ca_verify_full() {
    assert!(uses_tls(&params_with_ssl("require")));
    assert!(uses_tls(&params_with_ssl("verify-ca")));
    assert!(uses_tls(&params_with_ssl("verify-full")));
}

#[test]
fn uses_tls_is_true_for_prefer_allow_and_unset() {
    // The #192 regression: these three must use TLS so tokio_postgres sends
    // the SSLRequest and can negotiate (or fall back) rather than being
    // forced to plaintext by a NoTls connector whose can_connect is false.
    assert!(uses_tls(&params_with_ssl("prefer")));
    assert!(uses_tls(&params_with_ssl("allow")));
    let mut unset = params_with_ssl("prefer");
    unset.ssl_mode = None;
    assert!(
        uses_tls(&unset),
        "unset ssl_mode must use TLS (builtin treats unset as prefer)"
    );
}

#[test]
fn uses_tls_is_true_for_an_unrecognized_mode() {
    // An unknown ssl_mode is left unmapped by resolve_ssl_mode (tokio_postgres
    // keeps its Prefer default), and uses_tls must still wire a TLS connector
    // so the connection attempts TLS rather than being forced to plaintext.
    // Matching the builtin: only an explicit `disable` opts out of TLS.
    assert!(uses_tls(&params_with_ssl("bogus")));
}

// The #192 fix also closes a second parity gap: `prefer`/`allow`/unset
// previously fell through `build_tls_connector` to `with_platform_verifier()`
// (which validates the server cert against the OS trust store), but the
// builtin uses `NoCertVerifier` (no validation) for `prefer`/`allow`/`require`.
// The three now share the `NoCertVerifier` branch with `require`.
//
// We can't assert the verifier type directly — rustls's `ClientConfig.verifier`
// field is `pub(super)`, private to external crates — and distinguishing
// NoCertVerifier from a platform verifier behaviorally requires an actual TLS
// handshake. The load-bearing live proof that prefer/unset actually negotiate
// TLS (not plaintext) lives in `client_live_tests::prefer_unset_negotiates_tls
// _not_plaintext` (uses `pg_stat_ssl`). Here we lock in the structural
// precondition: the connector still builds for all three non-validating modes
// without a CA. The non-vacuous guard against a revert of the NoCertVerifier
// arm is `build_tls_connector_rejects_an_unrecognized_ssl_mode` below (an
// unrecognized mode that reached the old platform-verifier fallthrough would
// build, not error — that test fails on a revert).

#[test]
fn build_tls_connector_prefer_allow_unset_build_without_a_ca() {
    build_tls_connector(&params_with_ssl("prefer"))
        .expect("prefer must build a connector without ssl_ca");
    build_tls_connector(&params_with_ssl("allow"))
        .expect("allow must build a connector without ssl_ca");
    let mut unset = params_with_ssl("prefer");
    unset.ssl_mode = None;
    build_tls_connector(&unset).expect("unset must build a connector without ssl_ca");
}

// Coverage for the unrecognized-ssl_mode finding from the #195 ensemble
// review: a typo'd or unrecognized ssl_mode must produce a clear config error,
// not silently fall through to with_platform_verifier (verify-full behavior).
// Before this guard, "bogus" matched neither the NoCertVerifier arm nor the
// cert-validation block and reached the platform-verifier fallthrough,
// silently enforcing full chain+hostname validation against the OS trust
// store — breaking self-signed-cert connections with a misleading cert error
// instead of naming the bad ssl_mode. The builtin rejects unknown modes via
// `parse_postgres_ssl_mode`'s `_ => Err(unknown_ssl_mode_message(...))` arm.

#[test]
fn build_tls_connector_rejects_an_unrecognized_ssl_mode() {
    let err = build_tls_connector(&params_with_ssl("bogus"))
        .expect_err("an unrecognized ssl_mode must be rejected, not silently used");
    assert!(
        err.contains("Unknown SSL mode") && err.contains("bogus"),
        "error must name the unrecognized mode; got: {err}"
    );
    assert!(
        err.contains("verify-ca") && err.contains("verify-full"),
        "error must list the accepted values; got: {err}"
    );
}

#[test]
fn build_tls_connector_rejects_a_typo_of_require() {
    // A plausible typo: missing the 'u' in "require". Must error, not silently
    // fall through to platform-verifier TLS (which would break a connection
    // that previously worked in plaintext, with a misleading cert error).
    let err = build_tls_connector(&params_with_ssl("reqire"))
        .expect_err("a typo of require must be rejected");
    assert!(
        err.contains("Unknown SSL mode"),
        "expected unknown-ssl_mode error; got: {err}"
    );
}

// Normalization (trim + lowercase, matching the builtin's normalized_ssl_mode
// + non_empty_ssl_mode): case and whitespace variants must route the same way
// as their canonical form. These close the edge-case findings from the #195
// review (case-sensitivity, whitespace, empty-string).

#[test]
fn uses_tls_is_false_for_case_and_whitespace_variants_of_disable() {
    assert!(!uses_tls(&params_with_ssl("DISABLE")));
    assert!(!uses_tls(&params_with_ssl("Disable")));
    assert!(!uses_tls(&params_with_ssl(" disable ")));
    assert!(!uses_tls(&params_with_ssl("\tdisable\n")));
}

#[test]
fn uses_tls_treats_trimmed_empty_as_unset_not_disable() {
    // An empty/whitespace ssl_mode is treated as unset (prefer), not disable —
    // matching the builtin's non_empty_ssl_mode, which filters trimmed-empty
    // to None. So it must use TLS, not plaintext.
    assert!(uses_tls(&params_with_ssl("")));
    assert!(uses_tls(&params_with_ssl("   ")));
}

#[test]
fn build_tls_connector_accepts_case_and_whitespace_variants_of_prefer() {
    // The normalized match arm must accept uppercase/whitespace variants,
    // not just the canonical lowercase "prefer".
    build_tls_connector(&params_with_ssl("PREFER"))
        .expect("PREFER must build (normalized to prefer)");
    build_tls_connector(&params_with_ssl(" Prefer "))
        .expect("\" Prefer \" must build (normalized to prefer)");
}

// Coverage for #44: build_tls_connector's `require` branch fell through to
// with_platform_verifier(), which DOES validate the server cert against the
// OS trust store — contradicting the function's own doc comment ("require
// forces TLS without certificate validation") and the builtin's actual
// behavior (NoCertVerifier: no validation at all for this mode). Unlike
// VerifyCaCertVerifier (which still skips hostname but validates the
// chain), NoCertVerifier accepts anything — not even a hostname check —
// matching the builtin's own NoCertVerifier exactly.

#[test]
fn no_cert_verifier_accepts_a_cert_with_no_matching_hostname_or_chain() {
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName, UnixTime};

    let verifier = NoCertVerifier::new();

    let end_entity: CertificateDer =
        CertificateDer::pem_slice_iter(FIXTURE_SERVER_CERT_PEM.as_bytes())
            .next()
            .unwrap()
            .unwrap();
    // Deliberately mismatched hostname vs. the cert's CN/SAN
    // (cert-hostname.example) — proves this verifier doesn't even do the
    // hostname check VerifyCaCertVerifier skips deliberately; it does no
    // checking of any kind.
    let server_name = ServerName::try_from("totally-unrelated-hostname.internal").unwrap();

    let result = verifier.verify_server_cert(&end_entity, &[], &server_name, &[], UnixTime::now());
    assert!(
        result.is_ok(),
        "require mode must accept any certificate, matching the builtin's NoCertVerifier"
    );
}

#[test]
fn build_tls_connector_require_builds_successfully_with_no_ssl_ca() {
    let params = params_with_ssl("require");
    build_tls_connector(&params)
        .expect("require mode must build a connector without needing ssl_ca set");
}

// Coverage for #90: `require`/`verify-ca` lazily install the rustls
// CryptoProvider as a side effect of `NoCertVerifier::new()` /
// `VerifyCaCertVerifier::new()`, but `verify-full` (both branches below)
// builds a `ClientConfig`/`WebPkiServerVerifier` directly, with no such side
// effect. (`prefer`/`allow`/unset now go through `NoCertVerifier::new()` too
// after #192, and `disable` never reaches `build_tls_connector` at all.) In
// rustls 0.23 with only the `ring` feature enabled (no auto-install), the
// first TLS operation in the *process* to reach `ClientConfig::builder()` (or
// `WebPkiServerVerifier::builder()`, or `with_platform_verifier()`) panics
// with "no process-level CryptoProvider available" unless something
// installed a provider first. `build_tls_connector` now calls
// `ensure_rustls_crypto_provider()` unconditionally at its top, so these
// branches build successfully regardless of call order — these tests
// don't (and structurally can't, since the shared test binary runs many
// tests in one process and a `Once` is process-global) prove the panic
// would occur without the fix; they lock in that the previously-under-
// verified `verify-full` branches still succeed with it.
#[test]
fn build_tls_connector_verify_full_with_ca_builds_successfully() {
    let ca_path = write_temp_file(FIXTURE_CA_CERT_PEM);
    let mut params = params_with_ssl("verify-full");
    params.ssl_ca = Some(ca_path.to_str().unwrap().to_string());

    let result = build_tls_connector(&params);
    std::fs::remove_file(&ca_path).ok();

    result.expect("verify-full with an explicit CA must build a connector without panicking");
}

#[test]
fn build_tls_connector_verify_full_without_ca_builds_successfully() {
    // No ssl_ca -> falls through to WebPkiServerVerifier is NOT used here;
    // verify-full without a CA takes the platform-trust fallthrough path
    // (needs_cert_validation is true but user_ca is None, so the `if let
    // Some(ca_path) = user_ca` block is skipped entirely).
    let params = params_with_ssl("verify-full");
    build_tls_connector(&params)
        .expect("verify-full without ssl_ca must build a connector without panicking");
}

#[test]
fn build_tls_connector_prefer_mode_builds_successfully() {
    // `prefer` now shares the NoCertVerifier branch with `require`/`allow`/
    // unset (#192), instead of falling through to with_platform_verifier().
    // The connector must still build without panicking.
    let params = params_with_ssl("prefer");
    build_tls_connector(&params).expect("prefer mode must build a connector without panicking");
}

#[test]
fn build_tls_connector_disable_mode_does_not_build_a_tls_connector() {
    // `disable` never reaches `build_tls_connector` in production — `uses_tls`
    // is false for it, so the pool wires `NoTls` directly. The function
    // rejects it as an unrecognized-for-this-path mode rather than silently
    // building a TLS connector the pool would never use. This guards against
    // a future refactor that routes `disable` here by accident.
    let params = params_with_ssl("disable");
    let err = build_tls_connector(&params)
        .expect_err("disable must not build a TLS connector — it uses NoTls");
    assert!(
        err.contains("Unknown SSL mode") && err.contains("disable"),
        "expected an unknown-ssl_mode error for disable, got: {err}"
    );
}

// Coverage for #46: verify-ca without an explicit ssl_ca silently fell
// through to with_platform_verifier() instead of erroring — the builtin
// driver's build_postgres_tls_connector errors instead ("verify-ca mode
// requires an explicit CA file..."), since platform roots are deliberately
// not used for this mode (macOS EKU checks reject them).
#[test]
fn build_tls_connector_verify_ca_without_ssl_ca_returns_a_clear_error() {
    let params = params_with_ssl("verify-ca");

    let err = build_tls_connector(&params)
        .expect_err("verify-ca without ssl_ca must be rejected as a config error");
    assert!(
        err.contains("verify-ca") && err.contains("ssl_ca"),
        "unexpected error message: {err}"
    );
}

// `with_text_types` lifts the untyped `&[&(dyn ToSql + Sync)]` param slice
// the catalog-query helpers (`query_rows`/`query_strings`) take into the
// `(value, Type)` pairs the unnamed-statement protocol needs, binding each
// param as `TEXT`. This is the bridge that lets those helpers switch from
// the named `query` path (which mints `s{N}` and splits Parse/Bind across
// two Syncs — the pooler-unsafe behavior behind tabularis#1097) to tokio's
// inherent `query_typed` (unnamed `parse("")`, single Sync). Every caller
// passes string params to catalog queries, so `TEXT` is always the right
// wire type; a non-string value would fail `to_sql_checked`'s `accepts`
// check at bind time with a `WrongType` error rather than mis-encoding.
#[test]
fn with_text_types_pairs_each_param_with_text_oid() {
    let a: String = "public".to_string();
    let b: String = "users".to_string();
    let params: &[&(dyn ToSql + Sync)] = &[&a, &b];
    let typed = with_text_types(params);
    assert_eq!(typed.len(), 2, "one (value, Type) pair per input param");
    assert_eq!(typed[0].1, Type::TEXT, "first param bound as TEXT");
    assert_eq!(typed[1].1, Type::TEXT, "second param bound as TEXT");
    // Verify the value references are preserved in order — without this, a
    // bug that duplicates params[0] for every entry (or swaps them) would
    // pass the len/Type assertions above while silently mis-binding catalog
    // queries ($1 and $2 would both get "public").
    assert!(
        std::ptr::eq(typed[0].0, params[0]),
        "first value reference preserved"
    );
    assert!(
        std::ptr::eq(typed[1].0, params[1]),
        "second value reference preserved"
    );
}

#[test]
fn with_text_types_empty_slice_yields_empty_vec() {
    let params: &[&(dyn ToSql + Sync)] = &[];
    let typed = with_text_types(params);
    assert!(
        typed.is_empty(),
        "no params -> no typed pairs (unnamed parse with no bind OIDs)"
    );
}

// Coverage for #61: `build_pool` previously never set `max_size`, so deadpool
// fell back to `PoolConfig::default()` = `get_default_pool_max_size()` =
// logical_cores × 2 (up to ~32 on a 16-thread machine) — up to ~3× more
// backend connections per target than the built-in's pinned 10. The parity
// suite doesn't catch this (82 tests compare query results, not connection
// counts). These tests close that gap by inspecting the built pool's
// `status().max_size` directly.
//
// deadpool's `Pool::new` is lazy (no connection at creation time when no
// startup script is set), so a fake host is enough — no live DB needed, and
// these run in CI's ordinary Test job. `build_pool_pub` calls
// `get_or_create_pool`, which caches into the shared `POOLS` static, so each
// test holds `POOLS_TEST_LOCK` (to serialize with the cache-count tests
// above) AND `POOL_MAX_SIZE_TEST_LOCK` (to serialize with the `settings`
// tests), then resets the pool-size global to the default first. Two locks
// are acquired settings-first; order is consistent with `settings_tests`.

#[tokio::test]
async fn build_pool_default_max_size_is_ten_not_cpu_times_two() {
    let _settings_guard = settings::test_support::lock_and_reset().await;
    let _pools_guard = POOLS_TEST_LOCK.lock().await;
    settings::set_pool_max_size(&serde_json::json!({ "poolMaxSize": 10 }));
    let p = params("pool-size-default-test-host", 5432, "db", "user");
    let pool = build_pool_pub(&p)
        .await
        .expect("lazy pool builds without a live DB when no startup script is set");
    let status = pool.status();
    assert_eq!(
        status.max_size,
        10,
        "default pool max size must be the built-in's pinned 10 (parity), not \
         deadpool's cpu×2 default ({} on this machine)",
        num_cpus_from_deadpool_default(),
    );
}

#[tokio::test]
async fn build_pool_honors_custom_pool_max_size_setting() {
    let _settings_guard = settings::test_support::lock_and_reset().await;
    let _pools_guard = POOLS_TEST_LOCK.lock().await;
    settings::set_pool_max_size(&serde_json::json!({ "poolMaxSize": 3 }));
    let p = params("pool-size-custom-test-host", 5432, "db", "user");
    let pool = build_pool_pub(&p)
        .await
        .expect("lazy pool builds without a live DB when no startup script is set");
    assert_eq!(
        pool.status().max_size,
        3,
        "a poolMaxSize of 3 must be applied to the built pool's max_size"
    );
}

#[tokio::test]
async fn build_pool_clamps_oversized_pool_max_size_to_cap() {
    let _settings_guard = settings::test_support::lock_and_reset().await;
    let _pools_guard = POOLS_TEST_LOCK.lock().await;
    settings::set_pool_max_size(&serde_json::json!({ "poolMaxSize": 10_000 }));
    let p = params("pool-size-cap-test-host", 5432, "db", "user");
    let pool = build_pool_pub(&p)
        .await
        .expect("lazy pool builds without a live DB when no startup script is set");
    assert_eq!(
        pool.status().max_size,
        64,
        "an oversized poolMaxSize must be clamped to the 64 cap before the pool is built"
    );
}

/// What deadpool's cpu×2 default *would* be on this machine — only used in
/// the failure message above, never as an assertion (the test must pass on
/// any core count). Computed from the live `num_cpus` rather than a constant
/// so the diagnostic stays accurate across CI runners.
fn num_cpus_from_deadpool_default() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get() * 2)
        .unwrap_or(20)
}

/// `pooler_error_lead` selects the lead sentence of the pooler guidance
/// `format_query_error` prepends to a 26000/42P05 error (#160/#163). The
/// `false` branch is the new behavior this PR added (older hosts that don't
/// send `pooler_mode` still see conditional guidance instead of a bare
/// message); the `true` branch is definitive. `format_query_error` itself
/// needs a live `tokio_postgres::Error` to exercise (its `DbError`/SQLSTATE
/// can't be built without a server response), so the pure phrasing selection
/// is tested here and the SQLSTATE-gated assembly is covered by the live
/// `client_live_tests` error-message tests.
#[test]
fn pooler_error_lead_definitive_when_pooler_declared() {
    let lead = pooler_error_lead(true);
    assert!(
        lead.starts_with("This error occurs under transaction-mode poolers"),
        "definitive lead for a declared transaction pooler: {lead:?}"
    );
    assert!(
        !lead.starts_with("If this connection"),
        "must not hedge when the host declared pooler_mode=transaction"
    );
}

#[test]
fn pooler_error_lead_conditional_when_pooler_not_declared() {
    // The PR's new behavior: even when is_transaction_pooler is false, the
    // guidance is emitted (conditionally phrased) rather than suppressed —
    // so users on older hosts that can't send pooler_mode still see the hint.
    let lead = pooler_error_lead(false);
    assert!(
        lead.starts_with("If this connection goes through a transaction-mode pooler"),
        "conditional lead for an undeclared pooler: {lead:?}"
    );
    assert!(
        lead.contains("Supabase/Supavisor"),
        "conditional lead should name the pooler flavors users hit (#163): {lead:?}"
    );
}

// Coverage for #197: `split_postgres_socket_path` ports the builtin's
// `split_postgres_socket_path` (`pool_manager.rs` at `012cd844^`, line 667-
// 680) verbatim — recognizing the `.s.PGSQL.<port>` socket-file shape vs a
// bare directory path.

#[test]
fn split_postgres_socket_path_recognizes_the_socket_file_shape() {
    // `/var/run/postgresql/.s.PGSQL.5432` -> dir + port from the filename.
    assert_eq!(
        split_postgres_socket_path("/var/run/postgresql/.s.PGSQL.5432", 5432),
        ("/var/run/postgresql".to_string(), 5432)
    );
}

#[test]
fn split_postgres_socket_path_recognizes_a_custom_port_in_the_filename() {
    assert_eq!(
        split_postgres_socket_path("/tmp/.s.PGSQL.54320", 5432),
        ("/tmp".to_string(), 54320)
    );
}

#[test]
fn split_postgres_socket_path_treats_a_bare_directory_as_dir_plus_default_port() {
    // A path whose last component isn't `.s.PGSQL.<port>` is the directory
    // itself, paired with the default port.
    assert_eq!(
        split_postgres_socket_path("/var/run/postgresql", 5432),
        ("/var/run/postgresql".to_string(), 5432)
    );
}

#[test]
fn split_postgres_socket_path_handles_root_dir_when_socket_is_at_root() {
    // `/.s.PGSQL.5432` -> dir is `/` (not empty), port 5432.
    assert_eq!(
        split_postgres_socket_path("/.s.PGSQL.5432", 5432),
        ("/".to_string(), 5432)
    );
}

#[test]
fn split_postgres_socket_path_passes_through_a_path_with_no_slash() {
    // No `/` in the path -> the whole thing is the directory.
    assert_eq!(
        split_postgres_socket_path("relative_dir", 5432),
        ("relative_dir".to_string(), 5432)
    );
}

#[test]
fn split_postgres_socket_path_does_not_match_a_non_pgsql_filename() {
    // A filename that isn't `.s.PGSQL.<port>` is not a socket file — the
    // whole path is treated as the directory with the default port.
    assert_eq!(
        split_postgres_socket_path("/var/run/postgresql/postgres.sock", 5432),
        ("/var/run/postgresql/postgres.sock".to_string(), 5432)
    );
}

// Coverage for #197: the pool cache key must distinguish a Unix-socket
// connection from a TCP connection to the same database, and two socket
// connections to the same path must share a key.

#[test]
fn connection_key_distinguishes_socket_from_tcp_for_the_same_database() {
    // Same host:port on both — the distinction can ONLY come from the socket
    // endpoint. If the socket keying were reverted (falling back to host:port),
    // both keys would be identical and this test would fail. (The earlier
    // version of this test used different host:port values, which made it
    // vacuous — it passed even with the socket keying reverted. #198 review.)
    let tcp = params("localhost", 5432, "db", "user");
    let mut sock = tcp.clone();
    sock.unix_socket_path = Some("/var/run/postgresql".to_string());
    // Same host:port as tcp — the socket must override it in the key.
    assert_ne!(
        connection_key(&tcp),
        connection_key(&sock),
        "a socket connection and a TCP connection with the same host:port + database \
         must not share a pool — the socket endpoint must distinguish them"
    );
}

#[test]
fn connection_key_socket_key_ignores_host_and_port() {
    // Two socket connections to the same path but different (bogus) host/port
    // must share a key — the socket overrides host:port, so the key should
    // not depend on them.
    let mut a = params("ignored.invalid", 1, "db", "user");
    a.unix_socket_path = Some("/var/run/postgresql".to_string());
    let mut b = params("also.invalid", 9999, "db", "user");
    b.unix_socket_path = Some("/var/run/postgresql".to_string());
    assert_eq!(
        connection_key(&a),
        connection_key(&b),
        "same socket + database must share a key regardless of host/port \
         (the socket overrides them)"
    );
}

#[test]
fn connection_key_distinguishes_different_socket_paths() {
    let mut a = params("ignored.invalid", 1, "db", "user");
    a.unix_socket_path = Some("/var/run/postgresql".to_string());
    let mut b = params("ignored.invalid", 1, "db", "user");
    b.unix_socket_path = Some("/tmp".to_string());
    assert_ne!(
        connection_key(&a),
        connection_key(&b),
        "different socket paths must produce distinct keys"
    );
}

#[test]
fn connection_key_treats_a_blank_socket_path_as_absent() {
    // A whitespace-only socket path is treated as unset (local_socket_path
    // filters it), so the key falls back to host:port.
    let mut with_blank = params("localhost", 5432, "db", "user");
    with_blank.unix_socket_path = Some("   ".to_string());
    let without = params("localhost", 5432, "db", "user");
    assert_eq!(
        connection_key(&with_blank),
        connection_key(&without),
        "a blank socket path must be treated as absent (falls back to host:port)"
    );
}

// Coverage for the #198 review's test-coverage finding: build_socket_pool's
// .host_path(&dir) call (the load-bearing Unix-socket-dialing call) had no
// CI-runnable test. Extracted build_socket_pg_config so a unit test can
// assert the config dials a Host::Unix path (not a TCP host) and forces
// SslMode::Disable — without a live socket.

#[cfg(unix)]
#[test]
fn build_socket_pg_config_dials_a_unix_socket_not_tcp() {
    use tokio_postgres::config::Host;

    let mut params = params("ignored.invalid", 1, "db", "user");
    params.unix_socket_path = Some("/var/run/postgresql/.s.PGSQL.5432".to_string());
    let config = super::build_socket_pg_config(&params, "/var/run/postgresql/.s.PGSQL.5432");

    // The host list must contain a Unix socket path, not a TCP hostname.
    // If .host_path were typo'd to .host, this would be Host::Tcp instead.
    let hosts = config.get_hosts();
    assert_eq!(hosts.len(), 1, "exactly one host (the socket dir)");
    match &hosts[0] {
        Host::Unix(path) => {
            assert_eq!(
                path,
                std::path::Path::new("/var/run/postgresql"),
                "host_path must point at the socket directory, not the .s.PGSQL file"
            );
        }
        Host::Tcp(h) => panic!("expected Host::Unix, got Host::Tcp({h}) — .host_path was not used"),
    }
}

#[cfg(unix)]
#[test]
fn build_socket_pg_config_extracts_port_from_spgsql_filename() {
    let mut params = params("ignored.invalid", 1, "db", "user");
    params.unix_socket_path = Some("/tmp/.s.PGSQL.54320".to_string());
    let config = super::build_socket_pg_config(&params, "/tmp/.s.PGSQL.54320");
    let ports = config.get_ports();
    assert_eq!(
        ports,
        vec![54320],
        "port must be extracted from .s.PGSQL.54320, not params.port=1"
    );
}

#[cfg(unix)]
#[test]
fn build_socket_pg_config_forces_ssl_mode_disable() {
    let mut params = params("ignored.invalid", 1, "db", "user");
    params.unix_socket_path = Some("/var/run/postgresql".to_string());
    // Even if the user set ssl_mode=require, the socket config must force Disable.
    params.ssl_mode = Some("require".to_string());
    let config = super::build_socket_pg_config(&params, "/var/run/postgresql");
    assert_eq!(
        config.get_ssl_mode(),
        tokio_postgres::config::SslMode::Disable,
        "socket config must force SslMode::Disable regardless of params.ssl_mode"
    );
}

// Coverage for the #198 review's cancel-path finding: uses_tls must return
// false when a Unix socket is set, even if ssl_mode is verify-ca/require/etc.
// The socket branch forces SslMode::Disable, so the cancel path must match or
// it would try to build a TLS connector and fail, silently dropping the cancel.

#[test]
fn uses_tls_returns_false_when_a_socket_is_set_regardless_of_ssl_mode() {
    let mut params = params("ignored.invalid", 1, "db", "user");
    params.unix_socket_path = Some("/var/run/postgresql".to_string());

    for mode in [
        "disable",
        "allow",
        "prefer",
        "require",
        "verify-ca",
        "verify-full",
    ] {
        params.ssl_mode = Some(mode.to_string());
        assert!(
            !uses_tls(&params),
            "uses_tls must return false for ssl_mode={mode} when a socket is set (socket forces Disable)"
        );
    }
}

#[test]
fn uses_tls_returns_true_for_tls_modes_when_no_socket() {
    let params = params("localhost", 5432, "db", "user");
    let mut p = params.clone();
    p.ssl_mode = Some("require".to_string());
    assert!(uses_tls(&p));
    p.ssl_mode = Some("verify-ca".to_string());
    assert!(uses_tls(&p));
    p.ssl_mode = Some("verify-full".to_string());
    assert!(uses_tls(&p));
}

// Coverage for the #198 review's precedence-mismatch finding: connection_key
// must key on the socket even when connection_string is also set (matching
// build_pool_impl's order — socket first). Before the fix, connection_key
// checked connection_string first and ignored the socket, so two requests
// sharing a connection_string but differing in socket path collided on one
// cached pool.

#[test]
fn connection_key_includes_socket_even_when_connection_string_is_set() {
    // Two connections with the SAME connection_string but DIFFERENT socket
    // paths. If the socket is ignored in the key (the pre-fix bug), both
    // produce cs:{same hash} and collide. After the fix, the socket takes
    // precedence and each gets its own socket:{path} key.
    let conn_str = "postgres://user@localhost:5432/db";
    let mut a = params("ignored.invalid", 1, "db", "user");
    a.connection_string = Some(conn_str.to_string());
    a.unix_socket_path = Some("/var/run/postgresql".to_string());

    let mut b = params("ignored.invalid", 1, "db", "user");
    b.connection_string = Some(conn_str.to_string());
    b.unix_socket_path = Some("/tmp".to_string());

    assert_ne!(
        connection_key(&a),
        connection_key(&b),
        "two connections sharing a connection_string but with different socket paths \
         must get distinct keys — the socket takes precedence over connection_string \
         (matching build_pool_impl's order)"
    );
}

#[test]
fn connection_key_socket_precedence_keys_on_socket_not_connection_string() {
    // A socket connection's key must start with "socket:" (not "cs:"), even
    // when connection_string is set — proving the socket won the precedence.
    let mut params = params("ignored.invalid", 1, "db", "user");
    params.connection_string = Some("postgres://user@localhost:5432/db".to_string());
    params.unix_socket_path = Some("/var/run/postgresql".to_string());
    let key = connection_key(&params);
    assert!(
        key.starts_with("socket:"),
        "socket connection's key must start with 'socket:' even when connection_string is set; got: {key}"
    );
}

// --- read-only mode (#202) -------------------------------------------------

fn read_only_params() -> ConnectionParams {
    let mut p = params("localhost", 5432, "db", "postgres");
    p.read_only = true;
    p
}

#[test]
fn a_read_only_connection_never_shares_a_pool_with_a_writable_one() {
    let writable = connection_key(&params("localhost", 5432, "db", "postgres"));
    let protected = connection_key(&read_only_params());
    assert_ne!(
        writable, protected,
        "a protected pool must not hand out writable sessions"
    );
    assert!(protected.starts_with("read_only:"), "{protected}");
}

#[test]
fn a_read_only_key_is_stable_and_distinguishes_targets_like_a_writable_one() {
    let a = connection_key(&read_only_params());
    assert_eq!(a, connection_key(&read_only_params()));
    let mut other = read_only_params();
    other.database = Some("other".into());
    assert_ne!(a, connection_key(&other));
}

#[test]
fn the_read_only_key_prefix_applies_to_socket_and_connection_string_keys_too() {
    let mut cs = read_only_params();
    cs.connection_string = Some("postgres://u:p@h/db".into());
    assert!(connection_key(&cs).starts_with("read_only:cs:"));
    #[cfg(unix)]
    {
        let mut sock = read_only_params();
        sock.unix_socket_path = Some("/var/run/postgresql/.s.PGSQL.5432".into());
        assert!(connection_key(&sock).starts_with("read_only:socket:"));
    }
}

#[test]
fn the_startup_script_does_not_change_a_read_only_key() {
    // It never runs in read-only mode, so two scripts must not split the pool.
    let mut a = read_only_params();
    a.startup_script = Some("SET a = 1".into());
    let mut b = read_only_params();
    b.startup_script = Some("SET b = 2".into());
    assert_eq!(connection_key(&a), connection_key(&b));
    assert_eq!(connection_key(&a), connection_key(&read_only_params()));
}

#[test]
fn the_startup_script_still_splits_a_writable_key() {
    let mut a = params("localhost", 5432, "db", "postgres");
    a.startup_script = Some("SET a = 1".into());
    let mut b = params("localhost", 5432, "db", "postgres");
    b.startup_script = Some("SET b = 2".into());
    assert_ne!(connection_key(&a), connection_key(&b));
}

#[test]
fn a_read_only_connection_asks_the_server_for_read_only_sessions() {
    assert_eq!(
        super::session_options(&read_only_params()),
        Some("-c default_transaction_read_only=on".to_string())
    );
    assert_eq!(super::session_options(&params("h", 1, "d", "u")), None);
}

#[test]
fn a_read_only_connection_behind_a_transaction_pooler_sends_no_startup_option() {
    // PgBouncer refuses `-c default_transaction_read_only=on` in the startup
    // packet ("unsupported startup parameter in options"), so sending it would
    // make the connection impossible to open. A session-level SET would be
    // worse: the backend is shared, so it could leak read-only mode to another
    // client. Each protected call opens its own `BEGIN READ ONLY` instead.
    let mut p = read_only_params();
    p.pooler_mode = Some("transaction".into());
    assert_eq!(super::session_options(&p), None);
    p.pooler_mode = Some("session".into());
    assert!(super::session_options(&p).is_some());
}

#[cfg(unix)]
#[test]
fn a_read_only_socket_connection_asks_for_read_only_sessions_too() {
    let mut p = read_only_params();
    p.unix_socket_path = Some("/var/run/postgresql/.s.PGSQL.5432".into());
    let config = super::build_socket_pg_config(&p, "/var/run/postgresql/.s.PGSQL.5432");
    assert_eq!(
        config.get_options(),
        Some("-c default_transaction_read_only=on")
    );
    let writable = super::build_socket_pg_config(
        &params("h", 1, "d", "u"),
        "/var/run/postgresql/.s.PGSQL.5432",
    );
    assert_eq!(writable.get_options(), None);
}
