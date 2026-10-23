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

use super::{
    build_search_path_sql, raw_explain_output, returns_result_set, sets_session_local_state,
    strip_leading_sql_comments, supports_trailing_limit_clause, transaction_effect,
    TransactionEffect,
};

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

#[test]
fn raw_explain_output_matches_the_host_adapters_raw_shape() {
    // #89: the host's plugin adapter (tabularis plugins/driver.rs) only
    // classifies a response as ExplainQueryOutput::Raw when it finds
    // engine/format/payload as strings via .as_str() — anything else
    // (including the bare EXPLAIN JSON this plugin used to return) falls
    // through to the parsed-plan path instead.
    let plan = serde_json::json!([{"Plan": {"Node Type": "Seq Scan"}}]);
    let wire = raw_explain_output(&plan, "SELECT 1");

    let obj = wire.as_object().expect("must be a JSON object");
    assert_eq!(
        obj.get("engine").and_then(serde_json::Value::as_str),
        Some("postgres")
    );
    assert_eq!(
        obj.get("format").and_then(serde_json::Value::as_str),
        Some("postgres-json")
    );
    assert_eq!(
        obj.get("original_query")
            .and_then(serde_json::Value::as_str),
        Some("SELECT 1")
    );

    // payload must be the JSON *stringified*, not the live JSON value — the
    // host adapter reads it with object.get("payload")?.as_str(), which
    // returns None (not an error) for a JSON object/array, silently
    // dropping this plugin's output into the Plan fallback path instead.
    let payload = obj
        .get("payload")
        .and_then(serde_json::Value::as_str)
        .expect("payload must be a JSON string, not a nested object/array");
    let reparsed: serde_json::Value =
        serde_json::from_str(payload).expect("payload must be valid JSON once parsed");
    assert_eq!(reparsed, plan);
}

#[test]
fn raw_explain_output_payload_is_not_the_live_json_value() {
    let plan = serde_json::json!({"Node Type": "Index Scan"});
    let wire = raw_explain_output(&plan, "SELECT * FROM t WHERE id = 1");
    let payload_value = wire.get("payload").unwrap();
    assert!(
        payload_value.is_string(),
        "payload must be Value::String, got {payload_value:?}"
    );
}

#[test]
fn transaction_effect_detects_opening_statements() {
    for query in [
        "BEGIN",
        "begin;",
        "BEGIN TRANSACTION",
        "BEGIN ISOLATION LEVEL SERIALIZABLE",
        "START TRANSACTION",
        "start transaction read write",
    ] {
        assert_eq!(
            transaction_effect(query),
            TransactionEffect::Opens,
            "{query} should open a transaction"
        );
    }
}

#[test]
fn transaction_effect_detects_closing_statements() {
    for query in ["COMMIT", "commit;", "ROLLBACK", "END", "END TRANSACTION"] {
        assert_eq!(
            transaction_effect(query),
            TransactionEffect::Closes,
            "{query} should close the transaction"
        );
    }
}

#[test]
fn transaction_effect_ignores_savepoint_rollback() {
    // Unwinding to a savepoint leaves the transaction open, so the
    // connection must stay pinned to the session.
    assert_eq!(
        transaction_effect("ROLLBACK TO SAVEPOINT before_update"),
        TransactionEffect::None
    );
    assert_eq!(
        transaction_effect("ROLLBACK TO before_update"),
        TransactionEffect::None
    );
}

#[test]
fn transaction_effect_detects_chained_and_two_phase_statements() {
    for query in [
        "COMMIT AND CHAIN",
        "commit work and chain",
        "ROLLBACK AND CHAIN",
        "ABORT AND CHAIN",
    ] {
        assert_eq!(
            transaction_effect(query),
            TransactionEffect::Chains,
            "{query}"
        );
    }
    for query in ["COMMIT AND NO CHAIN", "ABORT", "PREPARE TRANSACTION 'tx1'"] {
        assert_eq!(
            transaction_effect(query),
            TransactionEffect::Closes,
            "{query}"
        );
    }
    // Two-phase commit runs outside a transaction block and never touches the session's.
    for query in ["COMMIT PREPARED 'tx1'", "ROLLBACK PREPARED 'tx1'"] {
        assert_eq!(
            transaction_effect(query),
            TransactionEffect::None,
            "{query}"
        );
    }
}

