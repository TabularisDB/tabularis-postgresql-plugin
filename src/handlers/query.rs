//! Query execution handlers.

use deadpool_postgres::Object as PgClient;
use serde_json::{json, Value};
use std::time::Instant;

use crate::client;
use crate::extract::extract_value;
use crate::models::{inner_params, ConnectionParams};
use crate::rpc::{error_response, ok_response};

pub async fn execute_query(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let query = params.get("query").and_then(Value::as_str).unwrap_or("");
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map(|v| v as u32);
    let page = params.get("page").and_then(Value::as_u64).unwrap_or(1) as u32;
    let schema = params.get("schema").and_then(Value::as_str);

    match exec_query(&conn_params, query, limit, page, schema).await {
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

    // Acquire ONE connection for the entire batch (session state must survive)
    let pool = match client::build_pool_pub(&conn_params).await {
        Ok(p) => p,
        Err(e) => return error_response(id, -32603, &e),
    };
    let pg_client = match pool.get().await {
        Ok(c) => c,
        Err(e) => {
            return error_response(
                id,
                -32603,
                &format!("Connection failed: {}", client::format_pool_error(&e)),
            )
        }
    };

    if let Some(s) = schema {
        let set_path = format!("SET search_path TO \"{}\"", s.replace('"', "\"\""));
        if let Err(e) = pg_client.batch_execute(&set_path).await {
            return error_response(
                id,
                -32603,
                &format!("Failed to set search_path: {}", client::format_pg_error(&e)),
            );
        }
    }

    let mut results: Vec<Value> = Vec::new();

    for query in &queries {
        let start = Instant::now();
        let outcome = exec_query_on_client(&pg_client, query, limit, page).await;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

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

    ok_response(id, json!(results))
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

    match exec_query(&conn_params, &explain_sql, None, 1, schema).await {
        Ok(result) => {
            // The host wraps this in ExplainQueryOutput::Plan { plan: res }
            // We just return the raw explain JSON from the first row/col
            if let Some(rows) = result.get("rows").and_then(Value::as_array) {
                if let Some(first_row) = rows.first().and_then(Value::as_array) {
                    if let Some(plan_json) = first_row.first() {
                        return ok_response(id, plan_json.clone());
                    }
                }
            }
            ok_response(id, result)
        }
        Err(e) => error_response(id, -32603, &e),
    }
}

/// Execute a SQL query and return a QueryResult-shaped JSON value.
async fn exec_query(
    conn_params: &ConnectionParams,
    query: &str,
    limit: Option<u32>,
    page: u32,
    schema: Option<&str>,
) -> Result<Value, String> {
    let pool = client::build_pool_pub(conn_params).await?;
    let pg_client = pool
        .get()
        .await
        .map_err(|e| format!("Connection failed: {}", client::format_pool_error(&e)))?;

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
