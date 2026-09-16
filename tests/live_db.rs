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

// Coverage for #66: `tokio_postgres::Error`'s own `Display` impl prints the
// generic "db error" string for any server-side error (its `Kind::Db` arm),
// throwing away the real message in the wrapped `DbError`. `exec_query_on_client`
// previously stringified the error with `format!("{e}")` directly instead of
// checking `as_db_error()` first, so every query error (a syntax error, a
// missing column, a constraint violation) surfaced as the unhelpful literal
// "db error" — see issue #66.
#[test]
fn query_syntax_error_surfaces_the_real_postgres_message_not_generic_db_error() {
    let mut plugin = Plugin::spawn();
    let response = plugin.call(
        "execute_query",
        json!({ "params": conn_params(), "query": "select foo" }),
    );
    let error = response
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .expect("an invalid query must produce a JSON-RPC error");
    assert_ne!(
        error, "db error",
        "error message must surface the real PostgreSQL error, not the generic \
         tokio_postgres::Error::Display fallback"
    );
    assert!(
        error.contains("foo"),
        "error message should mention the offending identifier, got: {error}"
    );
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

    // Self-contained: create (and reset) our own scratch enum type/table
    // rather than depending on tabularis's seed fixtures, since this test
    // must not require anything outside this repo (and CI's PostgreSQL
    // service container starts empty — see GitHub issue #7, where this
    // exact read path returned `null` for a non-null enum column).
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "DO $$ BEGIN \
                       CREATE TYPE live_db_test_mood AS ENUM ('happy', 'sad'); \
                       EXCEPTION WHEN duplicate_object THEN null; END $$",
        }),
    );
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "CREATE TABLE IF NOT EXISTS live_db_enum_scratch \
                       (id SERIAL PRIMARY KEY, current_mood live_db_test_mood)",
        }),
    );
    plugin.call_ok(
        "execute_query",
        json!({ "params": params, "query": "TRUNCATE live_db_enum_scratch RESTART IDENTITY" }),
    );
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "INSERT INTO live_db_enum_scratch (current_mood) VALUES ('happy'), (NULL)",
        }),
    );

    let result = plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "SELECT id, current_mood FROM live_db_enum_scratch ORDER BY id",
        }),
    );
    let rows = result.get("rows").and_then(Value::as_array).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0][1],
        json!("happy"),
        "a non-null enum column must round-trip as its label string, not null"
    );
    assert_eq!(
        rows[1][1],
        Value::Null,
        "a genuinely-NULL enum column must still come back as null"
    );
}

#[test]
fn execute_query_returns_a_real_hstore_value_not_null() {
    let mut plugin = Plugin::spawn();
    let params = conn_params();

    // Self-contained, same shape as the enum regression test above (#7):
    // hstore is an extension type, so this must not assume it's already
    // installed on whatever database CI points at (#68/#69).
    plugin.call_ok(
        "execute_query",
        json!({ "params": params, "query": "CREATE EXTENSION IF NOT EXISTS hstore" }),
    );
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "CREATE TABLE IF NOT EXISTS live_db_hstore_scratch \
                       (id SERIAL PRIMARY KEY, attrs hstore)",
        }),
    );
    plugin.call_ok(
        "execute_query",
        json!({ "params": params, "query": "TRUNCATE live_db_hstore_scratch RESTART IDENTITY" }),
    );
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "INSERT INTO live_db_hstore_scratch (attrs) VALUES \
                       ('\"comment\"=>\"This is a test\", \"count\"=>\"1\"'), (NULL)",
        }),
    );

    let result = plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "SELECT id, attrs FROM live_db_hstore_scratch ORDER BY id",
        }),
    );
    let rows = result.get("rows").and_then(Value::as_array).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0][1],
        json!({"comment": "This is a test", "count": "1"}),
        "a non-null hstore column must round-trip as a JSON object, not null"
    );
    assert_eq!(
        rows[1][1],
        Value::Null,
        "a genuinely-NULL hstore column must still come back as null"
    );
}

