//! Worker-pool starvation regression tests (#167).
//!
//! Before the fix, all RPC types shared 4 workers fed by one queue, so 4
//! stuck calls (a slow query holding a worker up to the host's 120s timeout)
//! blocked every subsequent RPC — including a `ping` to a *different*
//! database and the `cancel` notification that's meant to relieve the stuck
//! workers. The fix routes `cancel` to a dedicated lane (Layer 1) and
//! `ping`/`test_connection`/`initialize`/`shutdown` to a lifecycle lane
//! (Layer 2) — so neither queues behind 4 stuck general workers.
//!
//! A worker-abort (Layer 3 — dropping the in-flight `handle_line` future on
//! cancel, to free the worker even if the server ignores `pg_cancel_backend`)
//! was considered but NOT added: it suppresses the cancelled query's `57014`
//! error response that the host reads (the `#126` contract). The
//! "server ignores `pg_cancel_backend`" gap is a documented pre-existing
//! limit matching the builtin — see `cancel::cancel`'s doc. No test here
//! exercises a worker-abort; a stuck worker frees only when the server
//! returns `57014` or the host's call timeout elapses.
//!
//! These tests spawn the real plugin binary (like `tests/shutdown.rs`) and
//! reuse its hanging-listener harness. The hung queries point at a TCP
//! listener that accepts but never answers the PostgreSQL handshake (a
//! worker blocks on the handshake read); the healthy `ping` points at the
//! live database.
//!
//! # Running
//!
//! ```bash
//! cargo build
//! POSTGRES_PLUGIN_BIN=target/debug/postgresql-plugin cargo test --test worker_starvation -- --test-threads=1
//! ```

use std::io::Write;
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The hung-query listener's port is per-test (ephemeral), and the ping
/// target is the live DB. Generous bounds: a ping past 4 hung workers should
/// return in well under the lifecycle lane's capacity, not near a timeout.
const PING_DEADLINE: Duration = Duration::from_secs(10);

/// A bare TCP listener that accepts connections and **holds each socket
/// open** without ever sending the PostgreSQL startup handshake — a worker
/// connecting to it blocks forever on the handshake read. Reused verbatim
/// from `tests/shutdown.rs` (same load-bearing "keep the socket open"
/// property; see that file's doc comment).
fn spawn_hanging_listener() -> (std::thread::JoinHandle<()>, u16) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind hanging listener");
    let port = listener.local_addr().expect("local addr").port();
    let handle = std::thread::spawn(move || {
        let mut held: Vec<std::net::TcpStream> = Vec::new();
        for s in listener.incoming().filter_map(Result::ok) {
            held.push(s);
        }
        drop(held);
    });
    (handle, port)
}

fn plugin_bin() -> String {
    std::env::var("POSTGRES_PLUGIN_BIN").expect("POSTGRES_PLUGIN_BIN must be set")
}

/// A `ping` JSON-RPC line (lifecycle lane) pointing at the live DB
/// (`pg-tabularis-test` :54320, `postgres`/`password`/`testdb`) — the
/// *healthy* target a ping must reach even while 4 general workers are stuck
/// against a different (hanging) listener.
fn ping_line_live(id: u64) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"ping\",\"params\":{{\"params\":{{\"driver\":\"postgres\",\"host\":\"127.0.0.1\",\"port\":54320,\"database\":\"testdb\",\"username\":\"postgres\",\"password\":\"password\",\"ssl_mode\":\"disable\"}}}}}}\n"
    )
}

/// An `execute_query` line (general lane) pointing at `port`. Against the
/// hanging listener this blocks on the connect handshake, holding a general
/// worker indefinitely.
fn execute_query_line(id: u64, port: u16) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"execute_query\",\"params\":{{\"params\":{{\"driver\":\"postgres\",\"host\":\"127.0.0.1\",\"port\":{port},\"database\":\"x\",\"username\":\"u\",\"password\":\"p\",\"ssl_mode\":\"disable\"}},\"query\":\"SELECT 1\"}}}}\n"
    )
}

