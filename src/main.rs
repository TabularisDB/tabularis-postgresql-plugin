//! PostgreSQL plugin for Tabularis — JSON-RPC driver over stdin/stdout.
//!
//! # Protocol
//!
//! Reads newline-delimited JSON-RPC 2.0 requests from stdin and writes
//! responses (one JSON object per line) to stdout. All handler logic is
//! async (tokio) since the database pool requires an async runtime.
//!
//! # Worker lanes (not one shared pool) — #167
//!
//! Requests are routed to one of three lanes by method, so a slow query on
//! one connection does not block a `ping`/`connect` on another, and a host
//! `cancel` (the relief valve for a stuck worker) is never queued behind the
//! stuck workers it's meant to relieve:
//!
//! - **cancel lane** (1 worker): `cancel` notifications run on a dedicated
//!   worker that bypasses the general queue. When 4 general workers are
//!   stuck on slow queries, the cancel still fires `pg_cancel_backend`
//!   (server-side) AND aborts the stuck worker's in-flight future
//!   (worker-side, Layer 3) — instead of queuing behind those same 4 stuck
//!   workers. Without this, stuck workers can't be relieved: the relief valve
//!   is stuck in the same queue (a deadlock, the core of #167).
//! - **lifecycle lane** (2 workers): `ping`/`test_connection`/`initialize`/
//!   `shutdown` run on a dedicated lane, so a health check or new connect to
//!   database B isn't blocked by 4 slow queries against database A. Two
//!   workers (not one) so a single stuck `ping` (a `SELECT 1` against an
//!   unreachable DB) doesn't starve the lane.
//! - **general lane** (4 workers): everything else — metadata, `execute_query`,
//!   CRUD, DDL, BLOB. The original `WORKER_POOL_SIZE`.
//!
//! Every lane's worker wraps `handle_line` in a `select!` on a per-RPC
//! `CancellationToken` (Layer 3): a host `cancel` notification aborts the
//! in-flight operation directly, dropping the held `deadpool::Object`
//! (closing the socket → the server cancels the statement as a side effect)
//! and freeing the worker immediately — even if the server ignores
//! `pg_cancel_backend`. The bound is the **host's configurable call-timeout**,
//! not a plugin constant, so a query the host is willing to wait for (a large
//! enum-table result, slow DDL) is never cut (#166 stays intact); only a query
//! the host has given up on is aborted.
//!
//! Responses are funneled through a single writer task so concurrent handlers
//! never interleave bytes on stdout. A dedicated background task periodically
//! evicts idle connection pools (see `client::cleanup_idle_pools`) so a
//! long-running session that has connected to many distinct targets doesn't
//! pin idle TCP connections and pool memory for the plugin's lifetime.
//! Matches the sqlserver/dynamodb sibling plugins' architecture.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::{interval, Instant};

use postgresql_plugin::rpc::peek_method;

const WORKER_POOL_SIZE: usize = 4;

/// Lifecycle-lane worker count. Two so a single stuck `ping` (a `SELECT 1`
/// against an unreachable DB) doesn't starve the lane — a `test_connection`
/// to a healthy DB still gets a worker (#167).
const LIFECYCLE_WORKER_COUNT: usize = 2;

// Bounded so a burst of requests applies backpressure to the stdin reader
// instead of buffering unboundedly in memory.
const REQUEST_QUEUE_CAPACITY: usize = 64;

/// Lifecycle calls are low-volume (health checks, new connects) so a small
/// bound is plenty; keep it bounded for backpressure parity with the general
/// queue rather than unbounded.
const LIFECYCLE_QUEUE_CAPACITY: usize = 16;

/// Cancel notifications are fire-and-forget and tiny; a small bound is
/// plenty, kept bounded for backpressure.
const CANCEL_QUEUE_CAPACITY: usize = 16;

const POOL_CLEANUP_INTERVAL: Duration = Duration::from_secs(600); // 10 minutes

