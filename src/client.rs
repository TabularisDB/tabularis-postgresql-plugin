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
//! connections. Caching by `host:port:database:user:startup_script` plus
//! every TLS param (matches the builtin's `build_connection_key` pattern in
//! `src-tauri/src/pool_manager.rs`, minus the per-connection_id refinement
//! that plugin doesn't need yet) closes that gap.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{LazyLock, Mutex};

use deadpool_postgres::{Config, ManagerConfig, Pool, RecyclingMethod, Runtime, SslMode};
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{NoTls, Row};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::models::ConnectionParams;

static POOLS: LazyLock<Mutex<HashMap<String, Pool>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Build a connection pool from the given params and verify connectivity
/// by acquiring one client and running `SELECT 1`.
pub async fn test_connection(params: &ConnectionParams) -> Result<(), String> {
    let pool = get_or_create_pool(params).await?;
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
    let pool = get_or_create_pool(params).await?;
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
    let pool = get_or_create_pool(params).await?;
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
    let pool = get_or_create_pool(params).await?;
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
    let pool = get_or_create_pool(params).await?;
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
pub async fn build_pool_pub(params: &ConnectionParams) -> Result<Pool, String> {
    get_or_create_pool(params).await
}

/// Identifies a connection target for pool-cache purposes.
/// Matches on host:port:database:user:startup_script, plus every TLS param
/// (ssl_mode/ssl_ca/ssl_cert/ssl_key) — otherwise two connections differing
/// only in TLS configuration would incorrectly share a pool and its
/// already-negotiated TLS setup. Matches the builtin's `build_connection_key`
/// TLS-param keying in `src-tauri/src/pool_manager.rs`, minus the
/// per-connection_id refinement that plugin doesn't need yet.
fn connection_key(params: &ConnectionParams) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}:{}:{}:{}",
        params.host.as_deref().unwrap_or(""),
        params.port.unwrap_or(5432),
        params.database.as_deref().unwrap_or(""),
        params.username.as_deref().unwrap_or(""),
        params.startup_script.as_deref().unwrap_or(""),
        params.ssl_mode.as_deref().unwrap_or(""),
        params.ssl_ca.as_deref().unwrap_or(""),
        params.ssl_cert.as_deref().unwrap_or(""),
        params.ssl_key.as_deref().unwrap_or(""),
    )
}

/// Return the cached pool for this connection's identity, or build and cache
/// a new one if this is the first request for that identity.
async fn get_or_create_pool(params: &ConnectionParams) -> Result<Pool, String> {
    let key = connection_key(params);

    {
        let pools = POOLS
            .lock()
            .map_err(|_| "pool cache lock poisoned".to_string())?;
        if let Some(pool) = pools.get(&key) {
            return Ok(pool.clone());
        }
    }

    let pool = build_pool(params).await?;
    let mut pools = POOLS
        .lock()
        .map_err(|_| "pool cache lock poisoned".to_string())?;
    // Another call may have raced us to create this pool between the read
    // above and this write — keep whichever is already cached.
    Ok(pools.entry(key).or_insert(pool).clone())
}

/// Drop pools that currently have no checked-out connections. Called
/// periodically so long-idle sessions don't linger for the plugin's
/// lifetime — matches the sqlserver/dynamodb sibling plugins' pattern.
pub fn cleanup_idle_pools() {
    if let Ok(mut pools) = POOLS.lock() {
        pools.retain(|_, pool| {
            let status = pool.status();
            status.size > status.available
        });
    }
}

