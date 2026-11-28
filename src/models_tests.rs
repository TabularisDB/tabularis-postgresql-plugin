//! Unit tests for `ConnectionParams::is_transaction_pooler` and the
//! `pooler_mode` field parsing (#157).

use crate::models::ConnectionParams;
use serde_json::json;

fn params_with_pooler_mode(mode: Option<&str>) -> ConnectionParams {
    ConnectionParams::from_value(&json!({
        "driver": "postgres",
        "host": "localhost",
        "port": 5432,
        "database": "db",
        "username": "user",
        "pooler_mode": mode,
    }))
}

#[test]
fn is_transaction_pooler_true_for_transaction() {
    assert!(params_with_pooler_mode(Some("transaction")).is_transaction_pooler());
}

#[test]
fn is_transaction_pooler_false_for_none() {
    let params = ConnectionParams::from_value(&json!({
        "driver": "postgres",
        "host": "localhost",
        "port": 5432,
        "database": "db",
        "username": "user",
    }));
    assert!(!params.is_transaction_pooler());
}

#[test]
fn is_transaction_pooler_false_for_session() {
    assert!(!params_with_pooler_mode(Some("session")).is_transaction_pooler());
}

#[test]
fn is_transaction_pooler_false_for_empty_string() {
    assert!(!params_with_pooler_mode(Some("")).is_transaction_pooler());
}

#[test]
fn is_transaction_pooler_false_for_typo() {
    assert!(!params_with_pooler_mode(Some("transation")).is_transaction_pooler());
}

/// Case-insensitive: the host might send "Transaction" or "TRANSACTION"
/// from a differently-cased dropdown label or config file. The comparison
/// must not silently disable pooler-safe behavior on a valid-but-cased
/// value (boundary-and-edge-cases finding from the #158 ensemble review).
#[test]
fn is_transaction_pooler_case_insensitive() {
    assert!(params_with_pooler_mode(Some("Transaction")).is_transaction_pooler());
    assert!(params_with_pooler_mode(Some("TRANSACTION")).is_transaction_pooler());
    assert!(params_with_pooler_mode(Some("Transaction")).is_transaction_pooler());
}

/// Trimmed: a `pooler_mode` with surrounding whitespace (e.g. a hand-edited
/// `connections.json` with a trailing space) still routes to the pinned path
/// — matching how `startup_script` is trimmed in `build_pool`. Without the
/// trim, "transaction " would silently bypass the fix and the user would hit
/// the #160/#163 hang on the shared cached client (the very thing this PR
/// fixes), with no diagnostic pointing at the whitespace.
#[test]
fn is_transaction_pooler_trims_surrounding_whitespace() {
    assert!(params_with_pooler_mode(Some("transaction ")).is_transaction_pooler());
    assert!(params_with_pooler_mode(Some(" transaction")).is_transaction_pooler());
    assert!(params_with_pooler_mode(Some(" transaction ")).is_transaction_pooler());
}

/// Non-string JSON for `pooler_mode` (number, bool, null) is silently
/// treated as None — not a transaction pooler.
#[test]
fn is_transaction_pooler_false_for_non_string_json() {
    let params = ConnectionParams::from_value(&json!({
        "driver": "postgres",
        "host": "localhost",
        "port": 5432,
        "database": "db",
        "username": "user",
        "pooler_mode": 42,
    }));
    assert!(!params.is_transaction_pooler());
}

// Coverage for #197: `unix_socket_path` must be parsed from the JSON-RPC
// params (snake_case key — the host's ConnectionParams has no rename_all, so
// fields serialize as snake_case) and `local_socket_path` must trim/filter
// it. The host strips `unix_socket_path` to `None` at every tunnel
// activation site, so a present non-empty value means "dial this socket
// locally."

#[test]
fn from_value_parses_unix_socket_path() {
    let params = ConnectionParams::from_value(&json!({
        "driver": "postgres",
        "host": "localhost",
        "port": 5432,
        "database": "db",
        "username": "user",
        "unix_socket_path": "/var/run/postgresql",
    }));
    assert_eq!(
        params.unix_socket_path.as_deref(),
        Some("/var/run/postgresql"),
        "unix_socket_path must be parsed from the snake_case JSON key"
    );
}

#[test]
fn from_value_leaves_unix_socket_path_none_when_absent() {
    let params = ConnectionParams::from_value(&json!({
        "driver": "postgres",
        "host": "localhost",
        "port": 5432,
        "database": "db",
        "username": "user",
    }));
    assert!(
        params.unix_socket_path.is_none(),
        "unix_socket_path must be None when the key is absent"
    );
}

#[test]
fn local_socket_path_trims_and_filters_empty() {
    let mut params = ConnectionParams::from_value(&json!({
        "driver": "postgres",
        "host": "localhost",
        "database": "db",
        "username": "user",
        "unix_socket_path": "  /var/run/postgresql  ",
    }));
    assert_eq!(params.local_socket_path(), Some("/var/run/postgresql"));

    params.unix_socket_path = Some("   ".to_string());
    assert!(
        params.local_socket_path().is_none(),
        "a whitespace-only socket path must be treated as unset"
    );

    params.unix_socket_path = Some("".to_string());
    assert!(
        params.local_socket_path().is_none(),
        "an empty socket path must be treated as unset"
    );
}

fn params_from(extra: serde_json::Value) -> ConnectionParams {
    let mut base = json!({"host": "h", "database": "d", "username": "u"});
    for (k, v) in extra.as_object().unwrap() {
        base[k] = v.clone();
    }
    ConnectionParams::from_value(&base)
}

#[test]
fn read_only_is_true_only_for_a_json_true() {
    assert!(params_from(json!({"read_only": true})).read_only);
    assert!(!params_from(json!({"read_only": false})).read_only);
    assert!(!params_from(json!({})).read_only);
    assert!(!params_from(json!({"read_only": null})).read_only);
}

#[test]
fn a_non_boolean_read_only_value_is_not_treated_as_true() {
    // The host sends a bool. Anything else is malformed, and the protected RPC
    // enforces read-only on every call regardless of this flag, so a malformed
    // value cannot weaken that path.
    for bad in [json!("true"), json!(1), json!([true]), json!({"x": true})] {
        assert!(!params_from(json!({ "read_only": bad })).read_only);
    }
}

#[test]
fn the_startup_script_is_trimmed_and_blank_means_none() {
    assert_eq!(
        params_from(json!({"startup_script": "  SET x = 1  "})).startup_script_to_run(),
        Some("SET x = 1")
    );
    assert_eq!(
        params_from(json!({"startup_script": "   \n"})).startup_script_to_run(),
        None
    );
    assert_eq!(params_from(json!({})).startup_script_to_run(), None);
}

#[test]
fn a_read_only_connection_never_runs_its_startup_script() {
    // Startup SQL is user code and would run before protection is installed.
    let params = params_from(json!({"read_only": true, "startup_script": "SET x = 1"}));
    assert_eq!(params.startup_script_to_run(), None);
}
