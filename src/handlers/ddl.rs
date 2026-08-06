//! DDL generation handlers — get_create_table_sql, get_add_column_sql,
//! get_alter_column_sql, get_create_index_sql, get_create_foreign_key_sql
//! (pure SQL string generation, no DB round-trip) plus drop_index and
//! drop_foreign_key (which execute against the database).
//!
//! Mirrors the built-in driver's DDL generation exactly
//! (`src-tauri/src/drivers/postgres/mod.rs` get_create_table_sql and
//! friends, `helpers.rs::is_implicit_cast_compatible`) so both drivers
//! produce byte-identical SQL for the same inputs.

use serde_json::Value;

use crate::client;
use crate::models::{inner_params, ColumnDefinition, ConnectionParams};
use crate::rpc::{error_response, ok_response};
use crate::utils::identifiers::qualified;

pub async fn get_create_table_sql(id: Value, params: &Value) -> Value {
    let table_name = params.get("table_name").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");
    let columns: Vec<ColumnDefinition> = params
        .get("columns")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    ok_response(id, Value::from(vec![build_create_table_sql(table_name, &columns, schema)]))
}

pub async fn get_add_column_sql(id: Value, params: &Value) -> Value {
    let table = params.get("table").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");
    let column: Option<ColumnDefinition> = params
        .get("column")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    match column {
        Some(column) => ok_response(id, Value::from(vec![build_add_column_sql(table, &column, schema)])),
        None => error_response(id, -32602, "Invalid params: missing or malformed 'column'"),
    }
}

pub async fn get_alter_column_sql(id: Value, params: &Value) -> Value {
    let table = params.get("table").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");
    let old_column: Option<ColumnDefinition> = params
        .get("old_column")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let new_column: Option<ColumnDefinition> = params
        .get("new_column")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    match (old_column, new_column) {
        (Some(old_column), Some(new_column)) => {
            match build_alter_column_sql(table, &old_column, &new_column, schema) {
                Ok(stmts) => ok_response(id, Value::from(stmts)),
                Err(e) => error_response(id, -32603, &e),
            }
        }
        _ => error_response(id, -32602, "Invalid params: missing or malformed old_column/new_column"),
    }
}

pub async fn get_create_index_sql(id: Value, params: &Value) -> Value {
    let table = params.get("table").and_then(Value::as_str).unwrap_or("");
    let index_name = params.get("index_name").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");
    let is_unique = params.get("is_unique").and_then(Value::as_bool).unwrap_or(false);
    let columns: Vec<String> = params
        .get("columns")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    ok_response(
        id,
        Value::from(vec![build_create_index_sql(table, index_name, &columns, is_unique, schema)]),
    )
}

pub async fn get_create_foreign_key_sql(id: Value, params: &Value) -> Value {
    let table = params.get("table").and_then(Value::as_str).unwrap_or("");
    let fk_name = params.get("fk_name").and_then(Value::as_str).unwrap_or("");
    let column = params.get("column").and_then(Value::as_str).unwrap_or("");
    let ref_table = params.get("ref_table").and_then(Value::as_str).unwrap_or("");
    let ref_column = params.get("ref_column").and_then(Value::as_str).unwrap_or("");
    let on_delete = params.get("on_delete").and_then(Value::as_str);
    let on_update = params.get("on_update").and_then(Value::as_str);
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    ok_response(
        id,
        Value::from(vec![build_create_foreign_key_sql(
            table, fk_name, column, ref_table, ref_column, on_delete, on_update, schema,
        )]),
    )
}

