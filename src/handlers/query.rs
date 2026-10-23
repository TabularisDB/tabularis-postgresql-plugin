//! Query execution handlers.

use deadpool_postgres::Object as PgClient;
use serde_json::{json, Value};
use std::time::Instant;
use tokio_postgres::types::{ToSql, Type};

use crate::cancel;
use crate::client;
use crate::extract::extract_value;
use crate::models::{inner_params, ConnectionParams};
use crate::rpc::{error_response, ok_response};
use crate::session;
use crate::utils::identifiers::quote_identifier;

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
        None => client::get_pool_client(conn_params).await?,
    };

    // Register a cancel handle for this in-flight batch, keyed by the request
    // id (#126). The guard deregisters on drop — at every return path below,
    // Ok or Err — so the map can't leak a finished query and a late cancel
    // for a reused id can't target a later one. The cancel only needs the
    // token (captured into the action) and the connection params, never the
    // borrowed client.
    // Held across the whole batch so a host `cancel(request_id)` can stop the
    // in-flight shared-client query. Re-armed after each one-shot routing
    // below (the `!in_transaction` branch) — see there for why it can't be a
    // single guard.
    let mut cancel_guard = Some(cancel::CancelGuard::register(
        request_id.as_u64(),
        &pg_client,
        conn_params.clone(),
    ));

    // Applying search_path to a reused connection would run inside the open
    // transaction and change what the rest of it sees.
    if let Some(s) = schema.filter(|_| !reused) {
        apply_search_path(&pg_client, s).await?;
    }

    let mut results: Vec<Value> = Vec::new();
    for query in queries {
        let start = Instant::now();

        // On a transaction-mode pooler, a result-set query with non-builtin
        // column types needs its column types pre-resolved before the data
        // fetch, or `query_typed`'s internal `typeinfo_statement` fires
        // mid-stream and deadlocks on large results (#169, same root as
        // #166/#168). `exec_query` routes single queries to the one-shot
        // pinned path (`exec_query_pooled_txn`) for this; the batch path
        // bypasses `exec_query`, so it makes the same routing decision here.
        // Two sub-cases, keyed on `in_transaction` (not `reused`):
        //
        // * `in_transaction` (the user's `BEGIN` has pinned the backend on
        //   this shared connection — either a session-pinned batch reusing a
        //   prior `BEGIN`, or a bare batch whose earlier statement was
        //   `BEGIN`): a plain `prepare` inside that transaction is safe (no
        //   reassignment can orphan the named statement) and keeps the
        //   SELECT on the same connection where transaction-local state
        //   (temp tables, SET variables) is visible. Run
        //   `exec_query_on_client` with `pre_resolve_types = true`.
        // * Not `in_transaction` (no open transaction pins the backend): a
        //   plain `prepare` could collide across reassignment (the `s0`→`s1`
        //   shift that killed the removed pre-warm, ca33b98). Route to the
        //   one-shot pinned path (`exec_query_pooled_txn`) — the proven
        //   #164 mechanism. The one-shot client dies with its stale
        //   client-side type cache; the shared batch client sits idle for
        //   that one query (acceptable: correctness over connection reuse
        //   for one statement; when there's no transaction there's no
        //   transaction-local state to lose by using a different connection).
        //
        // Non-result-set statements (DDL, writes) and direct connections
        // stay on the shared cached client — the unnamed protocol is
        // pooler-safe and needs no pinning (#149, #168).
        let outcome = if conn_params.is_transaction_pooler() && returns_result_set(query) {
            if in_transaction {
                // The user's open transaction pins the backend on this
                // connection. Pre-resolve column types via `prepare` inside
                // it (safe — no reassignment can orphan the named statement)
                // so `query_typed`'s `get_type` is a cache hit (no mid-stream
                // `typeinfo_statement`, no deadlock). Keeps the SELECT on the
                // shared connection where transaction-local state is visible.
                // `cancel_guard` stays registered for this shared client.
                exec_query_on_client(
                    &pg_client,
                    query,
                    limit,
                    page,
                    conn_params.is_transaction_pooler(),
                    /* pre_resolve_types = */ true,
                )
                .await
            } else {
                // No open transaction pins the backend. Route to the one-shot
                // pinned path. The one-shot client acquired inside
                // `exec_query_pooled_txn` is its own connection (never
                // returned to a pool, so no stale client-side type cache
                // hazard — #164's core property).
                //
                // Cancel-guard coexistence: `exec_query_pooled_txn` registers
                // its own `CancelGuard` under the SAME `request_id` we hold
                // below. `cancel::insert_action` is a `HashMap::insert`
                // (overwrite), and the one-shot's guard `Drop` removes the
                // key entirely — so if we kept a single guard across the whole
                // batch, the one-shot would evict it and every subsequent
                // shared-client query would lose cancel coverage (#126's
                // "a late cancel is never a no-op for an in-flight query"
                // guarantee). Take+drop our guard so the one-shot owns the
                // map entry while it runs, then re-register for the shared
                // client before the next iteration.
                drop(cancel_guard.take());
                let result =
                    exec_query_pooled_txn(conn_params, query, limit, page, schema, request_id)
                        .await;
                cancel_guard = Some(cancel::CancelGuard::register(
                    request_id.as_u64(),
                    &pg_client,
                    conn_params.clone(),
                ));
                result
            }
        } else {
            // Non-result-set (DDL, writes) or direct connection — the
            // unnamed protocol is pooler-safe and needs no pre-resolve.
            // `cancel_guard` stays registered for this shared client.
            exec_query_on_client(
                &pg_client,
                query,
                limit,
                page,
                conn_params.is_transaction_pooler(),
                !conn_params.is_transaction_pooler(),
            )
            .await
        };
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
    // `cancel_guard` drops here, deregistering the cancel handle before the
    // connection is re-pinned or released below, so a cancel racing the end
    // of the batch can't target a query that's already done. It is `Some`
    // here except after a final one-shot query (which left it `None`); a `None`
    // drop is a no-op, so the map is clean either way.
    drop(cancel_guard);

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
    // Transaction-pooler result-set queries take a pinned one-shot path
    // (#160 / #163): tokio-postgres's internal cached named `typeinfo_statement`
    // — fired for non-builtin result column types (custom enums, composites,
    // extension types like `citext`/Supabase `auth.uid`) — can deadlock when
    // it's fired mid-stream during `Row` deserialization (the typeinfo reply is
    // queued behind the query's `DataRow`s, but `query_typed` won't drain them
    // until `get_type` returns → deadlock once the row data fills the channel
    // buffer). On a transaction-mode pooler the same interleave additionally
    // orphans the statement across backend reassignment (`42P05` on PgBouncer,
    // or a hang on Supavisor). Pinning the backend with an explicit
    // transaction, clearing stale statements, pre-resolving column types, then
    // running the #149-unnamed data fetch on a connection that is *not*
    // returned to the pool closes the pooler case. See `exec_query_pooled_txn`.
    //
    // Direct (non-pooler) result-set queries have the same interleave but no
    // reassignment hazard, so they pre-resolve column types via a plain
    // `prepare` on the shared cached client inside `exec_query_on_client`
    // (#166) — cheaper than the one-shot path (preserves pool reuse) and safe
    // (no backend reassignment to collide with).
    //
    // Non-result-set statements (DDL, writes) stay on the shared cached
    // client — `execute_typed` is unnamed and needs no pinning. The batch
    // path (`run_batch_in_session`) mirrors this routing for its own queries
    // rather than calling here: a pooler result-set query with no open
    // transaction routes to `exec_query_pooled_txn`, and one inside an open
    // transaction pre-resolves on the shared client (see `run_batch_in_session`).
    if conn_params.is_transaction_pooler() && returns_result_set(query) {
        return exec_query_pooled_txn(conn_params, query, limit, page, schema, request_id).await;
    }

    let pg_client = client::get_pool_client(conn_params).await?;

    // Register a cancel handle for this in-flight query (#126). The guard
    // deregisters on drop — at every return path below, Ok or Err — so a
    // late cancel for a reused id can't target a later query.
    let _cancel_guard =
        cancel::CancelGuard::register(request_id.as_u64(), &pg_client, conn_params.clone());

    // Set search_path if schema is specified
    if let Some(s) = schema {
        apply_search_path(&pg_client, s).await?;
    }

    exec_query_on_client(
        &pg_client,
        query,
        limit,
        page,
        conn_params.is_transaction_pooler(),
        !conn_params.is_transaction_pooler(),
    )
    .await
}