#[test]
fn execute_query_returns_real_array_values_for_custom_oid_element_types() {
    let mut plugin = Plugin::spawn();
    let params = conn_params();

    // Self-contained, same shape as the enum/hstore regression tests above
    // (#7, #68/#69). Arrays of a custom-OID element type (enum[], hstore[])
    // have no hardcoded fast-path in extract.rs — before #72's fix they fell
    // through to the generic string fallback and came back as null.
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "DO $$ BEGIN \
                       CREATE TYPE live_db_test_array_mood AS ENUM ('happy', 'sad'); \
                       EXCEPTION WHEN duplicate_object THEN null; END $$",
        }),
    );
    plugin.call_ok(
        "execute_query",
        json!({ "params": params, "query": "CREATE EXTENSION IF NOT EXISTS hstore" }),
    );
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "CREATE TABLE IF NOT EXISTS live_db_array_scratch \
                       (id SERIAL PRIMARY KEY, \
                        moods live_db_test_array_mood[], \
                        attrs hstore[])",
        }),
    );
    plugin.call_ok(
        "execute_query",
        json!({ "params": params, "query": "TRUNCATE live_db_array_scratch RESTART IDENTITY" }),
    );
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "INSERT INTO live_db_array_scratch (moods, attrs) VALUES \
                       (ARRAY['happy'::live_db_test_array_mood, 'sad'::live_db_test_array_mood], \
                        ARRAY['a=>1'::hstore, 'b=>2'::hstore]), \
                       (NULL, NULL)",
        }),
    );

    let result = plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "SELECT id, moods, attrs FROM live_db_array_scratch ORDER BY id",
        }),
    );
    let rows = result.get("rows").and_then(Value::as_array).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0][1],
        json!(["happy", "sad"]),
        "an enum[] column must round-trip as a JSON array of label strings, not null"
    );
    assert_eq!(
        rows[0][2],
        json!([{"a": "1"}, {"b": "2"}]),
        "an hstore[] column must round-trip as a JSON array of objects, not null"
    );
    assert_eq!(
        rows[1][1],
        Value::Null,
        "a genuinely-NULL array column must still come back as null"
    );
    assert_eq!(
        rows[1][2],
        Value::Null,
        "a genuinely-NULL array column must still come back as null"
    );
}

#[test]
fn execute_query_preserves_null_slots_in_hardcoded_array_types() {
    let mut plugin = Plugin::spawn();
    let params = conn_params();

    // Self-contained, same shape as the tests above. Before #73's fix, the
    // hardcoded array fast-paths (int2/int4/int8/float4/float8/bool/text/
    // varchar) decoded via Vec<T>: FromSql, which errors the moment any
    // element is NULL — try_extract's catch-all then turned that error into
    // whole-column null instead of preserving the null element's position.
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "CREATE TABLE IF NOT EXISTS live_db_array_null_scratch \
                       (id SERIAL PRIMARY KEY, \
                        nums int4[], \
                        words text[], \
                        flags bool[])",
        }),
    );
    plugin.call_ok(
        "execute_query",
        json!({ "params": params, "query": "TRUNCATE live_db_array_null_scratch RESTART IDENTITY" }),
    );
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "INSERT INTO live_db_array_null_scratch (nums, words, flags) VALUES \
                       (ARRAY[1, NULL], ARRAY['a', NULL], ARRAY[true, NULL])",
        }),
    );

    let result = plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "SELECT nums, words, flags FROM live_db_array_null_scratch",
        }),
    );
    let rows = result.get("rows").and_then(Value::as_array).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0],
        json!([1, null]),
        "int4[] with a NULL element must preserve the null slot, not null the whole array"
    );
    assert_eq!(
        rows[0][1],
        json!(["a", null]),
        "text[] with a NULL element must preserve the null slot, not null the whole array"
    );
    assert_eq!(
        rows[0][2],
        json!([true, null]),
        "bool[] with a NULL element must preserve the null slot, not null the whole array"
    );
}