#[test]
fn in_transaction_after_treats_a_failed_commit_as_closing() {
    use TransactionEffect::*;
    // A COMMIT failing on a deferred constraint has already ended the transaction server-side.
    assert!(!Closes.in_transaction_after(false, true));
    assert!(!Chains.in_transaction_after(false, true));
    assert!(Chains.in_transaction_after(true, true));
    assert!(Chains.in_transaction_after(true, false));
    assert!(Opens.in_transaction_after(true, false));
    // A failed BEGIN or ordinary statement leaves the state as it was.
    assert!(!Opens.in_transaction_after(false, false));
    assert!(None.in_transaction_after(false, true));
    assert!(!None.in_transaction_after(true, false));
}

#[test]
fn transaction_effect_reads_optional_noise_words_and_inner_comments() {
    for (query, effect) in [
        ("ROLLBACK WORK TO SAVEPOINT sp1", TransactionEffect::None),
        (
            "ROLLBACK TRANSACTION TO SAVEPOINT sp1",
            TransactionEffect::None,
        ),
        ("COMMIT WORK", TransactionEffect::Closes),
        (
            "COMMIT -- and chain later if needed",
            TransactionEffect::Closes,
        ),
        ("COMMIT /* and chain */", TransactionEffect::Closes),
        ("START /* explicit */ TRANSACTION", TransactionEffect::Opens),
        (
            "COMMIT /* outer /* nested */ still comment */ AND CHAIN",
            TransactionEffect::Chains,
        ),
        (
            "ROLLBACK TRANSACTION AND NO CHAIN",
            TransactionEffect::Closes,
        ),
        ("PREPARE TRANSACTION 'and chain'", TransactionEffect::Closes),
    ] {
        assert_eq!(transaction_effect(query), effect, "{query}");
    }
}

#[test]
fn transaction_effect_ignores_ordinary_statements() {
    for query in [
        "SELECT 1",
        "UPDATE t SET a = 1",
        "SAVEPOINT before_update",
        // `BEGIN` appearing as data, not as the leading keyword.
        "SELECT 'BEGIN' AS word",
        "INSERT INTO log (msg) VALUES ('COMMIT')",
    ] {
        assert_eq!(
            transaction_effect(query),
            TransactionEffect::None,
            "{query} should not change the transaction state"
        );
    }
}

#[test]
fn transaction_effect_sees_through_leading_comments() {
    assert_eq!(
        transaction_effect("-- start the transaction\nBEGIN"),
        TransactionEffect::Opens
    );
    assert_eq!(
        transaction_effect("/* done */ COMMIT"),
        TransactionEffect::Closes
    );
}

#[test]
fn transaction_effect_handles_empty_input() {
    assert_eq!(transaction_effect(""), TransactionEffect::None);
    assert_eq!(transaction_effect("   \n"), TransactionEffect::None);
    assert_eq!(
        transaction_effect("-- only a comment"),
        TransactionEffect::None
    );
}

#[test]
fn transaction_effect_does_not_match_plpgsql_block_bodies() {
    // A PL/pgSQL `BEGIN … END` arrives inside a DO or CREATE FUNCTION
    // statement, whose leading keyword is neither, so the block body cannot
    // be mistaken for transaction control.
    assert_eq!(
        transaction_effect("DO $$ BEGIN RAISE NOTICE 'hi'; END $$"),
        TransactionEffect::None
    );
}

#[test]
fn build_search_path_sql_prepends_onto_existing_path() {
    // The selected schema resolves first; the connection's real path is kept
    // as fallback so queries against other schemas still resolve (#51).
    assert_eq!(
        build_search_path_sql("analyst", "\"$user\", public", false),
        "SET search_path TO \"analyst\", \"$user\", public"
    );
}

#[test]
fn build_search_path_sql_emits_only_schema_when_current_is_empty() {
    assert_eq!(
        build_search_path_sql("analyst", "", false),
        "SET search_path TO \"analyst\""
    );
}

#[test]
fn build_search_path_sql_emits_only_schema_when_current_is_whitespace() {
    assert_eq!(
        build_search_path_sql("analyst", "   ", false),
        "SET search_path TO \"analyst\""
    );
}

#[test]
fn build_search_path_sql_preserves_user_placeholder_verbatim() {
    // `current_setting('search_path')` returns `"$user"` double-quoted, and
    // PostgreSQL accepts that form back as the placeholder (probed against a
    // live database). The helper must not unquote it — unquoted `$user` is a
    // syntax error in `SET search_path TO`.
    assert_eq!(
        build_search_path_sql("analyst", "\"$user\", public", false),
        "SET search_path TO \"analyst\", \"$user\", public"
    );
}