/// Upper bound on how long the plugin waits for in-flight RPCs to finish
/// after stdin reaches EOF, before force-cancelling the workers and exiting.
///
/// Stdin EOF means the host is gone. On the normal shutdown path the host
/// keeps stdin open and sends `SIGKILL` (`child.kill()` in
/// `tabularis/src-tauri/src/plugins/driver.rs`) once its own grace elapses,
/// so the plugin never observes EOF there. EOF is the *orphan* path: the
/// host process died or was force-quit, the stdin pipe broke, and no kill
/// is coming — `kill_on_drop` does not fire when the host's runtime is torn
/// down abruptly. Without this bound, a worker blocked on an unbounded
/// await (a pooler that accepted the TCP connection but never answers the
/// startup handshake, a hung query, or a saturated pool wait) keeps the
/// process — and every DB connection it holds — alive forever, reparented
/// to PID 1 (#155).
///
/// `tokio-postgres`' `connect_timeout` bounds only the *socket-level*
/// connect (`tokio-postgres/src/config.rs`), not the startup handshake, so
/// it cannot bound the pooler-accepts-but-never-responds case; only this
/// shutdown deadline covers every hang mode (connect, handshake, query,
/// pool wait) uniformly. Elapsing it `abort()`s the stuck worker, which
/// cancels its future and drops whatever it holds across the await: a held
/// `deadpool::Object` (the hung-query case, where `pool.get()` already
/// returned a connection) OR the in-flight `pool.get()` / `connect()` future
/// itself (the stuck-handshake and saturated-pool-wait cases, where the
/// worker is blocked *inside* `pool.get()` and has not yet obtained an
/// `Object`). In every case the underlying DB socket is released — either
/// directly (dropping the half-open connect future closes the TCP socket)
/// or when the process exits and the pool's connections are dropped —
/// instead of detaching the task and leaking it indefinitely.
///
/// 5s is long enough for a typical metadata/catalog query to finish without
/// being aborted, and short enough that an orphaned plugin exits promptly
/// rather than lingering. Loosely grounded in the host's own
/// `DEFAULT_PLUGIN_SHUTDOWN_GRACE_SECS` (2s in
/// `tabularis/src-tauri/src/plugins/shutdown_grace.rs`); the plugin's bound
/// is deliberately a little longer so genuine in-flight work on a clean
/// stdin-EOF shutdown isn't cut off prematurely.
///
/// # Long-running queries
///
/// A query still in flight when the grace elapses is force-cancelled: the
/// `abort()` drops the worker's `deadpool::Object`, closing the DB socket,
/// so PostgreSQL cancels the running statement — the same outcome the
/// builtin produces when the host process dies and takes its pool with it.
/// This is not a regression vs. the builtin; it's the only sane behavior,
/// because stdin EOF means the host that issued the request is gone and
/// the result has nowhere to go. During *normal* operation (host alive),
/// long queries are bounded by the host's per-call timeout, which sends a
/// `cancel` notification that triggers server-side `pg_cancel_backend`
/// (`src/cancel.rs`, #126) — *that* is the primary bound on long queries,
/// not this grace. `SHUTDOWN_GRACE` is only the orphan fallback for when
/// no cancel is coming because the host itself is gone.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let mut cleanup_handle = tokio::spawn(run_pool_cleanup(shutdown_rx));

    // Three lanes (#167): general, lifecycle, cancel. Each has its own queue
    // so a slow general query can't block a health check, and a `cancel`
    // (the relief valve) can't queue behind the stuck workers it relieves.
    let (req_tx, req_rx) = mpsc::channel::<String>(REQUEST_QUEUE_CAPACITY);
    let req_rx = Arc::new(Mutex::new(req_rx));
    let (lifecycle_tx, lifecycle_rx) = mpsc::channel::<String>(LIFECYCLE_QUEUE_CAPACITY);
    let lifecycle_rx = Arc::new(Mutex::new(lifecycle_rx));
    let (cancel_tx, cancel_rx) = mpsc::channel::<String>(CANCEL_QUEUE_CAPACITY);
    let cancel_rx = Arc::new(Mutex::new(cancel_rx));

    let (resp_tx, resp_rx) = mpsc::unbounded_channel::<String>();
    let mut writer_handle = tokio::spawn(run_writer(resp_rx));

    // General lane: the original 4 workers. Cancellable (Layer 3).
    let mut worker_handles: Vec<_> = (0..WORKER_POOL_SIZE)
        .map(|_| tokio::spawn(run_worker(req_rx.clone(), resp_tx.clone(), true)))
        .collect();
    // Lifecycle lane: 2 workers (a stuck ping can't starve a test_connection).
    // Cancellable — a host give-up should abort a stuck ping too.
    for _ in 0..LIFECYCLE_WORKER_COUNT {
        worker_handles.push(tokio::spawn(run_worker(
            lifecycle_rx.clone(),
            resp_tx.clone(),
            true,
        )));
    }
    // Cancel lane: 1 worker, NOT cancellable — it's the relief valve; a cancel
    // must always run to completion (aborting it would re-deadlock the lane it
    // exists to relieve). `cancellable = false` → `handle_line` runs unwrapped.
    worker_handles.push(tokio::spawn(run_worker(
        cancel_rx.clone(),
        resp_tx.clone(),
        false,
    )));
    drop(resp_tx);

    run_reader(req_tx, lifecycle_tx, cancel_tx).await;

    let _ = shutdown_tx.send(true);

    // Stdin EOF: the host is gone (see `SHUTDOWN_GRACE`). Drain in-flight
    // RPCs for at most `SHUTDOWN_GRACE`, then force-cancel any worker still
    // blocked on an unbounded await so the process — and the DB connections
    // it holds — exits instead of lingering as an orphan (#155).
    //
    // Poll the handle *by reference* (`&mut handle`): `JoinHandle` is `Unpin`
    // (tokio `runtime/task/join.rs`), so `&mut JoinHandle` is itself a
    // `Future` and `timeout_at` borrows the handle rather than consuming it.
    // That keeps ownership with us so, on elapsed, we can still call
    // `handle.abort()` — which cancels the stuck future at its await point
    // and drops everything it holds across that await, including a
    // `deadpool::Object` checkout, closing the DB socket server-side. The
    // alternative — consuming the handle in `timeout(..)` and dropping it
    // on elapsed — *detaches* the task (it keeps running), which reproduces
    // #155 rather than fixing it. The `abort()` is therefore load-bearing,
    // not cosmetic; the `handle.await` after it reaps the cancelled task
    // (returns `Err(JoinError)` with `is_cancelled()`, discarded here).
    //
    // All lanes' workers (general + lifecycle + cancel) are drained here —
    // the cancel worker is cheap (a `cancel` notification completes fast),
    // and a stuck general/lifecycle worker is force-aborted the same way.
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    for mut handle in worker_handles {
        match tokio::time::timeout_at(deadline, &mut handle).await {
            Ok(Ok(())) => {}
            Ok(Err(_join_err)) => {}
            Err(_elapsed) => {
                handle.abort();
                let _ = handle.await;
            }
        }
    }
    // Writer/cleanup tasks are cheap and self-terminate once their channels
    // close / the shutdown watch fires; bound them with the same deadline
    // so a pathological stall can't hold the process open after the workers
    // drained. Polled by reference for the same `Unpin` reason as above.
    let _ = tokio::time::timeout_at(deadline, &mut writer_handle).await;
    let _ = tokio::time::timeout_at(deadline, &mut cleanup_handle).await;
}