#[test]
fn get_tables_and_get_columns_return_real_comments() {
    let mut plugin = Plugin::spawn();
    let params = conn_params();

    // Self-contained, same shape as the tests above. #74: get_tables/
    // get_columns must surface pg_description comments as an optional
    // "comment" field, matching the builtin driver's parity fix
    // (tabularis#764). Comments must be omitted (not present as null or
    // empty string) when no COMMENT ON was ever run for that object.
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "DROP TABLE IF EXISTS live_db_comment_scratch",
        }),
    );
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "CREATE TABLE live_db_comment_scratch \
                       (id SERIAL PRIMARY KEY, commented_col TEXT, plain_col TEXT)",
        }),
    );
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "COMMENT ON TABLE live_db_comment_scratch \
                       IS 'Table comment with an apostrophe''s test'",
        }),
    );
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "COMMENT ON COLUMN live_db_comment_scratch.commented_col \
                       IS 'Unicode: 日本語 café — with a\nnewline'",
        }),
    );
    // plain_col deliberately left without a COMMENT ON.

    let tables = plugin.call_ok(
        "get_tables",
        json!({ "params": params, "schema": "public" }),
    );
    let table = tables
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "live_db_comment_scratch")
        .expect("live_db_comment_scratch must be in get_tables' result");
    assert_eq!(
        table["comment"],
        json!("Table comment with an apostrophe's test"),
        "a table with a COMMENT ON must surface it via get_tables"
    );

    let columns = plugin.call_ok(
        "get_columns",
        json!({
            "params": params,
            "table": "live_db_comment_scratch",
            "schema": "public",
        }),
    );
    let columns = columns.as_array().unwrap();
    let commented = columns
        .iter()
        .find(|c| c["name"] == "commented_col")
        .expect("commented_col must be in get_columns' result");
    assert_eq!(
        commented["comment"],
        json!("Unicode: 日本語 café — with a\nnewline"),
        "a column with a COMMENT ON must surface Unicode/newlines correctly via get_columns"
    );

    let plain = columns
        .iter()
        .find(|c| c["name"] == "plain_col")
        .expect("plain_col must be in get_columns' result");
    assert!(
        plain.get("comment").is_none(),
        "a column with no COMMENT ON must omit the comment field entirely, got: {plain:?}"
    );
}

#[test]
fn show_command_with_a_limit_param_does_not_error() {
    let mut plugin = Plugin::spawn();
    let params = conn_params();

    // #70: the Console sends a limit/page param on every execute_query
    // call regardless of statement type. SHOW isn't a SELECT and doesn't
    // support a trailing LIMIT/OFFSET clause in PostgreSQL syntax, so
    // pagination must not push a SQL LIMIT into it — before this fix, this
    // produced "syntax error at or near LIMIT".
    let result = plugin.call_ok(
        "execute_query",
        json!({ "params": params, "query": "SHOW search_path", "limit": 100, "page": 1 }),
    );
    let rows = result.get("rows").and_then(Value::as_array).unwrap();
    assert_eq!(
        rows.len(),
        1,
        "SHOW search_path must return exactly one row"
    );
    assert!(
        result
            .get("pagination")
            .map(Value::is_null)
            .unwrap_or(false),
        "a non-SELECT statement must not carry fabricated pagination metadata, got: {result:?}"
    );
}

#[test]
fn call_with_a_limit_param_does_not_error() {
    let mut plugin = Plugin::spawn();
    let params = conn_params();

    // Same class of bug as SHOW (#70): CALL returns a result set but
    // PostgreSQL also rejects a trailing LIMIT after it.
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "CREATE OR REPLACE PROCEDURE live_db_noop_proc() \
                       LANGUAGE plpgsql AS $$ BEGIN END $$",
        }),
    );
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "CALL live_db_noop_proc()",
            "limit": 10,
            "page": 1,
        }),
    );
}

