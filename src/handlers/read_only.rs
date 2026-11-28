//! Protected read-only queries (`execute_read_only_query`).

use std::collections::BTreeSet;
use std::time::Duration;

use deadpool_postgres::Object as PgClient;
use serde_json::Value;
use tokio_postgres::SimpleQueryMessage;

use crate::cancel;
use crate::client;
use crate::handlers::query::{
    apply_search_path_local, exec_query_on_client, exec_query_pooled_txn, leading_keywords,
    returns_result_set, transaction_effect, OpenTransaction, TransactionEffect,
};
use crate::models::{inner_params, ConnectionParams};
use crate::rpc::{error_response, ok_response};

pub async fn execute_read_only_query(id: Value, params: &Value) -> Value {
    match run(&id, params).await {
        Ok(result) => ok_response(id, result),
        Err(error) => error_response(id, -32603, &error),
    }
}

async fn run(id: &Value, params: &Value) -> Result<Value, String> {
    let req = parse_request(params)?;
    refuse_transaction_control(&req.query)?;
    exec_read_only(&req, id).await
}

/// The same work `execute_query` does, inside a transaction the server will
/// not let write. Every path ends the transaction with `ROLLBACK`, and the
/// connection goes back to the pool only once that succeeded.
async fn exec_read_only(req: &ReadOnlyRequest, id: &Value) -> Result<Value, String> {
    // Behind a transaction pooler a result-set query runs on a one-shot
    // connection: the explicit transaction pins one backend, and the
    // connection is closed afterwards. Any other statement runs on the shared
    // pool, where the same `BEGIN READ ONLY` pins the backend for the call.
    if req.params.is_transaction_pooler() && returns_result_set(&req.query) {
        return exec_query_pooled_txn(
            &req.params,
            &req.query,
            req.limit,
            req.page,
            req.schema.as_deref(),
            id,
            /* read_only = */ true,
        )
        .await;
    }
    exec_on_shared_pool(req, id).await
}

/// A protected call on a pooled connection: `BEGIN READ ONLY`, the query,
/// `ROLLBACK`.
///
/// The connection returns to the pool only when `ROLLBACK` succeeded.
/// Otherwise it is detached and closed, so the server ends whatever
/// transaction it was in and a read-only (or half-finished) session can never
/// be handed to the next caller. The same holds if this future is dropped
/// mid-flight (a host timeout, a worker abort): [`Discard`] detaches the
/// connection and closes it.
async fn exec_on_shared_pool(req: &ReadOnlyRequest, id: &Value) -> Result<Value, String> {
    exec_on_shared_pool_with(req, id, BEGIN_READ_ONLY, "ROLLBACK").await
}

/// [`exec_on_shared_pool`] with the statements that open and end the
/// transaction as parameters, so a test can show that a transaction that is
/// not read-only stops the call before the query runs, and that an ending that
/// fails discards the connection rather than pooling it.
pub(crate) async fn exec_on_shared_pool_with(
    req: &ReadOnlyRequest,
    id: &Value,
    begin_sql: &str,
    end_sql: &str,
) -> Result<Value, String> {
    // Reserved before the connection exists, so a `cancel` that arrives while
    // the call is still connecting is remembered instead of dropped.
    let cancel_guard = cancel::CancelGuard::reserve(id.as_u64());
    let pg_client = client::get_pool_client(&req.params).await?;
    cancel_guard.attach(&pg_client, req.params.clone());

    let mut in_flight = Discard::new(pg_client, req.params.clone());
    let pg_client = in_flight.client();

    // An early return from here on drops `in_flight` un-released, which
    // discards the connection: it may be inside an open transaction.
    let baseline = begin_protected_with(pg_client, begin_sql).await?;

    // PostgreSQL discards a cancel that arrives while the backend is idle
    // between statements, and the protected call's own BEGIN and read-back are
    // such statements: a cancel landing there must stop the call here.
    let outcome = if cancel_guard.is_cancelled() {
        Err(CANCELLED_MESSAGE.to_string())
    } else {
        run_protected(pg_client, req).await
    };

    // Always end the transaction, whether the query succeeded or failed. A
    // failed statement leaves it aborted and ROLLBACK is the one command the
    // server still accepts.
    let check_session = !req.params.is_transaction_pooler();
    match end_protected(pg_client, end_sql, &baseline, check_session).await {
        Ok(true) => in_flight.release(cancel_guard),
        Ok(false) => log::warn!(
            "PostgreSQL: a read-only query left session state that ROLLBACK does not undo (a prepared statement or an advisory lock), closing the connection"
        ),
        Err(e) => log::warn!(
            "PostgreSQL: ending a read-only query failed, closing the connection: {e}"
        ),
    }
    outcome
}