/// Bounded lifetime of the transaction-pooler pinned path. Kept below the
/// host's plugin-call timeout (120s) so a wedged pooler connection surfaces
/// as an actionable error here instead of the opaque "timed out after 120s"
/// the host would otherwise emit. Long enough that a legitimately slow query
/// on a healthy pooler isn't cut short, short enough that a Supavisor-style
/// stall never reaches the host timeout (#160 / #163).
const POOLED_TXN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// Run a result-set query on a transaction-mode pooler through an explicit
/// pinned transaction on a one-shot client (#160 / #163).
///
/// On a transaction-mode pooler, `BEGIN` pins the client to one server backend
/// for the whole sequence. While pinned:
///
/// 1. `DEALLOCATE ALL` clears stale server-side prepared statements on that
///    backend (left behind by a previous assignment's `typeinfo_statement`).
/// 2. `search_path` is set locally (inside the transaction) if a schema was
///    selected.
/// 3. `prepare(&query)` (the original user query, before pagination) resolves
///    every result column's type via tokio's `typeinfo_statement` — *while the
///    backend is pinned*, so the named statement can't be orphaned
///    mid-resolution, and *before* the data fetch drains the response stream,
///    so tokio never interleaves a fresh named prepare+query into a mid-drain
///    unnamed batch (the Supavisor hang mechanism). Pagination's appended
///    `LIMIT`/`OFFSET` does not change result column types, so the warmed
///    cache applies to the paginated data fetch too.
/// 4. `query_typed(&final_query, &[])` runs the data fetch using the
///    unnamed protocol — #149's pooler-safe path is preserved (#1097 stays
///    fixed), and binary `Row` decoding / `extract_value` is unchanged.
/// 5. A final `DEALLOCATE ALL` clears the statements `prepare` created.
/// 6. `COMMIT` ends the pin.
///
/// The client comes from [`client::oneshot_pool_client`] — a transient pool
/// not inserted into the global cache. Both the client and its pool drop at
/// the end, so the connection (and tokio-postgres's private cached
/// `typeinfo_statement` handle, which `DEALLOCATE ALL` can't clear) dies with
/// it. No stale client-side type cache is ever returned to the pool.
///
/// The whole sequence is bounded by [`POOLED_TXN_TIMEOUT`]; a stall is an
/// actionable error, not a 120s host timeout. On any error after `BEGIN` we
/// best-effort `ROLLBACK` (the one-shot client drop closes the connection
/// regardless).
async fn exec_query_pooled_txn(
    conn_params: &ConnectionParams,
    query: &str,
    limit: Option<u32>,
    page: u32,
    schema: Option<&str>,
    request_id: &Value,
) -> Result<Value, String> {
    // One-shot client: not returned to any pool, so its caches die with it.
    let pg_client = client::oneshot_pool_client(conn_params).await?;

    // Register a cancel handle for this in-flight query (#126). The guard
    // deregisters on drop — at every return path below, Ok or Err.
    let _cancel_guard =
        cancel::CancelGuard::register(request_id.as_u64(), &pg_client, conn_params.clone());

    // Wrap the pinned work in a plugin-owned timeout so a Supavisor-style
    // stall surfaces here instead of at the host's 120s call timeout. A
    // timeout means the pooler wedged; the one-shot client drops and its
    // connection (and stale caches) dies regardless of the COMMIT/ROLLBACK
    // below.
    match tokio::time::timeout(
        POOLED_TXN_TIMEOUT,
        pin_pooled_txn(&pg_client, query, limit, page, schema),
    )
    .await
    {
        Ok(inner) => inner,
        Err(_elapsed) => Err(format!(
            "Query timed out after {}s on a transaction-mode pooler. \
             This is the known #160/#163 failure mode when the pooler can't \
             track prepared statements for non-builtin result column types \
             (custom enums, composites, extension types). For PgBouncer, set \
             `max_prepared_statements > 0` in pgbouncer.ini; for poolers \
             without that setting, use a direct/session connection or the \
             pooler's session mode. See: \
             https://www.pgbouncer.org/config.html#max_prepared_statements",
            POOLED_TXN_TIMEOUT.as_secs()
        )),
    }
}

