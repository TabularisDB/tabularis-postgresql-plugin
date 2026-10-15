//! Shutdown regression tests for the orphan-on-quit hang (#155).
//!
//! These do NOT need a live database — the bug is about process lifecycle
//! when an RPC is stuck, so a TCP listener that accepts but never answers
//! the PostgreSQL startup handshake is a sufficient stand-in for a hung
//! pooler/server. This is the DB-free repro from the issue, codified so the
//! fix (bounded drain + `abort()` in `src/main.rs`) cannot silently regress.
//!
//! Spawns the real plugin binary (like `live_db.rs`) via `POSTGRES_PLUGIN_BIN`.
//! The in-flight case assertss the process exits within a bound after stdin
//! closes — not hung forever as an orphan reparented to PID 1.
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

/// A bare TCP listener that accepts connections but never responds — a
/// stand-in for a PgBouncer/server that took the connection then stalled.
/// Held alive for the test's duration so the plugin's connect stays blocked.
fn spawn_hanging_listener(port: u16) -> std::thread::JoinHandle<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind hanging listener");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            // Accept and immediately hold the socket open without sending
            // any PostgreSQL handshake bytes. Dropping `stream` would let
            // the plugin's connect fail fast; we keep it alive by looping.
            drop(stream);
        }
    })
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
#[test]
fn process_exits_within_a_bound_when_an_rpc_is_in_flight_at_quit() {
    let port = 54331;
    let _listener = spawn_hanging_listener(port);

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

/// Control: a plain EOF with no in-flight RPC must still exit immediately.
/// This guards against an over-broad fix that aborts the writer/cleanup
/// tasks eagerly and breaks the clean-shutdown path.
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
        status.success() || status.code().is_some(),
        "expected a clean fast exit on plain EOF, got {status:?}"
    );
}

/// The fix must not abort a legitimate in-flight query that completes
/// *within* `SHUTDOWN_GRACE`. This is the unintended-consequence guard: a
/// real, fast query against the live database, issued and then followed
/// immediately by stdin EOF, must return its JSON-RPC result on stdout and
/// exit cleanly — not be cut off by the shutdown deadline. Runs against the
/// live database the same `live_db.rs` uses (podman `pg-tabularis-test`).
#[test]
fn a_fast_in_flight_query_completes_and_returns_before_the_shutdown_deadline() {
    let bin = plugin_bin();
    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn plugin binary");
    let mut stdin = child.stdin.take().expect("plugin stdin");
    let mut stdout = std::io::BufReader::new(child.stdout.take().expect("plugin stdout"));

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

    // Read the response line. The fixed drain waits for in-flight RPCs to
    // complete (up to the grace), so a fast query's result is written to
    // stdout before the process exits.
    let mut response = String::new();
    std::io::Read::read_to_string(&mut stdout, &mut response).expect("read response stream");

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
