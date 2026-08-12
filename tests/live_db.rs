//! Live-database integration test — a self-contained smoke test that
//! actually talks to a real PostgreSQL instance, unlike every other test in
//! this crate (all pure `#[cfg(test)]` unit tests, see `.rules/rust.md`
//! #4/#5). Closes the biggest gap in this repo's own CI: nothing here
//! previously verified the built binary against a live database
//! automatically — that only ever happened via a manual cross-repo parity
//! check against `tabularis`'s test suite.
//!
//! This is deliberately NOT the cross-repo 82-test parity suite (that stays
//! a manual/periodic check against `tabularis`, per the "Repo Extraction"
//! open question in `docs/planning/02-phase-1-plugin-build.md`). It's a
//! small self-check covering connect, a basic query, an insert, and the two
//! handlers found completely uncovered during the security-audit pass this
//! migration did (`startup_script`, `connection_string`).
//!
//! # Running locally
//!
//! Point `POSTGRES_PLUGIN_BIN` at a debug build and run against any
//! PostgreSQL 16 instance (defaults below match this session's local
//! Podman container: `postgres:16`, user `postgres`, password `password`,
//! db `testdb`, port `54320`):
//!
//! ```bash
//! cargo build
//! POSTGRES_PLUGIN_BIN=target/debug/postgresql-plugin cargo test --test live_db -- --test-threads=1
//! ```

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

use serde_json::{json, Value};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn conn_params() -> Value {
    json!({
        "host": env_or("PGHOST", "127.0.0.1"),
        "port": env_or("PGPORT", "54320").parse::<u16>().expect("PGPORT must be a valid port"),
        "username": env_or("PGUSER", "postgres"),
        "password": env_or("PGPASSWORD", "password"),
        "database": env_or("PGDATABASE", "testdb"),
    })
}

/// A running plugin process, driven over its stdin/stdout exactly like a
/// real host would — same shape as the manual JSON-RPC smoke tests run
/// throughout this migration.
struct Plugin {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Plugin {
    fn spawn() -> Self {
        let bin = std::env::var("POSTGRES_PLUGIN_BIN").expect("POSTGRES_PLUGIN_BIN must be set");
        let mut child = Command::new(bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn plugin binary");
        let stdin = child.stdin.take().expect("no stdin");
        let stdout = BufReader::new(child.stdout.take().expect("no stdout"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    /// Send one JSON-RPC request and return its parsed response.
    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": id,
        });
        let mut line = serde_json::to_string(&request).expect("serialize request");
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .expect("write to plugin stdin");
        self.stdin.flush().expect("flush plugin stdin");

        let mut response_line = String::new();
        self.stdout
            .read_line(&mut response_line)
            .expect("read from plugin stdout");
        let response: Value =
            serde_json::from_str(response_line.trim()).expect("parse JSON-RPC response");
        assert_eq!(
            response.get("id").and_then(Value::as_u64),
            Some(id),
            "response id must match the request that produced it"
        );
        response
    }

    /// Call and assert the response carries a `result`, not an `error`.
    fn call_ok(&mut self, method: &str, params: Value) -> Value {
        let response = self.call(method, params);
        assert!(
            response.get("error").is_none(),
            "{method} returned an error: {:?}",
            response.get("error")
        );
        response
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("{method} returned neither result nor error"))
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn test_connection_succeeds_against_live_database() {
    let mut plugin = Plugin::spawn();
    plugin.call_ok("test_connection", json!({ "params": conn_params() }));
}

#[test]
fn execute_query_returns_rows_from_live_database() {
    let mut plugin = Plugin::spawn();
    let result = plugin.call_ok(
        "execute_query",
        json!({ "params": conn_params(), "query": "SELECT 1 AS one" }),
    );
    let rows = result
        .get("rows")
        .and_then(Value::as_array)
        .expect("rows array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], json!(1));
}

#[test]
fn insert_record_persists_a_row() {
    let mut plugin = Plugin::spawn();
    let params = conn_params();

    // Self-contained: create (and reset) our own scratch table rather than
    // depending on tabularis's seed fixtures, since this test must not
    // require anything outside this repo.
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "CREATE TABLE IF NOT EXISTS live_db_test_scratch \
                       (id SERIAL PRIMARY KEY, name TEXT, value INTEGER)",
        }),
    );
    plugin.call_ok(
        "execute_query",
        json!({ "params": params, "query": "TRUNCATE live_db_test_scratch RESTART IDENTITY" }),
    );

