//! Connection lifecycle handlers: initialize, ping, test_connection, shutdown.

use serde_json::Value;

use crate::rpc::{ok_response, error_response};
use crate::models::{ConnectionParams, inner_params};
use crate::client;

/// Receive plugin settings from the host. Currently a no-op.
pub async fn initialize(id: Value, _params: &Value) -> Value {
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
