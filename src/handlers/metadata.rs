//! Schema discovery and metadata handlers.

use serde_json::{json, Value};

use crate::client;
use crate::models::{ConnectionParams, inner_params};
use crate::rpc::{error_response, not_implemented, ok_response};

pub async fn get_databases(id: Value, params: &Value) -> Value {
    let mut conn_params = ConnectionParams::from_value(inner_params(params));
    // Must connect to 'postgres' maintenance DB to list all databases.
    conn_params.database = Some("postgres".to_string());

    match client::query_strings(
        &conn_params,
        "SELECT datname::text FROM pg_database WHERE datistemplate = false ORDER BY datname",
        &[],
        "datname",
    )
    .await
    {
        Ok(databases) => ok_response(id, json!(databases)),
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn get_schemas(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));

    match client::query_strings(
        &conn_params,
        "SELECT schema_name::text FROM information_schema.schemata \
         WHERE schema_name NOT IN ('pg_catalog', 'information_schema', 'pg_toast') \
         AND schema_name NOT LIKE 'pg_temp_%' \
         AND schema_name NOT LIKE 'pg_toast_temp_%' \
         ORDER BY schema_name",
        &[],
        "schema_name",
    )
    .await
    {
        Ok(schemas) => ok_response(id, json!(schemas)),
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn get_tables(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let schema = params
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or("public");

    match client::query_strings(
        &conn_params,
        "SELECT table_name::text as name FROM information_schema.tables \
         WHERE table_schema = $1 AND table_type = 'BASE TABLE' \
         ORDER BY table_name ASC",
        &[&schema],
        "name",
    )
    .await
    {
        Ok(names) => {
            let tables: Vec<Value> = names.into_iter().map(|n| json!({"name": n})).collect();
            ok_response(id, json!(tables))
        }
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn get_columns(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let table = params.get("table").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    let query = r#"
        SELECT
            c.column_name::text,
            CASE
                WHEN c.data_type = 'USER-DEFINED' THEN c.udt_name::text
                ELSE c.data_type::text
            END AS data_type,
            c.is_nullable::text,
            c.column_default::text,
            c.is_identity::text,
            c.character_maximum_length,
            (SELECT string_agg('''' || replace(e.enumlabel, '''', '''''') || '''', ',' ORDER BY e.enumsortorder)
             FROM pg_enum e
             JOIN pg_type t ON t.oid = e.enumtypid
             JOIN pg_namespace tn ON tn.oid = t.typnamespace
             WHERE t.typname = c.udt_name AND tn.nspname = c.udt_schema) AS enum_values,
            EXISTS (
                SELECT 1
                FROM pg_constraint pk_con
                JOIN pg_class pk_table ON pk_table.oid = pk_con.conrelid
                JOIN pg_namespace pk_schema ON pk_schema.oid = pk_table.relnamespace
                JOIN unnest(pk_con.conkey) AS pk_col(attnum) ON true
                JOIN pg_attribute pk_att
                    ON pk_att.attrelid = pk_table.oid
                    AND pk_att.attnum = pk_col.attnum
                    AND NOT pk_att.attisdropped
                WHERE pk_con.contype = 'p'
                    AND pk_schema.nspname = c.table_schema
                    AND pk_table.relname = c.table_name
                    AND pk_att.attname = c.column_name
            ) AS is_pk
        FROM information_schema.columns c
        WHERE c.table_schema = $1 AND c.table_name = $2
        ORDER BY c.ordinal_position
    "#;

    match client::query_rows(&conn_params, query, &[&schema, &table]).await {
        Ok(rows) => {
            let columns: Vec<Value> = rows.iter().map(row_to_table_column).collect();
            ok_response(id, json!(columns))
        }
        Err(e) => error_response(id, -32603, &e),
    }
}

/// Map one `information_schema.columns`-shaped row (as queried by
/// `get_columns`/`get_view_columns`) to the host's `TableColumn` JSON shape.
fn row_to_table_column(r: &tokio_postgres::Row) -> Value {
    let name: String = r.try_get("column_name").unwrap_or_default();
    let raw_data_type: String = r.try_get("data_type").unwrap_or_default();
    let enum_values: Option<String> = r.try_get("enum_values").ok().flatten();
    let is_nullable_str: String = r.try_get("is_nullable").unwrap_or_default();
    let column_default: Option<String> = r.try_get("column_default").ok().flatten();
    let is_identity: String = r.try_get("is_identity").unwrap_or_default();
    let char_max_len: Option<i64> = r
        .try_get::<_, Option<i64>>("character_maximum_length")
        .ok()
        .flatten();
    let is_pk: bool = r.try_get("is_pk").unwrap_or(false);

    let data_type = match enum_values {
        Some(ref vals) if !vals.is_empty() => format!("enum({})", vals),
        _ => raw_data_type,
    };

    let is_auto_increment = is_identity == "YES"
        || column_default.as_deref().map_or(false, |d| d.contains("nextval"));

    let is_nullable = is_nullable_str == "YES";

    let default_value = column_default.as_deref().and_then(|d| {
        if is_auto_increment || d.is_empty() || d == "NULL" || d.starts_with("NULL::") {
            None
        } else {
            Some(d.to_string())
        }
    });

    let mut col = json!({
        "name": name,
        "data_type": data_type,
        "is_pk": is_pk,
        "is_nullable": is_nullable,
        "is_auto_increment": is_auto_increment,
    });

    if let Some(dv) = default_value {
        col.as_object_mut().unwrap().insert("default_value".to_string(), json!(dv));
    }
    if let Some(len) = char_max_len.and_then(|v| u64::try_from(v).ok()) {
        col.as_object_mut()
            .unwrap()
            .insert("character_maximum_length".to_string(), json!(len));
    }

    col
}

pub async fn get_foreign_keys(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let table = params.get("table").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    let query = r#"
        SELECT
            con.conname::text AS constraint_name,
            src_att.attname::text AS column_name,
            ref_nsp.nspname::text AS foreign_schema_name,
            ref_cl.relname::text AS foreign_table_name,
            ref_att.attname::text AS foreign_column_name,
            CASE con.confupdtype
                WHEN 'a' THEN 'NO ACTION'
                WHEN 'r' THEN 'RESTRICT'
                WHEN 'c' THEN 'CASCADE'
                WHEN 'n' THEN 'SET NULL'
                WHEN 'd' THEN 'SET DEFAULT'
            END::text AS update_rule,
            CASE con.confdeltype
                WHEN 'a' THEN 'NO ACTION'
                WHEN 'r' THEN 'RESTRICT'
                WHEN 'c' THEN 'CASCADE'
                WHEN 'n' THEN 'SET NULL'
                WHEN 'd' THEN 'SET DEFAULT'
            END::text AS delete_rule
        FROM pg_constraint con
        JOIN pg_class src_cl ON src_cl.oid = con.conrelid
        JOIN pg_namespace src_nsp ON src_nsp.oid = src_cl.relnamespace
        JOIN pg_class ref_cl ON ref_cl.oid = con.confrelid
        JOIN pg_namespace ref_nsp ON ref_nsp.oid = ref_cl.relnamespace
        JOIN unnest(con.conkey, con.confkey) AS cols(src_attnum, ref_attnum) ON true
        JOIN pg_attribute src_att
            ON src_att.attrelid = src_cl.oid
            AND src_att.attnum = cols.src_attnum
            AND NOT src_att.attisdropped
        JOIN pg_attribute ref_att
            ON ref_att.attrelid = ref_cl.oid
            AND ref_att.attnum = cols.ref_attnum
            AND NOT ref_att.attisdropped
        WHERE con.contype = 'f'
          AND con.conparentid = 0
          AND src_nsp.nspname = $1
          AND src_cl.relname = $2
        ORDER BY con.conname, cols.src_attnum
    "#;

    match client::query_rows(&conn_params, query, &[&schema, &table]).await {
        Ok(rows) => {
            let fks: Vec<Value> = rows
                .iter()
                .map(|r| {
                    let name: String = r.try_get("constraint_name").unwrap_or_default();
                    let column_name: String = r.try_get("column_name").unwrap_or_default();
                    let ref_table: String = r.try_get("foreign_table_name").unwrap_or_default();
                    let ref_column: String = r.try_get("foreign_column_name").unwrap_or_default();
                    let on_update: Option<String> = r.try_get("update_rule").ok().flatten();
                    let on_delete: Option<String> = r.try_get("delete_rule").ok().flatten();

                    json!({
                        "name": name,
                        "column_name": column_name,
                        "ref_table": ref_table,
                        "ref_column": ref_column,
                        "on_delete": on_delete,
                        "on_update": on_update,
                    })
                })
                .collect();
            ok_response(id, json!(fks))
        }
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn get_indexes(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let table = params.get("table").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    let query = r#"
        SELECT
            i.relname AS index_name,
            COALESCE(
                a.attname::text,
                pg_get_indexdef(ix.indexrelid, k.n::int, true)
            ) AS column_name,
            ix.indisunique AS is_unique,
            ix.indisprimary AS is_primary,
            k.n::int AS seq_in_index,
            (k.attnum = 0) AS is_expression
        FROM
            pg_class t
            JOIN pg_namespace n ON t.relnamespace = n.oid
            JOIN pg_index ix ON t.oid = ix.indrelid
            JOIN pg_class i ON i.oid = ix.indexrelid
            CROSS JOIN LATERAL unnest(string_to_array(ix.indkey::text, ' ')::int2[])
                WITH ORDINALITY AS k(attnum, n)
            LEFT JOIN pg_attribute a
                ON a.attrelid = t.oid
                AND a.attnum = k.attnum
                AND k.attnum <> 0
        WHERE
            t.relkind IN ('r', 'm')
            AND n.nspname = $1
            AND t.relname = $2
        ORDER BY
            i.relname,
            k.n
    "#;

    match client::query_rows(&conn_params, query, &[&schema, &table]).await {
        Ok(rows) => {
            let indexes: Vec<Value> = rows
                .iter()
                .map(|r| {
                    let name: String = r.try_get("index_name").unwrap_or_default();
                    let column_name: String = r.try_get("column_name").unwrap_or_default();
                    let is_unique: bool = r.try_get("is_unique").unwrap_or(false);
                    let is_primary: bool = r.try_get("is_primary").unwrap_or(false);
                    let seq_in_index: i32 = r.try_get("seq_in_index").unwrap_or(1);
                    let is_expression: bool = r.try_get("is_expression").unwrap_or(false);

                    json!({
                        "name": name,
                        "column_name": column_name,
                        "is_unique": is_unique,
                        "is_primary": is_primary,
                        "seq_in_index": seq_in_index,
                        "is_expression": is_expression,
                    })
                })
                .collect();
            ok_response(id, json!(indexes))
        }
        Err(e) => error_response(id, -32603, &e),
    }
}
pub async fn get_views(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    match client::query_strings(
        &conn_params,
        "SELECT viewname as name FROM pg_views WHERE schemaname = $1 ORDER BY viewname ASC",
        &[&schema],
        "name",
    )
    .await
    {
        Ok(names) => {
            let views: Vec<Value> = names
                .into_iter()
                .map(|n| json!({"name": n, "definition": null}))
                .collect();
            ok_response(id, json!(views))
        }
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn get_view_definition(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let view_name = params.get("view_name").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    let qualified = crate::utils::identifiers::qualified(schema, view_name);

    match client::query_rows(
        &conn_params,
        "SELECT pg_get_viewdef(($1::text)::regclass, true) as definition",
        &[&qualified],
    )
    .await
    {
        Ok(rows) => {
            if let Some(row) = rows.first() {
                let definition: String = row.try_get("definition").unwrap_or_default();
                let full = format!("CREATE OR REPLACE VIEW {} AS\n{}", qualified, definition);
                ok_response(id, json!(full))
            } else {
                error_response(id, -32603, "View not found")
            }
        }
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn get_view_columns(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let view_name = params.get("view_name").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    let query = r#"
        SELECT
            c.column_name::text,
            CASE
                WHEN c.data_type = 'USER-DEFINED' THEN c.udt_name::text
                ELSE c.data_type::text
            END AS data_type,
            c.is_nullable::text,
            c.column_default::text,
            c.is_identity::text,
            c.character_maximum_length,
            (SELECT string_agg('''' || replace(e.enumlabel, '''', '''''') || '''', ',' ORDER BY e.enumsortorder)
             FROM pg_enum e
             JOIN pg_type t ON t.oid = e.enumtypid
             JOIN pg_namespace tn ON tn.oid = t.typnamespace
             WHERE t.typname = c.udt_name AND tn.nspname = c.udt_schema) AS enum_values,
            EXISTS (
                SELECT 1
                FROM pg_constraint pk_con
                JOIN pg_class pk_table ON pk_table.oid = pk_con.conrelid
                JOIN pg_namespace pk_schema ON pk_schema.oid = pk_table.relnamespace
                JOIN unnest(pk_con.conkey) AS pk_col(attnum) ON true
                JOIN pg_attribute pk_att
                    ON pk_att.attrelid = pk_table.oid
                    AND pk_att.attnum = pk_col.attnum
                    AND NOT pk_att.attisdropped
                WHERE pk_con.contype = 'p'
                    AND pk_schema.nspname = c.table_schema
                    AND pk_table.relname = c.table_name
                    AND pk_att.attname = c.column_name
            ) AS is_pk
        FROM information_schema.columns c
        WHERE c.table_schema = $1 AND c.table_name = $2
        ORDER BY c.ordinal_position
    "#;

    match client::query_rows(&conn_params, query, &[&schema, &view_name]).await {
        Ok(rows) => {
            let columns: Vec<Value> = rows.iter().map(row_to_table_column).collect();
            ok_response(id, json!(columns))
        }
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn create_view(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let view_name = params.get("view_name").and_then(Value::as_str).unwrap_or("");
    let definition = params.get("definition").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    let query = format!(
        "CREATE VIEW {} AS {}",
        crate::utils::identifiers::qualified(schema, view_name),
        definition
    );
    match client::execute_typed(&conn_params, &query, &[]).await {
        Ok(_) => ok_response(id, Value::Null),
        Err(e) => error_response(id, -32603, &format!("Failed to create view: {}", e)),
    }
}

pub async fn alter_view(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let view_name = params.get("view_name").and_then(Value::as_str).unwrap_or("");
    let definition = params.get("definition").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    let query = format!(
        "CREATE OR REPLACE VIEW {} AS {}",
        crate::utils::identifiers::qualified(schema, view_name),
        definition
    );
    match client::execute_typed(&conn_params, &query, &[]).await {
        Ok(_) => ok_response(id, Value::Null),
        Err(e) => error_response(id, -32603, &format!("Failed to alter view: {}", e)),
    }
}

pub async fn drop_view(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let view_name = params.get("view_name").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    let query = format!(
        "DROP VIEW IF EXISTS {}",
        crate::utils::identifiers::qualified(schema, view_name)
    );
    match client::execute_typed(&conn_params, &query, &[]).await {
        Ok(_) => ok_response(id, Value::Null),
        Err(e) => error_response(id, -32603, &format!("Failed to drop view: {}", e)),
    }
}

pub async fn get_materialized_views(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    match client::query_strings(
        &conn_params,
        "SELECT matviewname as name FROM pg_matviews WHERE schemaname = $1 ORDER BY matviewname ASC",
        &[&schema],
        "name",
    )
    .await
    {
        Ok(names) => {
            let views: Vec<Value> = names
                .into_iter()
                .map(|n| json!({"name": n, "definition": null}))
                .collect();
            ok_response(id, json!(views))
        }
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn get_materialized_view_columns(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let view_name = params.get("view_name").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    // Materialized views are not exposed via information_schema.columns, so
    // their columns must be read from the system catalog.
    let query = r#"
        SELECT
            a.attname AS column_name,
            format_type(a.atttypid, a.atttypmod) AS data_type,
            a.attnotnull AS not_null
        FROM pg_attribute a
        JOIN pg_class c ON c.oid = a.attrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'm'
          AND a.attnum > 0 AND NOT a.attisdropped
        ORDER BY a.attnum
    "#;

    match client::query_rows(&conn_params, query, &[&schema, &view_name]).await {
        Ok(rows) => {
            let columns: Vec<Value> = rows
                .iter()
                .map(|r| {
                    let name: String = r.try_get("column_name").unwrap_or_default();
                    let data_type: String = r.try_get("data_type").unwrap_or_default();
                    let not_null: bool = r.try_get("not_null").unwrap_or(false);
                    json!({
                        "name": name,
                        "data_type": data_type,
                        "is_pk": false,
                        "is_nullable": !not_null,
                        "is_auto_increment": false,
                    })
                })
                .collect();
            ok_response(id, json!(columns))
        }
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn get_materialized_view_definition(id: Value, _params: &Value) -> Value { not_implemented(id, "get_materialized_view_definition") }

pub async fn refresh_materialized_view(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let view_name = params.get("view_name").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    let query = format!(
        "REFRESH MATERIALIZED VIEW {}",
        crate::utils::identifiers::qualified(schema, view_name)
    );
    match client::execute_typed(&conn_params, &query, &[]).await {
        Ok(_) => ok_response(id, Value::Null),
        Err(e) => error_response(id, -32603, &format!("Failed to refresh materialized view: {}", e)),
    }
}

pub async fn get_routines(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    // PG 11+ uses prokind; older versions use proisagg/proiswindow flags.
    // CI runs PG 16, so we use the modern query.
    let query = r#"
        SELECT proname, prokind
        FROM pg_proc
        WHERE pronamespace = (SELECT oid FROM pg_namespace WHERE nspname = $1)
        AND prokind IN ('f', 'p')
        ORDER BY proname
    "#;

    match client::query_rows(&conn_params, query, &[&schema]).await {
        Ok(rows) => {
            let routines: Vec<Value> = rows
                .iter()
                .map(|r| {
                    let name: String = r.try_get("proname").unwrap_or_default();
                    let prokind: i8 = r.try_get("prokind").unwrap_or(b'f' as i8);
                    let routine_type = if prokind as u8 as char == 'p' {
                        "PROCEDURE"
                    } else {
                        "FUNCTION"
                    };
                    json!({
                        "name": name,
                        "routine_type": routine_type,
                        "definition": null,
                    })
                })
                .collect();
            ok_response(id, json!(routines))
        }
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn get_routine_parameters(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let routine_name = params.get("routine_name").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    let return_type_query = r#"
        SELECT data_type, routine_type
        FROM information_schema.routines
        WHERE routine_schema = $1 AND routine_name = $2
        LIMIT 1
    "#;
    let routine_info = match client::query_rows(&conn_params, return_type_query, &[&schema, &routine_name]).await {
        Ok(rows) => rows,
        Err(e) => return error_response(id, -32603, &e),
    };

    let mut parameters: Vec<Value> = Vec::new();

    if let Some(info) = routine_info.first() {
        let routine_type: String = info.try_get("routine_type").unwrap_or_default();
        if routine_type == "FUNCTION" {
            let data_type: String = info.try_get("data_type").unwrap_or_default();
            if !data_type.eq_ignore_ascii_case("void") && !data_type.eq_ignore_ascii_case("trigger") {
                parameters.push(json!({
                    "name": "",
                    "data_type": data_type,
                    "mode": "OUT",
                    "ordinal_position": 0,
                }));
            }
        }
    }

    let query = r#"
        SELECT p.parameter_name, p.data_type, p.parameter_mode, p.ordinal_position
        FROM information_schema.parameters p
        JOIN information_schema.routines r ON p.specific_name = r.specific_name
        WHERE r.routine_schema = $1 AND r.routine_name = $2
        ORDER BY p.ordinal_position
    "#;
    match client::query_rows(&conn_params, query, &[&schema, &routine_name]).await {
        Ok(rows) => {
            parameters.extend(rows.iter().map(|r| {
                let name: Option<String> = r.try_get("parameter_name").ok().flatten();
                let data_type: String = r.try_get("data_type").unwrap_or_default();
                let mode: String = r.try_get("parameter_mode").unwrap_or_default();
                let ordinal_position: i32 = r.try_get("ordinal_position").unwrap_or(0);
                json!({
                    "name": name.unwrap_or_default(),
                    "data_type": data_type,
                    "mode": mode,
                    "ordinal_position": ordinal_position,
                })
            }));
            ok_response(id, json!(parameters))
        }
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn get_routine_definition(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let routine_name = params.get("routine_name").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    let query = r#"
        SELECT pg_get_functiondef(p.oid) as definition
        FROM pg_proc p
        JOIN pg_namespace n ON p.pronamespace = n.oid
        WHERE n.nspname = $1 AND p.proname = $2
        LIMIT 1
    "#;

    match client::query_rows(&conn_params, query, &[&schema, &routine_name]).await {
        Ok(rows) => match rows.first() {
            Some(row) => {
                let definition: String = row.try_get("definition").unwrap_or_default();
                ok_response(id, json!(definition))
            }
            None => error_response(id, -32603, &format!("Routine '{}' not found", routine_name)),
        },
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn get_triggers(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    let query = r#"
        SELECT
            t.trigger_name AS name,
            t.event_object_table AS table_name,
            string_agg(t.event_manipulation, ' OR ' ORDER BY t.event_manipulation) AS event,
            t.action_timing AS timing
        FROM information_schema.triggers t
        WHERE t.trigger_schema = $1
        GROUP BY t.trigger_name, t.event_object_table, t.action_timing
        ORDER BY t.trigger_name
    "#;

    match client::query_rows(&conn_params, query, &[&schema]).await {
        Ok(rows) => {
            let triggers: Vec<Value> = rows
                .iter()
                .map(|r| {
                    let name: String = r.try_get("name").unwrap_or_default();
                    let table_name: String = r.try_get("table_name").unwrap_or_default();
                    let event: String = r.try_get("event").unwrap_or_default();
                    let timing: String = r.try_get("timing").unwrap_or_default();
                    json!({
                        "name": name,
                        "table_name": table_name,
                        "event": event,
                        "timing": timing,
                        "definition": null,
                    })
                })
                .collect();
            ok_response(id, json!(triggers))
        }
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn get_trigger_definition(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let trigger_name = params.get("trigger_name").and_then(Value::as_str).unwrap_or("");
    let table_name = params.get("table_name").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    let query = r#"
        SELECT pg_get_triggerdef(t.oid, true) AS definition
        FROM pg_trigger t
        JOIN pg_class c ON t.tgrelid = c.oid
        JOIN pg_namespace n ON c.relnamespace = n.oid
        WHERE t.tgname = $1
          AND c.relname = $2
          AND n.nspname = $3
          AND NOT t.tgisinternal
        LIMIT 1
    "#;

    match client::query_rows(&conn_params, query, &[&trigger_name, &table_name, &schema]).await {
        Ok(rows) => match rows.first() {
            Some(row) => {
                let definition: String = row.try_get("definition").unwrap_or_default();
                ok_response(id, json!(definition))
            }
            None => error_response(id, -32603, &format!("Trigger '{}' not found", trigger_name)),
        },
        Err(e) => error_response(id, -32603, &e),
    }
}

pub async fn create_trigger(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let trigger_sql = params.get("trigger_sql").and_then(Value::as_str).unwrap_or("");

    match client::execute_typed(&conn_params, trigger_sql, &[]).await {
        Ok(_) => ok_response(id, Value::Null),
        Err(e) => error_response(id, -32603, &format!("Failed to create trigger: {}", e)),
    }
}

pub async fn drop_trigger(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let trigger_name = params.get("trigger_name").and_then(Value::as_str).unwrap_or("");
    let table_name = params.get("table_name").and_then(Value::as_str).unwrap_or("");
    let schema = params.get("schema").and_then(Value::as_str).unwrap_or("public");

    let query = format!(
        "DROP TRIGGER IF EXISTS {} ON {}",
        crate::utils::identifiers::quote_identifier(trigger_name),
        crate::utils::identifiers::qualified(schema, table_name),
    );
    match client::execute_typed(&conn_params, &query, &[]).await {
        Ok(_) => ok_response(id, Value::Null),
        Err(e) => error_response(id, -32603, &format!("Failed to drop trigger: {}", e)),
    }
}

pub async fn get_schema_snapshot(id: Value, _params: &Value) -> Value { not_implemented(id, "get_schema_snapshot") }
pub async fn get_all_columns_batch(id: Value, _params: &Value) -> Value { not_implemented(id, "get_all_columns_batch") }
pub async fn get_all_foreign_keys_batch(id: Value, _params: &Value) -> Value { not_implemented(id, "get_all_foreign_keys_batch") }