/// A `cancel` sent as a regular JSON-RPC *request* (with a top-level `id`).
/// `handle_line` dispatches it to the same `cancel` handler the host's
/// fire-and-forget notification hits, which returns `ok_response(id, Null)` —
/// a stdout response the test can read. This is what makes the cancel-lane
/// test observable: a misrouted cancel (sent to the general lane) would queue
/// behind 4 stuck workers and never respond, where a cancel-lane cancel
/// responds immediately. No live DB is needed (the cancel is a no-op for
/// the hung queries — they have no registered `CancelGuard` — but the
/// handler still returns its response).
fn cancel_request(id: u64, request_id: u64) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"cancel\",\"params\":{{\"id\":{request_id}}}}}\n"
    )
}

/// Layer 2: a `ping`/`test_connection` to a *different* database must NOT
/// queue behind 4 stuck general queries. Before the fix all RPCs shared 4
/// workers, so 4 hung `execute_query` calls blocked a `ping` to another DB
/// until the host's 120s timeout. With the lifecycle lane, the `ping` runs
/// on its own worker and returns within `PING_DEADLINE`.
///
/// The 4 hung queries point at a hanging listener (block on the handshake
/// read); the `ping` points at the live DB (`pg-tabularis-test` :54320),
/// which responds fast. Skipped if the live DB isn't reachable.
#[test]
fn ping_to_another_db_returns_past_four_hung_general_queries() {
    if !live_db_up() {
        eprintln!("skipped: live DB at 127.0.0.1:54320 not reachable");
        return;
    }
    let (_listener, hung_port) = spawn_hanging_listener();

    let mut child = Command::new(plugin_bin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn plugin binary");
    let mut stdin = child.stdin.take().expect("plugin stdin");
    let stdout = child.stdout.take().expect("plugin stdout");

    // Saturate the 4 general workers with hung execute_query calls (ids
    // 1-4), each blocking on the hanging listener's handshake read.
    for id in 1..=4 {
        stdin
            .write_all(execute_query_line(id, hung_port).as_bytes())
            .expect("write hung execute_query");
    }
    stdin.flush().expect("flush");

    // Give the workers a moment to pick up the hung calls and block.
    std::thread::sleep(Duration::from_millis(300));

    // A ping to the live DB (a different database) — lifecycle lane. Before
    // the fix this queued behind the 4 stuck general workers and timed out;
    // with the fix it runs on the lifecycle lane and returns fast.
    let ping = ping_line_live(100);
    let start = Instant::now();
    stdin.write_all(ping.as_bytes()).expect("write ping");
    stdin.flush().expect("flush");

    let response =
        read_stdout_until_marker(stdout, "\"id\":100", PING_DEADLINE).unwrap_or_else(|| {
            // Cleanup: kill the stuck process so the test doesn't leave it hanging.
            let _ = child.kill();
            panic!(
                "ping to another DB did not return within {PING_DEADLINE:?} — the \
             lifecycle lane (#167 Layer 2) regressed: the ping queued behind \
             the 4 stuck general workers"
            )
        });
    let elapsed = start.elapsed();

    // The ping's response carries id 100 and a result (no error against the
    // live DB). Assert it came back promptly, not near the deadline.
    assert!(
        response.contains("\"result\""),
        "expected the ping's response (id 100, result), got: {response}"
    );
    assert!(
        elapsed < PING_DEADLINE,
        "ping took {elapsed:?} — expected a prompt lifecycle-lane return, not \
         near the {PING_DEADLINE:?} deadline (the lane regressed)"
    );

    // Cleanup: close stdin so the process drains and exits.
    drop(stdin);
    let _ = wait_with_deadline(&mut child, Duration::from_secs(20));
}

/// Layer 1: a `cancel` must NOT queue behind the stuck general workers it's
/// meant to relieve. Before the fix the cancel shared the same 4-worker queue,
/// so 4 stuck queries deadlocked the relief valve. With the cancel lane, the
/// cancel runs on its own worker regardless.
///
/// To make the routing observable, the cancel is sent as a regular JSON-RPC
/// *request* (with a top-level `id`), not a fire-and-forget notification: the
/// `cancel` handler returns `ok_response(id, Null)`, a stdout line the test
/// reads. If the cancel were misrouted to the general lane (the #167
/// regression), it would queue behind the 4 stuck workers and never respond —
/// the read would time out and the test would panic. A cancel-lane cancel
/// responds immediately. No live DB is needed: the cancel targets a hung
/// query with no registered `CancelGuard` (a no-op), but the handler still
/// returns its response on the cancel lane.
#[test]
fn cancel_runs_on_its_own_lane_not_behind_hung_general_workers() {
    let (_listener, hung_port) = spawn_hanging_listener();

    let mut child = Command::new(plugin_bin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn plugin binary");
    let mut stdin = child.stdin.take().expect("plugin stdin");
    let stdout = child.stdout.take().expect("plugin stdout");

    // Saturate the 4 general workers with hung execute_query calls.
    for id in 1..=4 {
        stdin
            .write_all(execute_query_line(id, hung_port).as_bytes())
            .expect("write hung execute_query");
    }
    stdin.flush().expect("flush");
    std::thread::sleep(Duration::from_millis(300));

    // A cancel sent as a request (id 200) for one of the hung queries. Before
    // the fix this queued behind the 4 stuck workers and never got a worker;
    // with the cancel lane it runs immediately and returns its response. The
    // cancel itself is a no-op for these hung queries (stuck on the connect
    // handshake with no registered `CancelGuard`, so server-side
    // `pg_cancel_backend` can't fire — the documented "server ignores cancel"
    // limit, not a regression), but the handler still returns
    // `ok_response(200, Null)` on the cancel lane, which is the observable
    // signal the cancel ran without queueing behind the stuck workers.
    let cancel = cancel_request(200, 1);
    let start = Instant::now();
    stdin.write_all(cancel.as_bytes()).expect("write cancel");
    stdin.flush().expect("flush");

    let response =
        read_stdout_until_marker(stdout, "\"id\":200", PING_DEADLINE).unwrap_or_else(|| {
            let _ = child.kill();
            panic!(
                "cancel (id 200) did not respond within {PING_DEADLINE:?} — the \
                 cancel lane (#167 Layer 1) regressed: the cancel queued behind \
                 the 4 stuck general workers instead of running on its own lane"
            )
        });
    let elapsed = start.elapsed();

    // The cancel's response carries id 200 and a null result (no error — an
    // unknown/unregistered cancel id is a no-op, never an error). Assert it
    // came back promptly, not near the deadline.
    assert!(
        response.contains("\"result\""),
        "expected the cancel's response (id 200, result), got: {response}"
    );
    assert!(
        elapsed < PING_DEADLINE,
        "cancel took {elapsed:?} — expected a prompt cancel-lane return, not \
         near the {PING_DEADLINE:?} deadline (the lane regressed)"
    );

    // Cleanup: the process has stuck workers; close stdin to trigger the
    // bounded drain (#155) and exit.
    drop(stdin);
    let status = wait_with_deadline(&mut child, Duration::from_secs(20))
        .expect("plugin did not exit after stdin EOF — the shutdown drain (#155) regressed");
    assert!(
        status.code().is_some(),
        "process killed by harness, not a clean drain exit; status: {status:?}"
    );
}

fn live_db_up() -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], 54320)),
        Duration::from_millis(500),
    )
    .is_ok()
}