/// The pinned-transaction body, split out so [`exec_query_pooled_txn`] can
/// wrap it in a timeout without holding the one-shot client's lifetime in a
/// `Future`. `pg_client` must already be acquired.
///
/// Every error path after `BEGIN` except a `COMMIT` failure best-effort
/// rolls back before returning. A failed `COMMIT` doesn't need it: PostgreSQL
/// itself rolls the transaction back when `COMMIT` fails (e.g. on a deferred
/// constraint). In all cases the one-shot client drops on return and the
/// server aborts any leftover transaction when the connection closes, so the
/// explicit rollback is belt-and-suspenders — but explicit is cheaper than
/// relying on that.
async fn pin_pooled_txn(
    pg_client: &PgClient,
    query: &str,
    limit: Option<u32>,
    page: u32,
    schema: Option<&str>,
) -> Result<Value, String> {
    // Pin the backend for the whole sequence.
    if let Err(e) = pg_client.batch_execute("BEGIN").await {
        return Err(client::format_pg_error(&e));
    }

    let result = run_pinned_body(pg_client, query, limit, page, schema).await;

    match result {
        Ok(value) => {
            if let Err(e) = pg_client.batch_execute("COMMIT").await {
                return Err(format!("COMMIT failed: {}", client::format_pg_error(&e)));
            }
            Ok(value)
        }
        Err(e) => {
            // best-effort rollback; the one-shot client drops and closes
            // regardless, so the server aborts the txn even if this fails.
            let _ = pg_client.batch_execute("ROLLBACK").await;
            Err(e)
        }
    }
}

