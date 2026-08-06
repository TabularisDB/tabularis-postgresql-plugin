//! CRUD operation handlers — insert_record, update_record, delete_record.
//!
//! Mirrors the built-in driver's SQL generation and binding exactly
//! (`src-tauri/src/drivers/postgres/mod.rs` insert/update/delete_record +
//! `binding.rs`) so both drivers produce identical affected_rows and
//! identical persisted data for the same inputs.

use serde_json::Value;
use tokio_postgres::types::{ToSql, Type};

use crate::binding::{bind_pg_value, build_pk_map_predicate, BindOptions};
use crate::client;
use crate::models::{inner_params, ConnectionParams};
use crate::rpc::{error_response, ok_response};

pub async fn insert_record(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let table = params.get("table").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");
    let data = params
        .get("data")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    match exec_insert(&conn_params, table, data, schema).await {
        Ok(affected) => ok_response(id, Value::from(affected)),
        Err(e) => error_response(id, -32603, &e),
    }
}

async fn exec_insert(
    conn_params: &ConnectionParams,
    table: &str,
    data: serde_json::Map<String, Value>,
    schema: &str,
) -> Result<u64, String> {
    let qualified = format!("\"{}\".\"{}\"", schema.replace('"', "\"\""), table.replace('"', "\"\""));

    // Stable column order: iterate the map once into a Vec (matches the
    // builtin's "lock in an arbitrary-but-consistent order" behavior).
    let entries: Vec<(String, Value)> = data.into_iter().collect();

    if entries.is_empty() {
        let query = format!("INSERT INTO {} DEFAULT VALUES", qualified);
        return client::execute_typed(conn_params, &query, &[]).await;
    }

    let column_types = client::get_column_types_map(conn_params, table, schema).await.unwrap_or_default();
    let enum_types = client::get_enum_column_types(conn_params, schema, table).await.unwrap_or_default();

    let mut cols: Vec<String> = Vec::with_capacity(entries.len());
    let mut sql_fragments: Vec<String> = Vec::with_capacity(entries.len());
    let mut owned_params: Vec<crate::binding::TypedPgParam> = Vec::new();
    let mut placeholder_idx = 1usize;

    for (col_name, val) in entries {
        cols.push(format!("\"{}\"", col_name.replace('"', "\"\"")));
        let column_type = column_types.get(&col_name).map(String::as_str);
        let options = BindOptions {
            column_type,
            enum_type: enum_types.get(&col_name).map(String::as_str),
            allow_default: false,
        };
        let bound = bind_pg_value(val, placeholder_idx, &options)?;
        sql_fragments.push(bound.sql);
        if let Some(param) = bound.param {
            owned_params.push(param);
            placeholder_idx += 1;
        }
    }

    let query = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        qualified,
        cols.join(", "),
        sql_fragments.join(", ")
    );

    let typed_params: Vec<(&(dyn ToSql + Sync), Type)> = owned_params
        .iter()
        .map(|(p, t)| (p.as_ref() as &(dyn ToSql + Sync), t.clone()))
        .collect();

    client::execute_typed(conn_params, &query, &typed_params).await
}

pub async fn update_record(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let table = params.get("table").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");
    let col_name = params.get("col_name").and_then(Value::as_str).unwrap_or("");
    let new_val = params.get("new_val").cloned().unwrap_or(Value::Null);
    let pk_map = params
        .get("pk_map")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    match exec_update(&conn_params, table, &pk_map, col_name, new_val, schema).await {
        Ok(affected) => ok_response(id, Value::from(affected)),
        Err(e) => error_response(id, -32603, &e),
    }
}

async fn exec_update(
    conn_params: &ConnectionParams,
    table: &str,
    pk_map: &serde_json::Map<String, Value>,
    col_name: &str,
    new_val: Value,
    schema: &str,
) -> Result<u64, String> {
    let qualified = format!("\"{}\".\"{}\"", schema.replace('"', "\"\""), table.replace('"', "\"\""));

    let column_types = client::get_column_types_map(conn_params, table, schema).await.unwrap_or_default();
    let enum_types = client::get_enum_column_types(conn_params, schema, table).await.unwrap_or_default();

    let options = BindOptions {
        column_type: column_types.get(col_name).map(String::as_str),
        enum_type: enum_types.get(col_name).map(String::as_str),
        allow_default: true,
    };
    let bound = bind_pg_value(new_val, 1, &options)?;

    let mut owned_params: Vec<crate::binding::TypedPgParam> = Vec::new();
    let mut placeholder_idx = 1usize;
    if let Some(param) = bound.param {
        owned_params.push(param);
        placeholder_idx = 2;
    }

    let (predicate, pk_params) = build_pk_map_predicate(pk_map, &column_types, placeholder_idx)?;
    owned_params.extend(pk_params);

    let query = format!(
        "UPDATE {} SET \"{}\" = {} WHERE {}",
        qualified,
        col_name.replace('"', "\"\""),
        bound.sql,
        predicate
    );

    let typed_params: Vec<(&(dyn ToSql + Sync), Type)> = owned_params
        .iter()
        .map(|(p, t)| (p.as_ref() as &(dyn ToSql + Sync), t.clone()))
        .collect();

    client::execute_typed(conn_params, &query, &typed_params).await
}

pub async fn delete_record(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let table = params.get("table").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");
    let pk_map = params
        .get("pk_map")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    match exec_delete(&conn_params, table, &pk_map, schema).await {
        Ok(affected) => ok_response(id, Value::from(affected)),
        Err(e) => error_response(id, -32603, &e),
    }
}

async fn exec_delete(
    conn_params: &ConnectionParams,
    table: &str,
    pk_map: &serde_json::Map<String, Value>,
    schema: &str,
) -> Result<u64, String> {
    let qualified = format!("\"{}\".\"{}\"", schema.replace('"', "\"\""), table.replace('"', "\"\""));

    let column_types = client::get_column_types_map(conn_params, table, schema).await.unwrap_or_default();

    let (predicate, owned_params) = build_pk_map_predicate(pk_map, &column_types, 1)?;

    let query = format!("DELETE FROM {} WHERE {}", qualified, predicate);

    let typed_params: Vec<(&(dyn ToSql + Sync), Type)> = owned_params
        .iter()
        .map(|(p, t)| (p.as_ref() as &(dyn ToSql + Sync), t.clone()))
        .collect();

    client::execute_typed(conn_params, &query, &typed_params).await
}
