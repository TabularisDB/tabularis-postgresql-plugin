//! PostgreSQL-dialect SQL builders and RPC handlers for stored-routine
//! management: `build_routine_call_sql`, `routine_create_template`,
//! `drop_routine`.
//!
//! Mirrors the built-in driver exactly
//! (`src-tauri/src/drivers/postgres/routines.rs` for the pure string
//! builders, `mod.rs`'s `drop_routine` for the live identity-signature
//! lookup) so both drivers produce byte-identical SQL for the same inputs.
//! `get_routine_edit_script` is intentionally not implemented here: both the
//! builtin and the host's plugin-bridge fallback resolve it to
//! `get_routine_definition`, which this plugin already implements.

use serde_json::{json, Value};

use crate::client;
use crate::models::{inner_params, ConnectionParams, RoutineCallArg};
use crate::rpc::{error_response, ok_response};
use crate::utils::identifiers::{qualified, quote_identifier};

pub async fn build_routine_call_sql(id: Value, params: &Value) -> Value {
    let routine_name = params
        .get("routine_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let routine_type = params
        .get("routine_type")
        .and_then(Value::as_str)
        .unwrap_or("FUNCTION");
    let schema = params
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or("public");
    let args: Vec<RoutineCallArg> = params
        .get("args")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let sql = routine_call_sql(routine_name, routine_type, &args, schema);
    ok_response(id, json!(sql))
}

pub async fn routine_create_template(id: Value, params: &Value) -> Value {
    let routine_type = params
        .get("routine_type")
        .and_then(Value::as_str)
        .unwrap_or("FUNCTION");
    let schema = params
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or("public");

    ok_response(id, json!(routine_template(routine_type, schema)))
}

pub async fn drop_routine(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    let routine_name = params
        .get("routine_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let routine_type = params
        .get("routine_type")
        .and_then(Value::as_str)
        .unwrap_or("FUNCTION");
    let schema = params
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or("public");

    match exec_drop_routine(&conn_params, routine_name, routine_type, schema).await {
        Ok(()) => ok_response(id, Value::Null),
        Err(e) => error_response(id, -32603, &e),
    }
}

/// Drops a routine, resolving its exact identity signature first: PostgreSQL
/// identifies routines by name *and* argument types, so a bare
/// `DROP FUNCTION name` fails as soon as overloads exist.
async fn exec_drop_routine(
    conn_params: &ConnectionParams,
    routine_name: &str,
    routine_type: &str,
    schema: &str,
) -> Result<(), String> {
    let query = r#"
        SELECT pg_get_function_identity_arguments(p.oid) AS args
        FROM pg_proc p
        JOIN pg_namespace n ON p.pronamespace = n.oid
        WHERE n.nspname = $1 AND p.proname = $2
    "#;
    let rows = client::query_rows(conn_params, query, &[&schema, &routine_name]).await?;

    match rows.len() {
        0 => Err(format!(
            "Routine '{}' not found in schema '{}'",
            routine_name, schema
        )),
        1 => {
            let identity_args: String = rows[0].try_get("args").unwrap_or_default();
            let sql = drop_routine_sql(routine_name, routine_type, &identity_args, schema);
            client::execute_typed(conn_params, &sql, &[])
                .await
                .map(|_| ())
        }
        n => Err(format!(
            "Routine '{}' has {} overloads; drop it manually specifying the argument types",
            routine_name, n
        )),
    }
}

/// Builds the invocation script. Functions go through `SELECT * FROM` so
/// both scalar and set-returning functions come back as a result set; their
/// pure `OUT` parameters are NOT part of the call signature in PostgreSQL, so
/// they are excluded from the argument list (passing them raises
/// `function ... does not exist`). Procedures use `CALL`; there OUT
/// parameters ARE required in the argument list and are rendered as `NULL`
/// placeholders, with INOUT values echoed back by the server as the
/// procedure's result row.
fn routine_call_sql(
    routine_name: &str,
    routine_type: &str,
    args: &[RoutineCallArg],
    schema: &str,
) -> String {
    let name = qualified(schema, routine_name);
    let is_function = routine_type.eq_ignore_ascii_case("FUNCTION");
    let rendered: Vec<String> = args
        .iter()
        .filter(|arg| !(is_function && arg.mode.eq_ignore_ascii_case("OUT")))
        .map(render_sql_literal)
        .collect();
    let arg_list = rendered.join(", ");
    if is_function {
        format!("SELECT * FROM {}({});", name, arg_list)
    } else {
        format!("CALL {}({});", name, arg_list)
    }
}

fn render_sql_literal(arg: &RoutineCallArg) -> String {
    match &arg.value {
        None => "NULL".to_string(),
        Some(v) if arg.is_raw => v.clone(),
        Some(v) => format!("'{}'", v.replace('\'', "''")),
    }
}

/// Starter script for a new routine. `CREATE OR REPLACE` keeps the script
/// re-runnable while iterating on the body.
fn routine_template(routine_type: &str, schema: &str) -> String {
    let prefix = if schema.is_empty() {
        String::new()
    } else {
        format!("{}.", quote_identifier(schema))
    };
    if routine_type.eq_ignore_ascii_case("FUNCTION") {
        format!(
            r#"CREATE OR REPLACE FUNCTION {prefix}my_function(p_value integer)
RETURNS integer
LANGUAGE plpgsql
AS $$
BEGIN
    RETURN p_value;
END;
$$;
"#
        )
    } else {
        format!(
            r#"CREATE OR REPLACE PROCEDURE {prefix}my_procedure(p_value integer)
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE NOTICE 'value: %', p_value;
END;
$$;
"#
        )
    }
}

/// `DROP` statement for a routine identified by its exact signature (the
/// output of `pg_get_function_identity_arguments`), which is how PostgreSQL
/// disambiguates overloads.
fn drop_routine_sql(
    routine_name: &str,
    routine_type: &str,
    identity_args: &str,
    schema: &str,
) -> String {
    let keyword = if routine_type.eq_ignore_ascii_case("PROCEDURE") {
        "PROCEDURE"
    } else {
        "FUNCTION"
    };
    format!(
        "DROP {} {}({})",
        keyword,
        qualified(schema, routine_name),
        identity_args
    )
}

#[cfg(test)]
#[path = "routines_tests.rs"]
mod routines_tests;
