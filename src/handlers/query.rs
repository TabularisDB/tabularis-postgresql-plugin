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
    // Whether any earlier non-result-set statement in this batch set
    // session-local state (a session `SET`, a `CREATE TEMP TABLE`) that a
    // later result-set `SELECT` would see only on this shared connection —
    // not on a fresh one-shot connection (#173). Sticky for the rest of the
    // batch: once set, a later `SELECT` that needs to see it stays on the
    // shared client. See the routing comment below.
    let mut session_state_set = false;

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
        // The routing keys on whether the SELECT must run on this shared
        // connection, with three cases (plus the cancel-guard note):
        //
        // * `in_transaction` (the user's `BEGIN` has pinned the backend on
        //   this shared connection — either a session-pinned batch reusing a
        //   prior `BEGIN`, or a bare batch whose earlier statement was
        //   `BEGIN`): a plain `prepare` inside that transaction is safe (no
        //   reassignment can orphan the named statement) and keeps the
        //   SELECT on the same connection where transaction-local state
        //   (temp tables, SET variables) is visible. Run
        //   `exec_query_on_client` with `pre_resolve_types = true`.
        // * `session_state_set` (a prior non-result-set statement in this
        //   bare batch set session-local state — a session `SET` or a
        //   `CREATE TEMP TABLE` outside a transaction — #173): the SELECT
        //   must stay on this shared connection to see that state. A fresh
        //   one-shot connection never saw the `SET`/temp table (the one-shot
        //   applies only the host's `schema` via `SET LOCAL`), so routing it
        //   there would silently lose the state — wrong float precision, or
        //   `ERROR: relation "t" does not exist`. Keep it on the shared
        //   client with `pre_resolve_types = true` (the same path as the
        //   `in_transaction` case). On a real PgBouncer a session `SET`
        //   between autocommit statements is already lost to backend
        //   reassignment (pre-existing on `main`), so this primarily
        //   restores the direct-PostgreSQL + `pooler_mode=transaction`
        //   config (one TCP connection = one backend, held) and the
        //   temp-table case; see `sets_session_local_state` for the
        //   classifier and why transaction-scoped forms are excluded.
        // * Otherwise (no open transaction, no prior session state): route
        //   to the one-shot pinned path (`exec_query_pooled_txn`) — the
        //   proven #164 mechanism. The one-shot client dies with its stale
        //   client-side type cache; the shared batch client sits idle for
        //   that one query (acceptable: when there's no transaction and no
        //   session state there's nothing to lose by using a different
        //   connection). A plain `prepare` here could collide across
        //   reassignment (the `s0`→`s1` shift that killed the removed
        //   pre-warm, ca33b98), so the one-shot is the safe choice.
        //
        // Non-result-set statements (DDL, writes) and direct connections
        // stay on the shared cached client — the unnamed protocol is
        // pooler-safe and needs no pinning (#149, #168).
        let shared_client = in_transaction || session_state_set;
        // Both arms of the pooler-result-set branch converge on `QueryOutcome`;
        // the one-shot path wraps its `Result` with `txn_aborted=false` (it
        // runs inside its *own* BEGIN/COMMIT and ROLLBACKs internally on error,
        // so it never leaves the *user's* transaction aborted — #164).
        let outcome: QueryOutcome =
            if conn_params.is_transaction_pooler() && returns_result_set(query) {
                if shared_client {
                    // Run on this shared connection (the user's open transaction
                    // pins it, or a prior statement set session-local state the
                    // SELECT must see — #173). Pre-resolve column types via
                    // `prepare` (safe — the backend is pinned by an open
                    // transaction, or there's no pooler reassignment to collide
                    // with on the direct+`pooler_mode=transaction` config) so
                    // `query_typed`'s `get_type` is a cache hit (no mid-stream
                    // `typeinfo_statement`, no deadlock). Keeps the SELECT on
                    // the shared connection where the session/transaction-local
                    // state is visible. `cancel_guard` stays registered for
                    // this shared client. `in_transaction` is threaded so a
                    // prepare failure inside the open transaction short-circuits
                    // (#170) rather than cascading into 25P02.
                    exec_query_on_client(
                        &pg_client,
                        query,
                        limit,
                        page,
                        conn_params.is_transaction_pooler(),
                        /* pre_resolve_types = */ true,
                        /* in_transaction = */ in_transaction,
                    )
                    .await
                } else {
                    // No open transaction pins the backend and no prior statement
                    // set session state. Route to the one-shot pinned path. The
                    // one-shot client acquired inside `exec_query_pooled_txn` is
                    // its own connection (never returned to a pool, so no stale
                    // client-side type cache hazard — #164's core property).
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
                    QueryOutcome {
                        result,
                        txn_aborted: false,
                    }
                }
            } else {
                // Non-result-set (DDL, writes) or direct connection — the
                // unnamed protocol is pooler-safe and needs no pre-resolve.
                // `cancel_guard` stays registered for this shared client.
                // `in_transaction` is threaded for the direct-connection batch
                // path inside a user's `BEGIN` (pre-existing from #168): a
                // prepare failure there aborts the txn the same way (#170).
                exec_query_on_client(
                    &pg_client,
                    query,
                    limit,
                    page,
                    conn_params.is_transaction_pooler(),
                    !conn_params.is_transaction_pooler(),
                    /* in_transaction = */ in_transaction,
                )
                .await
            };
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

        in_transaction = in_transaction_after_outcome(
            transaction_effect(query),
            outcome.result.is_ok(),
            in_transaction,
            outcome.txn_aborted,
        );
        // Track session-local state set by a non-result-set statement so a
        // later result-set SELECT in this bare batch routes to the shared
        // client above instead of the one-shot (#173). A `SET`/`CREATE TEMP
        // TABLE` here flips the flag; result-set statements and transaction
        // verbs don't, and `SET LOCAL` deliberately doesn't (transaction-
        // scoped, not session-local). Sticky once set. Gated on success, like
        // `in_transaction` above: a `SET`/`CREATE TEMP TABLE` the server
        // rejected established no session state, so it must not steer a later
        // SELECT onto the shared client (which, outside a transaction on a
        // real pooler, runs an unpinned `prepare`).
        if outcome.result.is_ok() && sets_session_local_state(query) {
            session_state_set = true;
        }

        match outcome.result {
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
    // transaction routes to `exec_query_pooled_txn`, *unless* a prior
    // statement in the batch set session-local state (`session_state_set`,
    // #173), in which case it pre-resolves on the shared client (see
    // `run_batch_in_session`); one inside an open transaction likewise
    // pre-resolves on the shared client.
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

    let outcome = exec_query_on_client(
        &pg_client,
        query,
        limit,
        page,
        conn_params.is_transaction_pooler(),
        !conn_params.is_transaction_pooler(),
        /* in_transaction = */
        false, // single-query path has no session, never an open txn
    )
    .await;
    // `txn_aborted` can only be true inside an open transaction, which this
    // single-query path never has. Unwrap to the inner `Result`.
    debug_assert!(
        !outcome.txn_aborted,
        "single-query path is never in a transaction; txn_aborted must be false"
    );
    outcome.result
}

