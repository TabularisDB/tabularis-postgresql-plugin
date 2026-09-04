//! Tests for `pool_max_size_from_value` and the `set_pool_max_size` /
//! `pool_max_size` process-global pair. Mirrors the built-in driver's
//! `postgres_pool_max_size_from_value` test cases in
//! `tabularis/src-tauri/src/pool_manager_tests.rs` (ported verbatim), plus
//! this plugin's own `initialize`-shaped coverage.

use serde_json::json;

use super::test_support::lock_and_reset_blocking;
use super::{pool_max_size, pool_max_size_from_value, set_pool_max_size, DEFAULT_POOL_MAX_SIZE};

// --- Verbatim ports of the built-in's four parity cases (tabularis#681) ---
// These exercise the pure parser and need no serialization. ---

#[test]
fn pool_max_size_defaults_without_setting() {
    assert_eq!(pool_max_size_from_value(None), DEFAULT_POOL_MAX_SIZE);
}

#[test]
fn pool_max_size_accepts_numeric_and_string_settings() {
    assert_eq!(pool_max_size_from_value(Some(&json!(1))), 1);
    assert_eq!(pool_max_size_from_value(Some(&json!("3"))), 3);
}

#[test]
fn pool_max_size_ignores_invalid_or_zero_settings() {
    assert_eq!(
        pool_max_size_from_value(Some(&json!(0))),
        DEFAULT_POOL_MAX_SIZE
    );
    assert_eq!(
        pool_max_size_from_value(Some(&json!("not-a-number"))),
        DEFAULT_POOL_MAX_SIZE
    );
}

#[test]
fn pool_max_size_caps_oversized_settings() {
    assert_eq!(pool_max_size_from_value(Some(&json!(10_000))), 64);
}

// --- Edge cases the built-in's tests don't spell out but the parser
//     must handle (defense in depth): negative, null, bool, float, empty
//     string, and the exact cap boundary. Each falls back to the default
//     or clamps — never panics, since `initialize` failures are silently
//     ignored by the host and a panic would kill the handshake. ---

#[test]
fn pool_max_size_ignores_negative_i64() {
    // `as_u64` rejects negatives; `as_i64` accepts -5 but
    // `u64::try_from(-5)` fails, so the chain falls through to default.
    assert_eq!(
        pool_max_size_from_value(Some(&json!(-5))),
        DEFAULT_POOL_MAX_SIZE
    );
}

#[test]
fn pool_max_size_ignores_null_bool_object_and_array() {
    assert_eq!(
        pool_max_size_from_value(Some(&json!(null))),
        DEFAULT_POOL_MAX_SIZE
    );
    assert_eq!(
        pool_max_size_from_value(Some(&json!(true))),
        DEFAULT_POOL_MAX_SIZE
    );
    assert_eq!(
        pool_max_size_from_value(Some(&json!({"max_size": 10}))),
        DEFAULT_POOL_MAX_SIZE
    );
    assert_eq!(
        pool_max_size_from_value(Some(&json!([10]))),
        DEFAULT_POOL_MAX_SIZE
    );
}

#[test]
fn pool_max_size_ignores_float_strings_and_empty_string() {
    // `as_str().parse::<u64>()` rejects "3.0", " 3 " (whitespace), and "".
    assert_eq!(
        pool_max_size_from_value(Some(&json!("3.0"))),
        DEFAULT_POOL_MAX_SIZE
    );
    assert_eq!(
        pool_max_size_from_value(Some(&json!(" 3 "))),
        DEFAULT_POOL_MAX_SIZE
    );
    assert_eq!(
        pool_max_size_from_value(Some(&json!(""))),
        DEFAULT_POOL_MAX_SIZE
    );
}

#[test]
fn pool_max_size_accepts_boundary_values() {
    // 1 is the smallest valid; 64 is the cap (not rejected). 65 clamps to 64.
    assert_eq!(pool_max_size_from_value(Some(&json!(1))), 1);
    assert_eq!(pool_max_size_from_value(Some(&json!(64))), 64);
    assert_eq!(pool_max_size_from_value(Some(&json!(65))), 64);
}

#[test]
fn pool_max_size_accepts_stringified_boundary() {
    assert_eq!(pool_max_size_from_value(Some(&json!("64"))), 64);
}

// --- set_pool_max_size / pool_max_size: the initialize-handshake path.
//     These touch the process-global `POOL_MAX_SIZE`, so each holds the
//     shared `POOL_MAX_SIZE_TEST_LOCK` (via `lock_and_reset`) and starts
//     from the default, making them order-independent across this module
//     and the `build_pool` tests in `client_tests.rs`. ---

#[test]
fn set_pool_max_size_stores_parsed_value() {
    let _guard = lock_and_reset_blocking();
    set_pool_max_size(&json!({ "poolMaxSize": 8 }));
    assert_eq!(pool_max_size(), 8);
}

#[test]
fn set_pool_max_size_defaults_when_setting_absent() {
    let _guard = lock_and_reset_blocking();
    set_pool_max_size(&json!({ "unrelated": "value" }));
    assert_eq!(pool_max_size(), DEFAULT_POOL_MAX_SIZE);
}

#[test]
fn set_pool_max_size_defaults_when_settings_not_an_object() {
    // A malformed host payload where `settings` isn't an object: `.get`
    // returns None, so the default applies. Defense in depth.
    let _guard = lock_and_reset_blocking();
    set_pool_max_size(&json!("not-an-object"));
    assert_eq!(pool_max_size(), DEFAULT_POOL_MAX_SIZE);
}

#[test]
fn set_pool_max_size_clamps_oversized() {
    let _guard = lock_and_reset_blocking();
    set_pool_max_size(&json!({ "poolMaxSize": 10_000 }));
    assert_eq!(pool_max_size(), 64);
}

#[test]
fn set_pool_max_size_accepts_string_value() {
    let _guard = lock_and_reset_blocking();
    set_pool_max_size(&json!({ "poolMaxSize": "20" }));
    assert_eq!(pool_max_size(), 20);
}

#[test]
fn set_pool_max_size_most_recent_initialize_wins() {
    // Matches the built-in, which reads the current config on each pool
    // build: a corrected re-init replaces the prior value (unlike a
    // first-wins `OnceLock`).
    let _guard = lock_and_reset_blocking();
    set_pool_max_size(&json!({ "poolMaxSize": 5 }));
    assert_eq!(pool_max_size(), 5);
    set_pool_max_size(&json!({ "poolMaxSize": 12 }));
    assert_eq!(pool_max_size(), 12);
}

#[test]
fn set_pool_max_size_zero_resets_to_default_not_retained() {
    // Zero is invalid → default, so a re-init with zero doesn't shrink the
    // pool below the safe default (defense in depth: pgBouncer users who
    // accidentally set 0 get 10, not a dead 0-connection pool).
    let _guard = lock_and_reset_blocking();
    set_pool_max_size(&json!({ "poolMaxSize": 7 }));
    assert_eq!(pool_max_size(), 7);
    set_pool_max_size(&json!({ "poolMaxSize": 0 }));
    assert_eq!(pool_max_size(), DEFAULT_POOL_MAX_SIZE);
}