/// Build a deadpool-postgres pool for the given connection parameters.
///
/// When `connection_string` is set, it takes precedence over the discrete
/// host/port/database/username/password fields — matching the README's
/// documented behavior ("as an alternative to the discrete fields above").
async fn build_pool(params: &ConnectionParams) -> Result<Pool, String> {
    let mut cfg = Config::new();

    match params
        .connection_string
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        Some(conn_str) => {
            let parsed = tokio_postgres::Config::from_str(conn_str)
                .map_err(|e| format!("Invalid connection string: {e}"))?;
            let host = parsed.get_hosts().first().and_then(|h| match h {
                tokio_postgres::config::Host::Tcp(host) => Some(host.clone()),
                #[cfg(unix)]
                tokio_postgres::config::Host::Unix(_) => None,
            });
            cfg.host = host;
            cfg.port = parsed.get_ports().first().copied();
            cfg.dbname = parsed.get_dbname().map(str::to_string);
            cfg.user = parsed.get_user().map(str::to_string);
            cfg.password = parsed
                .get_password()
                .map(|p| String::from_utf8_lossy(p).into_owned());
        }
        None => {
            cfg.host = params.host.clone();
            cfg.port = params.port;
            cfg.dbname = params.database.clone();
            cfg.user = params.username.clone();
            cfg.password = params.password.clone();
        }
    }

    cfg.manager = Some(ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    });
    cfg.ssl_mode = resolve_ssl_mode(params.ssl_mode.as_deref());

    let script = params
        .startup_script
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if needs_tls(params) {
        let tls_config = build_tls_connector(params)?;
        let tls = MakeRustlsConnect::new(tls_config);
        if let Some(script) = script {
            preflight_startup_script(&cfg, tls.clone(), script).await?;
        }
        let mut builder = cfg
            .builder(tls)
            .map_err(|e| format!("Pool creation failed (TLS): {e}"))?
            .runtime(Runtime::Tokio1);
        if let Some(script) = script {
            builder = builder.post_create(startup_script_hook(script));
        }
        builder
            .build()
            .map_err(|e| format!("Pool creation failed (TLS): {e}"))
    } else {
        if let Some(script) = script {
            preflight_startup_script(&cfg, NoTls, script).await?;
        }
        let mut builder = cfg
            .builder(NoTls)
            .map_err(|e| format!("Pool creation failed: {e}"))?
            .runtime(Runtime::Tokio1);
        if let Some(script) = script {
            builder = builder.post_create(startup_script_hook(script));
        }
        builder
            .build()
            .map_err(|e| format!("Pool creation failed: {e}"))
    }
}

/// Format a startup-script execution failure so the surfaced error clearly
/// names the startup script as the cause, instead of reading like a bad host
/// or wrong credentials.
fn startup_script_error(err: impl std::fmt::Display) -> String {
    format!("Startup script failed: {err}")
}

/// Build the `post_create` hook that runs the startup script on every new
/// pooled connection (matches the builtin driver's `post_create` hook — see
/// `src-tauri/src/pool_manager.rs`).
fn startup_script_hook(script: &str) -> deadpool_postgres::Hook {
    let script = script.to_string();
    deadpool_postgres::Hook::async_fn(move |client, _metrics| {
        let script = script.clone();
        Box::pin(async move {
            client
                .batch_execute(&script)
                .await
                .map_err(|e| deadpool_postgres::HookError::message(startup_script_error(e)))?;
            Ok(())
        })
    })
}

/// Validate the startup script on a throwaway connection so a broken script
/// fails fast with a clearly attributed error, **without** applying its side
/// effects (the script runs inside a transaction that is rolled back). This
/// preflight exists only for early, well-labelled failures — the per-pool
/// `post_create` hook is the single place the script actually takes effect.
/// Matches the builtin driver's `run_postgres_startup_script` preflight.
async fn preflight_startup_script<T>(cfg: &Config, tls: T, script: &str) -> Result<(), String>
where
    T: tokio_postgres::tls::MakeTlsConnect<tokio_postgres::Socket> + Clone + Sync + Send + 'static,
    T::Stream: Sync + Send,
    T::TlsConnect: Sync + Send,
    <T::TlsConnect as tokio_postgres::tls::TlsConnect<tokio_postgres::Socket>>::Future: Send,
{
    let pg_config = cfg
        .get_pg_config()
        .map_err(|e| format!("Pool creation failed: {e}"))?;
    let (mut client, connection) = pg_config
        .connect(tls)
        .await
        .map_err(|e| format!("Connection failed: {e}"))?;
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });
    let outcome: Result<(), tokio_postgres::Error> = async {
        let tx = client.transaction().await?;
        tx.batch_execute(script).await?;
        tx.rollback().await
    }
    .await;
    drop(client);
    driver.abort();
    outcome.map_err(startup_script_error)
}

