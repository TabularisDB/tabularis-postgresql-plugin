//! Connections pinned to a host session (an editor tab).
//!
//! `execute_query_batch` already runs every statement of one batch on a
//! single pooled connection, so `BEGIN … COMMIT` inside a script works.
//! What did not work is the workflow the transaction exists for: run
//! `BEGIN`, inspect, run the changes, verify them, and only then `COMMIT`,
//! each as its own run. Between runs the connection went back to the pool,
//! so the next run could land on a different one and the transaction was
//! stranded.
//!
//! When the host sends a `session_id`, a batch that leaves a transaction
//! open keeps its connection here until the session commits, rolls back,
//! closes, or goes idle. The pool recycles with `RecyclingMethod::Fast`,
//! which resets nothing, so a pinned connection is always rolled back
//! before it goes back to the pool — otherwise the next borrower would
//! inherit the transaction and its locks.
//!
//! Each session has its own lock, held for a whole run, so two overlapping
//! calls for one session run one after the other on the same connection
//! instead of each taking a fresh one.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use deadpool_postgres::Client;
use tokio::sync::{Mutex, OwnedMutexGuard};

/// A pooled client held between batches because the session that owns it
/// left an explicit transaction open.
struct PinnedSession {
    client: Client,
    last_used: Instant,
}

/// A session's pinned connection, if any. Holding the guard serializes runs.
#[derive(Default)]
pub struct Slot(Option<PinnedSession>);

impl Slot {
    /// Take the pinned connection; the caller must [`Slot::pin`] it again or end its transaction.
    pub fn take(&mut self) -> Option<Client> {
        self.0.take().map(|s| s.client)
    }

    /// Pin `client` until the session ends its transaction.
    pub fn pin(&mut self, client: Client) {
        self.0 = Some(PinnedSession {
            client,
            last_used: Instant::now(),
        });
    }
}

/// A pinned connection holds its transaction's locks until the session ends
/// it. An abandoned session would hold them indefinitely, so one untouched
/// for this long is rolled back and released by the periodic
/// [`sweep_idle`].
const MAX_IDLE: Duration = Duration::from_secs(30 * 60);

type SessionMap = HashMap<String, Arc<Mutex<Slot>>>;

fn sessions() -> &'static Mutex<SessionMap> {
    static SESSIONS: OnceLock<Mutex<SessionMap>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// End the transaction before the client goes back to the pool.
///
/// A failing `ROLLBACK` is only logged: the connection is already unusable
/// and the pool will discard it.
pub async fn rollback_and_release(client: Client) {
    if let Err(e) = client.batch_execute("ROLLBACK").await {
        log::warn!("ROLLBACK while releasing a pinned session failed: {e}");
    }
}

/// Lock `session_id`'s slot, waiting for any run already holding it.
pub async fn lock(session_id: &str) -> OwnedMutexGuard<Slot> {
    let slot = sessions()
        .lock()
        .await
        .entry(session_id.to_string())
        .or_default()
        .clone();
    slot.lock_owned().await
}

/// Roll back and release every session idle past [`MAX_IDLE`], and forget
/// slots nothing holds. A slot in use by a run is skipped.
pub async fn sweep_idle() {
    let mut expired = Vec::new();
    {
        let now = Instant::now();
        let mut map = sessions().lock().await;
        map.retain(|_, slot| {
            let Ok(mut guard) = slot.try_lock() else {
                return true;
            };
            if guard
                .0
                .as_ref()
                .is_some_and(|s| now.duration_since(s.last_used) > MAX_IDLE)
            {
                expired.extend(guard.take());
            }
            // Only the map holds it and nothing is pinned, so nobody can be waiting on it.
            guard.0.is_some() || Arc::strong_count(slot) > 1
        });
    }

    if expired.is_empty() {
        return;
    }
    log::info!(
        "Releasing {} pinned session(s) idle for over {} minutes",
        expired.len(),
        MAX_IDLE.as_secs() / 60
    );
    for client in expired {
        rollback_and_release(client).await;
    }
}

/// Roll back and release the connection pinned to `session_id`, if any,
/// after any run still in flight for it finishes.
pub async fn release(session_id: &str) {
    let client = lock(session_id).await.take();
    if let Some(client) = client {
        log::info!("Releasing pinned session {session_id}");
        rollback_and_release(client).await;
    }
}

/// Roll back and release every pinned connection, so shutdown leaves no
/// transaction open on the server.
pub async fn release_all() {
    let slots: Vec<Arc<Mutex<Slot>>> = sessions().lock().await.drain().map(|(_, s)| s).collect();
    let mut clients = Vec::new();
    for slot in slots {
        clients.extend(slot.lock().await.take());
    }

    if clients.is_empty() {
        return;
    }
    log::info!("Releasing {} pinned session(s)", clients.len());
    for client in clients {
        rollback_and_release(client).await;
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod session_tests;
