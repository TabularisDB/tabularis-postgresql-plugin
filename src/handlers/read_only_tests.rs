//! Unit tests for the pure parts of `execute_read_only_query`. The protection
//! itself (a write failing in the database, nothing leaking to a writable
//! session) is covered against a live database in `client_live_tests.rs`.

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
        "-- x\rCOMMIT\nWORK",
        "-- x\rPREPARE\nTRANSACTION 'x'",
        "--\rEND",
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
        "SELECT 'PREPARE x'",
        "SELECT * FROM copy_log",
        "-- PREPARE x\nSELECT 1",
        "EXPLAIN ANALYZE SELECT 1",
        "SELECT analyze_me()",
    ] {
        assert!(refuse_transaction_control(sql).is_ok(), "{sql}");
    }
}

#[test]
fn commands_that_act_outside_the_transaction_are_refused_by_name() {
    for sql in [
        "PREPARE p AS SELECT 1",
        "prepare p(int) as select $1",
        "DEALLOCATE ALL",
        "DEALLOCATE p",
        "COPY (SELECT 1) TO '/tmp/x'",
        "COPY (SELECT 1) TO PROGRAM 'id'",
        "COPY t FROM STDIN",
        "ANALYZE t",
        "ANALYSE",
        "CLUSTER t",
        "REINDEX TABLE t",
        "CHECKPOINT",
        "LOAD 'auto_explain'",
        "/* c */ PREPARE p AS SELECT 1",
        "-- c\rPREPARE p AS SELECT 1",
    ] {
        let err = refuse_transaction_control(sql).unwrap_err();
        assert!(err.contains("not allowed"), "{sql}: {err}");
        assert!(err.contains("read-only"), "{sql}: {err}");
    }
}

fn baseline(sql: i64, protocol: i64, locks: i64) -> SessionBaseline {
    SessionBaseline {
        sql_prepared: sql,
        protocol_prepared: protocol,
        advisory_locks: locks,
    }
}

#[test]
fn a_session_that_is_as_clean_as_before_may_return_to_the_pool() {
    assert!(baseline(0, 2, 0).is_unchanged_by(&baseline(0, 2, 0)));
    // The client library may have cached one more statement meanwhile.
    assert!(baseline(0, 2, 0).is_unchanged_by(&baseline(0, 3, 0)));
    // A statement or lock the connection already held is not this call's doing.
    assert!(baseline(1, 2, 1).is_unchanged_by(&baseline(1, 2, 1)));
}

#[test]
fn a_session_that_kept_state_past_rollback_is_not_returned_to_the_pool() {
    // A `PREPARE` survived the ROLLBACK.
    assert!(!baseline(0, 2, 0).is_unchanged_by(&baseline(1, 2, 0)));
    // A statement the client library cached was deallocated under it.
    assert!(!baseline(0, 2, 0).is_unchanged_by(&baseline(0, 0, 0)));
    // A session-level advisory lock is still held.
    assert!(!baseline(0, 2, 0).is_unchanged_by(&baseline(0, 2, 1)));
}

#[test]
fn the_session_counts_are_read_in_one_statement_with_fixed_columns() {
    assert_eq!(SESSION_COUNTS_SQL.matches("count(*)").count(), 3);
    assert!(SESSION_COUNTS_SQL.contains("from_sql"));
    assert!(SESSION_COUNTS_SQL.contains("pg_backend_pid()"));
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

#[test]
fn page_zero_is_clamped_to_the_first_page() {
    // Page 0 would underflow the offset arithmetic.
    assert_eq!(parse_request(&req(json!({"page": 0}))).unwrap().page, 1);
}

#[test]
fn the_largest_limit_and_page_are_clamped_not_truncated() {
    // `as u32` wraps 1e10 into 1_410_065_408; a clamp keeps it the largest value.
    let r = parse_request(&req(
        json!({"limit": 10_000_000_000_u64, "page": 10_000_000_000_u64}),
    ))
    .unwrap();
    assert_eq!(r.limit, Some(u32::MAX));
    assert_eq!(r.page, u32::MAX);
}

#[test]
fn a_limit_or_page_of_the_wrong_type_is_treated_as_absent() {
    let r = parse_request(&req(json!({"limit": "100", "page": -3}))).unwrap();
    assert_eq!((r.limit, r.page), (None, 1));
}

#[test]
fn behind_a_pooler_the_call_undoes_only_the_session_state_it_created() {
    let clean = rollback_and_clean_sql("ROLLBACK", &baseline(0, 2, 0));
    assert!(clean.starts_with("ROLLBACK;"), "{clean}");
    assert!(clean.contains("pg_advisory_unlock_all()"), "{clean}");
    assert!(clean.contains("DEALLOCATE"), "{clean}");
    assert!(clean.contains("from_sql"), "{clean}");
    // Only SQL-level statements: the client library's own are not `from_sql`.
    assert!(!clean.contains("DEALLOCATE ALL"), "{clean}");
}

#[test]
fn state_the_session_already_held_is_not_released_by_the_call() {
    let held_lock = rollback_and_clean_sql("ROLLBACK", &baseline(0, 2, 1));
    assert!(!held_lock.contains("pg_advisory_unlock_all"), "{held_lock}");
    assert!(held_lock.contains("DEALLOCATE"), "{held_lock}");
    let held_statement = rollback_and_clean_sql("ROLLBACK", &baseline(1, 2, 0));
    assert!(!held_statement.contains("DEALLOCATE"), "{held_statement}");
    assert!(
        held_statement.contains("pg_advisory_unlock_all"),
        "{held_statement}"
    );
    let both = rollback_and_clean_sql("ROLLBACK", &baseline(3, 2, 1));
    assert_eq!(both, "ROLLBACK");
}

#[tokio::test]
async fn a_cancel_request_that_never_completes_is_given_up_on() {
    // A server that is down makes the cancel's own connect wait on the OS.
    // The outer timeout makes an unbounded wait fail here instead of hanging.
    let finished = tokio::time::timeout(
        Duration::from_secs(2),
        bounded(Duration::from_millis(50), std::future::pending::<()>()),
    )
    .await
    .expect("bounded() must give up on its own");
    assert!(!finished);
}

#[tokio::test]
async fn a_cancel_request_that_finishes_in_time_is_not_cut_short() {
    let quick = tokio::time::sleep(Duration::from_millis(10));
    assert!(bounded(Duration::from_secs(5), quick).await);
}

#[test]
fn the_cancel_bound_is_short_enough_not_to_pile_up_tasks() {
    assert!(CANCEL_TIMEOUT <= Duration::from_secs(10));
    assert!(CANCEL_TIMEOUT >= Duration::from_secs(1));
}
