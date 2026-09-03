//! Connection lifecycle handlers: initialize, ping, test_connection, shutdown.

use serde_json::Value;

use crate::client;
use crate::models::{inner_params, ConnectionParams};
use crate::rpc::{error_response, ok_response};
use crate::settings;

/// Receive plugin settings from the host. The host sends
/// `json!({ "settings": settings })` (a `HashMap<String, Value>` built from
/// this plugin's `.tabularium` setting definitions — see `RpcDriver::new` in
/// `tabularis/src-tauri/src/plugins/driver.rs`) and silently ignores any
/// error or non-response, so this must never panic. Currently the only
/// setting is `poolMaxSize`; an absent/invalid value falls back to the
/// built-in's default (10) inside the parser.
pub async fn initialize(id: Value, params: &Value) -> Value {
    let settings_value = params.get("settings").cloned().unwrap_or(Value::Null);
    settings::set_pool_max_size(&settings_value);
    ok_response(id, Value::Null)
}

/// Lightweight health check — verify we can reach the database.
pub async fn ping(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    match client::test_connection(&conn_params).await {
        Ok(()) => ok_response(id, Value::Null),
        Err(e) => error_response(id, -32603, &e),
    }
}

/// Full connection test with error reporting.
pub async fn test_connection(id: Value, params: &Value) -> Value {
    let conn_params = ConnectionParams::from_value(inner_params(params));
    match client::test_connection(&conn_params).await {
        Ok(()) => ok_response(id, Value::Null),
        Err(e) => error_response(id, -32603, &e),
    }
}

/// Graceful shutdown — drain pools and exit.
pub async fn shutdown(id: Value, _params: &Value) -> Value {
    ok_response(id, Value::Null)
}