/// What the server reports when the call is cancelled before its query ran.
/// Matches the text of a cancelled statement (SQLSTATE 57014).
pub(crate) const CANCELLED_MESSAGE: &str = "ERROR: canceling statement due to user request";

async fn run_protected(pg_client: &PgClient, req: &ReadOnlyRequest) -> Result<Value, String> {
    if let Some(schema) = req.schema.as_deref() {
        apply_search_path_local(pg_client, schema).await?;
    }
    let outcome = exec_query_on_client(
        pg_client,
        &req.query,
        req.limit,
        req.page,
        req.params.is_transaction_pooler(),
        !req.params.is_transaction_pooler(),
        OpenTransaction::EndedByCaller,
    )
    .await;
    outcome.result
}

/// A pooled connection that is detached and closed, not returned to the pool,
/// unless [`Discard::release`] says its transaction ended cleanly. Dropping it
/// any other way (a failed `ROLLBACK`, a cancelled or timed-out call) asks the
/// server to cancel the statement still running on it and closes the
/// connection, so the next borrower can never inherit a read-only or
/// half-finished session, and an abandoned statement does not keep running.
struct Discard(Option<PgClient>, ConnectionParams);

impl Discard {
    fn new(client: PgClient, params: ConnectionParams) -> Self {
        Self(Some(client), params)
    }

    fn client(&self) -> &PgClient {
        self.0
            .as_ref()
            .expect("the connection is held until release")
    }

    /// Hand the connection back to the pool. Takes the call's cancel guard and
    /// drops it first, so a late `cancel` for this request finds nothing
    /// registered and cannot reach the statement of whoever borrows the
    /// connection next. The order cannot be reversed by a caller.
    fn release(&mut self, cancel_guard: cancel::CancelGuard) {
        drop(cancel_guard);
        self.0 = None;
    }
}

impl Drop for Discard {
    fn drop(&mut self) {
        let Some(client) = self.0.take() else { return };
        // Closing the socket alone does not stop a running statement: the
        // server only notices the client is gone when it next writes to it.
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                let (token, params) = (client.cancel_token(), self.1.clone());
                // The cancel opens a connection of its own, which would
                // otherwise wait for the OS to give up on an unreachable server.
                runtime.spawn(bounded(CANCEL_TIMEOUT, cancel::run_cancel(token, params)));
            }
            Err(_) => log::warn!("PostgreSQL: no runtime to send a cancel request from"),
        }
        // `take` removes it from the pool for good; dropping the raw client
        // then closes the socket.
        drop(PgClient::take(client));
    }
}

/// How long a discarded connection's cancel request may take.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs `work` for at most `limit`. Answers whether it finished in time.
pub(crate) async fn bounded(limit: Duration, work: impl std::future::Future<Output = ()>) -> bool {
    tokio::time::timeout(limit, work).await.is_ok()
}

/// The statement that opens a protected call's transaction.
pub(crate) const BEGIN_READ_ONLY: &str = "BEGIN READ ONLY";

/// What a session holds that `ROLLBACK` does not undo, read inside the
/// protected transaction and again after it ends. A `PREPARE` (directly, or
/// from dynamic SQL in a `DO` block or function) survives `ROLLBACK`, and would
/// be inherited by whoever borrows the connection next; a `DEALLOCATE` would
/// remove a statement the client library still has cached; a session-level
/// advisory lock stays held for the life of the connection. A connection that
/// fails the comparison is closed, which releases all three.
///
/// The SQL-level statements and the locks are compared by identity, so a call
/// that releases one the session already held and takes another is still seen.
/// A lock the session already held and the call took again cannot be told
/// apart: the catalog does not show the nesting depth.
#[derive(Debug)]
pub(crate) struct SessionBaseline {
    sql_prepared: BTreeSet<String>,
    protocol_prepared: i64,
    advisory_locks: BTreeSet<String>,
}

/// The names of the statements the SQL `PREPARE` command created, the number
/// the client library created through the wire protocol, and the advisory locks
/// this backend holds, each lock as `classid:objid:objsubid:mode`.
const SESSION_STATE_SQL: &str = "\
     (SELECT coalesce(json_agg(name ORDER BY name), '[]') FROM pg_prepared_statements WHERE from_sql)::text, \
     (SELECT count(*) FROM pg_prepared_statements WHERE NOT from_sql), \
     (SELECT coalesce(json_agg(classid || ':' || objid || ':' || objsubid || ':' || mode ORDER BY classid, objid, objsubid, mode), '[]') \
        FROM pg_locks WHERE locktype = 'advisory' AND pid = pg_backend_pid())::text";

