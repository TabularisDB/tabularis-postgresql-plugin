//! Unit tests for `client.rs`. Sibling test file per repo convention
//! (`.rules/rust.md` #4/#5) — loaded via `#[cfg(test)] mod client_tests;`.

use super::{connection_key, get_or_create_pool, POOLS};
use crate::models::ConnectionParams;

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
fn get_or_create_pool_reuses_cached_entry_for_identical_params() {
    // deadpool's Pool::new is lazy (no connection attempt at creation
    // time), so this exercises only the cache bookkeeping, not real
    // connectivity. Use a key unlikely to collide with other tests
    // running in the same process.
    let p = params("cache-test-host-unique", 5432, "db", "user");
    let key = connection_key(&p);

    let before = POOLS.lock().unwrap().len();
    get_or_create_pool(&p).expect("first call creates and caches a pool");
    let after_first = POOLS.lock().unwrap().len();
    assert_eq!(after_first, before + 1, "first call should insert one entry");
    assert!(POOLS.lock().unwrap().contains_key(&key));

    get_or_create_pool(&p).expect("second call should hit the cache");
    let after_second = POOLS.lock().unwrap().len();
    assert_eq!(
        after_second, after_first,
        "second call with identical params must not create a new entry"
    );
}