#[test]
fn build_search_path_sql_quotes_schema_with_special_characters() {
    // A schema name needing quoting is escaped via `quote_identifier`, not
    // emitted raw.
    assert_eq!(
        build_search_path_sql("a\"b", "public", false),
        "SET search_path TO \"a\"\"b\", public"
    );
}

// `local: true` selects `SET LOCAL` (transaction-scoped) over `SET`
// (session-scoped). The pinned one-shot transaction path (#160/#163) uses
// `SET LOCAL` so the change can't leak across pooler reassignment. These
// mirror the `SET` cases above so the local branch is exercised for every
// shape the shared cached path covers — a unit-test analog of
// `apply_search_path_local`, which is otherwise only reachable through the
// live pinned-path test (and only when a `schema` param is sent).
#[test]
fn build_search_path_sql_local_prepends_onto_existing_path() {
    assert_eq!(
        build_search_path_sql("analyst", "\"$user\", public", true),
        "SET LOCAL search_path TO \"analyst\", \"$user\", public"
    );
}

#[test]
fn build_search_path_sql_local_emits_only_schema_when_current_is_empty() {
    assert_eq!(
        build_search_path_sql("analyst", "", true),
        "SET LOCAL search_path TO \"analyst\""
    );
}

#[test]
fn build_search_path_sql_local_emits_only_schema_when_current_is_whitespace() {
    assert_eq!(
        build_search_path_sql("analyst", "   ", true),
        "SET LOCAL search_path TO \"analyst\""
    );
}

#[test]
fn build_search_path_sql_local_preserves_user_placeholder_verbatim() {
    assert_eq!(
        build_search_path_sql("analyst", "\"$user\", public", true),
        "SET LOCAL search_path TO \"analyst\", \"$user\", public"
    );
}

#[test]
fn build_search_path_sql_local_quotes_schema_with_special_characters() {
    assert_eq!(
        build_search_path_sql("a\"b", "public", true),
        "SET LOCAL search_path TO \"a\"\"b\", public"
    );
}

// --- sets_session_local_state (#173) -----------------------------------------
//
// A bare batch (no `session_id`, no `BEGIN`) on a `pooler_mode = "transaction"`
// connection runs its result-set `SELECT` on the shared batch client when an
// earlier statement set session-local state, instead of the #172 one-shot
// path (a fresh connection that never saw the `SET` / temp table). This
// classifier decides which earlier statements flip that routing. See
// `run_batch_in_session`'s routing comment and the `sets_session_local_state`
// doc comment for why transaction-scoped forms are excluded (the
// `in_transaction` branch already keeps those on the shared client).

#[test]
fn sets_session_local_state_detects_session_set() {
    // A `SET` without `LOCAL` persists for the session on the held connection,
    // so a later `SELECT` on the same connection sees it. The GUC name and
    // `=`/`TO` forms both count.
    for query in [
        "SET extra_float_digits = 3",
        "SET search_path TO analytics",
        "SET statement_timeout = 5000",
        "SET timezone TO 'UTC'",
    ] {
        assert!(
            sets_session_local_state(query),
            "{query} should count as session-local state"
        );
    }
}

#[test]
fn sets_session_local_state_detects_set_role_and_session_authorization() {
    // `SET ROLE` / `SET SESSION AUTHORIZATION` change session identity —
    // session-scoped, visible to a later statement on the same connection.
    for query in [
        "SET ROLE analyst",
        "SET SESSION AUTHORIZATION postgres",
        "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL READ COMMITTED",
    ] {
        assert!(
            sets_session_local_state(query),
            "{query} should count as session-local state"
        );
    }
}

#[test]
fn sets_session_local_state_detects_create_temp_table() {
    // Both `TEMP` and `TEMPORARY`; a temp table created outside a transaction
    // lives for the session and is visible only on the same connection.
    for query in [
        "CREATE TEMP TABLE t (id int)",
        "CREATE TEMPORARY TABLE t AS SELECT 1 AS id",
        "CREATE TEMP TABLE t (id int) ON COMMIT DROP",
    ] {
        assert!(
            sets_session_local_state(query),
            "{query} should count as session-local state"
        );
    }
}