/// The pinned-transaction steps between `BEGIN` and `COMMIT`/`ROLLBACK`:
/// clear stale statements, set search_path, pre-resolve column types, run
/// the unnamed data fetch, clear statements again. Any error propagates up
/// to [`pin_pooled_txn`]'s rollback branch.
async fn run_pinned_body(
    pg_client: &PgClient,
    query: &str,
    limit: Option<u32>,
    page: u32,
    schema: Option<&str>,
) -> Result<Value, String> {
    // Clear stale server-side prepared statements left by a previous
    // assignment's typeinfo query (or the connection's prior use).
    pg_client
        .batch_execute("DEALLOCATE ALL")
        .await
        .map_err(|e| format!("DEALLOCATE ALL failed: {}", client::format_pg_error(&e)))?;

    // Apply search_path inside the pinned transaction. `SET LOCAL` keeps the
    // change scoped to this transaction so it can't leak across pooler
    // reassignment — and since the backend is pinned for the transaction,
    // the `SET LOCAL` is guaranteed to land on the same backend that runs
    // the query.
    if let Some(s) = schema {
        apply_search_path_local(pg_client, s).await?;
    }

    // Pre-resolve result column types while the backend is pinned. This is
    // the core of the fix: `prepare`'s internal `get_type`/`typeinfo_statement`
    // round-trip runs inside the pin, before the data fetch drains anything.
    // A 26000 here (the pooler couldn't track the statement even while
    // pinned — only possible if it rejects named statements outright) is
    // surfaced via `format_query_error` so the user sees the actionable
    // `max_prepared_statements` guidance.
    let is_transaction_pooler = true;
    if let Err(e) = pg_client.prepare(query).await {
        return Err(client::format_query_error(&e, is_transaction_pooler));
    }

    // Data fetch: unnamed protocol (#149's pooler-safe path). Binary Row
    // Decoding + extract_value unchanged. `pre_resolve_types = false` because
    // `run_pinned_body` already pre-resolved via its own `prepare` above.
    let result = exec_query_on_client(
        pg_client,
        query,
        limit,
        page,
        is_transaction_pooler,
        /* pre_resolve_types = */ false,
    )
    .await;

    // Clear the statements `prepare` created before COMMIT. Errors here are
    // non-fatal — the one-shot client drops and closes regardless. Log and
    // continue to the result.
    if let Err(e) = pg_client.batch_execute("DEALLOCATE ALL").await {
        log::debug!(
            "pooled-txn trailing DEALLOCATE failed: {}",
            client::format_pg_error(&e)
        );
    }

    result
}

