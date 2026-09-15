//! Unit tests for `query.rs`'s pure statement-classification functions.
//! Sibling test file per repo convention (`.rules/rust.md` #4/#5) — loaded
//! via `#[cfg(test)] #[path = "query_tests.rs"] mod query_tests;`.
//!
//! `exec_query_on_client` itself takes a live `deadpool_postgres::Object`,
//! which can only be constructed against a real connection — that
//! end-to-end path (including the actual #70 repro, `SHOW search_path`
//! with a `limit` param) is covered by
//! `tests/live_db.rs::show_command_with_a_limit_param_does_not_error`.
//! These tests exercise the pure classification logic that decides whether
//! pagination is applied to a statement.

use super::{is_select_query, returns_result_set, strip_leading_sql_comments};

#[test]
fn strip_leading_sql_comments_skips_line_comments() {
    assert_eq!(
        strip_leading_sql_comments("-- a comment\nSELECT 1"),
        "SELECT 1"
    );
}

#[test]
fn strip_leading_sql_comments_skips_block_comments() {
    assert_eq!(
        strip_leading_sql_comments("/* a comment */ SELECT 1"),
        "SELECT 1"
    );
}

#[test]
fn strip_leading_sql_comments_skips_multiple_mixed_comments() {
    assert_eq!(
        strip_leading_sql_comments("-- one\n/* two */\n-- three\nSELECT 1"),
        "SELECT 1"
    );
}

#[test]
fn strip_leading_sql_comments_leaves_uncommented_query_untouched() {
    assert_eq!(strip_leading_sql_comments("SELECT 1"), "SELECT 1");
}

#[test]
fn strip_leading_sql_comments_returns_empty_for_an_unterminated_comment() {
    assert_eq!(strip_leading_sql_comments("-- no newline"), "");
    assert_eq!(strip_leading_sql_comments("/* no close"), "");
}

#[test]
fn returns_result_set_recognizes_every_row_producing_statement_type() {
    for stmt in [
        "SELECT 1",
        "WITH t AS (SELECT 1) SELECT * FROM t",
        "SHOW search_path",
        "EXPLAIN SELECT 1",
        "DESCRIBE foo",
        "VALUES (1)",
        "TABLE foo",
        "PRAGMA foo",
        "CALL foo()",
    ] {
        assert!(
            returns_result_set(stmt),
            "expected {stmt:?} to return a result set"
        );
    }
}

#[test]
fn returns_result_set_rejects_mutation_statements() {
    for stmt in [
        "INSERT INTO t VALUES (1)",
        "UPDATE t SET x = 1",
        "DELETE FROM t",
    ] {
        assert!(
            !returns_result_set(stmt),
            "expected {stmt:?} to not return a result set"
        );
    }
}

#[test]
fn returns_result_set_sees_through_leading_comments() {
    // #70's underlying gap: before this fix, a comment-headed row-producing
    // statement was misclassified as non-result-set-bearing (since the raw
    // trim_start() left "-- ..." at the front), routing it through
    // `execute()` and silently discarding the actual row data.
    assert!(returns_result_set("-- note\nSELECT 1"));
    assert!(returns_result_set("-- note\nSHOW search_path"));
}

#[test]
fn is_select_query_accepts_only_the_select_keyword() {
    assert!(is_select_query("SELECT 1"));
    assert!(is_select_query("select 1"));
    assert!(is_select_query("-- note\nSELECT 1"));
}

#[test]
fn is_select_query_rejects_other_row_producing_statements() {
    // The core of #70: SHOW/EXPLAIN/WITH/VALUES/TABLE/PRAGMA/CALL all
    // return a result set (returns_result_set == true) but don't support a
    // trailing SQL LIMIT/OFFSET clause the way a SELECT does — only
    // is_select_query should gate pagination.
    for stmt in [
        "WITH t AS (SELECT 1) SELECT * FROM t",
        "SHOW search_path",
        "EXPLAIN SELECT 1",
        "DESCRIBE foo",
        "VALUES (1)",
        "TABLE foo",
        "PRAGMA foo",
        "CALL foo()",
    ] {
        assert!(
            !is_select_query(stmt),
            "expected {stmt:?} to not be a SELECT"
        );
    }
}