impl SessionBaseline {
    /// Reads the three values `SESSION_STATE_SQL` selects, starting at column `first`.
    fn from_columns(
        first: usize,
        text: impl Fn(usize) -> Option<String>,
        count: impl Fn(usize) -> Option<i64>,
    ) -> Result<Self, String> {
        let unreadable = || "could not read the session state".to_string();
        let set = |i: usize| -> Result<BTreeSet<String>, String> {
            let json = text(i).ok_or_else(unreadable)?;
            serde_json::from_str(&json).map_err(|_| unreadable())
        };
        Ok(Self {
            sql_prepared: set(first)?,
            protocol_prepared: count(first + 1).ok_or_else(unreadable)?,
            advisory_locks: set(first + 2)?,
        })
    }

    /// Whether a session now at `after` is as clean as it was at `self`: it
    /// holds nothing it did not hold before, and the client library's own
    /// statements are all still there.
    fn is_unchanged_by(&self, after: &SessionBaseline) -> bool {
        after.sql_prepared.is_subset(&self.sql_prepared)
            && after.protocol_prepared >= self.protocol_prepared
            && after.advisory_locks.is_subset(&self.advisory_locks)
    }
}

/// Opens a protected call's transaction and confirms, in the database, that
/// it really is read-only. On any error the caller must treat the connection
/// as unusable (it may be inside an open transaction) and discard it.
pub(crate) async fn begin_protected(pg_client: &PgClient) -> Result<SessionBaseline, String> {
    begin_protected_with(pg_client, BEGIN_READ_ONLY).await
}

/// [`begin_protected`] with the opening statement as a parameter, so the
/// confirmation can be shown to reject a transaction that is not read-only.
pub(crate) async fn begin_protected_with(
    pg_client: &PgClient,
    begin_sql: &str,
) -> Result<SessionBaseline, String> {
    pg_client
        .batch_execute(begin_sql)
        .await
        .map_err(|e| client::format_pg_error(&e))?;
    let unprotected = |e: String| format!("Read-only protection could not be enabled: {e}");
    let sql = format!("SELECT current_setting('transaction_read_only'), {SESSION_STATE_SQL}");
    let row = pg_client
        .query_typed_one(&sql, &[])
        .await
        .map_err(|e| unprotected(client::format_pg_error(&e)))?;
    let setting: String = row
        .try_get(0)
        .map_err(|e| unprotected(client::format_pg_error(&e)))?;
    require_read_only_transaction(&setting)?;
    SessionBaseline::from_columns(
        1,
        |i| row.try_get::<_, String>(i).ok(),
        |i| row.try_get::<_, i64>(i).ok(),
    )
    .map_err(unprotected)
}

/// The statements that end a protected call behind a transaction pooler.
///
/// A closed client connection does not close the server backend there, so a
/// session-level advisory lock or a `PREPARE` the call created would stay on a
/// backend other clients share. They are undone in the same message as the
/// `ROLLBACK`, which the pooler keeps on one backend until it finishes. Only
/// what the call can have created is released: when the session already held a
/// lock or a statement at the start, it cannot be told apart from the call's
/// own, so that kind is left alone.
pub(crate) fn rollback_and_clean_sql(end_sql: &str, baseline: &SessionBaseline) -> String {
    let mut sql = end_sql.to_string();
    if baseline.advisory_locks.is_empty() {
        sql.push_str("; SELECT pg_advisory_unlock_all()");
    }
    if baseline.sql_prepared.is_empty() {
        sql.push_str(
            "; DO $ro$ DECLARE r record; BEGIN \
             FOR r IN SELECT name FROM pg_prepared_statements WHERE from_sql LOOP \
             EXECUTE 'DEALLOCATE ' || quote_ident(r.name); END LOOP; END $ro$",
        );
    }
    sql
}