/// Read the connection's base `search_path` and set it locally (`SET LOCAL`)
/// for the current pinned transaction. Same base-snapshot logic as
/// [`apply_search_path`], but the `SET LOCAL` (rather than `SET`) keeps the
/// change scoped to this transaction so it can't leak across pooler
/// reassignment — and since the backend is pinned for the transaction, the
/// `SET LOCAL` is guaranteed to land on the same backend that runs the query.
async fn apply_search_path_local(pg_client: &PgClient, schema: &str) -> Result<(), String> {
    let read = pg_client
        .query_typed_one(
            "SELECT current_setting($1, true), current_setting('search_path')",
            &[(
                &client::BASE_SEARCH_PATH_GUC as &(dyn ToSql + Sync),
                Type::TEXT,
            )],
        )
        .await
        .map_err(|e| {
            format!(
                "Failed to read search_path: {}",
                client::format_pg_error(&e)
            )
        })?;
    let snapshot: Option<String> = read.try_get(0).map_err(|e| {
        format!(
            "Failed to read search_path: {}",
            client::format_pg_error(&e)
        )
    })?;
    let live: String = read.try_get(1).map_err(|e| {
        format!(
            "Failed to read search_path: {}",
            client::format_pg_error(&e)
        )
    })?;
    let current = snapshot.unwrap_or(live);
    let set_path = build_search_path_sql(schema, &current, true);
    pg_client
        .batch_execute(&set_path)
        .await
        .map_err(|e| format!("Failed to set search_path: {}", client::format_pg_error(&e)))
}

/// Build a `SET [LOCAL] search_path TO` statement that prepends `schema` onto
/// the connection's existing `search_path` rather than replacing it (#51), so
/// queries against objects in a different schema still resolve. `current`
/// is the verbatim value of `current_setting('search_path')`, emitted
/// unchanged — PostgreSQL accepts its own display form back as the right-hand
/// side of `SET search_path TO`, including the `"$user"` placeholder (probed
/// against a live database: `"$user"` set back verbatim still resolves to the
/// user's schema; unquoting it is a syntax error).
///
/// `local` selects `SET LOCAL` (scoped to the current transaction) over `SET`
/// (session-scoped). The pinned one-shot transaction path uses `SET LOCAL` so
/// the change can't leak across pooler reassignment; the shared cached path
/// uses `SET`.
fn build_search_path_sql(schema: &str, current: &str, local: bool) -> String {
    let kw = if local {
        "SET LOCAL search_path TO"
    } else {
        "SET search_path TO"
    };
    if current.trim().is_empty() {
        format!("{kw} {}", quote_identifier(schema))
    } else {
        format!("{kw} {}, {}", quote_identifier(schema), current)
    }
}