/// Bounded lifetime of the transaction-pooler pinned path *and* the
/// `query_typed`/column-discovery paths in `exec_query_on_client` when the
/// pre-resolve `prepare` failed (#170). Kept below the host's plugin-call
/// timeout (120s) so a wedged pooler connection surfaces as an actionable
/// error here instead of the opaque "timed out after 120s" the host would
/// otherwise emit. Long enough that a legitimately slow query on a healthy
/// pooler isn't cut short, short enough that a Supavisor-style stall never
/// reaches the host timeout (#160 / #163).
const POOLED_TXN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// The outcome of `exec_query_on_client`, carrying a flag the batch caller needs
/// to decide whether the transaction it was tracking is still open (#170).
///
/// `txn_aborted` is `true` only when the pre-resolve `prepare` failed *inside an
/// open transaction* and the handler short-circuited — ROLLBACK'd the aborted
/// transaction server-side and returned the prepare's real error instead of
/// running `query_typed` into a guaranteed 25P02 ("current transaction is
/// aborted"). The caller (`run_batch_in_session`) treats `txn_aborted=true` as
/// `Closes`-equivalent: force `in_transaction=false` so the (now-healthy)
/// connection is not re-pinned as if its transaction were still open.
///
/// It is `false` on every other path: success, a prepare failure *outside* a
/// transaction (the recoverable swallow — a 42P05 on a real PgBouncer bare
/// batch where `query_typed` may still succeed), a `query_typed`/extraction
/// error, or a timeout fired outside a transaction.
struct QueryOutcome {
    result: Result<Value, String>,
    txn_aborted: bool,
}

