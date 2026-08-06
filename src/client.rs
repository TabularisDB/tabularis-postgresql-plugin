//! PostgreSQL connection pool management via deadpool-postgres.
//!
//! Provides pool construction with optional TLS (via rustls), a process-wide
//! cache keyed by connection identity, and query helpers for common patterns
//! (single-column string queries, parameterized queries).
//!
//! # Pool caching
//!
//! Every RPC call originally built a brand-new `Pool` (connect, run one
//! query, discard) — noted as a Sprint 1 TODO ("Pool caching by connection
//! key will be added in Sprint 2") that was never followed up. Besides being
//! wasteful, a fresh TCP connect on every single call has no retry margin: a
//! transient connection hiccup on one call (e.g. a setup step in a test) is
//! silently swallowed by the caller and never retried, unlike a persistent
//! pool where a single connection failure doesn't affect already-established
//! connections. Caching by `host:port:database:user` (matches the builtin's
//! `build_connection_key` pattern in `src-tauri/src/pool_manager.rs`, minus
//! the TLS/connection_id refinements that plugin doesn't need yet) closes
//! that gap.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use deadpool_postgres::{Config, ManagerConfig, Pool, RecyclingMethod, Runtime};
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{NoTls, Row};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::models::ConnectionParams;

static POOLS: LazyLock<Mutex<HashMap<String, Pool>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Build a connection pool from the given params and verify connectivity
/// by acquiring one client and running `SELECT 1`.
pub async fn test_connection(params: &ConnectionParams) -> Result<(), String> {
    let pool = get_or_create_pool(params)?;
    let client = pool
        .get()
        .await
        .map_err(|e| format!("Connection failed: {e}"))?;
    client
        .query_one("SELECT 1", &[])
        .await
        .map_err(|e| format!("Query failed: {e}"))?;
    Ok(())
}

/// Run a query and extract a single text column from each row.
/// Used for schema discovery methods that return `Vec<String>`.
pub async fn query_strings(
    params: &ConnectionParams,
    query: &str,
    query_params: &[&(dyn ToSql + Sync)],
    column: &str,
) -> Result<Vec<String>, String> {
    let pool = get_or_create_pool(params)?;
    let client = pool
        .get()
        .await
        .map_err(|e| format!("Connection failed: {e}"))?;
    let rows = client
        .query(query, query_params)
        .await
        .map_err(|e| format!("Query failed: {e}"))?;

    let results = rows
        .iter()
        .map(|r| r.try_get::<_, String>(column).unwrap_or_default())
        .collect();
    Ok(results)
}

/// Run a query and return the raw rows for caller-side mapping.
pub async fn query_rows(
    params: &ConnectionParams,
    query: &str,
    query_params: &[&(dyn ToSql + Sync)],
) -> Result<Vec<Row>, String> {
    let pool = get_or_create_pool(params)?;
    let client = pool
        .get()
        .await
        .map_err(|e| format!("Connection failed: {e}"))?;
    client
        .query(query, query_params)
        .await
        .map_err(|e| format!("Query failed: {e}"))
}

/// Execute a statement with explicit per-placeholder wire types, pinned via
/// `prepare_typed`. Required for `CAST($N AS X)`-style placeholders where
/// letting the server infer the type from query context would reject the
/// bind before PostgreSQL's own parser sees the value. Returns affected rows.
pub async fn execute_typed(
    params: &ConnectionParams,
    query: &str,
    typed_params: &[(&(dyn ToSql + Sync), Type)],
) -> Result<u64, String> {
    let pool = get_or_create_pool(params)?;
    let client = pool
        .get()
        .await
        .map_err(|e| format!("Connection failed: {e}"))?;
    let types: Vec<Type> = typed_params.iter().map(|(_, t)| t.clone()).collect();
    let stmt = client
        .prepare_typed(query, &types)
        .await
        .map_err(|e| format!("Prepare failed: {e}"))?;
    let values: Vec<&(dyn ToSql + Sync)> = typed_params.iter().map(|(v, _)| *v).collect();
    client
        .execute(&stmt, &values)
        .await
        .map_err(|e| format!("Execute failed: {e}"))
}

/// Run a SELECT with explicit per-placeholder wire types (same rationale as
/// `execute_typed`) and return the resulting rows.
pub async fn query_typed(
    params: &ConnectionParams,
    query: &str,
    typed_params: &[(&(dyn ToSql + Sync), Type)],
) -> Result<Vec<Row>, String> {
    let pool = get_or_create_pool(params)?;
    let client = pool
        .get()
        .await
        .map_err(|e| format!("Connection failed: {e}"))?;
    let types: Vec<Type> = typed_params.iter().map(|(_, t)| t.clone()).collect();
    let stmt = client
        .prepare_typed(query, &types)
        .await
        .map_err(|e| format!("Prepare failed: {e}"))?;
    let values: Vec<&(dyn ToSql + Sync)> = typed_params.iter().map(|(v, _)| *v).collect();
    client
        .query(&stmt, &values)
        .await
        .map_err(|e| format!("Query failed: {e}"))
}

