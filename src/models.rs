//! Shared request/response shapes.
//!
//! Mirrors the `ConnectionParams` struct the host sends. Fields are optional
//! since different database types leave different fields blank.

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ConnectionParams {
    pub driver: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub ssl_mode: Option<String>,
    pub ssl_ca: Option<String>,
    pub ssl_cert: Option<String>,
    pub ssl_key: Option<String>,
    pub connection_string: Option<String>,
    pub startup_script: Option<String>,
    /// Absolute path of the Unix socket the database listens on. When set
    /// (and no SSH/K8s/SSM tunnel is active — the host strips this to `None`
    /// at every tunnel activation site, see `tabularis`'s
    /// `application/tunnels.rs:847/852/904/941/1007/1026`), the plugin dials
    /// the socket instead of `host`/`port`. TLS is forced off for socket
    /// connections (a socket peer cannot negotiate it, and the path is
    /// local). Mirrors the builtin driver's `unix_socket_path` field and
    /// `local_unix_socket_path()` behavior, minus the tunnel gate (the host
    /// applies that before the params reach the plugin — see #197).
    pub unix_socket_path: Option<String>,
    /// Connection pooler mode, set by the host when the user declares the
    /// connection goes through a transaction-mode pooler (e.g. PgBouncer
    /// `pool_mode = transaction`). When `"transaction"`, the plugin avoids
    /// its own named-statement `prepare()` calls (which split across two
    /// Syncs and can be orphaned by pooler reassignment) and surfaces
    /// actionable error messages for tokio-postgres's internal
    /// `typeinfo_statement` (which requires `max_prepared_statements > 0`
    /// on the pooler side for non-builtin column types — see #157,
    /// tabularis#1097). `None` for direct connections — no behavior change.
    pub pooler_mode: Option<String>,
    /// The connection's **Read-only mode** (a saved per-connection policy).
    /// The host forwards it to every call. While it is on, the plugin makes
    /// each new session read-only at the database level (except behind a
    /// transaction pooler, where only `execute_read_only_query` calls are
    /// protected) and never runs the saved `startup_script` (see
    /// [`Self::startup_script_to_run`]). `execute_read_only_query` protects
    /// each call whatever this flag says.
    pub read_only: bool,
}

impl ConnectionParams {
    pub fn from_value(value: &Value) -> Self {
        let obj = value.as_object();
        let get_str = |k: &str| {
            obj.and_then(|o| o.get(k))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let port = obj
            .and_then(|o| o.get("port"))
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok());

        Self {
            driver: get_str("driver"),
            host: get_str("host"),
            port,
            database: get_str("database"),
            username: get_str("username"),
            password: get_str("password"),
            ssl_mode: get_str("ssl_mode"),
            ssl_ca: get_str("ssl_ca"),
            ssl_cert: get_str("ssl_cert"),
            ssl_key: get_str("ssl_key"),
            connection_string: get_str("connection_string"),
            startup_script: get_str("startup_script"),
            unix_socket_path: get_str("unix_socket_path"),
            pooler_mode: get_str("pooler_mode"),
            read_only: obj
                .and_then(|o| o.get("read_only"))
                .and_then(Value::as_bool)
                == Some(true),
        }
    }

    /// The Unix socket path the plugin should dial locally: trimmed and
    /// non-empty, or `None` when unset/blank. The host strips
    /// `unix_socket_path` to `None` at every tunnel activation site (SSH/K8s/
    /// SSM — see `tabularis`'s `application/tunnels.rs`), so by the time
    /// params reach the plugin, a present non-empty value means "no tunnel is
    /// active, dial this socket." Mirrors the builtin's
    /// `local_unix_socket_path()` (`tabularis`'s `models.rs:373-378`) minus
    /// the tunnel gate (the host applies that upstream — see #197).
    pub fn local_socket_path(&self) -> Option<&str> {
        self.unix_socket_path
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
    }

    /// The startup script to run on each new connection: trimmed, `None` when
    /// blank, and always `None` in read-only mode. Startup SQL is user code
    /// that runs before write protection is installed, so a read-only
    /// connection must not run it (the host omits it too; this keeps the
    /// plugin safe on its own).
    pub fn startup_script_to_run(&self) -> Option<&str> {
        if self.read_only {
            return None;
        }
        self.startup_script
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// True when the connection goes through a transaction-mode pooler
    /// (e.g. PgBouncer `pool_mode = transaction`). Drives pooler-safe
    /// behavior: the plugin avoids its own named-statement `prepare()` calls
    /// and surfaces actionable error messages for tokio-postgres's internal
    /// `typeinfo_statement` (#157, tabularis#1097).
    pub fn is_transaction_pooler(&self) -> bool {
        self.pooler_mode
            .as_deref()
            .is_some_and(|m| m.trim().eq_ignore_ascii_case("transaction"))
    }
}

/// Extract the nested `params` object every RPC method receives.
/// Tabularis wraps connection params in `params.params`.
pub fn inner_params(value: &Value) -> &Value {
    value.get("params").unwrap_or(value)
}

/// Mirrors `crate::models::ColumnDefinition` on the host — a single column's
/// shape for DDL generation (CREATE TABLE, ADD COLUMN, ALTER COLUMN).
#[derive(Debug, Clone, Deserialize)]
pub struct ColumnDefinition {
    pub name: String,
    pub data_type: String,
    pub is_nullable: bool,
    pub is_pk: bool,
    pub is_auto_increment: bool,
    pub default_value: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
}

/// Mirrors `crate::models::RoutineCallArg` on the host — one argument of a
/// stored-routine invocation built by `build_routine_call_sql`.
#[derive(Debug, Clone, Deserialize)]
pub struct RoutineCallArg {
    pub name: String,
    /// "IN", "OUT", or "INOUT".
    pub mode: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub is_raw: bool,
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;
