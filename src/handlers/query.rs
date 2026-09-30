//! Query execution handlers.

use deadpool_postgres::Object as PgClient;
use serde_json::{json, Value};
use std::time::Instant;

use crate::cancel;
use crate::client;
use crate::extract::extract_value;
use crate::models::{inner_params, ConnectionParams};
use crate::rpc::{error_response, ok_response};
use crate::session;

pub async fn execute_query(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let query = params.get("query").and_then(Value::as_str).unwrap_or("");
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map(|v| v as u32);
    let page = params.get("page").and_then(Value::as_u64).unwrap_or(1) as u32;
    let schema = params.get("schema").and_then(Value::as_str);
    let session_id = params.get("session_id").and_then(Value::as_str);

    // One statement at a time is how a transaction is actually driven -
    // BEGIN, the changes, a verifying SELECT, COMMIT - so this path needs
    // the same pinning as a batch. Implemented as a one-statement batch so
    // there is a single session code path.
    if session_id.is_some() {
        let queries = [query.to_string()];
        return match run_batch_in_session(
            &conn_params,
            &queries,
            limit,
            page,
            schema,
            session_id,
            &id,
        )
        .await
        {
            Ok((mut results, in_transaction)) => {
                let statement = results.pop().unwrap_or(Value::Null);
                match statement.get("error").and_then(Value::as_str) {
                    // Still a reply, not an RPC error, so the host learns the state a failed COMMIT left.
                    Some(error) => ok_response(
                        id,
                        json!({ "error": error, "in_transaction": in_transaction }),
                    ),
                    None => {
                        let result = statement.get("result").cloned().unwrap_or(Value::Null);
                        ok_response(
                            id,
                            json!({ "result": result, "in_transaction": in_transaction }),
                        )
                    }
                }
            }
            Err(e) => error_response(id, -32603, &e),
        };
    }

    match exec_query(&conn_params, query, limit, page, schema, &id).await {
        Ok(result) => ok_response(id, result),
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn execute_query_batch(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let queries: Vec<String> = params
        .get("queries")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map(|v| v as u32);
    let page = params.get("page").and_then(Value::as_u64).unwrap_or(1) as u32;
    let schema = params.get("schema").and_then(Value::as_str);
    // The host sends the editor tab's id. A batch that leaves a transaction
    // open keeps its connection under this key so the next batch from the
    // same tab continues that transaction.
    let session_id = params.get("session_id").and_then(Value::as_str);

    match run_batch_in_session(&conn_params, &queries, limit, page, schema, session_id, &id).await {
        // Only a session-aware call gets the richer shape; a host that did
        // not send a session_id still receives the bare array it expects.
        Ok((results, in_transaction)) => match session_id {
            Some(_) => ok_response(
                id,
                json!({ "results": results, "in_transaction": in_transaction }),
            ),
            None => ok_response(id, json!(results)),
        },
        Err(e) => error_response(id, -32603, &e),
    }
}

/// Run `queries` on one connection, reusing the session's pinned one when
/// it left a transaction open and re-pinning it if one is still open after.
///
/// Returns one per-statement result object per query — the same shape
/// `execute_query_batch` replies with — plus whether the session is still
/// inside a transaction. Shared with `execute_query` so a single statement
/// and a batch take the same session path.
///
/// A batch that leaves a transaction open with no session to pin it to is
/// rolled back: the pool recycles with `RecyclingMethod::Fast`, so the next
/// borrower would otherwise inherit the transaction and its locks.
async fn run_batch_in_session(
    conn_params: &ConnectionParams,
    queries: &[String],
    limit: Option<u32>,
    page: u32,
    schema: Option<&str>,
    session_id: Option<&str>,
    request_id: &Value,
) -> Result<(Vec<Value>, bool), String> {
    // Held for the whole run, so an overlapping call for the same session waits its turn.
    let mut slot = match session_id {
        Some(sid) => Some(session::lock(sid).await),
        None => None,
    };
    let pinned = slot.as_mut().and_then(|s| s.take());
    // A pinned connection only exists because its transaction is still open.
    let mut in_transaction = pinned.is_some();
    let reused = pinned.is_some();

    // Acquire ONE connection for the entire batch (session state must survive)
    let pg_client = match pinned {
        Some(client) => client,
        None => {
            let pool = client::build_pool_pub(conn_params).await?;
            pool.get()
                .await
                .map_err(|e| format!("Connection failed: {}", client::format_pool_error(&e)))?
        }
    };

    // Register a cancel handle for this in-flight batch, keyed by the request
    // id (#126). The guard deregisters on drop — at every return path below,
    // Ok or Err — so the map can't leak a finished query and a late cancel
    // for a reused id can't target a later one. The cancel only needs the
    // token (captured into the action) and the connection params, never the
    // borrowed client.
    let _cancel_guard =
        cancel::CancelGuard::register(request_id.as_u64(), &pg_client, conn_params.clone());

    // Applying search_path to a reused connection would run inside the open
    // transaction and change what the rest of it sees.
    if let Some(s) = schema.filter(|_| !reused) {
        let set_path = format!("SET search_path TO \"{}\"", s.replace('"', "\"\""));
        pg_client
            .batch_execute(&set_path)
            .await
            .map_err(|e| format!("Failed to set search_path: {}", client::format_pg_error(&e)))?;
    }

    let mut results: Vec<Value> = Vec::new();
    for query in queries {
        let start = Instant::now();
        let outcome = exec_query_on_client(&pg_client, query, limit, page).await;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

        in_transaction =
            transaction_effect(query).in_transaction_after(outcome.is_ok(), in_transaction);

        match outcome {
            Ok(result) => results.push(json!({
                "result": result,
                "error": null,
                "execution_time_ms": elapsed_ms,
            })),
            Err(e) => results.push(json!({
                "result": null,
                "error": e,
                "execution_time_ms": elapsed_ms,
            })),
        }
    }
    // `_cancel_guard` drops here, deregistering the cancel handle before the
    // connection is re-pinned or released below, so a cancel racing the end
    // of the batch can't target a query that's already done.

    match slot.as_mut() {
        Some(slot) if in_transaction => slot.pin(pg_client),
        _ => {
            if in_transaction {
                session::rollback_and_release(pg_client).await;
            }
        }
    }

    Ok((results, in_transaction))
}

/// Roll back and release the connection pinned to a session. Called when
/// the owning editor tab closes.
pub async fn release_session(id: Value, params: &Value) -> Value {
    match params.get("session_id").and_then(Value::as_str) {
        Some(session_id) => {
            session::release(session_id).await;
            ok_response(id, json!({ "released": true }))
        }
        None => error_response(id, -32602, "session_id is required"),
    }
}

/// Cancel the in-flight query for a request id (#126). The host sends this
/// as a fire-and-forget JSON-RPC *notification* when its plugin-call timeout
/// fires, so the orphaned statement stops server-side instead of running
/// on holding its locks. `params.id` is the original (timed-out) request's
/// id. An unknown id (already finished, or arrived after cleanup) is a
/// no-op — never an error — so a late/duplicate cancel can't disrupt a
/// later query that reused the id.
///
/// This handler returns `Value::Null` only when invoked as a regular
/// request; the no-response behavior for a true notification (no top-level
/// `id` field) is handled in `rpc::handle_line`/`main.rs`'s worker, which
/// skips the stdout write entirely.
pub async fn cancel(id: Value, params: &Value) -> Value {
    let Some(target_id) = params.get("id").and_then(Value::as_u64) else {
        return error_response(id, -32602, "id (a u64) is required");
    };
    cancel::cancel(target_id).await;
    ok_response(id, Value::Null)
}

pub async fn explain_query(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let query = params.get("query").and_then(Value::as_str).unwrap_or("");
    let analyze = params
        .get("analyze")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let schema = params.get("schema").and_then(Value::as_str);

    let explain_sql = if analyze {
        format!("EXPLAIN (FORMAT JSON, ANALYZE, BUFFERS) {}", query)
    } else {
        format!("EXPLAIN (FORMAT JSON) {}", query)
    };

    match exec_query(&conn_params, &explain_sql, None, 1, schema, &id).await {
        Ok(result) => {
            let plan_json = result
                .get("rows")
                .and_then(Value::as_array)
                .and_then(|rows| rows.first())
                .and_then(Value::as_array)
                .and_then(|first_row| first_row.first());
            match plan_json {
                Some(plan_json) => ok_response(id, raw_explain_output(plan_json, query)),
                None => ok_response(id, result),
            }
        }
        Err(e) => error_response(id, -32603, &e),
    }
}

/// Wrap an EXPLAIN plan value in the `Raw { engine, format, payload,
/// original_query }` shape the host's plugin adapter recognizes
/// (`tabularis` `plugins/driver.rs::explain_query`, which reads
/// `engine`/`format`/`payload` as strings via `.as_str()` before
/// classifying the result as `ExplainQueryOutput::Raw`; any other shape
/// falls through to the parsed-plan path instead). `payload` must be the
/// JSON **stringified**, not the live JSON value itself, to match the
/// builtin driver's `RawExplainOutput` contract exactly.
fn raw_explain_output(plan_json: &Value, original_query: &str) -> Value {
    json!({
        "engine": "postgres",
        "format": "postgres-json",
        "payload": plan_json.to_string(),
        "original_query": original_query,
    })
}

/// Execute a SQL query and return a QueryResult-shaped JSON value.
async fn exec_query(
    conn_params: &ConnectionParams,
    query: &str,
    limit: Option<u32>,
    page: u32,
    schema: Option<&str>,
    request_id: &Value,
) -> Result<Value, String> {
    let pool = client::build_pool_pub(conn_params).await?;
    let pg_client = pool
        .get()
        .await
        .map_err(|e| format!("Connection failed: {}", client::format_pool_error(&e)))?;

    // Register a cancel handle for this in-flight query (#126). The guard
    // deregisters on drop — at every return path below, Ok or Err — so a
    // late cancel for a reused id can't target a later query.
    let _cancel_guard =
        cancel::CancelGuard::register(request_id.as_u64(), &pg_client, conn_params.clone());

    // Set search_path if schema is specified
    if let Some(s) = schema {
        let set_path = format!("SET search_path TO \"{}\"", s.replace('"', "\"\""));
        pg_client
            .batch_execute(&set_path)
            .await
            .map_err(|e| format!("Failed to set search_path: {}", client::format_pg_error(&e)))?;
    }

    exec_query_on_client(&pg_client, query, limit, page).await
}

/// Execute a query on an existing client (used by both single and batch execution).
async fn exec_query_on_client(
    pg_client: &PgClient,
    query: &str,
    limit: Option<u32>,
    page: u32,
) -> Result<Value, String> {
    // Check if the statement returns a result set
    if !returns_result_set(query) {
        let affected = pg_client
            .execute(query, &[])
            .await
            .map_err(|e| client::format_pg_error(&e))?;
        return Ok(json!({
            "columns": [],
            "rows": [],
            "affected_rows": affected,
            "truncated": false,
            "pagination": null,
        }));
    }

    // Only statements whose PostgreSQL syntax actually accepts a trailing
    // LIMIT/OFFSET get one appended (#70: `SHOW search_path` with a `limit`
    // param produced "syntax error at or near LIMIT" — SHOW and CALL, unlike
    // SELECT/WITH/VALUES/TABLE/EXPLAIN, don't accept that clause). Statements
    // that don't get real SQL pagination still respect `limit` by capping
    // rows client-side below (`manual_limit`).
    let is_paginated = supports_trailing_limit_clause(query) && limit.is_some();
    let (final_query, page_size) = if is_paginated {
        let lim = limit.unwrap();
        (
            crate::utils::pagination::build_paginated_query(query, lim, page),
            lim,
        )
    } else {
        (query.to_string(), 0u32)
    };
    let manual_limit = if is_paginated { None } else { limit };

    // Execute query
    let rows = pg_client
        .query(&final_query, &[])
        .await
        .map_err(|e| client::format_pg_error(&e))?;

    if rows.is_empty() {
        // Get columns from the statement if possible
        let columns: Vec<String> = if let Ok(stmt) = pg_client.prepare(&final_query).await {
            stmt.columns()
                .iter()
                .map(|c| c.name().to_string())
                .collect()
        } else {
            vec![]
        };

        let pagination = if is_paginated {
            Some(json!({
                "page": page,
                "page_size": page_size,
                "total_rows": null,
                "has_more": false,
            }))
        } else {
            None
        };

        return Ok(json!({
            "columns": columns,
            "rows": [],
            "affected_rows": 0,
            "truncated": false,
            "pagination": pagination,
        }));
    }

    // Extract columns from first row
    let columns: Vec<String> = rows[0]
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect();

    // Determine has_more/truncate — via the paginated page size for a real
    // SELECT, or by capping to the raw limit client-side otherwise.
    let cap = if is_paginated {
        Some(page_size as usize)
    } else {
        manual_limit.map(|l| l as usize)
    };
    let truncated = cap.is_some_and(|c| rows.len() > c);
    let result_rows = match cap {
        Some(c) if truncated => &rows[..c],
        _ => &rows[..],
    };

    // Extract row values
    let json_rows: Vec<Value> = result_rows
        .iter()
        .map(|row| {
            let values: Vec<Value> = (0..row.columns().len())
                .map(|i| extract_value(row, i))
                .collect();
            Value::Array(values)
        })
        .collect();

    let pagination = if is_paginated {
        Some(json!({
            "page": page,
            "page_size": page_size,
            "total_rows": null,
            "has_more": truncated,
        }))
    } else {
        None
    };

    Ok(json!({
        "columns": columns,
        "rows": json_rows,
        "affected_rows": 0,
        "truncated": truncated,
        "pagination": pagination,
    }))
}

/// What a statement does to the surrounding transaction.
///
/// Decides whether the connection a batch ran on must be kept for the next
/// batch of the same session (an explicit transaction is still open) or may
/// go back to the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionEffect {
    /// Opens an explicit transaction (`BEGIN`, `START TRANSACTION`).
    Opens,
    /// Closes the current transaction (`COMMIT`, `ROLLBACK`, `END`).
    ///
    /// `ROLLBACK TO SAVEPOINT` does not close it and is classified as
    /// [`TransactionEffect::None`].
    Closes,
    /// Ends the current transaction and opens a new one (`COMMIT AND CHAIN`).
    Chains,
    /// Leaves the transaction state as it was.
    None,
}

impl TransactionEffect {
    /// Whether a transaction is open after a statement with this effect ran.
    pub fn in_transaction_after(self, succeeded: bool, before: bool) -> bool {
        match (self, succeeded) {
            (TransactionEffect::Opens | TransactionEffect::Chains, true) => true,
            // PostgreSQL ends the transaction even when COMMIT itself fails, e.g. on a deferred constraint.
            (TransactionEffect::Closes, _) | (TransactionEffect::Chains, false) => false,
            _ => before,
        }
    }
}

/// Classify a statement's effect on the transaction state.
///
/// Only the leading keywords are inspected, so a `BEGIN` inside a string
/// literal or a later clause cannot be mistaken for transaction control. A
/// PL/pgSQL `BEGIN … END` body is not a concern: it arrives inside a `DO`
/// or `CREATE FUNCTION` statement, whose leading keyword is neither.
pub fn transaction_effect(query: &str) -> TransactionEffect {
    // Six cover the longest form, `ROLLBACK TRANSACTION AND NO CHAIN`.
    let mut words = leading_keywords(query, 6).into_iter();
    let Some(first) = words.next() else {
        return TransactionEffect::None;
    };
    let mut rest: Vec<String> = words.collect();
    // `WORK` / `TRANSACTION` after a transaction-control verb is optional noise.
    if matches!(first.as_str(), "COMMIT" | "END" | "ROLLBACK" | "ABORT")
        && matches!(
            rest.first().map(String::as_str),
            Some("WORK" | "TRANSACTION")
        )
    {
        rest.remove(0);
    }
    let second = rest.first().map(String::as_str);
    let chains = rest.windows(2).any(|p| p[0] == "AND" && p[1] == "CHAIN");

    match first.as_str() {
        // `BEGIN` alone, `BEGIN TRANSACTION`, `BEGIN ISOLATION LEVEL …`.
        "BEGIN" => TransactionEffect::Opens,
        "START" if second == Some("TRANSACTION") => TransactionEffect::Opens,
        // Two-phase commit acts on a prepared transaction, not this session's.
        "COMMIT" | "ROLLBACK" if second == Some("PREPARED") => TransactionEffect::None,
        // `ROLLBACK [WORK] TO [SAVEPOINT] x` unwinds to a savepoint and leaves the transaction open.
        "ROLLBACK" if second == Some("TO") => TransactionEffect::None,
        "COMMIT" | "END" | "ROLLBACK" | "ABORT" if chains => TransactionEffect::Chains,
        "COMMIT" | "END" | "ROLLBACK" | "ABORT" => TransactionEffect::Closes,
        // `PREPARE TRANSACTION` dissociates the transaction from the session.
        "PREPARE" if second == Some("TRANSACTION") => TransactionEffect::Closes,
        _ => TransactionEffect::None,
    }
}

/// The first `n` keywords of `query`, uppercased. Every `--` and (nested)
/// `/* */` comment is skipped, wherever it appears, and reading stops at a
/// string literal, dollar quote or `;`, so no data is read as a keyword.
fn leading_keywords(query: &str, n: usize) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut chars = query.chars().peekable();
    let mut comment_depth = 0usize;
    while let Some(c) = chars.next() {
        if comment_depth > 0 {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                comment_depth -= 1;
            } else if c == '/' && chars.peek() == Some(&'*') {
                chars.next();
                comment_depth += 1;
            }
            continue;
        }
        if c.is_ascii_alphanumeric() || c == '_' {
            word.push(c.to_ascii_uppercase());
            continue;
        }
        if !word.is_empty() {
            words.push(std::mem::take(&mut word));
            if words.len() == n {
                return words;
            }
        }
        match (c, chars.peek()) {
            ('-', Some('-')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                comment_depth = 1;
            }
            ('\'' | '"' | '$' | ';', _) => return words,
            _ => {}
        }
    }
    if !word.is_empty() && words.len() < n {
        words.push(word);
    }
    words
}

