//! Registry of in-flight query cancel actions, keyed by JSON-RPC request id.
//!
//! Populated by the query handlers (`execute_query`, `execute_query_batch`,
//! `explain_query`) before running a statement, and drained by [`CancelGuard`]
//! when the statement finishes — success, error, or an early `?` — so the
//! map never grows unbounded. Consulted by the `cancel` notification handler
//! (`handlers::query::cancel`) to trigger a server-side `pg_cancel_backend`
//! for a specific in-flight call.
//!
//! # Protocol
//!
//! The host owns *when* to give up on a call (its configurable call
//! timeout — see `tabularis`'s `plugins/call_timeout.rs`, tabularis#833);
//! this plugin owns *how* to actually stop the backend statement, since only
//! the driver knows its database's cancel mechanism. On timeout the host
//! sends a fire-and-forget `cancel` notification carrying the timed-out
//! call's request id; `rpc::handle_line` never replies to it. See
//! `tabularis#832` / `tabularis-postgresql-plugin#126` for the full design
//! discussion.
//!
//! The registered action is boxed behind [`CancelAction`] rather than
//! storing a raw `tokio_postgres::CancelToken` directly, so the registry's
//! own bookkeeping (insert/remove/trigger, and `CancelGuard`'s drop
//! behavior) can be unit-tested with a fake action — a real `CancelToken`
//! can only be obtained from a live `Client`, which needs a live database
//! connection. See `cancel_tests.rs`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{LazyLock, Mutex};

use deadpool_postgres::Object as PgClient;
use tokio_postgres::CancelToken;

use crate::client;
use crate::models::ConnectionParams;

/// A registered in-flight statement's one-shot cancel action.
type CancelAction = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

static HANDLES: LazyLock<Mutex<HashMap<u64, CancelAction>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn insert_action(request_id: u64, action: CancelAction) {
    if let Ok(mut handles) = HANDLES.lock() {
        handles.insert(request_id, action);
    }
}

fn remove_action(request_id: u64) {
    if let Ok(mut handles) = HANDLES.lock() {
        handles.remove(&request_id);
    }
}

/// RAII guard: registers a cancel action for `request_id` (if `Some`) on
/// construction, deregisters it on drop. A `None` `request_id` (should not
/// happen with a real host, which always sends a numeric `id`, but this
/// stays defensive rather than panicking on an unexpected shape) is a no-op
/// guard.
pub struct CancelGuard {
    request_id: Option<u64>,
}

impl CancelGuard {
    /// Register a cancel action for `pg_client`'s current backend, keyed by
    /// `request_id`. The action is built eagerly from the live client and
    /// connection params so `cancel()` never needs to touch the pool or
    /// rebuild connection state later — it only has to run the action
    /// that's already sitting in the map.
    pub fn register(request_id: Option<u64>, pg_client: &PgClient, params: ConnectionParams) -> Self {
        if let Some(id) = request_id {
            let token = pg_client.cancel_token();
            insert_action(id, Box::new(move || Box::pin(run_cancel(token, params))));
        }
        Self { request_id }
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if let Some(id) = self.request_id {
            remove_action(id);
        }
    }
}

/// Send `pg_cancel_backend` for `token`'s connection, using the same
/// TLS-vs-plaintext choice the original pool made for `params` (see
/// `client::needs_tls`/`client::make_tls_connect`). Errors are logged and
/// swallowed: this runs from a fire-and-forget notification handler with no
/// response channel back to the host, and the server never reports whether
/// a cancellation attempt actually landed anyway (see
/// `tokio_postgres::CancelToken::cancel_query`'s own doc comment).
async fn run_cancel(token: CancelToken, params: ConnectionParams) {
    let result = if client::needs_tls(&params) {
        match client::make_tls_connect(&params) {
            Ok(tls) => token.cancel_query(tls).await,
            Err(e) => {
                log::warn!("cancel: failed to build TLS connector: {e}");
                return;
            }
        }
    } else {
        token.cancel_query(tokio_postgres::NoTls).await
    };

    if let Err(e) = result {
        log::warn!(
            "cancel: pg_cancel_backend request failed: {}",
            client::format_pg_error(&e)
        );
    }
}

/// Trigger the registered cancel action for `request_id`, if one is still
/// registered. Unknown ids (already finished, already cancelled — the
/// action is one-shot and removed on trigger — or no cancel action was ever
/// registered, e.g. a metadata call, which doesn't run through
/// `CancelGuard`) are a no-op: there is nothing meaningful to report back
/// either way.
pub async fn cancel(request_id: u64) {
    let action = match HANDLES.lock() {
        Ok(mut handles) => handles.remove(&request_id),
        Err(_) => return,
    };
    if let Some(action) = action {
        action().await;
    }
}

#[cfg(test)]
#[path = "cancel_tests.rs"]
mod cancel_tests;