#[test]
fn cte_values_and_table_statements_still_paginate_correctly_across_pages() {
    let mut plugin = Plugin::spawn();
    let params = conn_params();

    // Regression guard: an earlier version of the #70 fix matched the
    // builtin driver's narrower is_select_query (literal SELECT prefix
    // only), which routed WITH/VALUES/TABLE through client-side capping
    // instead of real SQL pagination — silently returning the same first
    // page for every `page` value even though `WITH ... SELECT ...
    // LIMIT n OFFSET m` is valid PostgreSQL syntax. Verified directly
    // against this live database that these statement types accept a
    // trailing LIMIT, so page 2 must return different rows than page 1.
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "CREATE TABLE IF NOT EXISTS live_db_pagination_scratch \
                       (id SERIAL PRIMARY KEY, v INTEGER)",
        }),
    );
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "TRUNCATE live_db_pagination_scratch RESTART IDENTITY",
        }),
    );
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "INSERT INTO live_db_pagination_scratch (v) \
                       SELECT generate_series(1, 10)",
        }),
    );

    let cte_page1 = plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "WITH t AS (SELECT * FROM live_db_pagination_scratch) \
                       SELECT * FROM t ORDER BY id",
            "limit": 5,
            "page": 1,
        }),
    );
    let cte_page2 = plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "WITH t AS (SELECT * FROM live_db_pagination_scratch) \
                       SELECT * FROM t ORDER BY id",
            "limit": 5,
            "page": 2,
        }),
    );
    assert_ne!(
        cte_page1.get("rows"),
        cte_page2.get("rows"),
        "a paginated CTE must return different rows on page 2, not repeat page 1"
    );
    assert_eq!(
        cte_page1.get("rows"),
        Some(&json!([[1, 1], [2, 2], [3, 3], [4, 4], [5, 5]]))
    );
    assert_eq!(
        cte_page2.get("rows"),
        Some(&json!([[6, 6], [7, 7], [8, 8], [9, 9], [10, 10]]))
    );

    let table_page1 = plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "TABLE live_db_pagination_scratch",
            "limit": 5,
            "page": 1,
        }),
    );
    let table_page2 = plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "TABLE live_db_pagination_scratch",
            "limit": 5,
            "page": 2,
        }),
    );
    assert_ne!(
        table_page1.get("rows"),
        table_page2.get("rows"),
        "a paginated TABLE statement must return different rows on page 2"
    );

    let values_page1 = plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "VALUES (1),(2),(3),(4),(5),(6)",
            "limit": 3,
            "page": 1,
        }),
    );
    let values_page2 = plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "VALUES (1),(2),(3),(4),(5),(6)",
            "limit": 3,
            "page": 2,
        }),
    );
    assert_eq!(values_page1.get("rows"), Some(&json!([[1], [2], [3]])));
    assert_eq!(values_page2.get("rows"), Some(&json!([[4], [5], [6]])));
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
    // Coverage for #66: startup_script_error previously stringified the
    // tokio_postgres::Error directly, so a DbError (a syntax error in the
    // script, the common case) collapsed to the generic "db error" instead
    // of the real PostgreSQL message.
    assert!(
        !error.contains("db error") && error.contains("syntax error"),
        "error should surface the real PostgreSQL syntax error, not the generic \
         tokio_postgres::Error::Display fallback, got: {error}"
    );
}

// Coverage for #43: build_pool never called cfg.ssl_mode(...), so
// tokio_postgres's own default (SslMode::Prefer) applied regardless of the
// plugin's ssl_mode value, letting ssl_mode=require silently connect over
// plaintext instead of failing. CI's live-db-integration fixture runs a
// plain `postgres:16` container with no SSL configured (see
// .github/workflows/ci.yml), so this must fail here just like it would
// against any server that hasn't been configured to offer TLS.
#[test]
fn ssl_mode_require_fails_against_a_server_without_tls() {
    let mut plugin = Plugin::spawn();
    let mut params = conn_params();
    params["ssl_mode"] = json!("require");

    let response = plugin.call("test_connection", json!({ "params": params }));
    assert!(
        response.get("error").is_some(),
        "ssl_mode=require must fail against a server with no TLS, not silently connect over plaintext"
    );
}

