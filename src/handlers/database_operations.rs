//! Native whole-database operations; mirrors the built-in PostgreSQL driver.
use crate::{
    client,
    models::{inner_params, ConnectionParams},
    rpc::{error_response, ok_response},
};
use serde_json::Value;

pub async fn database_operation(id: Value, request: &Value) -> Value {
    match execute(request).await {
        Ok(()) => ok_response(id, Value::Null),
        Err(error) => error_response(id, -32603, &error),
    }
}

async fn execute(request: &Value) -> Result<(), String> {
    let raw = inner_params(request);
    if raw.get("read_only").and_then(Value::as_bool) == Some(true) {
        return Err("Connection is read-only".into());
    }
    let original_params = ConnectionParams::from_value(raw);
    let mut params = original_params.clone();
    // A browsed target must override the database embedded in a legacy URI.
    // Retain the URI's endpoint and credentials, exactly as client.rs parses them.
    if let Some(uri) = params
        .connection_string
        .as_deref()
        .filter(|uri| !uri.trim().is_empty())
    {
        let parsed: tokio_postgres::Config = uri
            .parse()
            .map_err(|error| format!("Invalid connection string: {error}"))?;
        params.host = parsed.get_hosts().first().and_then(|host| match host {
            tokio_postgres::config::Host::Tcp(host) => Some(host.clone()),
            #[cfg(unix)]
            tokio_postgres::config::Host::Unix(_) => None,
        });
        params.port = parsed.get_ports().first().copied();
        params.username = parsed.get_user().map(str::to_string);
        params.password = parsed
            .get_password()
            .map(|password| String::from_utf8_lossy(password).into_owned());
        params.connection_string = None;
    }
    let database = params.database.as_deref().unwrap_or("");
    if database.is_empty()
        || database.contains('\0')
        || ["postgres", "template0", "template1"]
            .iter()
            .any(|name| database.eq_ignore_ascii_case(name))
    {
        return Err("Database is empty, invalid, or protected".into());
    }
    let operation = request
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or("");
    let sql = match operation {
        "empty" => include_str!("database_empty.sql"),
        "truncate" => include_str!("database_truncate.sql"),
        "drop" => {
            if original_params.database == params.database {
                client::close_database_pool(&original_params)?;
            }
            client::close_database_pool(&params)?;
            let mut admin = params.clone();
            admin.database = Some("postgres".into());
            admin.startup_script = None;
            let client = client::oneshot_pool_client(&admin).await?;
            return client
                .batch_execute(&format!(
                    "DROP DATABASE \"{}\"",
                    database.replace('"', "\"\"")
                ))
                .await
                .map_err(|e| client::format_pg_error(&e));
        }
        _ => return Err("Unsupported database operation".into()),
    };
    let mut client = client::get_pool_client(&params).await?;
    let actual: String = client
        .query_one("SELECT current_database()", &[])
        .await
        .map_err(|e| client::format_pg_error(&e))?
        .get(0);
    if actual != database {
        return Err("Connected database does not match the requested target".into());
    }
    let tx = client
        .transaction()
        .await
        .map_err(|e| client::format_pg_error(&e))?;
    tx.batch_execute(sql)
        .await
        .map_err(|e| client::format_pg_error(&e))?;
    tx.commit().await.map_err(|e| client::format_pg_error(&e))
}