/// Strip leading SQL comments (`-- …` line comments and `/* … */` block
/// comments) and whitespace so the first statement keyword is at position 0.
/// Matches the builtin driver's `drivers/common/query.rs::strip_leading_sql_comments`
/// — `returns_result_set`/`is_select_query` must agree on where a statement
/// "really" starts, or a comment-headed query gets misclassified.
fn strip_leading_sql_comments(query: &str) -> &str {
    let mut s = query;
    loop {
        s = s.trim_start();
        if s.starts_with("--") {
            match s.find('\n') {
                Some(pos) => s = &s[pos + 1..],
                None => return "",
            }
        } else if s.starts_with("/*") {
            match s.find("*/") {
                Some(pos) => s = &s[pos + 2..],
                None => return "",
            }
        } else {
            break;
        }
    }
    s
}

/// Check if a SQL statement returns a result set (SELECT, WITH, SHOW, etc.)
fn returns_result_set(query: &str) -> bool {
    let upper = strip_leading_sql_comments(query).to_uppercase();
    upper.starts_with("SELECT")
        || upper.starts_with("WITH")
        || upper.starts_with("SHOW")
        || upper.starts_with("EXPLAIN")
        || upper.starts_with("DESCRIBE")
        || upper.starts_with("VALUES")
        || upper.starts_with("TABLE")
        || upper.starts_with("PRAGMA")
        || upper.starts_with("CALL")
}

