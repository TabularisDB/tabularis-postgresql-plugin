//! BLOB (bytea) helpers — save_blob_to_file, fetch_blob_as_data_url.
//!
//! Mirrors the built-in driver's exact query shape
//! (`src-tauri/src/drivers/postgres/mod.rs::save_blob_column_to_file` /
//! `fetch_blob_column_as_data_url`) — a single-column SELECT filtered by the
//! row's primary key, using the same `build_pk_map_predicate` helper as
//! update_record/delete_record.

use serde_json::Value;
use tokio_postgres::types::{ToSql, Type};

use crate::binding::build_pk_map_predicate;
use crate::client;
use crate::models::{inner_params, ConnectionParams};
use crate::rpc::{error_response, ok_response};

pub async fn save_blob_to_file(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let table = params.get("table").and_then(Value::as_str).unwrap_or("");
    let col_name = params.get("col_name").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");
    let file_path = params.get("file_path").and_then(Value::as_str).unwrap_or("");
    let pk_map = params
        .get("pk_map")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    match fetch_blob_bytes(&conn_params, table, col_name, &pk_map, schema).await {
        Ok(bytes) => match std::fs::write(file_path, bytes) {
            Ok(_) => ok_response(id, Value::Null),
            Err(e) => error_response(id, -32603, &e.to_string()),
        },
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn fetch_blob_as_data_url(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let table = params.get("table").and_then(Value::as_str).unwrap_or("");
    let col_name = params.get("col_name").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");
    let pk_map = params
        .get("pk_map")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    match fetch_blob_bytes(&conn_params, table, col_name, &pk_map, schema).await {
        Ok(bytes) => ok_response(id, Value::from(encode_blob_full(&bytes))),
        Err(e) => error_response(id, -32603, &e),
    }
}

async fn fetch_blob_bytes(
    conn_params: &ConnectionParams,
    table: &str,
    col_name: &str,
    pk_map: &serde_json::Map<String, Value>,
    schema: &str,
) -> Result<Vec<u8>, String> {
    let qualified = format!("\"{}\".\"{}\"", schema.replace('"', "\"\""), table.replace('"', "\"\""));
    let column_types = client::get_column_types_map(conn_params, table, schema).await.unwrap_or_default();

    let (predicate, owned_params) = build_pk_map_predicate(pk_map, &column_types, 1)?;
    let query = format!(
        "SELECT \"{}\" FROM {} WHERE {}",
        col_name.replace('"', "\"\""),
        qualified,
        predicate
    );

    let typed_params: Vec<(&(dyn ToSql + Sync), Type)> = owned_params
        .iter()
        .map(|(p, t)| (p.as_ref() as &(dyn ToSql + Sync), t.clone()))
        .collect();

    let rows = client::query_typed(conn_params, &query, &typed_params).await?;
    let row = rows.first().ok_or_else(|| "Row not found".to_string())?;
    row.try_get::<_, Vec<u8>>(0).map_err(|e| e.to_string())
}

/// Encode raw bytes into the canonical BLOB wire format:
/// `"BLOB:<size>:<mime_type>:<base64_data>"`. MIME type is sniffed from the
/// content's magic bytes; unrecognized content falls back to
/// `application/octet-stream`. Matches `encode_blob_full` in
/// `src-tauri/src/drivers/common/blob.rs`.
fn encode_blob_full(data: &[u8]) -> String {
    let mime_type = infer::get(data).map(|k| k.mime_type()).unwrap_or("application/octet-stream");
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data);
    format!("BLOB:{}:{}:{}", data.len(), mime_type, b64)
}

#[cfg(test)]
#[path = "blob_tests.rs"]
mod blob_tests;
