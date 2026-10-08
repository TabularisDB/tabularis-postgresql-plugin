//! JSON-RPC dispatch and response helpers.

use serde_json::{json, Value};

use crate::handlers;

/// Parse one JSON-RPC line and return the response value, or `None` for a
/// *notification* (a valid Request object — with a string `method` member —
/// that has no top-level `id` field) that requires no response per JSON-RPC
/// convention. Never panics — parse errors and method failures are surfaced
/// as JSON-RPC error responses.
///
/// Notification-ness is decided by the **absence of the `id` field**, not by
/// method: a `cancel` with an `id` present is treated as a normal request
/// (defensive — the host shouldn't send one, but if it does the caller gets
/// a response rather than a silent drop). Only an `id`-less `cancel` (the
/// fire-and-forget form the host will send on timeout per #126) returns
/// `None` so `main.rs`'s worker skips the stdout write entirely — a stray
/// response line for a notification would corrupt the protocol stream.
///
/// Only a JSON **object** can be a JSON-RPC request or notification. A
/// non-object JSON value (array, number, string, bool, null) that parses
/// successfully is an Invalid Request and gets a `-32600` response — not
/// silently swallowed as a notification (#135). An object missing a string
/// `method` member is likewise an Invalid Request, not a notification — a
/// notification must be a valid Request (#137).
pub async fn handle_line(line: &str) -> Option<Value> {
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(err) => {
            return Some(error_response(
                Value::Null,
                -32700,
                &format!("parse error: {err}"),
            ))
        }
    };

    // A non-object JSON value (array, number, string, bool, null) is not a
    // valid JSON-RPC request — only objects carry method/id/params. Return
    // -32600 Invalid Request so the client gets a response instead of
    // hanging (#135). Must happen before the notification check below: the
    // old `!request.as_object().is_some_and(..)` predicate treated any
    // non-object as a notification and silently dropped it.
    if !request.is_object() {
        return Some(error_response(
            Value::Null,
            -32600,
            "Invalid Request: JSON-RPC request must be a JSON object",
        ));
    }

    // Per JSON-RPC 2.0, a Request object MUST contain a `method` member that
    // is a string. An object missing `method` (or with a non-string `method`)
    // is an Invalid Request (-32600), not a notification — a notification is a
    // valid Request (with `method`) that happens to lack an `id`. This check
    // must run before the notification check below: otherwise an object like
    // `{}` (no `id`, no `method`) would be treated as a notification and
    // silently swallowed → the client hangs (#137, same bug class as #135 but
    // for objects).
    if !request
        .as_object()
        .is_some_and(|o| o.get("method").is_some_and(Value::is_string))
    {
        return Some(error_response(
            request.get("id").cloned().unwrap_or(Value::Null),
            -32600,
            "Invalid Request: JSON-RPC request must contain a string 'method' member",
        ));
    }

    // A request with no `id` field is a JSON-RPC notification: no response.
    // `id: null` is *not* a notification (it's a request whose id is null) —
    // distinguish "field absent" from "field present and null" so a host
    // that legitimately uses `id: null` still gets a response.
    let is_notification = !request.as_object().is_some_and(|o| o.contains_key("id"));

    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    let response = match method.as_str() {
        // Connection lifecycle
        "initialize" => handlers::connection::initialize(id, &params).await,
        "ping" => handlers::connection::ping(id, &params).await,
        "test_connection" => handlers::connection::test_connection(id, &params).await,
        "shutdown" => handlers::connection::shutdown(id, &params).await,

        // Metadata — stubs for future sprints
        "get_databases" => handlers::metadata::get_databases(id, &params).await,
        "get_schemas" => handlers::metadata::get_schemas(id, &params).await,
        "get_tables" => handlers::metadata::get_tables(id, &params).await,
        "get_columns" => handlers::metadata::get_columns(id, &params).await,
        "get_table_ddl" => handlers::metadata::get_table_ddl(id, &params).await,
        "get_foreign_keys" => handlers::metadata::get_foreign_keys(id, &params).await,
        "get_indexes" => handlers::metadata::get_indexes(id, &params).await,
        "get_views" => handlers::metadata::get_views(id, &params).await,
        "get_view_definition" => handlers::metadata::get_view_definition(id, &params).await,
        "get_view_columns" => handlers::metadata::get_view_columns(id, &params).await,
        "get_materialized_views" => handlers::metadata::get_materialized_views(id, &params).await,
        "get_materialized_view_columns" => {
            handlers::metadata::get_materialized_view_columns(id, &params).await
        }
        "get_materialized_view_definition" => {
            handlers::metadata::get_materialized_view_definition(id, &params).await
        }
        "refresh_materialized_view" => {
            handlers::metadata::refresh_materialized_view(id, &params).await
        }
        "get_routines" => handlers::metadata::get_routines(id, &params).await,
        "get_routine_parameters" => handlers::metadata::get_routine_parameters(id, &params).await,
        "get_routine_definition" => handlers::metadata::get_routine_definition(id, &params).await,
        "build_routine_call_sql" => handlers::routines::build_routine_call_sql(id, &params).await,
        "routine_create_template" => handlers::routines::routine_create_template(id, &params).await,
        "drop_routine" => handlers::routines::drop_routine(id, &params).await,
        "get_triggers" => handlers::metadata::get_triggers(id, &params).await,
        "get_trigger_definition" => handlers::metadata::get_trigger_definition(id, &params).await,
        "get_schema_snapshot" => handlers::metadata::get_schema_snapshot(id, &params).await,
        "get_all_columns_batch" => handlers::metadata::get_all_columns_batch(id, &params).await,
        "get_all_foreign_keys_batch" => {
            handlers::metadata::get_all_foreign_keys_batch(id, &params).await
        }

        // View mutation
        "create_view" => handlers::metadata::create_view(id, &params).await,
        "alter_view" => handlers::metadata::alter_view(id, &params).await,
        "drop_view" => handlers::metadata::drop_view(id, &params).await,
        "create_trigger" => handlers::metadata::create_trigger(id, &params).await,
        "drop_trigger" => handlers::metadata::drop_trigger(id, &params).await,

        // Query execution
        "execute_query" => handlers::query::execute_query(id, &params).await,
        "execute_query_batch" => handlers::query::execute_query_batch(id, &params).await,
        "release_session" => handlers::query::release_session(id, &params).await,
        "explain_query" => handlers::query::explain_query(id, &params).await,
        // Fire-and-forget notification: the host sends this when its plugin-call
        // timeout fires so the plugin cancels the server-side statement (#126).
        // A true notification carries no top-level `id`; `handle_line` detects
        // that and skips the response write, so this handler's return value is
        // only used if a caller (incorrectly) sends `cancel` as a request.
        "cancel" => handlers::query::cancel(id, &params).await,

        // CRUD
        "insert_record" => handlers::crud::insert_record(id, &params).await,
        "update_record" => handlers::crud::update_record(id, &params).await,
        "delete_record" => handlers::crud::delete_record(id, &params).await,

        // DDL
        "get_create_table_sql" => handlers::ddl::get_create_table_sql(id, &params).await,
        "get_add_column_sql" => handlers::ddl::get_add_column_sql(id, &params).await,
        "get_alter_column_sql" => handlers::ddl::get_alter_column_sql(id, &params).await,
        "get_create_index_sql" => handlers::ddl::get_create_index_sql(id, &params).await,
        "get_create_foreign_key_sql" => {
            handlers::ddl::get_create_foreign_key_sql(id, &params).await
        }
        "drop_index" => handlers::ddl::drop_index(id, &params).await,
        "drop_foreign_key" => handlers::ddl::drop_foreign_key(id, &params).await,

        // BLOB
        "save_blob_to_file" => handlers::blob::save_blob_to_file(id, &params).await,
        "fetch_blob_as_data_url" => handlers::blob::fetch_blob_as_data_url(id, &params).await,

        other => not_implemented(id, other),
    };

    // A notification (no `id` field) gets no response — return None so the
    // worker skips the stdout write. An `id`-less request that nonetheless
    // produced an error response (e.g. a parse error on a notification) is
    // also suppressed: the host isn't waiting for one, and writing it could
    // corrupt the stream by responding to a message the host doesn't expect
    // a reply to. This only affects `cancel` today; every other method is
    // only ever sent as a request with an `id`.
    //
    // Note: the method-presence and is_object guards above return early with
    // `Some(error_response(...))` for invalid objects/non-objects, bypassing
    // this suppression. That's intentional — those inputs are Invalid Requests
    // (-32600), not notifications, so they should get a response even if they
    // lack an `id`. An `id`-less Invalid Request like `{}` produces an
    // `id: null` response the host currently can't deserialize (tabularis#916);
    // the host logs and drops it, which is still better than the pre-fix hang
    // (the host sees *something* rather than nothing).
    if is_notification {
        None
    } else {
        Some(response)
    }
}

pub fn ok_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "result": result,
        "id": id,
    })
}

pub fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "error": { "code": code, "message": message },
        "id": id,
    })
}

pub fn not_implemented(id: Value, method: &str) -> Value {
    error_response(
        id,
        -32601,
        &format!("Method not found (-32601): '{method}' is not implemented"),
    )
}

#[cfg(test)]
#[path = "rpc_tests.rs"]
mod rpc_tests;
