//! Shutdown regression tests for the orphan-on-quit hang (#155).
//!
//! Two tests do NOT need a live database — the bug is about process lifecycle
//! when an RPC is stuck, so a TCP listener that accepts and *holds* the
//! connection open without ever answering the PostgreSQL startup handshake is
//! a sufficient stand-in for a hung pooler/server. A third test exercises a
//! real fast query against the live database and is skipped if the DB is not
//! reachable. This codifies the DB-free repro from the issue so the fix
//! (bounded drain + `abort()` in `src/main.rs`) cannot silently regress.
//!
//! Spawns the real plugin binary (like `live_db.rs`) via `POSTGRES_PLUGIN_BIN`.
//!
//! # Running
//!
//! ```bash
//! cargo build
//! POSTGRES_PLUGIN_BIN=target/debug/postgresql-plugin cargo test --test shutdown
//! ```

use std::io::Write;
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The fix's bound (`SHUTDOWN_GRACE` in `src/main.rs`) plus margin for the
/// stdin-close delay and process teardown. The test must prove the process
/// exits well under the "forever" it ran before the fix, not that it hits
/// an exact number — so this is a generous upper bound, and the elapsed
/// assertion below also checks it's not absurdly long.
const EXIT_DEADLINE: Duration = Duration::from_secs(20);

/// A bare TCP listener that accepts connections and **holds each socket
/// open** without ever sending the PostgreSQL startup handshake. This is
/// the load-bearing part: the accepted `TcpStream` is pushed into a `Vec`
/// and kept alive for the listener's lifetime so the kernel never sends
/// FIN. The plugin's connect therefore (1) completes the TCP handshake,
/// (2) writes the StartupMessage, (3) blocks forever reading a response
/// that never comes — the exact #155 hang. Dropping `stream` instead (the
/// earlier version of this helper) would close the socket immediately and
/// let the plugin's connect fail fast, which exercises the worker's error
/// path, NOT the `SHUTDOWN_GRACE` abort path — making the test a false
/// guard that passes on unfixed code.
///
/// Uses an ephemeral port (binds `127.0.0.1:0`) and returns the assigned
/// port so parallel test runs don't collide on a hardcoded one.
fn spawn_hanging_listener() -> (std::thread::JoinHandle<()>, u16) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind hanging listener");
    let port = listener.local_addr().expect("local addr").port();
    // The accept thread owns the listener; the held streams keep the
    // connections from closing. The thread runs until the process exits.
    let handle = std::thread::spawn(move || {
        let mut held: Vec<std::net::TcpStream> = Vec::new();
        for s in listener.incoming().filter_map(Result::ok) {
            // Keep the socket open — do NOT drop it. No bytes are
            // written, so the plugin's handshake read blocks forever.
            held.push(s);
        }
        drop(held);
    });
    (handle, port)
}

fn plugin_bin() -> String {
    std::env::var("POSTGRES_PLUGIN_BIN").expect("POSTGRES_PLUGIN_BIN must be set")
}

/// Build the `test_connection` JSON-RPC line pointing at the hanging listener.
fn test_connection_line(port: u16) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"test_connection\",\"params\":{{\"params\":{{\"driver\":\"postgres\",\"host\":\"127.0.0.1\",\"port\":{port},\"database\":\"x\",\"username\":\"u\",\"password\":\"p\",\"ssl_mode\":\"disable\"}}}}}}\n"
    )
}

