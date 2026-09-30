//! Unit tests for the cancel-action registry's bookkeeping and lifecycle.
//!
//! `CancelGuard::register` itself (which needs a live `PgClient` to call
//! `.cancel_token()`) and `run_cancel`'s actual `pg_cancel_backend` round
//! trip need a live database connection and are left to a live-database
//! integration test (tracked in tabularis-postgresql-plugin#126). These
//! tests cover the part that doesn't need one: the registry's
//! insert/remove/trigger bookkeeping and `CancelGuard`'s drop behavior,
//! using a fake action in place of a real `CancelToken`.
//!
//! Each test uses its own request id so they can run concurrently against
//! the shared `HANDLES` map without a cross-test lock.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::*;

/// A fake `CancelAction` that records whether it ran, for tests that don't
/// have a live `CancelToken` to register.
fn recording_action() -> (CancelAction, Arc<AtomicBool>) {
    let called = Arc::new(AtomicBool::new(false));
    let flag = called.clone();
    let action: CancelAction = Box::new(move || {
        Box::pin(async move {
            flag.store(true, Ordering::SeqCst);
        })
    });
    (action, called)
}

#[tokio::test]
async fn cancel_invokes_the_registered_action_and_removes_it() {
    let (action, called) = recording_action();
    insert_action(1, action);

    cancel(1).await;

    assert!(called.load(Ordering::SeqCst));
    assert!(!HANDLES.lock().unwrap().contains_key(&1));
}

#[tokio::test]
async fn cancel_on_an_unknown_id_is_a_no_op() {
    // Must not panic even though nothing was ever registered for this id.
    cancel(999_999).await;
}

#[tokio::test]
async fn a_cancel_action_can_only_ever_be_invoked_once() {
    let (action, called) = recording_action();
    insert_action(2, action);

    cancel(2).await;
    called.store(false, Ordering::SeqCst); // reset, to detect a spurious second run
    cancel(2).await; // already removed by the first cancel() — must be a no-op

    assert!(!called.load(Ordering::SeqCst));
}

#[test]
fn dropping_a_cancel_guard_deregisters_its_action_without_invoking_it() {
    let (action, called) = recording_action();
    insert_action(3, action);
    assert!(HANDLES.lock().unwrap().contains_key(&3));

    let guard = CancelGuard { request_id: Some(3) };
    drop(guard);

    assert!(!HANDLES.lock().unwrap().contains_key(&3));
    assert!(!called.load(Ordering::SeqCst));
}

#[test]
fn a_guard_with_no_request_id_is_a_no_op_on_drop() {
    // Must not panic even though nothing was ever registered for it.
    let guard = CancelGuard { request_id: None };
    drop(guard);
}