/// Read the connection's *base* `search_path` and prepend `schema` onto it
/// (#51). The base is the value captured at connection creation by the
/// `post_create` hook in `client.rs` (after any startup script) — stored in
/// the custom GUC `app.tabularis_base_search_path`. Reading the base rather
/// than the live `current_setting('search_path')` is what makes the prepend
/// idempotent across recycled pooled connections (`RecyclingMethod::Fast`
/// resets nothing): without it, a recycled connection would report an
/// already-prepended path and each call would stack another schema, leaking
/// a previous selection into object resolution.
///
/// If the snapshot GUC is unset (e.g. the connection predates the snapshot
/// hook, or the hook failed at creation), fall back to the live
/// `current_setting('search_path')` — degrading to the pre-fix behavior
/// rather than erroring, so an older pool isn't broken by the new code.
async fn apply_search_path(pg_client: &PgClient, schema: &str) -> Result<(), String> {
    // current_setting(name, true) returns NULL when the GUC is unset
    // (missing_ok), so the snapshot's absence is a clean fallback, not an
    // error.
    //
    // Uses the unnamed-statement protocol (`query_typed_one`) rather than
    // the named `query_one` so this is pooler-safe under transaction-mode
    // poolers (PgBouncer) — see `client`'s module note (tabularis#1097). The
    // GUC name is bound as `TEXT`; PostgreSQL's `current_setting(text, bool)`
    // accepts it directly.
    let read = pg_client
        .query_typed_one(
            "SELECT current_setting($1, true), current_setting('search_path')",
            &[(
                &client::BASE_SEARCH_PATH_GUC as &(dyn ToSql + Sync),
                Type::TEXT,
            )],
        )
        .await
        .map_err(|e| {
            format!(
                "Failed to read search_path: {}",
                client::format_pg_error(&e)
            )
        })?;
    let snapshot: Option<String> = read.try_get(0).map_err(|e| {
        format!(
            "Failed to read search_path: {}",
            client::format_pg_error(&e)
        )
    })?;
    let live: String = read.try_get(1).map_err(|e| {
        format!(
            "Failed to read search_path: {}",
            client::format_pg_error(&e)
        )
    })?;
    // Prefer the stable base snapshot; fall back to the live value if it's
    // unset so an older pool (no snapshot hook) keeps working.
    let current = snapshot.unwrap_or(live);
    let set_path = build_search_path_sql(schema, &current, false);
    pg_client
        .batch_execute(&set_path)
        .await
        .map_err(|e| format!("Failed to set search_path: {}", client::format_pg_error(&e)))
}

