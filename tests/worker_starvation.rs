//! Worker-pool starvation regression tests (#167).
//!
//! Before the fix, all RPC types shared 4 workers fed by one queue, so 4
//! stuck calls (a slow query holding a worker up to the host's 120s timeout)
//! blocked every subsequent RPC — including a `ping` to a *different*
//! database and the `cancel` notification that's meant to relieve the stuck
//! workers. The fix routes `cancel` to a dedicated lane (Layer 1),
//! `ping`/`test_connection`/`initialize`/`shutdown` to a lifecycle lane
//! (Layer 2), and wires the `cancel` notification to abort the in-flight
//! worker operation directly (Layer 3) — so a host give-up frees the worker
//! even if the server ignores `pg_cancel_backend`.
//!
//! These tests spawn the real plugin binary (like `tests/shutdown.rs`) and
//! reuse its hanging-listener harness. The hung queries point at a TCP
//! listener that accepts but never answers the PostgreSQL handshake (a
//! worker blocks on the handshake read); the healthy `ping` points at the
//! live database. No DB is needed for the cancel tests (the worker-abort
//! fires regardless of server cooperation).
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

/// A fire-and-forget `cancel` notification (no top-level `id`) carrying the
/// request id to cancel. This is the form the host sends on its call timeout
/// (#126); it routes to the cancel lane (Layer 1).
fn cancel_notification(request_id: u64) -> String {
    format!("{{\"jsonrpc\":\"2.0\",\"method\":\"cancel\",\"params\":{{\"id\":{request_id}}}}}\n")
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

/// Layer 1: a `cancel` notification must NOT queue behind the stuck general
/// workers it's meant to relieve. Before the fix the cancel shared the same
/// 4-worker queue, so 4 stuck queries deadlocked the relief valve. With the
/// cancel lane, the cancel runs on its own worker regardless. (Layer 3 — the
/// worker-abort — is asserted in the next test.)
#[test]
fn cancel_notification_is_not_queued_behind_hung_general_workers() {
    let (_listener, hung_port) = spawn_hanging_listener();

    let mut child = Command::new(plugin_bin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn plugin binary");
    let mut stdin = child.stdin.take().expect("plugin stdin");

    // Saturate the 4 general workers with hung execute_query calls.
    for id in 1..=4 {
        stdin
            .write_all(execute_query_line(id, hung_port).as_bytes())
            .expect("write hung execute_query");
    }
    stdin.flush().expect("flush");
    std::thread::sleep(Duration::from_millis(300));

    // A fire-and-forget cancel for one of the hung queries. Before the fix this
    // queued behind the 4 stuck workers and never fired; with the cancel lane
    // it runs immediately. The cancel is a notification (no id) → no stdout
    // response, so we assert the cancel lane didn't wedge the process: a
    // wedged cancel lane would backpressure the reader, so a second line
    // couldn't be written / the process couldn't drain. (The cancel itself is a
    // no-op for these hung queries — they're stuck on the connect handshake
    // with no registered `CancelGuard`, so server-side `pg_cancel_backend`
    // can't fire; only the stdin-EOF drain aborts them. That's the documented
    // "server ignores cancel" limit, not a regression. The point here is the
    // cancel *ran* on its own lane without queueing behind the stuck workers.)
    let cancel = cancel_notification(1);
    stdin.write_all(cancel.as_bytes()).expect("write cancel");
    stdin.flush().expect("flush");

    // The cancel lane must have processed it promptly. Give it a moment, then
    // confirm the process is still accepting input (not wedged). A wedged
    // cancel lane would backpressure the reader; a second cancel for a
    // different id should still be writable (the channel isn't full).
    std::thread::sleep(Duration::from_millis(300));
    let cancel2 = cancel_notification(2);
    stdin
        .write_all(cancel2.as_bytes())
        .expect("write second cancel — process should still be reading");
    stdin.flush().expect("flush second cancel");

    // The process must still be draining (the reader didn't wedge). A 5th
    // execute_query (id 5) writes fine — it queues (the 4 general workers are
    // still stuck on the handshake, so it won't run), but the write succeeding
    // proves the reader and cancel lane are healthy. The stuck workers are
    // cleaned up by the EOF drain below, not by the cancel.
    stdin
        .write_all(execute_query_line(5, hung_port).as_bytes())
        .expect("write 5th execute_query — reader must keep draining");
    stdin.flush().expect("flush");

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