    let affected = plugin.call_ok(
        "insert_record",
        json!({
            "params": params,
            "table": "live_db_test_scratch",
            "schema": "public",
            "data": { "name": "smoke-test", "value": 42 },
        }),
    );
    assert_eq!(affected, json!(1), "insert should affect exactly one row");

    let result = plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "SELECT name, value FROM live_db_test_scratch",
        }),
    );
    let rows = result.get("rows").and_then(Value::as_array).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], json!(["smoke-test", 42]));
}

#[test]
fn execute_query_returns_a_real_enum_value_not_null() {
    let mut plugin = Plugin::spawn();
    let params = conn_params();

    // Relies on `test_schema.with_enum` / `test_schema.mood`, seeded by
    // tabularis's tests/fixtures/postgres_seed.sh against this same
    // container — see GitHub issue #7, where this exact query returned
    // `null` for a non-null enum column.
    let result = plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "SELECT id, current_mood FROM test_schema.with_enum WHERE id = 1",
            "schema": "test_schema",
        }),
    );
    let rows = result.get("rows").and_then(Value::as_array).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][1],
        json!("happy"),
        "a non-null enum column must round-trip as its label string, not null"
    );

    // The genuinely-NULL case must still come back as null, not as the
    // fix's happy-path string.
    let null_result = plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "SELECT id, NULL::test_schema.mood AS current_mood \
                       FROM test_schema.with_enum WHERE id = 1",
            "schema": "test_schema",
        }),
    );
    let null_rows = null_result.get("rows").and_then(Value::as_array).unwrap();
    assert_eq!(null_rows[0][1], Value::Null);
}

#[test]
fn connection_string_connects_with_no_discrete_fields() {
    let mut plugin = Plugin::spawn();
    let p = conn_params();
    let conn_str = format!(
        "postgres://{}:{}@{}:{}/{}",
        p["username"].as_str().unwrap(),
        p["password"].as_str().unwrap(),
        p["host"].as_str().unwrap(),
        p["port"].as_u64().unwrap(),
        p["database"].as_str().unwrap(),
    );

    plugin.call_ok(
        "test_connection",
        json!({ "params": { "connection_string": conn_str } }),
    );
}

#[test]
fn startup_script_runs_on_every_pooled_connection() {
    let mut plugin = Plugin::spawn();
    let mut params = conn_params();
    params["startup_script"] = json!("SET search_path = public, pg_catalog");

    plugin.call_ok("test_connection", json!({ "params": params }));

    let result = plugin.call_ok(
        "execute_query",
        json!({ "params": params, "query": "SHOW search_path" }),
    );
    let rows = result.get("rows").and_then(Value::as_array).unwrap();
    let search_path = rows[0][0].as_str().unwrap();
    assert!(
        search_path.contains("public"),
        "startup_script's SET search_path should have taken effect, got: {search_path}"
    );
}

#[test]
fn broken_startup_script_fails_fast_with_clear_attribution() {
    let mut plugin = Plugin::spawn();
    let mut params = conn_params();
    // Use a host/port/database unique to this test so it can't reuse a
    // pool already cached (and validated) by another test in this file —
    // the pool cache key folds in startup_script, but a fresh identity is
    // the clearest way to guarantee a first-use preflight actually runs.
    params["startup_script"] = json!("THIS IS NOT VALID SQL");

    let response = plugin.call("test_connection", json!({ "params": params }));
    let error = response
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .expect("a broken startup script must produce a JSON-RPC error");
    assert!(
        error.starts_with("Startup script failed:"),
        "error should be clearly attributed to the startup script, got: {error}"
    );
}