/// Determine whether TLS should be used based on ssl_mode.
fn needs_tls(params: &ConnectionParams) -> bool {
    matches!(
        params.ssl_mode.as_deref(),
        Some("require" | "verify-ca" | "verify-full")
    )
}

/// Map this plugin's `ssl_mode` strings to `deadpool_postgres::SslMode`,
/// so `require`/`verify-ca`/`verify-full` actually force TLS at the protocol
/// level instead of leaving `tokio_postgres`'s own default (`SslMode::Prefer`)
/// in effect, which silently accepts a plaintext connection when the server
/// doesn't offer TLS. Matches the builtin driver's `ssl_mode` mapping in
/// `build_postgres_configurations` (`src-tauri/src/pool_manager.rs`) exactly.
/// Certificate/hostname verification is unaffected — that's handled
/// separately by `build_tls_connector`.
fn resolve_ssl_mode(ssl_mode: Option<&str>) -> Option<SslMode> {
    match ssl_mode {
        Some("disable") => Some(SslMode::Disable),
        Some("allow" | "prefer") => Some(SslMode::Prefer),
        Some("require" | "verify-ca" | "verify-full") => Some(SslMode::Require),
        _ => None,
    }
}

/// Build a rustls ClientConfig. `verify-ca`/`verify-full` validate the
/// server's certificate chain — against a caller-supplied CA bundle
/// (`ssl_ca`) when present, or the platform trust store otherwise.
/// `verify-ca` deliberately skips hostname verification (that's the entire
/// distinction from `verify-full` — matches libpq `sslmode=verify-ca`
/// semantics, see `VerifyCaCertVerifier` below). `require` forces TLS
/// without certificate validation (matches the builtin driver's `require`
/// behavior — see `src-tauri/src/pool_manager.rs`). When `ssl_cert`/
/// `ssl_key` are both supplied, presents them as a client certificate for
/// servers requiring mTLS (e.g. Google Cloud SQL) — matches the builtin
/// driver's `build_postgres_tls_connector` client-auth handling.
fn build_tls_connector(params: &ConnectionParams) -> Result<rustls::ClientConfig, String> {
    use rustls_platform_verifier::BuilderVerifierExt;

    let user_ca = params.ssl_ca.as_deref().filter(|s| !s.trim().is_empty());
    let user_cert = params.ssl_cert.as_deref().filter(|s| !s.trim().is_empty());
    let user_key = params.ssl_key.as_deref().filter(|s| !s.trim().is_empty());

    let client_auth = match (user_cert, user_key) {
        (Some(cert), Some(key)) => Some(load_client_cert_from_pem(cert, key)?),
        (Some(_), None) => {
            return Err(
                "Client certificate provided (ssl_cert) without a client private key (ssl_key)"
                    .to_string(),
            );
        }
        (None, Some(_)) => {
            return Err(
                "Client private key provided (ssl_key) without a client certificate (ssl_cert)"
                    .to_string(),
            );
        }
        (None, None) => None,
    };

    let needs_cert_validation = matches!(
        params.ssl_mode.as_deref(),
        Some("verify-ca" | "verify-full")
    );

    if needs_cert_validation {
        if let Some(ca_path) = user_ca {
            let roots = load_roots_from_pem(ca_path)?;
            if params.ssl_mode.as_deref() == Some("verify-ca") {
                let verifier = VerifyCaCertVerifier::new(roots)?;
                let builder = rustls::ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(std::sync::Arc::new(verifier));
                return match client_auth {
                    Some((certs, key)) => builder
                        .with_client_auth_cert(certs, key)
                        .map_err(|e| format!("Failed to configure client certificate: {e}")),
                    None => Ok(builder.with_no_client_auth()),
                };
            }
            let verifier =
                rustls::client::WebPkiServerVerifier::builder(std::sync::Arc::new(roots))
                    .build()
                    .map_err(|e| format!("Failed to build certificate verifier: {e}"))?;
            let builder = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(verifier);
            return match client_auth {
                Some((certs, key)) => builder
                    .with_client_auth_cert(certs, key)
                    .map_err(|e| format!("Failed to configure client certificate: {e}")),
                None => Ok(builder.with_no_client_auth()),
            };
        }
    }

    let builder = rustls::ClientConfig::builder()
        .with_platform_verifier()
        .map_err(|e| format!("Failed to build platform TLS verifier: {e}"))?;
    match client_auth {
        Some((certs, key)) => builder
            .with_client_auth_cert(certs, key)
            .map_err(|e| format!("Failed to configure client certificate: {e}")),
        None => Ok(builder.with_no_client_auth()),
    }
}