/// Fetch data types for every column in a table as a name -> type map.
/// Used by insert to resolve type-aware binding for all columns in one query.
pub async fn get_column_types_map(
    params: &ConnectionParams,
    table: &str,
    schema: &str,
) -> Result<std::collections::HashMap<String, String>, String> {
    let query = r#"
        SELECT
            column_name,
            CASE
                WHEN data_type = 'USER-DEFINED' THEN udt_name
                ELSE data_type
            END AS resolved_type
        FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = $2
    "#;
    let rows = query_rows(params, query, &[&schema, &table]).await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let name: String = r.try_get("column_name").ok()?;
            let ty: String = r.try_get("resolved_type").ok()?;
            Some((name, ty))
        })
        .collect())
}

/// Fetch the schema-qualified, quoted enum type name for every enum column
/// in a table (e.g. `current_mood -> "test_schema"."mood"`). Columns not
/// backed by an enum type are absent from the map.
pub async fn get_enum_column_types(
    params: &ConnectionParams,
    schema: &str,
    table: &str,
) -> Result<std::collections::HashMap<String, String>, String> {
    let query = "SELECT a.attname::text AS column_name, \
        tn.nspname::text AS type_schema, t.typname::text AS type_name \
        FROM pg_attribute a \
        JOIN pg_class c ON c.oid = a.attrelid \
        JOIN pg_namespace n ON n.oid = c.relnamespace \
        JOIN pg_type t ON t.oid = a.atttypid \
        JOIN pg_namespace tn ON tn.oid = t.typnamespace \
        WHERE n.nspname = $1 AND c.relname = $2 \
        AND a.attnum > 0 AND NOT a.attisdropped AND t.typtype = 'e'";

    let rows = query_rows(params, query, &[&schema, &table]).await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let col: String = r.try_get("column_name").ok()?;
            let type_schema: String = r.try_get("type_schema").ok()?;
            let type_name: String = r.try_get("type_name").ok()?;
            Some((col, quote_qualified_type(&type_schema, &type_name)))
        })
        .collect())
}

/// Quote a schema-qualified type name (e.g. `"public"."mood"`) so it can be
/// spliced into a `CAST($N AS ...)` without becoming an injection vector.
fn quote_qualified_type(type_schema: &str, type_name: &str) -> String {
    format!(
        "\"{}\".\"{}\"",
        type_schema.replace('"', "\"\""),
        type_name.replace('"', "\"\""),
    )
}

/// Get the cached pool for these connection params, creating and caching one
/// on first use. Public for use by query handlers that need direct pool
/// access (e.g. to acquire one client for a multi-statement batch).
pub fn build_pool_pub(params: &ConnectionParams) -> Result<Pool, String> {
    get_or_create_pool(params)
}

/// Identifies a connection target for pool-cache purposes.
/// Matches on host:port:database:user — sufficient for this plugin's scope
/// (no per-connection TLS-mode/connection_id refinement, unlike the builtin).
fn connection_key(params: &ConnectionParams) -> String {
    format!(
        "{}:{}:{}:{}",
        params.host.as_deref().unwrap_or(""),
        params.port.unwrap_or(5432),
        params.database.as_deref().unwrap_or(""),
        params.username.as_deref().unwrap_or(""),
    )
}

/// Return the cached pool for this connection's identity, or build and cache
/// a new one if this is the first request for that identity.
fn get_or_create_pool(params: &ConnectionParams) -> Result<Pool, String> {
    let key = connection_key(params);

    {
        let pools = POOLS.lock().map_err(|_| "pool cache lock poisoned".to_string())?;
        if let Some(pool) = pools.get(&key) {
            return Ok(pool.clone());
        }
    }

    let pool = build_pool(params)?;
    let mut pools = POOLS.lock().map_err(|_| "pool cache lock poisoned".to_string())?;
    // Another call may have raced us to create this pool between the read
    // above and this write — keep whichever is already cached.
    Ok(pools.entry(key).or_insert(pool).clone())
}

/// Build a deadpool-postgres pool for the given connection parameters.
fn build_pool(params: &ConnectionParams) -> Result<Pool, String> {
    let mut cfg = Config::new();
    cfg.host = params.host.clone();
    cfg.port = params.port;
    cfg.dbname = params.database.clone();
    cfg.user = params.username.clone();
    cfg.password = params.password.clone();
    cfg.manager = Some(ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    });

    if needs_tls(params) {
        let tls_config = build_tls_connector()?;
        cfg.create_pool(Some(Runtime::Tokio1), MakeRustlsConnect::new(tls_config))
            .map_err(|e| format!("Pool creation failed (TLS): {e}"))
    } else {
        cfg.create_pool(Some(Runtime::Tokio1), NoTls)
            .map_err(|e| format!("Pool creation failed: {e}"))
    }
}

/// Determine whether TLS should be used based on ssl_mode.
fn needs_tls(params: &ConnectionParams) -> bool {
    matches!(
        params.ssl_mode.as_deref(),
        Some("require" | "verify-ca" | "verify-full")
    )
}

/// Build a rustls ClientConfig using the platform certificate verifier.
fn build_tls_connector() -> Result<rustls::ClientConfig, String> {
    use rustls_platform_verifier::BuilderVerifierExt;

    let config = rustls::ClientConfig::builder()
        .with_platform_verifier()
        .map_err(|e| format!("Failed to build platform TLS verifier: {e}"))?
        .with_no_client_auth();
    Ok(config)
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod client_tests;