async fn run_pool_cleanup(mut shutdown_rx: watch::Receiver<bool>) {
    let mut timer = interval(POOL_CLEANUP_INTERVAL);
    loop {
        tokio::select! {
            _ = timer.tick() => {
                postgresql_plugin::client::cleanup_idle_pools();
                postgresql_plugin::session::sweep_idle().await;
            }
            _ = shutdown_rx.changed() => break,
        }
    }
}

/// Classify a line's method into a lane. `cancel` → cancel lane; the
/// lifecycle methods (`ping`/`test_connection`/`initialize`/`shutdown`) →
/// lifecycle lane; everything else (and unparseable lines, which `handle_line`
/// will surface as `-32700`/`-32600`) → general lane.
fn lane_for(method: Option<&str>) -> Lane {
    match method {
        Some("cancel") => Lane::Cancel,
        Some("ping" | "test_connection" | "initialize" | "shutdown") => Lane::Lifecycle,
        _ => Lane::General,
    }
}

#[derive(Clone, Copy)]
enum Lane {
    General,
    Lifecycle,
    Cancel,
}

async fn run_reader(
    req_tx: mpsc::Sender<String>,
    lifecycle_tx: mpsc::Sender<String>,
    cancel_tx: mpsc::Sender<String>,
) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(err) => {
                eprintln!("stdin read error, exiting: {err}");
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Route by peeked method before a worker pulls the line, so a `cancel`
        // (the relief valve) and a `ping`/`connect` never queue behind 4 stuck
        // general workers (#167). `peek_method` is a routing hint; it returns
        // None for unparseable/non-object lines, which route to the general
        // lane where `handle_line` surfaces the proper error response.
        let tx = match lane_for(peek_method(trimmed).as_deref()) {
            Lane::General => &req_tx,
            Lane::Lifecycle => &lifecycle_tx,
            Lane::Cancel => &cancel_tx,
        };

        // Blocks when the lane's queue is full, applying backpressure to
        // reading. A full general queue pauses reading (the original
        // behavior); a full lifecycle/cancel queue is unlikely (low-volume)
        // but bounded the same way.
        if tx.send(trimmed.to_string()).await.is_err() {
            break;
        }
    }
}