/// Issue an RPC, then close stdin (simulating the host quitting), and
/// assert the process exits within `EXIT_DEADLINE`. Before the fix it hung
/// indefinitely (reparented to PID 1); after the fix it exits ~5s after
/// stdin closes once `SHUTDOWN_GRACE` elapses and the stuck worker is
/// `abort()`ed.
///
/// This test is the real regression guard: with the hanging listener
/// keeping the socket open, the worker blocks on the handshake read and
/// ONLY the `SHUTDOWN_GRACE` + `abort()` path lets the process exit. On
/// unfixed code this test hangs (the worker never finishes, the process
/// never exits) — verified empirically.
#[test]
fn process_exits_within_a_bound_when_an_rpc_is_in_flight_at_quit() {
    let (_listener, port) = spawn_hanging_listener();

    let mut child = Command::new(plugin_bin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn plugin binary");

    let mut stdin = child.stdin.take().expect("plugin stdin");
    stdin
        .write_all(test_connection_line(port).as_bytes())
        .expect("write test_connection request");
    stdin.flush().expect("flush");
    // Drop stdin to send EOF — the host is gone. `drop(stdin)` is enough;
    // no `sleep` needed since we've already written a full line.
    drop(stdin);

    let start = Instant::now();
    let status = wait_with_deadline(&mut child, EXIT_DEADLINE).expect(
        "plugin did not exit within the deadline — the orphan-on-quit hang (#155) regressed",
    );
    let elapsed = start.elapsed();

    assert!(
        status.code().is_some(),
        "process was killed by the test harness (no clean exit code), not by its own shutdown — \
         likely the hang regressed; status: {status:?}"
    );
    // The fix exits ~stdin-close-delay + SHUTDOWN_GRACE (~6s). A value near
    // the full 20s deadline would suggest it only barely fit and warrants
    // investigation; comfortably under it confirms the bounded drain fired.
    assert!(
        elapsed < EXIT_DEADLINE,
        "elapsed {elapsed:?} reached the deadline — expected a bounded ~6s exit, not a near-hang"
    );
}

/// Control: a plain EOF with no in-flight RPC must still exit cleanly and
/// immediately. This guards against an over-broad fix that aborts the
/// writer/cleanup tasks eagerly and breaks the clean-shutdown path. Asserts
/// a *successful* exit (`status.success()`), not just any exit code — a
/// non-zero exit on plain EOF would itself be a regression.
#[test]
fn process_exits_immediately_on_eof_with_no_in_flight_rpc() {
    let mut child = Command::new(plugin_bin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn plugin binary");
    // No request written — just close stdin immediately.
    drop(child.stdin.take());

    let status = wait_with_deadline(&mut child, Duration::from_secs(5))
        .expect("plugin did not exit quickly on a plain EOF — clean shutdown regressed");
    assert!(
        status.success(),
        "expected a clean (success) fast exit on plain EOF, got {status:?}"
    );
}

/// The fix must not abort a legitimate in-flight query that completes
/// *within* `SHUTDOWN_GRACE`. This is the unintended-consequence guard: a
/// real, fast query against the live database, issued and then followed
/// immediately by stdin EOF, must return its JSON-RPC result on stdout and
/// exit cleanly — not be cut off by the shutdown deadline. Runs against the
/// live database `live_db.rs` uses (podman `pg-tabularis-test`); skipped if
/// the DB is not reachable, so plain `cargo test` doesn't spuriously fail
/// on a machine without it.
#[test]
fn a_fast_in_flight_query_completes_and_returns_before_the_shutdown_deadline() {
    // Skip cleanly if the live DB isn't up — this test requires it, but
    // `cargo test` with no filter runs all integration tests. A missing DB
    // is an environment condition, not a regression.
    if std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], 54320)),
        Duration::from_millis(500),
    )
    .is_err()
    {
        eprintln!("skipped: live DB at 127.0.0.1:54320 not reachable");
        return;
    }

    let mut child = Command::new(plugin_bin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn plugin binary");
    let mut stdin = child.stdin.take().expect("plugin stdin");
    let stdout = child.stdout.take().expect("plugin stdout");

    // A real query against the live DB — fast, well under SHUTDOWN_GRACE.
    let request =
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"execute_query\",\"params\":{\"params\":{\"driver\":\"postgres\",\"host\":\"127.0.0.1\",\"port\":54320,\"database\":\"testdb\",\"username\":\"postgres\",\"password\":\"password\",\"ssl_mode\":\"disable\"},\"query\":\"SELECT 1 AS ok\"}}\n";
    stdin
        .write_all(request.as_bytes())
        .expect("write execute_query request");
    stdin.flush().expect("flush");
    // Close stdin right away: the query is in flight, EOF arrives while it
    // is still running. This is exactly the shape #155 reports — except the
    // query succeeds fast, so it must finish before the grace aborts it.
    drop(stdin);

    // Read the response on a bounded thread (NOT a blocking `read_to_string`
    // on the test thread, which would hang forever if the fix regressed).
    // The plugin writes one JSON-RPC line then exits.
    let response = read_stdout_bounded(stdout, Duration::from_secs(15))
        .expect("plugin did not produce a response line within the deadline");

    assert!(
        response.contains("\"ok\"") && response.contains("\"result\""),
        "expected the query result on stdout before exit, got: {response}"
    );

    let status = wait_with_deadline(&mut child, Duration::from_secs(20))
        .expect("plugin did not exit after the fast query completed — the drain regressed");
    assert!(
        status.code().is_some(),
        "process killed by harness, not a clean exit; status: {status:?}"
    );
}

/// Read stdout until EOF or `deadline` elapses, on a blocking thread so a
/// regressed hang surfaces as a test failure (timeout) rather than a hung
/// test. `read_to_string` blocks until EOF; running it on a joinable thread
/// bounded by a polling loop means a process that never exits still fails
/// the test instead of hanging it. Returns the bytes read, or `None` on
/// timeout.
fn read_stdout_bounded(
    mut stdout: std::process::ChildStdout,
    deadline: Duration,
) -> Option<String> {
    let handle = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut buf);
        buf
    });
    let start = Instant::now();
    loop {
        if handle.is_finished() {
            return handle.join().ok();
        }
        if start.elapsed() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Wait for a child with a deadline, polling `try_wait` so the test itself
/// never blocks forever on a regressed hang. Returns the exit status.
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