/// Execute a query on an existing client (used by both single and batch execution).
///
/// `is_transaction_pooler` selects error-message phrasing (definitive when
/// the host declared a transaction pooler, conditional when it didn't).
/// `pre_resolve_types` controls whether column types are pre-resolved via
/// `prepare` before the `query_typed` data fetch (#166/#168). The two are
/// independent: a session-pinned pooler connection passes
/// `is_transaction_pooler = true` (for correct error phrasing) AND
/// `pre_resolve_types = true` (because the backend is pinned by the user's
/// open transaction, so the `prepare` is safe) — see `run_batch_in_session`.
async fn exec_query_on_client(
    pg_client: &PgClient,
    query: &str,
    limit: Option<u32>,
    page: u32,
    is_transaction_pooler: bool,
    pre_resolve_types: bool,
) -> Result<Value, String> {
    // Check if the statement returns a result set
    if !returns_result_set(query) {
        // Unnamed-statement protocol (`execute_typed`) — pooler-safe under
        // transaction-mode poolers (PgBouncer); see `client`'s module note
        // (tabularis#1097). No params for this branch.
        let affected = pg_client
            .execute_typed(query, &[])
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

    // Pre-resolve result column types before the data fetch (#166 for direct
    // connections, #169 for session-pinned pooler connections inside an open
    // transaction). tokio-postgres's `query_typed` resolves non-builtin column
    // types (`get_type` → the internal named `typeinfo_statement`) *inside*
    // its `RowDescription` loop, mid-stream, over the same channel delivering
    // the query's `DataRow`s. The typeinfo reply is queued behind the pending
    // rows, but `query_typed` won't read the rows until `get_type` returns →
    // a deadlock once the row data fills the bounded `responses` channel +
    // socket buffer (hence the size threshold — small results pass, large
    // ones hang). This is not pooler-specific; it deadlocks on any
    // connection.
    //
    // `prepare` runs Parse+Describe+Sync with no Execute/no DataRows, so its
    // `get_type` interleave has an empty channel and completes cleanly,
    // warming `CachedTypeInfo.types` on the shared `InnerClient`. The
    // subsequent `query_typed` then hits that cache (`get_type`'s
    // `client.type_(oid)` returns early) → no `typeinfo_statement` mid-drain
    // → no deadlock, regardless of result size. This is equivalent to rc.8's
    // behavior (rc.8 used the named `query()` path which resolved types in
    // its prepare's Describe phase; this uses a separate `prepare` + the
    // unnamed `query_typed`, but the outcome — types resolved before the
    // data fetch, no mid-stream interleave — is the same), which worked.
    //
    // Gated on `pre_resolve_types`, set by the caller for the two cases where
    // a plain `prepare` is safe:
    //  * Direct connections (`!is_transaction_pooler`, the #166 path): no
    //    backend reassignment to collide with, so a named statement is fine.
    //  * Session-pinned pooler connections inside an open transaction (the
    //    #169 batch path, `in_transaction`): the user's `BEGIN` pins the
    //    backend, so a named statement can't be orphaned by reassignment.
    // The remaining pooler case — a result-set query with NO open
    // transaction — can't safely `prepare` on the shared client (the named
    // statement would collide across the `s0`→`s1` reassignment that killed
    // the removed pre-warm, commit ca33b98), so `run_batch_in_session` routes
    // it to the #164 one-shot pinned path instead of setting
    // `pre_resolve_types`.
    //
    // The pre-resolve is non-fatal on failure: a prepare error is swallowed
    // (`.ok()`) and the `query_typed` below runs anyway — for builtin types
    // `get_type` is always a `Type::from_oid` cache hit (no
    // `typeinfo_statement`, no hang), and a prepare failure on a valid
    // result-set query is rare. The `Statement` is kept alive (not dropped
    // immediately) so its `.columns()` can be reused by the empty-rows branch
    // below instead of preparing a second time. (The warmed type cache
    // persists on the shared `InnerClient` regardless of the `Statement`'s
    // lifetime — `Statement::Drop` sends a server `Close` but does not clear
    // `CachedTypeInfo.types` — so holding it alive is for `.columns()` reuse,
    // not cache persistence.)
    let prepared = if pre_resolve_types {
        pg_client.prepare(&final_query).await.ok()
    } else {
        None
    };

    // Execute query
    // Unnamed-statement protocol (`query_typed`) — pooler-safe under
    // transaction-mode poolers (PgBouncer); see `client`'s module note
    // (tabularis#1097). No params for user-issued queries.
    //
    // On a transaction-mode pooler, a prepared-statement error (26000/42P05)
    // from tokio-postgres's internal `typeinfo_statement` (fired by
    // non-builtin column types) is rewritten with actionable guidance via
    // `format_query_error` (#157).
    let rows = pg_client
        .query_typed(&final_query, &[])
        .await
        .map_err(|e| client::format_query_error(&e, is_transaction_pooler))?;

    if rows.is_empty() {
        // Get columns from the statement if possible. When the pre-resolve
        // ran and succeeded, reuse its `.columns()` instead of preparing a
        // second time. When the pre-resolve didn't run (or failed), try once
        // more for column discovery only if `prepare` is safe here (direct
        // connections, or session-pinned pooler connections where the backend
        // is pinned). On a transaction-mode pooler where `prepare` is unsafe
        // (the one-shot path handles its own pre-resolve), return `columns:
        // []` — the host handles empty columns gracefully (#157,
        // tabularis#1097).
        let columns: Vec<String> = if let Some(stmt) = prepared.as_ref() {
            stmt.columns()
                .iter()
                .map(|c| c.name().to_string())
                .collect()
        } else if pre_resolve_types {
            // Pre-resolve was attempted but failed; try once more for column
            // discovery (the pre-#166 behavior for direct empty result sets).
            if let Ok(stmt) = pg_client.prepare(&final_query).await {
                stmt.columns()
                    .iter()
                    .map(|c| c.name().to_string())
                    .collect()
            } else {
                vec![]
            }
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