/// Ends a protected call's transaction with `end_sql` (`ROLLBACK`). With
/// `check_session` it also reads the session state back in the same round
/// trip, and answers whether the connection is as clean as `baseline`: only
/// then may it return to the pool. A failure to end the transaction or to read
/// the state is an `Err`, and the caller discards the connection.
///
/// Without `check_session` (behind a transaction pooler, where the read could
/// be served by a different backend) the call's own session state is undone
/// instead, by [`rollback_and_clean_sql`].
async fn end_protected(
    pg_client: &PgClient,
    end_sql: &str,
    baseline: &SessionBaseline,
    check_session: bool,
) -> Result<bool, String> {
    if !check_session {
        pg_client
            .batch_execute(&rollback_and_clean_sql(end_sql, baseline))
            .await
            .map_err(|e| client::format_pg_error(&e))?;
        return Ok(true);
    }
    let messages = pg_client
        .simple_query(&format!("{end_sql}; SELECT {SESSION_STATE_SQL}"))
        .await
        .map_err(|e| client::format_pg_error(&e))?;
    let row = messages
        .iter()
        .find_map(|m| match m {
            SimpleQueryMessage::Row(row) => Some(row),
            _ => None,
        })
        .ok_or("the server did not report the session state")?;
    let after = SessionBaseline::from_columns(
        0,
        |i| row.get(i).map(str::to_string),
        |i| row.get(i).and_then(|v| v.parse().ok()),
    )?;
    Ok(baseline.is_unchanged_by(&after))
}

#[derive(Debug)]
pub(crate) struct ReadOnlyRequest {
    pub params: ConnectionParams,
    pub query: String,
    pub limit: Option<u32>,
    pub page: u32,
    pub schema: Option<String>,
}

pub(crate) fn parse_request(request: &Value) -> Result<ReadOnlyRequest, String> {
    let query = request
        .get("query")
        .and_then(Value::as_str)
        .ok_or("query is required")?;
    if leading_keywords(query, 1).is_empty() {
        return Err("query is empty".into());
    }
    let schema = request
        .get("schema")
        .and_then(Value::as_str)
        .filter(|schema| !schema.trim().is_empty())
        .map(str::to_string);
    Ok(ReadOnlyRequest {
        params: ConnectionParams::from_value(inner_params(request)),
        query: query.to_string(),
        limit: request
            .get("limit")
            .and_then(Value::as_u64)
            .map(|limit| limit.min(u64::from(u32::MAX)) as u32),
        page: request
            .get("page")
            .and_then(Value::as_u64)
            .map_or(1, |page| page.clamp(1, u64::from(u32::MAX)) as u32),
        schema,
    })
}

/// The protected call owns its read-only transaction, so a statement that
/// opens or ends one would step outside it: `COMMIT` would end the protection
/// before the call's own `ROLLBACK`, and `BEGIN READ WRITE` is refused by the
/// server only after it has already ended the protected one. The host refuses
/// these in read-only mode too; this keeps a direct caller (MCP) honest.
///
/// PostgreSQL's read-only mode also lets through a few commands whose effect
/// is not part of the transaction, so `ROLLBACK` cannot undo it: `PREPARE` and
/// `DEALLOCATE` change what the connection keeps for the next borrower, `COPY`
/// can write a server-side file or run a program, and `ANALYZE`, `CLUSTER`,
/// `REINDEX`, `CHECKPOINT` and `LOAD` write statistics, rewrite storage or load
/// code, and `DISCARD` resets session state (`DISCARD SEQUENCES` is not undone
/// by `ROLLBACK`). They are refused by name rather than left to a role's privileges.
pub(crate) fn refuse_transaction_control(query: &str) -> Result<(), String> {
    if !matches!(transaction_effect(query), TransactionEffect::None) {
        return Err(
            "Transaction control is not allowed in a read-only query: each read-only query runs in its own transaction"
                .into(),
        );
    }
    let first = leading_keywords(query, 1).into_iter().next();
    match first.as_deref() {
        Some(
            command @ ("PREPARE" | "DEALLOCATE" | "COPY" | "ANALYZE" | "ANALYSE" | "CLUSTER"
            | "REINDEX" | "CHECKPOINT" | "LOAD" | "DISCARD"),
        ) => Err(format!(
            "{command} is not allowed in a read-only query: PostgreSQL's read-only mode does not stop it from changing state outside the transaction"
        )),
        _ => Ok(()),
    }
}

/// Confirms the server reports the transaction as read-only. The value is
/// read back rather than assumed, so a protection that did not take effect
/// fails the call instead of silently running writable.
pub(crate) fn require_read_only_transaction(setting: &str) -> Result<(), String> {
    if setting == "on" {
        Ok(())
    } else {
        Err(format!(
            "Read-only protection could not be enabled (transaction_read_only is '{setting}'); the query was not run"
        ))
    }
}

#[cfg(test)]
#[path = "read_only_tests.rs"]
mod read_only_tests;
