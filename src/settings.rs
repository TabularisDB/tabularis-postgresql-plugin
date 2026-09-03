//! Plugin settings received from the host via the `initialize` RPC.
//!
//! The host sends `initialize` with `json!({ "settings": settings })`, where
//! `settings` is a `HashMap<String, serde_json::Value>` built from the
//! plugin's declared `.tabularium` setting definitions (see
//! `RpcDriver::new` in `tabularis/src-tauri/src/plugins/driver.rs`). The host
//! silently ignores any `initialize` error or non-response, so parsing here
//! must never panic — an invalid value falls back to a safe default rather
//! than killing the handshake.
//!
//! # poolMaxSize
//!
//! Mirrors the built-in `postgres` driver's configurable pool size
//! (tabularis#681): `postgres_pool_max_size_from_value` in
//! `src-tauri/src/pool_manager.rs`. Parsed from u64/i64/string, zero/invalid
//! → default, capped at 64. Defaults to **10** — the built-in's pin — rather
//! than deadpool's `get_default_pool_max_size()` (cpu×2), restoring parity
//! for the pgBouncer use case (tabularis#71) where a small client pool is
//! essential.

use std::sync::Mutex;

/// The built-in `postgres` driver's pinned pool size, and this plugin's
/// default when `poolMaxSize` is absent/invalid. Matches
/// `DEFAULT_POSTGRES_POOL_MAX_SIZE` in `tabularis/src-tauri/src/pool_manager.rs`.
pub(crate) const DEFAULT_POOL_MAX_SIZE: usize = 10;

/// Upper bound on a user-supplied pool size — matches
/// `MAX_POSTGRES_POOL_MAX_SIZE` in `tabularis/src-tauri/src/pool_manager.rs`.
/// Caps a wildly oversized setting (e.g. 10_000) at a sane ceiling rather
/// than letting one connection target exhaust server/backend slots. This is
/// the defense-in-depth bound: no matter how many times `initialize` runs
/// or what value arrives, `pool_max_size()` can never exceed 64.
const MAX_POOL_MAX_SIZE: usize = 64;

/// Process-wide pool max size, set from the `initialize` RPC and read by
/// every `build_pool` call. `Mutex` (not `OnceLock`): the value reflects the
/// *most recent* `initialize`, matching the built-in driver's behavior of
/// reading the current config value on each pool build
/// (`get_cached_config()` in `pool_manager.rs`). The host sends
/// `initialize` exactly once at startup, so in practice this is set once —
/// but a corrected re-init must not be ignored, and `Mutex` keeps the
/// parse/clamp logic testable without process-global ordering hazards.
/// Initialized to the default so pools are correctly sized even if
/// `initialize` never arrives or is silently dropped by the host.
static POOL_MAX_SIZE: Mutex<usize> = Mutex::new(DEFAULT_POOL_MAX_SIZE);

/// Parse a `poolMaxSize` setting value into a validated pool size, ported
/// verbatim from the built-in driver's `postgres_pool_max_size_from_value`
/// (`tabularis/src-tauri/src/pool_manager.rs`).
///
/// Accepts a u64, an i64 ≥ 0, or a decimal-string parsable as u64. Zero and
/// any non-parseable value (null, bool, object, negative, garbage string)
/// fall back to [`DEFAULT_POOL_MAX_SIZE`]. Any value above
/// [`MAX_POOL_MAX_SIZE`] is clamped down to it. The ordering of the
/// `or_else` chain matters: `as_u64` is tried first (the common JSON-number
/// path), then a non-negative `as_i64`, then a string parse.
pub(crate) fn pool_max_size_from_value(value: Option<&serde_json::Value>) -> usize {
    value
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_i64().and_then(|item| u64::try_from(item).ok()))
                .or_else(|| value.as_str().and_then(|item| item.parse::<u64>().ok()))
        })
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(MAX_POOL_MAX_SIZE))
        .unwrap_or(DEFAULT_POOL_MAX_SIZE)
}

/// Store the parsed pool max size from the `initialize` RPC. Overwrites any
/// prior value — the built-in driver reads the current config on each pool
/// build, and the plugin's analog is "most recent `initialize` wins".
/// Falls back to [`DEFAULT_POOL_MAX_SIZE`] when the setting is absent, so
/// the feature degrades gracefully to parity with the built-in even if the
/// host sends no settings. The value is clamped to [`MAX_POOL_MAX_SIZE`]
/// before storage, so a poisoned/oversized input can never be retained.
pub(crate) fn set_pool_max_size(settings: &serde_json::Value) {
    let size = pool_max_size_from_value(settings.get("poolMaxSize"));
    if let Ok(mut guard) = POOL_MAX_SIZE.lock() {
        *guard = size;
    }
}

/// The pool max size to apply when building a pool. Returns the value set
/// by the most recent `initialize`, otherwise [`DEFAULT_POOL_MAX_SIZE`].
/// This is the single read-side entry point for `build_pool`. A poisoned
/// lock (impossible in practice — `set_pool_max_size` cannot panic while
/// holding the guard, since `pool_max_size_from_value` is infallible) falls
/// back to the default rather than propagating an error.
pub(crate) fn pool_max_size() -> usize {
    POOL_MAX_SIZE
        .lock()
        .map(|guard| *guard)
        .unwrap_or(DEFAULT_POOL_MAX_SIZE)
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;

#[cfg(test)]
pub(crate) mod test_support {
    use super::{set_pool_max_size, DEFAULT_POOL_MAX_SIZE};
    use serde_json::json;
    use tokio::sync::Mutex;

    /// Single shared lock across every test that touches the process-global
    /// `POOL_MAX_SIZE` — both `settings_tests` (here) and the `build_pool`
    /// max-size tests in `client_tests.rs`. Without a shared lock, the two
    /// test modules' concurrent `set_pool_max_size` calls interleave and a
    /// test observes another's value (nondeterministic failures). Async
    /// (`tokio::sync`) so the `client_tests` `#[tokio::test]`s can hold the
    /// guard across their `.await` on `build_pool_pub` without tripping
    /// clippy's `await_holding_lock` (which a std `Mutex` guard would).
    pub(crate) static POOL_MAX_SIZE_TEST_LOCK: Mutex<()> = Mutex::const_new(());

    /// Reset the global to the default under the shared lock and yield the
    /// guard, for use in `#[tokio::test]`s that hold it across `.await`.
    pub(crate) async fn lock_and_reset() -> tokio::sync::MutexGuard<'static, ()> {
        let guard = POOL_MAX_SIZE_TEST_LOCK.lock().await;
        set_pool_max_size(&json!({ "poolMaxSize": DEFAULT_POOL_MAX_SIZE }));
        guard
    }

    /// Blocking variant for sync `#[test]`s (the storage tests in
    /// `settings_tests.rs` don't `.await`, so they use this). Acquires the
    /// same shared lock via `blocking_lock`, which is valid here because
    /// those tests run on the harness thread rather than inside a runtime
    /// task (which is what makes `blocking_lock` a misuse in general).
    pub(crate) fn lock_and_reset_blocking() -> tokio::sync::MutexGuard<'static, ()> {
        let guard = POOL_MAX_SIZE_TEST_LOCK.blocking_lock();
        set_pool_max_size(&json!({ "poolMaxSize": DEFAULT_POOL_MAX_SIZE }));
        guard
    }
}