pub async fn drop_index(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let index_name = params.get("index_name").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    let query = format!("DROP INDEX {}", qualified(schema, index_name));
    match client::execute_typed(&conn_params, &query, &[]).await {
        Ok(_) => ok_response(id, Value::Null),
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn drop_foreign_key(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let table = params.get("table").and_then(Value::as_str).unwrap_or("");
    let fk_name = params.get("fk_name").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    let query = format!(
        "ALTER TABLE {} DROP CONSTRAINT \"{}\"",
        qualified(schema, table),
        fk_name.replace('"', "\"\""),
    );
    match client::execute_typed(&conn_params, &query, &[]).await {
        Ok(_) => ok_response(id, Value::Null),
        Err(e) => error_response(id, -32603, &e),
    }
}

/// Render a column's declared type, substituting the appropriate serial
/// variant when the column is auto-increment (SERIAL/BIGSERIAL/SMALLSERIAL
/// cannot be combined with an explicit NOT NULL/DEFAULT clause the way a
/// plain integer type can).
fn resolve_column_type(column: &ColumnDefinition) -> String {
    if !column.is_auto_increment {
        return column.data_type.clone();
    }
    let upper = column.data_type.to_uppercase();
    if upper.contains("BIGINT") || upper.contains("BIGSERIAL") {
        "BIGSERIAL".to_string()
    } else if upper.contains("SMALLINT") || upper.contains("SMALLSERIAL") {
        "SMALLSERIAL".to_string()
    } else {
        "SERIAL".to_string()
    }
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn build_create_table_sql(table_name: &str, columns: &[ColumnDefinition], schema: &str) -> String {
    let mut col_defs = Vec::with_capacity(columns.len());
    let mut pk_cols = Vec::new();

    for col in columns {
        let type_str = resolve_column_type(col);
        let mut def = format!("{} {}", quote_ident(&col.name), type_str);
        if !col.is_nullable && !col.is_auto_increment {
            def.push_str(" NOT NULL");
        }
        if let Some(default) = &col.default_value {
            if !col.is_auto_increment {
                def.push_str(&format!(" DEFAULT {}", default));
            }
        }
        col_defs.push(def);
        if col.is_pk {
            pk_cols.push(quote_ident(&col.name));
        }
    }

    if !pk_cols.is_empty() {
        col_defs.push(format!("PRIMARY KEY ({})", pk_cols.join(", ")));
    }

    format!(
        "CREATE TABLE {} (\n  {}\n)",
        qualified(schema, table_name),
        col_defs.join(",\n  ")
    )
}

fn build_add_column_sql(table: &str, column: &ColumnDefinition, schema: &str) -> String {
    let type_str = resolve_column_type(column);
    let mut def = format!(
        "ALTER TABLE {} ADD COLUMN {} {}",
        qualified(schema, table),
        quote_ident(&column.name),
        type_str
    );
    if !column.is_nullable && !column.is_auto_increment {
        def.push_str(" NOT NULL");
    }
    if let Some(default) = &column.default_value {
        if !column.is_auto_increment {
            def.push_str(&format!(" DEFAULT {}", default));
        }
    }
    def
}

/// Normalize a data type string for cast-compatibility comparison:
/// strip a trailing `(...)` and uppercase. E.g. `"varchar(255)"` -> `"VARCHAR"`.
fn extract_base_type(data_type: &str) -> String {
    data_type.split('(').next().unwrap_or(data_type).trim().to_uppercase()
}

/// Whether an ALTER COLUMN TYPE from `old_type` to `new_type` can rely on
/// PostgreSQL's implicit cast rather than needing an explicit `USING` clause.
fn is_implicit_cast_compatible(old_type: &str, new_type: &str) -> bool {
    if old_type == new_type {
        return true;
    }

    const COMPATIBLE_GROUPS: &[&[&str]] = &[
        &["SMALLINT", "INTEGER", "BIGINT", "SERIAL", "BIGSERIAL", "SMALLSERIAL"],
        &["REAL", "DOUBLE PRECISION", "NUMERIC", "DECIMAL", "MONEY"],
        &["CHAR", "VARCHAR", "TEXT", "NAME", "CITEXT"],
        &["TIMESTAMP", "TIMESTAMPTZ"],
        &["TIME", "TIMETZ"],
        &["JSON", "JSONB"],
        &["BIT", "VARBIT"],
    ];

    COMPATIBLE_GROUPS
        .iter()
        .any(|group| group.contains(&old_type) && group.contains(&new_type))
}

fn build_alter_column_sql(
    table: &str,
    old_column: &ColumnDefinition,
    new_column: &ColumnDefinition,
    schema: &str,
) -> Result<Vec<String>, String> {
    let tbl = qualified(schema, table);
    let old_name = quote_ident(&old_column.name);
    let new_name = quote_ident(&new_column.name);
    let mut stmts = Vec::new();

    if old_column.name != new_column.name {
        stmts.push(format!("ALTER TABLE {} RENAME COLUMN {} TO {}", tbl, old_name, new_name));
    }

    let col_ref = &new_name;

    if old_column.data_type != new_column.data_type {
        let old_base = extract_base_type(&old_column.data_type);
        let new_base = extract_base_type(&new_column.data_type);

        if is_implicit_cast_compatible(&old_base, &new_base) {
            stmts.push(format!(
                "ALTER TABLE {} ALTER COLUMN {} TYPE {}",
                tbl, col_ref, new_column.data_type
            ));
        } else {
            stmts.push(format!(
                "ALTER TABLE {} ALTER COLUMN {} TYPE {} USING {}::{}",
                tbl, col_ref, new_column.data_type, col_ref, new_column.data_type
            ));
        }
    }

    if old_column.is_nullable != new_column.is_nullable {
        if new_column.is_nullable {
            stmts.push(format!("ALTER TABLE {} ALTER COLUMN {} DROP NOT NULL", tbl, col_ref));
        } else {
            stmts.push(format!("ALTER TABLE {} ALTER COLUMN {} SET NOT NULL", tbl, col_ref));
        }
    }

    if old_column.default_value != new_column.default_value {
        if let Some(default) = &new_column.default_value {
            stmts.push(format!(
                "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {}",
                tbl, col_ref, default
            ));
        } else {
            stmts.push(format!("ALTER TABLE {} ALTER COLUMN {} DROP DEFAULT", tbl, col_ref));
        }
    }

    if stmts.is_empty() {
        return Err("No changes detected".to_string());
    }
    Ok(stmts)
}

fn build_create_index_sql(table: &str, index_name: &str, columns: &[String], is_unique: bool, schema: &str) -> String {
    let unique = if is_unique { "UNIQUE " } else { "" };
    let cols: Vec<String> = columns.iter().map(|c| quote_ident(c)).collect();
    format!(
        "CREATE {}INDEX {} ON {} ({})",
        unique,
        quote_ident(index_name),
        qualified(schema, table),
        cols.join(", ")
    )
}

fn build_create_foreign_key_sql(
    table: &str,
    fk_name: &str,
    column: &str,
    ref_table: &str,
    ref_column: &str,
    on_delete: Option<&str>,
    on_update: Option<&str>,
    schema: &str,
) -> String {
    let mut query = format!(
        "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
        qualified(schema, table),
        quote_ident(fk_name),
        quote_ident(column),
        qualified(schema, ref_table),
        quote_ident(ref_column),
    );
    if let Some(action) = on_delete {
        query.push_str(&format!(" ON DELETE {}", action));
    }
    if let Some(action) = on_update {
        query.push_str(&format!(" ON UPDATE {}", action));
    }
    query
}

#[cfg(test)]
#[path = "ddl_tests.rs"]
mod ddl_tests;
