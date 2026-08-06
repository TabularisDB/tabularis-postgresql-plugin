//! JSON-RPC dispatch and response helpers.

use serde_json::{json, Value};

use crate::handlers;

/// Parse one JSON-RPC line and return the response value. Never panics —
/// parse errors and method failures are surfaced as JSON-RPC error responses.
pub async fn handle_line(line: &str) -> Value {
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(err) => return error_response(Value::Null, -32700, &format!("parse error: {err}")),
    };

    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    match method.as_str() {
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
        "get_foreign_keys" => handlers::metadata::get_foreign_keys(id, &params).await,
        "get_indexes" => handlers::metadata::get_indexes(id, &params).await,
        "get_views" => handlers::metadata::get_views(id, &params).await,
        "get_view_definition" => handlers::metadata::get_view_definition(id, &params).await,
        "get_view_columns" => handlers::metadata::get_view_columns(id, &params).await,
        "get_materialized_views" => handlers::metadata::get_materialized_views(id, &params).await,
        "get_materialized_view_columns" => handlers::metadata::get_materialized_view_columns(id, &params).await,
        "get_materialized_view_definition" => handlers::metadata::get_materialized_view_definition(id, &params).await,
        "refresh_materialized_view" => handlers::metadata::refresh_materialized_view(id, &params).await,
        "get_routines" => handlers::metadata::get_routines(id, &params).await,
        "get_routine_parameters" => handlers::metadata::get_routine_parameters(id, &params).await,
        "get_routine_definition" => handlers::metadata::get_routine_definition(id, &params).await,
        "get_triggers" => handlers::metadata::get_triggers(id, &params).await,
        "get_trigger_definition" => handlers::metadata::get_trigger_definition(id, &params).await,
        "get_schema_snapshot" => handlers::metadata::get_schema_snapshot(id, &params).await,
        "get_all_columns_batch" => handlers::metadata::get_all_columns_batch(id, &params).await,
        "get_all_foreign_keys_batch" => handlers::metadata::get_all_foreign_keys_batch(id, &params).await,

        // View mutation
        "create_view" => handlers::metadata::create_view(id, &params).await,
        "alter_view" => handlers::metadata::alter_view(id, &params).await,
        "drop_view" => handlers::metadata::drop_view(id, &params).await,
        "create_trigger" => handlers::metadata::create_trigger(id, &params).await,
        "drop_trigger" => handlers::metadata::drop_trigger(id, &params).await,

        // Query execution
        "execute_query" => handlers::query::execute_query(id, &params).await,
        "execute_query_batch" => handlers::query::execute_query_batch(id, &params).await,
        "explain_query" => handlers::query::explain_query(id, &params).await,

        // CRUD
        "insert_record" => handlers::crud::insert_record(id, &params).await,
        "update_record" => handlers::crud::update_record(id, &params).await,
        "delete_record" => handlers::crud::delete_record(id, &params).await,

        // DDL
        "get_create_table_sql" => handlers::ddl::get_create_table_sql(id, &params).await,
        "get_add_column_sql" => handlers::ddl::get_add_column_sql(id, &params).await,
        "get_alter_column_sql" => handlers::ddl::get_alter_column_sql(id, &params).await,
        "get_create_index_sql" => handlers::ddl::get_create_index_sql(id, &params).await,
        "get_create_foreign_key_sql" => handlers::ddl::get_create_foreign_key_sql(id, &params).await,
        "drop_index" => handlers::ddl::drop_index(id, &params).await,
        "drop_foreign_key" => handlers::ddl::drop_foreign_key(id, &params).await,

        // BLOB
        "save_blob_to_file" => handlers::blob::save_blob_to_file(id, &params).await,
        "fetch_blob_as_data_url" => handlers::blob::fetch_blob_as_data_url(id, &params).await,

        other => not_implemented(id, other),
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