// Coverage for #66: the connection-establishment handshake itself (bad
// database name, bad password) surfaces as a tokio_postgres::Error wrapped
// in deadpool_postgres::PoolError::Backend, from pool.get() — the same
// Kind::Db/"db error" pitfall as query execution, just one layer deeper.
// Every "Connection failed: {e}" call site previously stringified the
// PoolError directly instead of unwrapping to the inner DbError.
#[test]
fn connecting_to_a_nonexistent_database_surfaces_the_real_postgres_message() {
    let mut plugin = Plugin::spawn();
    let mut params = conn_params();
    params["database"] = json!("this_database_does_not_exist_xyz");

    let response = plugin.call("test_connection", json!({ "params": params }));
    let error = response
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .expect("connecting to a nonexistent database must produce a JSON-RPC error");
    assert!(
        !error.contains("db error") && error.contains("does not exist"),
        "error should surface the real PostgreSQL message, not the generic \
         tokio_postgres::Error::Display fallback, got: {error}"
    );
}

#[test]
fn connecting_with_a_wrong_password_surfaces_the_real_postgres_message() {
    let mut plugin = Plugin::spawn();
    let mut params = conn_params();
    params["password"] = json!("definitely_wrong_password");

    let response = plugin.call("test_connection", json!({ "params": params }));
    let error = response
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .expect("a wrong password must produce a JSON-RPC error");
    assert!(
        !error.contains("db error") && error.contains("password authentication failed"),
        "error should surface the real PostgreSQL message, not the generic \
         tokio_postgres::Error::Display fallback, got: {error}"
    );
}