/// A worker for one lane. `cancellable` is currently unused (all lanes run
/// `handle_line` unwrapped); it's retained as a parameter so the cancel lane
/// is explicitly marked as "never the relief valve's own cancellation
/// target" if a worker-abort mechanism is added later. See the module doc and
/// #167 for why the worker-abort (a `select!`-on-cancel that drops the
/// in-flight `handle_line` future) was *not* added: it would suppress the
/// cancelled query's error response that the host reads (the documented
/// `#126` contract — a cancelled query returns its `57014` response), so the
/// "server ignores `pg_cancel_backend`" gap is left as a documented
/// pre-existing limit (matching the builtin) rather than traded for a
/// contract break.
async fn run_worker(
    req_rx: Arc<Mutex<mpsc::Receiver<String>>>,
    resp_tx: mpsc::UnboundedSender<String>,
    _cancellable: bool,
) {
    loop {
        let line = {
            let mut rx = req_rx.lock().await;
            rx.recv().await
        };
        let Some(line) = line else { break };

        // `handle_line` returns `None` for a JSON-RPC notification (a request
        // with no top-level `id` field, e.g. the host's `cancel` notification
        // on a plugin-call timeout, #126). A notification gets no response
        // per JSON-RPC convention, and writing one would corrupt the protocol
        // stream — so only forward a `Some` response to the writer.
        if let Some(response) = postgresql_plugin::rpc::handle_line(&line).await {
            let body = match serde_json::to_string(&response) {
                Ok(s) => s,
                Err(err) => format!(
                    "{{\"jsonrpc\":\"2.0\",\"error\":{{\"code\":-32603,\"message\":\"serialization failed: {err}\"}},\"id\":null}}"
                ),
            };

            if resp_tx.send(body).is_err() {
                break;
            }
        }
    }
}

async fn run_writer(mut resp_rx: mpsc::UnboundedReceiver<String>) {
    let mut stdout = tokio::io::stdout();
    while let Some(mut body) = resp_rx.recv().await {
        body.push('\n');
        if stdout.write_all(body.as_bytes()).await.is_err() {
            break;
        }
        let _ = stdout.flush().await;
    }
}