/// Read stdout line-by-line until a line containing `marker` arrives, or
/// `deadline` elapses — on a blocking thread so a regressed hang surfaces as
/// a test failure (timeout) rather than a hung test. Unlike `shutdown.rs`'s
/// read-to-EOF helper, this returns as soon as the target response line
/// arrives, so a process still running (held open by stuck workers) doesn't
/// block the assertion on EOF that never comes.
fn read_stdout_until_marker(
    mut stdout: std::process::ChildStdout,
    marker: &str,
    deadline: Duration,
) -> Option<String> {
    let marker = marker.to_string();
    let handle = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(&mut stdout);
        loop {
            let mut line = String::new();
            match std::io::BufRead::read_line(&mut reader, &mut line) {
                Ok(0) => return None, // EOF
                Ok(_) => {
                    if line.contains(&marker) {
                        return Some(line);
                    }
                }
                Err(_) => return None,
            }
        }
    });
    let start = Instant::now();
    loop {
        if handle.is_finished() {
            return handle.join().ok().flatten();
        }
        if start.elapsed() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Wait for a child with a deadline, polling `try_wait`. Reused from
/// `tests/shutdown.rs`.
fn wait_with_deadline(
    child: &mut std::process::Child,
    deadline: Duration,
) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if start.elapsed() > deadline {
            let _ = child.kill();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