/// Check if a result-set-bearing statement's syntax actually accepts a
/// trailing `LIMIT`/`OFFSET` clause — narrower than `returns_result_set`.
/// Verified directly against a live PostgreSQL instance (`SELECT`, `WITH`,
/// `VALUES`, `TABLE`, `EXPLAIN` all accept a trailing `LIMIT`; `SHOW` and
/// `CALL` reject it with "syntax error at or near LIMIT"). Only statements
/// that pass this should be routed through
/// `pagination::build_paginated_query` (#70 — `SHOW search_path` with a
/// `limit` param produced that exact syntax error because pagination was
/// being applied to every result-set-bearing statement, not just the ones
/// whose grammar supports it).
///
/// This deliberately does NOT match the builtin driver's narrower
/// `is_select_query` (literal `SELECT` prefix only): that function also
/// routes `WITH`/`VALUES`/`TABLE`/`EXPLAIN` through client-side capping
/// instead of SQL pagination, which silently breaks page 2+ for a paginated
/// CTE even though `WITH ... SELECT ... LIMIT n OFFSET m` is valid syntax.
/// Copying that classification 1:1 would trade a real syntax-error bug for
/// a real (if quieter) pagination-correctness bug — not a net parity win.
fn supports_trailing_limit_clause(query: &str) -> bool {
    let upper = strip_leading_sql_comments(query).to_uppercase();
    upper.starts_with("SELECT")
        || upper.starts_with("WITH")
        || upper.starts_with("VALUES")
        || upper.starts_with("TABLE")
        || upper.starts_with("EXPLAIN")
}

#[cfg(test)]
#[path = "query_tests.rs"]
mod query_tests;