/// Whether a failed pre-resolve `prepare` should short-circuit (ROLLBACK +
/// surface the real error) instead of swallowing and running `query_typed`
/// (#170). Extracted as a pure helper so the decision is unit-testable without
/// a live database.
///
/// The swallow is *not always wrong*: outside an open transaction a prepare
/// failure can be recoverable — e.g. a 42P05 on a real PgBouncer bare batch
/// where `query_typed`'s unnamed protocol sidesteps the lost named statement
/// and still succeeds (#174's `session_state_set` path). Inside an open
/// transaction, a prepare Parse-time error (42P01, 42501, …) aborts the
/// transaction server-side, so `query_typed` is guaranteed to fail with 25P02
/// instead of the real error — the cascade #170 fixes. The `in_transaction`
/// gate is what separates those two cases.
fn should_short_circuit_on_prepare_err(in_transaction: bool) -> bool {
    in_transaction
}

/// Pure inner: classify whether an error aborted the open transaction, given
/// whether the error is a server `DbError` and whether a transaction is open.
/// Extracted so the classification is unit-testable without a live
/// `tokio_postgres::Error` (whose `DbError` can't be constructed without a
/// real server response — same constraint that drove `pooler_error_lead`'s
/// extraction in `client.rs`).
///
/// Companion to `should_short_circuit_on_prepare_err` (#170): that helper
/// decides whether to short-circuit *before* `query_typed` runs (a prepare
/// failure); this one classifies *after* a `query_typed`/`execute_typed`
/// runtime error (the #181 path). Together they are the two faces of the 25P02
/// cascade — #170 (prepare) + #181 (runtime).
fn error_aborts_open_txn_inner(is_db_error: bool, in_transaction: bool) -> bool {
    if !in_transaction {
        return false;
    }
    // A `DbError` is a server ERROR-class response — every one puts an open
    // transaction block into the 'E'/failed state (WARNING/NOTICE never arrive
    // as a query `Error`; they're delivered out-of-band as `AsyncMessage::Notice`,
    // so every `Error` carrying a `DbError` is ERROR-class). A non-`DbError`
    // error is transport/timeout/client-side — the server's verdict is unknown,
    // so conservatively assume aborted (the rare `FromSql` over-fire, where
    // the txn is actually untouched, is documented in `error_aborted_open_txn`).
    is_db_error || true
}

/// Whether a `query_typed`/`execute_typed` error inside an open transaction
/// aborted the transaction server-side (#181). See
/// `error_aborts_open_txn_inner` for the pure logic.
///
/// A server `DbError` (any ERROR-class response) always aborts an open
/// transaction. Non-`DbError` errors (transport/timeout) are conservatively
/// assumed to have aborted it — the server's verdict is unknown and leaving a
/// possibly-aborted transaction on a pooled connection would poison the pool
/// for the next borrower.
///
/// Residual: `Kind::FromSql` (a client-side row-decode failure) fires *after*
/// the server returned rows successfully, so the transaction is NOT aborted
/// by it — but `Kind` is private in tokio-postgres (only `is_closed()` leaks
/// one variant), so `FromSql` is indistinguishable from `Timeout`/`Closed`/
/// `Io` via the public API. The conservative `true` over-fires `FromSql`,
/// issuing a spurious ROLLBACK on a still-valid transaction. This is rare (a
/// bug in our type conversion, not normal operation) and recoverable (the
/// user re-issues `BEGIN`); it's the accepted cost of not poisoning the pool
/// on the more common `Timeout`/`Closed`/`Io` cases. The plugin's query/execute
/// paths pass `&[]` (no params), so `Kind::ToSql` never fires here at all.
///
/// Returns `false` when not `in_transaction` (nothing to abort).
fn error_aborted_open_txn(e: &tokio_postgres::Error, in_transaction: bool) -> bool {
    error_aborts_open_txn_inner(e.as_db_error().is_some(), in_transaction)
}