#[test]
fn sets_session_local_state_excludes_set_local() {
    // `SET LOCAL` is transaction-scoped: it dies at transaction end, so on the
    // one-shot path (which wraps its own `BEGIN`/`COMMIT`) it is correctly
    // scoped and the `in_transaction` branch handles it when there is an open
    // transaction. Either way it is not state a *bare-batch* SELECT on a fresh
    // one-shot connection would have seen, so excluding it keeps the routing
    // on the (cheaper, pooler-safe) one-shot path. `SET TRANSACTION` /
    // `SET CONSTRAINTS` are likewise transaction-control verbs, not GUCs —
    // outside a `BEGIN` they set no session state a later SELECT depends on.
    for query in [
        "SET LOCAL statement_timeout = 5000",
        "SET LOCAL search_path TO analytics",
        "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
        "SET CONSTRAINTS ALL IMMEDIATE",
    ] {
        assert!(
            !sets_session_local_state(query),
            "{query} is transaction-scoped, not session-local"
        );
    }
}

#[test]
fn sets_session_local_state_detects_global_local_temp_table() {
    // PostgreSQL accepts an optional deprecated `GLOBAL`/`LOCAL` qualifier
    // before `TEMP`/`TEMPORARY` (both still create a session-scoped temp table
    // — verified against a live database). Without the 4-keyword lookahead
    // the classifier would miss these and a later `SELECT * FROM t` would hit
    // the exact #173 `relation does not exist` regression.
    for query in [
        "CREATE LOCAL TEMP TABLE t (id int)",
        "CREATE LOCAL TEMPORARY TABLE t AS SELECT 1 AS id",
        "CREATE GLOBAL TEMP TABLE t (id int)",
        "CREATE GLOBAL TEMPORARY TABLE t (id int)",
    ] {
        assert!(
            sets_session_local_state(query),
            "{query} should count as session-local state (the GLOBAL/LOCAL \
             qualifier is deprecated noise before TEMP)"
        );
    }
}

#[test]
fn sets_session_local_state_excludes_permanent_create_and_other_ddl() {
    // A permanent `CREATE TABLE` (or `CREATE TYPE`/`CREATE INDEX`) is visible to
    // any connection, so the one-shot path sees it — routing to the shared
    // client is unnecessary. Only *temp* tables are connection-scoped.
    for query in [
        "CREATE TABLE t (id int)",
        "CREATE TYPE mood AS ENUM ('happy', 'sad')",
        "CREATE INDEX ON t (id)",
        "CREATE SCHEMA s",
        "CREATE OR REPLACE VIEW v AS SELECT 1",
    ] {
        assert!(
            !sets_session_local_state(query),
            "{query} is not session-local state"
        );
    }
}

#[test]
fn sets_session_local_state_excludes_result_set_and_other_statements() {
    // A `SELECT`/`UPDATE`/transaction-control verb sets no session state a
    // later statement depends on; routing them to shared is unnecessary.
    for query in [
        "SELECT 1.0/3.0",
        "SELECT * FROM t",
        "UPDATE t SET x = 1",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
        "INSERT INTO t VALUES (1)",
        "DELETE FROM t",
    ] {
        assert!(
            !sets_session_local_state(query),
            "{query} sets no session-local state"
        );
    }
}

#[test]
fn sets_session_local_state_sees_through_leading_comments() {
    // `leading_keywords` strips `--`/`/* */` comments, so a comment-headed
    // session `SET` is still classified — matching `transaction_effect`'s
    // comment handling (a comment-headed `BEGIN` still opens a transaction).
    assert!(sets_session_local_state(
        "-- tune precision\nSET extra_float_digits = 3"
    ));
    assert!(sets_session_local_state(
        "/* temp scratch */ CREATE TEMP TABLE t (id int)"
    ));
}

#[test]
fn sets_session_local_state_handles_empty_and_comment_only_input() {
    assert!(!sets_session_local_state(""));
    assert!(!sets_session_local_state("   \n"));
    assert!(!sets_session_local_state("-- only a comment"));
    assert!(!sets_session_local_state("/* block only */"));
}

#[test]
fn sets_session_local_state_ignores_set_inside_string_literal() {
    // `leading_keywords` stops at a string/quoted identifier/dollar quote, so
    // a `SET` that is data (e.g. inside a `SELECT 'SET x = 1'`) is not read as
    // a keyword — matching `transaction_effect`'s string-handling.
    assert!(!sets_session_local_state(
        "SELECT 'SET extra_float_digits = 3'"
    ));
}
