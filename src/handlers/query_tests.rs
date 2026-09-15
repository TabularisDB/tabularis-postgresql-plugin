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

use super::{returns_result_set, strip_leading_sql_comments, supports_trailing_limit_clause};

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
fn supports_trailing_limit_clause_accepts_statements_verified_against_live_postgres() {
    // Verified directly against a live PostgreSQL instance: these statement
    // types all accept a trailing `LIMIT`/`OFFSET` clause.
    for stmt in [
        "SELECT 1",
        "select 1",
        "-- note\nSELECT 1",
        "WITH t AS (SELECT 1) SELECT * FROM t",
        "VALUES (1)",
        "TABLE foo",
        "EXPLAIN SELECT 1",
    ] {
        assert!(
            supports_trailing_limit_clause(stmt),
            "expected {stmt:?} to accept a trailing LIMIT clause"
        );
    }
}

#[test]
fn supports_trailing_limit_clause_rejects_statements_verified_against_live_postgres() {
    // The core of #70: SHOW and CALL return a result set
    // (returns_result_set == true) but PostgreSQL rejects a trailing LIMIT
    // after either with "syntax error at or near LIMIT" — verified directly
    // against a live instance. Only these two should skip SQL pagination
    // and fall back to capping rows client-side instead.
    for stmt in ["SHOW search_path", "CALL foo()"] {
        assert!(
            !supports_trailing_limit_clause(stmt),
            "expected {stmt:?} to reject a trailing LIMIT clause"
        );
    }
}

#[test]
fn supports_trailing_limit_clause_does_not_silently_disable_cte_pagination() {
    // Regression guard: an earlier version of this fix matched the builtin
    // driver's narrower `is_select_query` (literal SELECT prefix only),
    // which routed WITH/VALUES/TABLE/EXPLAIN through client-side capping
    // instead of real SQL pagination — silently breaking page 2+ for a
    // paginated CTE even though `WITH ... SELECT ... LIMIT n OFFSET m` is
    // valid PostgreSQL syntax. This must stay true.
    assert!(supports_trailing_limit_clause(
        "WITH t AS (SELECT 1) SELECT * FROM t"
    ));
}