/// Recompute `in_transaction` after a batch statement, honoring the
/// `txn_aborted` signal from `exec_query_on_client` (#170). Extracted as a
/// pure helper so the recompute logic is unit-testable without a live
/// database.
///
/// When `txn_aborted=true`, the handler already ROLLBACK'd the aborted
/// transaction — force `false` regardless of the statement's
/// `transaction_effect`, so the connection is released rather than re-pinned
/// as if its (now-dead) transaction were still open. Otherwise defer to the
/// normal state machine: a failed `COMMIT` still closes (`Closes` arm), a
/// failed ordinary statement keeps the prior state (`None` arm).
fn in_transaction_after_outcome(
    effect: TransactionEffect,
    succeeded: bool,
    before: bool,
    txn_aborted: bool,
) -> bool {
    if txn_aborted {
        return false;
    }
    effect.in_transaction_after(succeeded, before)
}

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
    // `in_transaction = true` because this runs inside `pin_pooled_txn`'s own
    // BEGIN/COMMIT wrapper — but since `pre_resolve_types = false`, no prepare
    // runs in `exec_query_on_client` here, so the #170 short-circuit can't
    // fire (`prepare_err` is `None`); `txn_aborted` is always `false` on this
    // path. (`run_pinned_body`'s own `prepare` above already surfaced its
    // error directly at line 734-736.)
    let outcome = exec_query_on_client(
        pg_client,
        query,
        limit,
        page,
        is_transaction_pooler,
        /* pre_resolve_types = */ false,
        /* in_transaction = */ true,
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

    debug_assert!(
        !outcome.txn_aborted,
        "run_pinned_body passes pre_resolve_types=false; the #170 short-circuit can't fire"
    );
    outcome.result
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
/// `prepare` before the `query_typed` data fetch (#166/#168). `in_transaction`
/// is whether this query is running inside an *open* transaction (a
/// session-pinned batch reusing a prior `BEGIN`, or a bare batch whose earlier
/// statement was `BEGIN`) — it gates the #170 25P02 short-circuit: a failed
/// `prepare` inside an open transaction aborts it server-side, so
/// `query_typed` is guaranteed to 25P02 rather than surface the real error.
/// The two flags are independent: a session-pinned pooler connection passes
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
    in_transaction: bool,
) -> QueryOutcome {
    // Check if the statement returns a result set
    if !returns_result_set(query) {
        // Unnamed-statement protocol (`execute_typed`) — pooler-safe under
        // transaction-mode poolers (PgBouncer); see `client`'s module note
        // (tabularis#1097). No params for this branch.
        let affected = pg_client.execute_typed(query, &[]).await;
        // #181: a runtime error (a failed DDL/write, a constraint violation)
        // inside an open transaction aborts it server-side. Classify on the
        // raw `tokio_postgres::Error` before formatting, so the caller can
        // force `in_transaction=false` and the aborted connection isn't
        // re-pinned (which would 25P02-cascade the next batch's first
        // statement). Gate on `transaction_effect(query) != Closes` so a
        // failed `COMMIT`/`ROLLBACK`/`END` — which already ends the
        // transaction server-side and is handled by the existing `Closes`
        // path in `in_transaction_after_outcome` — is NOT given a spurious
        // ROLLBACK + "rolled back" notice (those verbs already rolled back).
        let txn_aborted = match &affected {
            Ok(_) => false,
            Err(e) => {
                error_aborted_open_txn(e, in_transaction)
                    && transaction_effect(query) != TransactionEffect::Closes
            }
        };
        if txn_aborted {
            // Best-effort ROLLBACK; the transaction is aborted, so ROLLBACK is
            // the one command PostgreSQL still accepts in this state. A
            // failure here is logged, not fatal — the connection still drops
            // back to the pool and `RecyclingMethod::Fast` will surface a
            // stale-state issue on reuse if it ever matters.
            if let Err(rollback_err) = pg_client.batch_execute("ROLLBACK").await {
                log::warn!(
                    "ROLLBACK after an execute_typed error aborted a transaction failed: {rollback_err}"
                );
            }
        }
        let result = affected
            .map(|affected| {
                json!({
                    "columns": [],
                    "rows": [],
                    "affected_rows": affected,
                    "truncated": false,
                    "pagination": null,
                })
            })
            .map_err(|e| {
                let msg = client::format_pg_error(&e);
                if txn_aborted {
                    // Tell the user the transaction was rolled back so the
                    // state change isn't implicit — a multi-statement batch's
                    // user needs to know earlier writes were discarded.
                    format!(
                        "{msg}\n\nThe transaction was rolled back (a statement in it \
                         failed with an error that aborted the transaction)."
                    )
                } else {
                    msg
                }
            });
        return QueryOutcome {
            result,
            txn_aborted,
        };
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
    // Gated on `pre_resolve_types`, set by the caller for the cases where a
    // plain `prepare` is safe:
    //  * Direct connections (`!is_transaction_pooler`, the #166 path): no
    //    backend reassignment to collide with, so a named statement is fine.
    //  * Session-pinned pooler connections inside an open transaction (the
    //    #169 batch path, `in_transaction`): the user's `BEGIN` pins the
    //    backend, so a named statement can't be orphaned by reassignment.
    //  * A transaction-pooler bare batch whose earlier statement set
    //    session-local state (the #173 batch path, `session_state_set`, no
    //    open transaction): the SELECT must stay on the shared client to see
    //    the `SET`/temp table. This re-runs the `prepare` pattern the removed
    //    pre-warm (ca33b98) used, so on a *real* PgBouncer it can 42P05 if the
    //    pooler reassigns the backend during the prepare's typeinfo round-trip
    //    — but the prepare is non-fatal (swallowed below), and on the
    //    direct+`pooler_mode=transaction` config the PR targets there's no
    //    reassignment, so it's safe there. The remaining no-open-transaction
    //    pooler case with NO prior session state still routes to the #164
    //    one-shot pinned path (see `run_batch_in_session`), not here.
    //
    // The pre-resolve's failure handling is gated on `in_transaction` (#170):
    //
    //  * Inside an open transaction (`in_transaction=true`): a prepare
    //    Parse-time error (42P01 relation does not exist, 42501 permission
    //    denied, syntax error, …) *aborts the transaction server-side*. Running
    //    `query_typed` next would fail with 25P02 ("current transaction is
    //    aborted") instead of the real, actionable error — a confusing cascade
    //    that, because `transaction_effect("SELECT")=None` keeps the batch
    //    caller's `in_transaction=true`, also re-pins the aborted connection
    //    and 25P02s every subsequent batch until an explicit ROLLBACK. So a
    //    prepare failure here short-circuits: best-effort ROLLBACK, surface the
    //    prepare's real error via `format_query_error`, and signal
    //    `txn_aborted=true` so the caller forces `in_transaction=false` and
    //    releases the connection instead of re-pinning it.
    //
    //  * Outside an open transaction (`in_transaction=false`): a prepare
    //    failure is *recoverable*. The canonical case is a 42P05 on a real
    //    PgBouncer bare batch (the `session_state_set` #173 path) where the
    //    pooler reassigned the backend during the prepare's `typeinfo_statement`
    //    round-trip — `query_typed`'s unnamed protocol sidesteps the lost named
    //    statement and can still succeed. So the swallow is preserved here:
    //    `query_typed` runs anyway, and for builtin types `get_type` is always
    //    a `Type::from_oid` cache hit (no `typeinfo_statement`, no hang). A
    //    prepare failure on a valid result-set query is rare, and when it does
    //    happen outside a transaction the worst case is the real error
    //    surfacing from `query_typed` rather than a 25P02 cascade.
    //
    // `query_typed` is bounded by `POOLED_TXN_TIMEOUT` *only when the
    // pre-resolve failed* — that's the path that can hang (cold type cache →
    // `get_type` fires `typeinfo_statement` mid-stream → the #160/#163
    // deadlock). When the prepare succeeded (cache warm) or was skipped,
    // `query_typed` is unbounded-safe, matching the pooler-pinned path's
    // contract and today's behavior. This bounds the hang (consequence A of
    // #170) without tightening the direct path for the common case.
    //
    // The `Statement` is kept alive (not dropped immediately) so its
    // `.columns()` can be reused by the empty-rows branch below instead of
    // preparing a second time. (The warmed type cache persists on the shared
    // `InnerClient` regardless of the `Statement`'s lifetime — `Statement::Drop`
    // sends a server `Close` but does not clear `CachedTypeInfo.types` — so
    // holding it alive is for `.columns()` reuse, not cache persistence.)
    let mut prepared: Option<tokio_postgres::Statement> = None;
    let prepare_err: Option<tokio_postgres::Error> = if pre_resolve_types {
        match pg_client.prepare(&final_query).await {
            Ok(stmt) => {
                prepared = Some(stmt);
                None
            }
            Err(e) => Some(e),
        }
    } else {
        None
    };

    // #170: short-circuit on a prepare failure *inside an open transaction*.
    //
    // The prepare already aborted the transaction server-side (any hard ERROR
    // during Parse/Describe/Sync puts the txn block into `inError`/`INFAILED`).
    // Running `query_typed` next would fail with 25P02 ("current transaction is
    // aborted") and hide the real, actionable error — and because
    // `transaction_effect("SELECT")=None` keeps the batch caller's
    // `in_transaction=true`, the aborted connection would be re-pinned and
    // every subsequent batch 25P02s until an explicit ROLLBACK (the cascade).
    //
    // The ROLLBACK here is deliberate and not a regression of psql semantics.
    // The transaction is *already dead* — the pending writes in it are
    // discardable either way (psql would discard them when the user issues the
    // ROLLBACK the aborted state forces; we discard them now). Ending the
    // transaction lets the remaining statements in the batch run cleanly
    // (autocommit) instead of 25P02ing, and surfaces the real error. We tell
    // the user the transaction was rolled back in the error message so the
    // state change isn't silent.
    //
    // A SAVEPOINT can't help here: the prepare aborted the *outer* transaction,
    // not a savepoint, so there's nothing to roll back *to* that would leave the
    // earlier writes intact. The alternative — keeping `in_transaction=true`
    // and letting the user ROLLBACK explicitly — is the 25P02 cascade this fix
    // exists to close.
    if let Some(e) = prepare_err.as_ref() {
        if should_short_circuit_on_prepare_err(in_transaction) {
            // Best-effort ROLLBACK; the transaction is aborted, so ROLLBACK is
            // the one command PostgreSQL still accepts in this state. A
            // failure here is logged, not fatal — the connection still drops
            // back to the pool and `RecyclingMethod::Fast` will surface a
            // stale-state issue on reuse if it ever matters.
            if let Err(rollback_err) = pg_client.batch_execute("ROLLBACK").await {
                log::warn!(
                    "ROLLBACK after a prepare failure aborted a transaction failed: {rollback_err}"
                );
            }
            // Surface the prepare's real SQLSTATE (e.g. 42P01) and tell the
            // user the transaction was rolled back, so the state change isn't
            // implicit — a multi-statement batch's user needs to know earlier
            // writes were discarded.
            return QueryOutcome {
                result: Err(format!(
                    "{}\n\nThe transaction was rolled back (a statement in it \
                     failed with an error that aborted the transaction).",
                    client::format_query_error(e, is_transaction_pooler),
                )),
                txn_aborted: true,
            };
        }
        // else: outside a transaction — preserve the recoverable swallow
        // (42P05 on a real PgBouncer bare batch). Fall through to
        // `query_typed`, bounded below.
    }

    // Execute query
    // Unnamed-statement protocol (`query_typed`) — pooler-safe under
    // transaction-mode poolers (PgBouncer); see `client`'s module note
    // (tabularis#1097). No params for user-issued queries.
    //
    // On a transaction-mode pooler, a prepared-statement error (26000/42P05)
    // from tokio-postgres's internal `typeinfo_statement` (fired by
    // non-builtin column types) is rewritten with actionable guidance via
    // `format_query_error` (#157).
    //
    // Bounded by `POOLED_TXN_TIMEOUT` only when the pre-resolve `prepare`
    // failed (cold type cache → mid-stream `typeinfo_statement` hang risk,
    // #160/#163/#170). When the prepare succeeded (cache warm) or was skipped,
    // `query_typed` is unbounded-safe.
    // #181: keep the raw `tokio_postgres::Error` here (defer `map_err` to the
    // single outer error arm below) so we can classify it with
    // `error_aborted_open_txn` before formatting — a runtime error inside an
    // open transaction aborted the txn and the caller must not re-pin the
    // aborted connection. The timed branch's `Ok(Err(e))` arm is only reached
    // when `!in_transaction` (the short-circuit above returned when
    // `in_transaction`), so classification there returns `false` (the
    // `!in_transaction` guard) and the outer arm's behavior is unchanged for
    // that sub-path.
    let rows: Result<Vec<tokio_postgres::Row>, tokio_postgres::Error> = if prepare_err.is_some() {
        match tokio::time::timeout(POOLED_TXN_TIMEOUT, pg_client.query_typed(&final_query, &[]))
            .await
        {
            Ok(Ok(rows)) => Ok(rows),
            Ok(Err(e)) => Err(e),
            Err(_elapsed) => {
                // The `in_transaction=true` + `prepare_err.is_some()` case was
                // intercepted by the short-circuit above (which already
                // ROLLBACK'd and returned), so this arm is only reached with
                // `in_transaction=false`. There's no open transaction to
                // roll back; `txn_aborted` is `false`. The dropped inner
                // `query_typed` future is reclaimed by tokio-postgres's
                // connection task paging through the abandoned response to
                // `ReadyForQuery`, so the connection returns to idle and the
                // pool reclaims it normally.
                debug_assert!(
                    !in_transaction,
                    "the short-circuit above returns when in_transaction; \
                     this timeout arm is only reached with in_transaction=false"
                );
                return QueryOutcome {
                    result: Err(format!(
                        "Query timed out after {}s. The pre-resolve `prepare` failed and the \
                         subsequent data fetch did not complete within the plugin's bound; the \
                         connection has been released. Re-run the query, or if it reproduces, \
                         check whether the result column types (enums, composites, extension \
                         types) are resolvable on this connection.",
                        POOLED_TXN_TIMEOUT.as_secs()
                    )),
                    txn_aborted: false,
                };
            }
        }
    } else {
        pg_client.query_typed(&final_query, &[]).await
    };
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => {
            // #181: a `query_typed` runtime error (division by zero, a
            // constraint violation at fetch, a 42P01 on a relation dropped
            // between prepare and fetch) inside an open transaction aborts
            // it server-side. Classify on the raw error before formatting,
            // ROLLBACK the now-dead transaction, and signal `txn_aborted`
            // so the caller forces `in_transaction=false` and the aborted
            // connection isn't re-pinned (which would 25P02-cascade the next
            // batch's first statement). Gate on `transaction_effect(query)
            // != Closes` so a failed `COMMIT`/`ROLLBACK`/`END` (already
            // handled by the `Closes` path in `in_transaction_after_outcome`)
            // doesn't get a spurious ROLLBACK + "rolled back" notice.
            let txn_aborted = error_aborted_open_txn(&e, in_transaction)
                && transaction_effect(query) != TransactionEffect::Closes;
            if txn_aborted {
                if let Err(rollback_err) = pg_client.batch_execute("ROLLBACK").await {
                    log::warn!(
                        "ROLLBACK after a query_typed error aborted a transaction failed: {rollback_err}"
                    );
                }
            }
            let msg = client::format_query_error(&e, is_transaction_pooler);
            return QueryOutcome {
                result: Err(if txn_aborted {
                    format!(
                        "{msg}\n\nThe transaction was rolled back (a statement in it \
                         failed with an error that aborted the transaction)."
                    )
                } else {
                    msg
                }),
                txn_aborted,
            };
        }
    };

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
        //
        // #170: when the pre-resolve `prepare` failed (`prepare_err.is_some()`),
        // the type cache may still be cold; bound this column-discovery prepare
        // with the same `POOLED_TXN_TIMEOUT` so a wedged pooler can't pin the
        // worker past the host's 120s. (This path is only reached when
        // `in_transaction=false` — the `in_transaction=true` prepare-failure
        // case short-circuited above — so a timeout here does not abort an
        // open transaction; `txn_aborted` stays `false`.) A timeout is treated
        // as "columns unavailable" (`vec![]`), matching the existing failure
        // shape.
        let columns: Vec<String> = if let Some(stmt) = prepared.as_ref() {
            stmt.columns()
                .iter()
                .map(|c| c.name().to_string())
                .collect()
        } else if pre_resolve_types {
            // Pre-resolve was attempted but failed; try once more for column
            // discovery (the pre-#166 behavior for direct empty result sets).
            let discovery = if prepare_err.is_some() {
                tokio::time::timeout(POOLED_TXN_TIMEOUT, pg_client.prepare(&final_query))
                    .await
                    .ok()
                    .and_then(|r| r.ok())
            } else {
                pg_client.prepare(&final_query).await.ok()
            };
            if let Some(stmt) = discovery {
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

        return QueryOutcome {
            result: Ok(json!({
                "columns": columns,
                "rows": [],
                "affected_rows": 0,
                "truncated": false,
                "pagination": pagination,
            })),
            txn_aborted: false,
        };
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

    QueryOutcome {
        result: Ok(json!({
            "columns": columns,
            "rows": json_rows,
            "affected_rows": 0,
            "truncated": truncated,
            "pagination": pagination,
        })),
        txn_aborted: false,
    }
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

/// Whether `query` sets session-local state — a session `SET` (without
/// `LOCAL`) or a `CREATE TEMP[ORARY] TABLE` outside a transaction — that a
/// later statement in the same bare batch would see only on the *same*
/// connection (#173).
///
/// `run_batch_in_session` uses this to keep a result-set `SELECT` on the
/// shared batch client (with a column-type pre-resolve) instead of the #172
/// one-shot path when routing to a fresh connection would lose the state:
/// the one-shot applies only the host's `schema` via `SET LOCAL` and never
/// replays the user's prior `SET`/temp table. On `main` (rc.11, before
/// #172) every statement of a batch ran on one shared `pg_client`, so
/// `["SET extra_float_digits = 3", "SELECT 1.0/3.0"]` (or `["CREATE TEMP
/// TABLE t ...", "SELECT * FROM t"]`) reliably shared state; #172's
/// `!in_transaction` routing to the one-shot broke that for transaction
/// poolers.
///
/// Transaction-scoped forms are deliberately **excluded**:
/// - `SET LOCAL ...` dies at transaction end. With no open transaction the
///   one-shot path wraps its own `BEGIN`/`COMMIT`, so a `SET LOCAL` is
///   correctly scoped there; with an open transaction the `in_transaction`
///   branch already keeps the SELECT on the shared client. Either way a
///   bare-batch SELECT on a fresh one-shot connection would never have seen
///   it, so excluding it keeps the cheaper, pooler-safe one-shot path.
/// - `SET TRANSACTION ...` / `SET CONSTRAINTS ...` are transaction-control
///   verbs (not GUCs): outside a `BEGIN` they're a no-op or set only the
///   next transaction's characteristics, not session state a later SELECT
///   depends on. Excluding them avoids a false positive that would route a
///   later SELECT to the shared client unnecessarily.
/// - `CREATE TEMP TABLE` *inside* a `BEGIN` is likewise covered by the
///   `in_transaction` branch; this classifier only decides the
///   `!in_transaction` case.
///
/// A permanent `CREATE TABLE`/`CREATE TYPE`/etc. is visible to any
/// connection, so the one-shot sees it — not session-local, excluded.
///
/// Soundness over completeness: uncommon session-state forms
/// (`SELECT set_config(..., false)`, `RESET`) may be missed and degrade
/// gracefully to the one-shot (the current, pre-fix behavior) — a false
/// negative is never a *new* regression. A false positive routes to the
/// shared client with a pre-resolve, the same safe path the
/// `in_transaction` branch uses, so it is not dangerous, only occasionally
/// suboptimal. (`SET SESSION AUTHORIZATION` is *not* missed — it starts
/// with `SET` and its second keyword is `SESSION`, not `LOCAL`, so it is
/// caught.)
///
/// Shares [`leading_keywords`] with [`transaction_effect`], so comment
/// stripping and string-literal handling match (a comment-headed `SET` still
/// counts; a `SET` inside a string literal does not).
pub fn sets_session_local_state(query: &str) -> bool {
    // Four keywords cover the longest form: `CREATE [GLOBAL|LOCAL] TEMP TABLE`
    // (`GLOBAL`/`LOCAL` are optional deprecated noise words before TEMP, both
    // still accepted by PostgreSQL and still create a session-scoped temp
    // table — verified live).
    let mut words = leading_keywords(query, 4).into_iter();
    let Some(first) = words.next() else {
        return false;
    };
    let second = words.next();
    let third = words.next();
    let fourth = words.next();
    match first.as_str() {
        // Any `SET` that is neither `SET LOCAL` nor a transaction-control verb
        // is session-scoped (`SET <guc>`, `SET ROLE`, `SET SESSION ...`).
        "SET" => !matches!(
            second.as_deref(),
            Some("LOCAL") | Some("TRANSACTION") | Some("CONSTRAINTS")
        ),
        // `CREATE [GLOBAL|LOCAL] TEMP[ORARY] TABLE` lives for the session and
        // is connection-scoped; the optional `GLOBAL`/`LOCAL` qualifier (both
        // deprecated, both accepted) is skipped. Permanent `CREATE TABLE` /
        // `CREATE UNLOGGED TABLE` / `CREATE TYPE` are visible everywhere, so
        // they don't pin the SELECT to the shared connection.
        "CREATE" => match second.as_deref() {
            Some("TEMP") | Some("TEMPORARY") => third.as_deref() == Some("TABLE"),
            Some("GLOBAL") | Some("LOCAL") => {
                matches!(third.as_deref(), Some("TEMP") | Some("TEMPORARY"))
                    && fourth.as_deref() == Some("TABLE")
            }
            _ => false,
        },
        _ => false,
    }
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
