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
