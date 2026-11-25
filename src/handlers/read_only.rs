//! Protected read-only queries (`execute_read_only_query`).

use deadpool_postgres::Object as PgClient;
use serde_json::Value;

use crate::cancel;
use crate::client;
use crate::handlers::query::{
    apply_search_path_local, exec_query_on_client, exec_query_pooled_txn, leading_keywords,
    returns_result_set, transaction_effect, TransactionEffect, TXN_ROLLED_BACK_NOTICE,
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
    // A one-shot connection is used behind a transaction pooler: the explicit
    // transaction pins one backend, and the connection is closed afterwards.
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
    exec_on_shared_pool_ending_with(req, id, "ROLLBACK").await
}

/// [`exec_on_shared_pool`] with the statement that ends the transaction as a
/// parameter, so a test can make the ending fail and show the connection is
/// discarded rather than pooled.
pub(crate) async fn exec_on_shared_pool_ending_with(
    req: &ReadOnlyRequest,
    id: &Value,
    end_sql: &str,
) -> Result<Value, String> {
    let pg_client = client::get_pool_client(&req.params).await?;
    let _cancel_guard = cancel::CancelGuard::register(id.as_u64(), &pg_client, req.params.clone());

    let mut in_flight = Discard::new(pg_client, req.params.clone());
    let pg_client = in_flight.client();

    // An early return from here on drops `in_flight` un-released, which
    // discards the connection: it may be inside an open transaction.
    begin_protected(pg_client).await?;
    let outcome = run_protected(pg_client, req).await;

    // Always end the transaction, whether the query succeeded or failed. A
    // failed statement leaves it aborted and ROLLBACK is the one command the
    // server still accepts.
    match pg_client.batch_execute(end_sql).await {
        Ok(()) => in_flight.release(),
        Err(e) => log::warn!(
            "PostgreSQL: ROLLBACK after a read-only query failed, closing the connection: {}",
            client::format_pg_error(&e)
        ),
    }
    outcome
}

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
        /* in_transaction = */ true,
    )
    .await;
    outcome.result.map_err(strip_rollback_notice)
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

    /// Hand the connection back to the pool.
    fn release(&mut self) {
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
                runtime.spawn(cancel::run_cancel(client.cancel_token(), self.1.clone()));
            }
            Err(_) => log::warn!("PostgreSQL: no runtime to send a cancel request from"),
        }
        // `take` removes it from the pool for good; dropping the raw client
        // then closes the socket.
        drop(PgClient::take(client));
    }
}

/// The statement that opens a protected call's transaction.
pub(crate) const BEGIN_READ_ONLY: &str = "BEGIN READ ONLY";

/// Opens a protected call's transaction and confirms, in the database, that
/// it really is read-only. On any error the caller must treat the connection
/// as unusable (it may be inside an open transaction) and discard it.
pub(crate) async fn begin_protected(pg_client: &PgClient) -> Result<(), String> {
    begin_protected_with(pg_client, BEGIN_READ_ONLY).await
}

/// [`begin_protected`] with the opening statement as a parameter, so the
/// confirmation can be shown to reject a transaction that is not read-only.
pub(crate) async fn begin_protected_with(
    pg_client: &PgClient,
    begin_sql: &str,
) -> Result<(), String> {
    pg_client
        .batch_execute(begin_sql)
        .await
        .map_err(|e| client::format_pg_error(&e))?;
    let row = pg_client
        .query_typed_one("SELECT current_setting('transaction_read_only')", &[])
        .await
        .map_err(|e| {
            format!(
                "Read-only protection could not be enabled: {}",
                client::format_pg_error(&e)
            )
        })?;
    let setting: String = row.try_get(0).map_err(|e| {
        format!(
            "Read-only protection could not be enabled: {}",
            client::format_pg_error(&e)
        )
    })?;
    require_read_only_transaction(&setting)
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
pub(crate) fn refuse_transaction_control(query: &str) -> Result<(), String> {
    match transaction_effect(query) {
        TransactionEffect::None => Ok(()),
        _ => Err(
            "Transaction control is not allowed in a read-only query: each read-only query runs in its own transaction"
                .into(),
        ),
    }
}

/// `exec_query_on_client` appends this when a failing statement aborted the
/// caller's open transaction and it rolled it back. Here the transaction is
/// the protection, not the user's, so telling them their transaction was
/// rolled back would be wrong.
pub(crate) fn strip_rollback_notice(error: String) -> String {
    error
        .strip_suffix(TXN_ROLLED_BACK_NOTICE)
        .map(str::to_string)
        .unwrap_or(error)
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