/// Validates the certificate chain against a custom root store but skips
/// hostname verification — matches libpq `sslmode=verify-ca` behavior (the
/// builtin driver's `src-tauri/src/pool_manager.rs::VerifyCaCertVerifier`).
///
/// Uses `verify_server_cert_signed_by_trust_anchor` directly rather than
/// wrapping `rustls::client::WebPkiServerVerifier` — the latter's
/// `verify_server_cert` unconditionally checks the hostname via
/// `verify_server_name`, with no way to opt out, which would make
/// `verify-ca` behave identically to `verify-full` (see issue #38).
#[derive(Debug)]
struct VerifyCaCertVerifier {
    roots: std::sync::Arc<rustls::RootCertStore>,
    supported: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl VerifyCaCertVerifier {
    fn new(roots: rustls::RootCertStore) -> Result<Self, String> {
        let provider = match rustls::crypto::CryptoProvider::get_default() {
            Some(provider) => provider.clone(),
            None => {
                let provider = rustls::crypto::ring::default_provider();
                let supported = provider.signature_verification_algorithms;
                // Ignore the error from losing an install race — another
                // caller's install still leaves a usable default installed.
                let _ = provider.install_default();
                return Ok(Self {
                    roots: std::sync::Arc::new(roots),
                    supported,
                });
            }
        };
        Ok(Self {
            roots: std::sync::Arc::new(roots),
            supported: provider.signature_verification_algorithms,
        })
    }
}

impl rustls::client::danger::ServerCertVerifier for VerifyCaCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let cert = rustls::server::ParsedCertificate::try_from(end_entity)?;
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.supported.all,
        )?;
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.supported)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.supported.supported_schemes()
    }
}

/// Load root certificates from a PEM file (used for `ssl_ca`-pinned
/// `verify-ca`/`verify-full` connections).
fn load_roots_from_pem(path: &str) -> Result<rustls::RootCertStore, String> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};

    let pem =
        std::fs::read(path).map_err(|e| format!("Failed to read ssl_ca file '{path}': {e}"))?;
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(&pem) {
        let cert = cert.map_err(|e| format!("Failed to parse ssl_ca '{path}': {e}"))?;
        roots
            .add(cert)
            .map_err(|e| format!("Failed to add ssl_ca cert from '{path}': {e}"))?;
    }
    if roots.is_empty() {
        return Err(format!(
            "ssl_ca '{path}' contained no PEM CERTIFICATE blocks"
        ));
    }
    Ok(roots)
}

/// Load a client certificate chain and private key from PEM files, for
/// mTLS-required servers (e.g. Google Cloud SQL). Uses the same
/// `pki_types::pem::PemObject` machinery as `load_roots_from_pem` rather than
/// `rustls_pemfile` — deliberately avoided as a dependency here after it was
/// removed for being unmaintained (RUSTSEC-2025-0134); `PrivateKeyDer`
/// supports PKCS1/SEC1/PKCS8 via the same trait.
fn load_client_cert_from_pem(
    cert_path: &str,
    key_path: &str,
) -> Result<
    (
        Vec<rustls::pki_types::CertificateDer<'static>>,
        rustls::pki_types::PrivateKeyDer<'static>,
    ),
    String,
> {
    use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};

    let cert_pem = std::fs::read(cert_path)
        .map_err(|e| format!("Failed to read ssl_cert file '{cert_path}': {e}"))?;
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to parse ssl_cert '{cert_path}': {e}"))?;
    if certs.is_empty() {
        return Err(format!(
            "ssl_cert '{cert_path}' contained no PEM CERTIFICATE blocks"
        ));
    }

    let key_pem = std::fs::read(key_path)
        .map_err(|e| format!("Failed to read ssl_key file '{key_path}': {e}"))?;
    let key = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|e| format!("Failed to parse ssl_key '{key_path}': {e}"))?;

    Ok((certs, key))
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod client_tests;
