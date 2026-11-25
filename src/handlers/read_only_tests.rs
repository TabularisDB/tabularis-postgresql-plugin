//! Unit tests for the pure parts of `execute_read_only_query`. The protection
//! itself (a write failing in the database, nothing leaking to a writable
//! session) is covered against a live database in `client_live_tests.rs` and
//! `tests/live_db.rs`.

use super::*;
use serde_json::json;

fn req(extra: Value) -> Value {
    let mut base = json!({
        "params": {"host": "h", "database": "d", "username": "u", "read_only": true},
        "query": "SELECT 1",
        "limit": 100,
        "page": 2,
        "schema": "analytics"
    });
    for (k, v) in extra.as_object().unwrap() {
        base[k] = v.clone();
    }
    base
}

#[test]
fn parses_the_same_shape_as_execute_query() {
    let r = parse_request(&req(json!({}))).unwrap();
    assert_eq!(r.query, "SELECT 1");
    assert_eq!(r.limit, Some(100));
    assert_eq!(r.page, 2);
    assert_eq!(r.schema.as_deref(), Some("analytics"));
    assert_eq!(r.params.database.as_deref(), Some("d"));
    assert!(r.params.read_only);
}

#[test]
fn page_defaults_to_one_and_limit_and_schema_are_optional() {
    let mut raw = req(json!({}));
    for k in ["limit", "page", "schema"] {
        raw.as_object_mut().unwrap().remove(k);
    }
    let r = parse_request(&raw).unwrap();
    assert_eq!((r.limit, r.page, r.schema), (None, 1, None));
}

#[test]
fn a_null_schema_means_no_schema() {
    assert_eq!(
        parse_request(&req(json!({"schema": null}))).unwrap().schema,
        None
    );
}

#[test]
fn a_blank_schema_means_no_schema() {
    for blank in ["", "   "] {
        assert_eq!(
            parse_request(&req(json!({"schema": blank})))
                .unwrap()
                .schema,
            None
        );
    }
}

#[test]
fn the_query_is_required_and_must_be_text() {
    let mut missing = req(json!({}));
    missing.as_object_mut().unwrap().remove("query");
    assert!(parse_request(&missing).unwrap_err().contains("query"));
    assert!(parse_request(&req(json!({"query": 5})))
        .unwrap_err()
        .contains("query"));
}

#[test]
fn an_empty_query_is_an_error_not_a_silent_success() {
    for q in ["", "   \n", "-- only a comment", "/* x */ ;"] {
        assert!(parse_request(&req(json!({ "query": q }))).is_err(), "{q:?}");
    }
}

#[test]
fn the_read_only_flag_is_not_required_for_a_protected_call() {
    // The host also sends protected calls for writable connections (MCP reads).
    let mut raw = req(json!({}));
    raw["params"].as_object_mut().unwrap().remove("read_only");
    let r = parse_request(&raw).unwrap();
    assert!(!r.params.read_only);
}

#[test]
fn transaction_control_is_refused() {
    for sql in [
        "BEGIN",
        "begin read write",
        "START TRANSACTION",
        "COMMIT",
        "commit and chain",
        "END",
        "ROLLBACK",
        "ABORT",
        "/* c */ COMMIT",
        "PREPARE TRANSACTION 'x'",
    ] {
        let err = refuse_transaction_control(sql).unwrap_err();
        assert!(err.contains("read-only"), "{sql}: {err}");
    }
}

#[test]
fn ordinary_reads_and_savepoint_free_statements_pass_the_transaction_check() {
    for sql in [
        "SELECT 1",
        "WITH x AS (SELECT 1) SELECT * FROM x",
        "SHOW search_path",
        "EXPLAIN SELECT 1",
        "SELECT 'COMMIT'",
        "TABLE t",
        "VALUES (1)",
        "SELECT pg_advisory_lock(1)",
    ] {
        assert!(refuse_transaction_control(sql).is_ok(), "{sql}");
    }
}

#[test]
fn the_notice_about_a_rolled_back_user_transaction_is_removed() {
    let with = format!(
        "ERROR: relation \"t\" does not exist{}",
        crate::handlers::query::TXN_ROLLED_BACK_NOTICE
    );
    assert_eq!(
        strip_rollback_notice(with),
        "ERROR: relation \"t\" does not exist"
    );
}

#[test]
fn an_error_without_the_notice_is_left_alone() {
    let plain = "ERROR: cannot execute INSERT in a read-only transaction".to_string();
    assert_eq!(strip_rollback_notice(plain.clone()), plain);
}

#[test]
fn protection_is_confirmed_only_by_an_exact_on() {
    assert!(require_read_only_transaction("on").is_ok());
    for bad in ["off", "", "ON ", "true", "1"] {
        let err = require_read_only_transaction(bad).unwrap_err();
        assert!(err.contains("could not be enabled"), "{bad:?}: {err}");
    }
}