// Characterizes `extract.rs`'s decode behavior across every type the flat
// `Type::` dispatch table supported before it was restructured into a
// `Kind`-first dispatch (mirroring the builtin driver's `extract/mod.rs`
// shape — see #82's tracking issue). This is the regression guard for that
// restructure: a future change to the dispatch shape that silently drops or
// misroutes a type would show up here as a wrong value, not just a `null`.
// Covers one column per scalar type, all six built-in range types, all
// eight hardcoded array fast-paths, and a fully-NULL row exercising every
// type's NULL path in one query.
#[test]
fn execute_query_decodes_every_currently_supported_type_correctly() {
    let mut plugin = Plugin::spawn();
    let params = conn_params();

    plugin.call_ok(
        "execute_query",
        json!({ "params": params, "query": "DROP TABLE IF EXISTS live_db_type_coverage_scratch" }),
    );
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "CREATE TABLE live_db_type_coverage_scratch ( \
                id serial PRIMARY KEY, \
                c_bool boolean, \
                c_int2 smallint, \
                c_int4 integer, \
                c_int8 bigint, \
                c_numeric numeric(10,2), \
                c_text text, \
                c_uuid uuid, \
                c_date date, \
                c_time time, \
                c_timestamp timestamp, \
                c_timestamptz timestamptz, \
                c_json json, \
                c_jsonb jsonb, \
                c_bytea bytea, \
                c_inet inet, \
                c_macaddr macaddr, \
                c_oid oid, \
                c_money money, \
                c_int4range int4range, \
                c_int8range int8range, \
                c_numrange numrange, \
                c_daterange daterange, \
                c_int4_arr integer[], \
                c_text_arr text[], \
                c_bool_arr boolean[] \
            )",
        }),
    );
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "INSERT INTO live_db_type_coverage_scratch VALUES ( \
                DEFAULT, true, 123, 123456, 123456789012, 12345.67, \
                'hello', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', \
                '2026-01-15', '13:45:30', '2026-01-15 13:45:30', \
                '2026-01-15 13:45:30+00', '{\"a\":1}', '{\"b\":2}', \
                E'\\\\xDEADBEEF', '192.168.1.1/24', '08:00:2b:01:02:03', \
                12345, 123.45, '[1,10)', '[100,1000)', '[1.5,9.5)', \
                '[2026-01-01,2026-02-01)', ARRAY[1,2,NULL]::int[], \
                ARRAY['a','b',NULL]::text[], ARRAY[true,false,NULL]::boolean[] \
            )",
        }),
    );
    plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "INSERT INTO live_db_type_coverage_scratch (id) VALUES (DEFAULT)",
        }),
    );

    let result = plugin.call_ok(
        "execute_query",
        json!({
            "params": params,
            "query": "SELECT * FROM live_db_type_coverage_scratch ORDER BY id",
        }),
    );
    let rows = result.get("rows").and_then(Value::as_array).unwrap();
    assert_eq!(rows.len(), 2);

    let populated = &rows[0];
    assert_eq!(populated[1], json!(true), "c_bool");
    assert_eq!(populated[2], json!(123), "c_int2");
    assert_eq!(populated[3], json!(123456), "c_int4");
    assert_eq!(populated[4], json!(123456789012_i64), "c_int8");
    assert_eq!(populated[5], json!("12345.67"), "c_numeric");
    assert_eq!(populated[6], json!("hello"), "c_text");
    assert_eq!(
        populated[7],
        json!("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"),
        "c_uuid"
    );
    assert_eq!(populated[8], json!("2026-01-15"), "c_date");
    assert_eq!(populated[9], json!("13:45:30"), "c_time");
    assert_eq!(populated[10], json!("2026-01-15 13:45:30"), "c_timestamp");
    assert_eq!(populated[11], json!("2026-01-15 13:45:30"), "c_timestamptz");
    assert_eq!(populated[12], json!({"a": 1}), "c_json");
    assert_eq!(populated[13], json!({"b": 2}), "c_jsonb");
    assert_eq!(
        populated[14],
        json!("BLOB:4:application/octet-stream:3q2+7w=="),
        "c_bytea"
    );
    assert_eq!(populated[15], json!("192.168.1.1/24"), "c_inet");
    assert_eq!(populated[16], json!("08:00:2b:01:02:03"), "c_macaddr");
    assert_eq!(populated[17], json!(12345), "c_oid");
    assert_eq!(populated[18], json!(12345), "c_money");
    assert_eq!(populated[19], json!("[1, 10)"), "c_int4range");
    assert_eq!(populated[20], json!("[100, 1000)"), "c_int8range");
    assert_eq!(populated[21], json!("[\"1.5\", \"9.5\")"), "c_numrange");
    assert_eq!(
        populated[22],
        json!("[\"2026-01-01\", \"2026-02-01\")"),
        "c_daterange"
    );
    assert_eq!(populated[23], json!([1, 2, null]), "c_int4_arr");
    assert_eq!(populated[24], json!(["a", "b", null]), "c_text_arr");
    assert_eq!(populated[25], json!([true, false, null]), "c_bool_arr");

    let all_null = &rows[1];
    for (col_idx, col_name) in [
        (1, "c_bool"),
        (2, "c_int2"),
        (3, "c_int4"),
        (4, "c_int8"),
        (5, "c_numeric"),
        (6, "c_text"),
        (7, "c_uuid"),
        (8, "c_date"),
        (9, "c_time"),
        (10, "c_timestamp"),
        (11, "c_timestamptz"),
        (12, "c_json"),
        (13, "c_jsonb"),
        (14, "c_bytea"),
        (15, "c_inet"),
        (16, "c_macaddr"),
        (17, "c_oid"),
        (18, "c_money"),
        (19, "c_int4range"),
        (20, "c_int8range"),
        (21, "c_numrange"),
        (22, "c_daterange"),
        (23, "c_int4_arr"),
        (24, "c_text_arr"),
        (25, "c_bool_arr"),
    ] {
        assert_eq!(
            all_null[col_idx],
            Value::Null,
            "{col_name} must decode to null when the column is genuinely NULL"
        );
    }
}
