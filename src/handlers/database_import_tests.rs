//! Unit tests for the pure parts of `import_database_sql`: request parsing,
//! statement classification and error formatting. The transactional behavior
//! itself is covered against a live database in `tests/live_db.rs`.

use super::*;
use crate::client;
use crate::models::ConnectionParams;
use serde_json::json;

fn req(extra: Value) -> Value {
    let mut base = json!({
        "params": {"host": "h", "database": "d", "username": "u"},
        "file_path": "/tmp/x.sql",
        "create_database": false
    });
    for (k, v) in extra.as_object().unwrap() {
        base[k] = v.clone();
    }
    base
}

#[test]
fn parses_the_host_contract_and_defaults_the_schema_to_public() {
    let r = parse_request(&req(json!({}))).unwrap();
    assert_eq!(r.file_path, "/tmp/x.sql");
    assert_eq!(r.schema, "public");
    assert_eq!(r.params.database.as_deref(), Some("d"));
}

#[test]
fn an_explicit_schema_is_used_and_a_blank_one_falls_back_to_public() {
    assert_eq!(
        parse_request(&req(json!({"schema": "analytics"})))
            .unwrap()
            .schema,
        "analytics"
    );
    assert_eq!(
        parse_request(&req(json!({"schema": ""}))).unwrap().schema,
        "public"
    );
    assert_eq!(
        parse_request(&req(json!({"schema": "  "}))).unwrap().schema,
        "public"
    );
    assert_eq!(
        parse_request(&req(json!({"schema": null}))).unwrap().schema,
        "public"
    );
}

#[test]
fn file_path_is_required_and_non_empty() {
    let mut missing = req(json!({}));
    missing.as_object_mut().unwrap().remove("file_path");
    assert!(parse_request(&missing).unwrap_err().contains("file_path"));
    assert!(parse_request(&req(json!({"file_path": ""})))
        .unwrap_err()
        .contains("file_path"));
    assert!(parse_request(&req(json!({"file_path": 7})))
        .unwrap_err()
        .contains("file_path"));
}

#[test]
fn a_read_only_connection_is_rejected() {
    let mut r = req(json!({}));
    r["params"]["read_only"] = json!(true);
    assert!(parse_request(&r)
        .unwrap_err()
        .to_lowercase()
        .contains("read-only"));
    r["params"]["read_only"] = json!(false);
    assert!(parse_request(&r).is_ok());
}

#[test]
fn create_database_is_rejected_with_a_clear_message() {
    let err = parse_request(&req(json!({"create_database": true}))).unwrap_err();
    assert!(err.contains("creating a database"), "{err}");
}

#[test]
fn ordinary_statements_run() {
    for sql in [
        "INSERT INTO t VALUES (1)",
        "CREATE TABLE begin_t (id int)",
        "SET search_path = public",
        "SELECT 'COMMIT'",
        "SAVEPOINT a",
        "ROLLBACK TO SAVEPOINT a",
        "RELEASE SAVEPOINT a",
        "CREATE FUNCTION f() RETURNS void AS $$ BEGIN PERFORM 1; END; $$ LANGUAGE plpgsql",
        "DO $$ BEGIN COMMIT; END $$",
    ] {
        assert_eq!(classify(sql), Action::Run, "{sql}");
    }
}

#[test]
fn transaction_open_and_commit_are_skipped_because_the_import_owns_the_transaction() {
    for sql in [
        "BEGIN",
        "begin transaction",
        "BEGIN ISOLATION LEVEL SERIALIZABLE",
        "START TRANSACTION",
        "COMMIT",
        "commit work",
        "END",
        "/* c */ COMMIT",
    ] {
        assert_eq!(classify(sql), Action::Skip, "{sql}");
    }
}

#[test]
fn rollback_abort_chain_and_prepare_transaction_are_rejected() {
    for sql in [
        "ROLLBACK",
        "rollback work",
        "ABORT",
        "COMMIT AND CHAIN",
        "ROLLBACK AND CHAIN",
        "PREPARE TRANSACTION 'x'",
    ] {
        assert!(matches!(classify(sql), Action::Reject(_)), "{sql}");
    }
}

#[test]
fn copy_to_or_from_the_client_stream_is_rejected_before_it_can_desync_the_protocol() {
    for sql in [
        "COPY public.t (a, b) FROM stdin",
        "copy t from STDIN with (format csv)",
        "COPY t FROM STDIN;",
        "COPY (SELECT 1) TO STDOUT",
        "COPY t TO stdout",
    ] {
        match classify(sql) {
            Action::Reject(why) => assert!(why.contains("COPY"), "{sql}: {why}"),
            other => panic!("{sql}: {other:?}"),
        }
    }
}

#[test]
fn server_side_copy_and_unrelated_statements_mentioning_copy_run() {
    assert_eq!(classify("COPY t TO '/tmp/out.csv'"), Action::Run);
    assert_eq!(classify("CREATE TABLE copy_of_stdin (id int)"), Action::Run);
    assert_eq!(
        classify("INSERT INTO t VALUES ('COPY FROM STDIN')"),
        Action::Run
    );
}

#[test]
fn pg_dump_restrict_markers_are_skipped_and_other_meta_commands_rejected() {
    assert_eq!(classify("\\restrict abc123"), Action::Skip);
    assert_eq!(classify("\\unrestrict abc123"), Action::Skip);
    for sql in ["\\connect other", "\\c other", "\\i file.sql", "\\."] {
        match classify(sql) {
            Action::Reject(why) => assert!(why.contains("meta-command"), "{sql}: {why}"),
            other => panic!("{sql}: {other:?}"),
        }
    }
}

#[test]
fn statement_errors_carry_index_line_reason_and_query() {
    let msg = statement_error(3, 12, "ERROR: boom", "INSERT INTO t VALUES (1)");
    assert_eq!(
        msg,
        "Error at statement 3 (line 12): ERROR: boom\nQuery: INSERT INTO t VALUES (1)"
    );
}

#[test]
fn a_huge_statement_is_truncated_in_the_error_message_on_a_char_boundary() {
    let text = format!("INSERT INTO t VALUES ('{}')", "é".repeat(50_000));
    let msg = statement_error(1, 1, "ERROR: x", &text);
    assert!(
        msg.chars().count() < 1_200,
        "message is {} chars",
        msg.chars().count()
    );
    assert!(msg.contains("truncated"));
    assert!(
        msg.starts_with("Error at statement 1 (line 1): ERROR: x\nQuery: INSERT INTO t VALUES ('é")
    );
}

#[test]
fn a_dollar_inside_an_identifier_is_not_a_dollar_quote_when_looking_for_copy_stdin() {
    for sql in [
        "COPY a$b$ FROM STDIN",
        "COPY t$x$y (a) FROM stdin WITH (FORMAT csv)",
    ] {
        assert!(matches!(classify(sql), Action::Reject(_)), "{sql}");
    }
}

#[test]
fn turning_off_standard_conforming_strings_is_rejected_because_it_changes_how_the_file_must_be_split(
) {
    for sql in [
        "SET standard_conforming_strings = off",
        "set standard_conforming_strings to 'off'",
        "SET SESSION standard_conforming_strings = false",
        "SET LOCAL standard_conforming_strings = 0",
        "SET standard_conforming_strings TO \"off\"",
    ] {
        match classify(sql) {
            Action::Reject(why) => {
                assert!(why.contains("standard_conforming_strings"), "{sql}: {why}")
            }
            other => panic!("{sql}: {other:?}"),
        }
    }
}

#[test]
fn leaving_standard_conforming_strings_on_is_fine() {
    for sql in [
        "SET standard_conforming_strings = on",
        "SET standard_conforming_strings TO 'on'",
        "SET standard_conforming_strings = true",
        "SET standard_conforming_strings = DEFAULT",
        "SELECT 'standard_conforming_strings = off'",
        "CREATE TABLE standard_conforming_strings (a int)",
    ] {
        assert_eq!(classify(sql), Action::Run, "{sql}");
    }
}

#[test]
fn a_commit_rejected_by_the_server_says_nothing_was_imported() {
    let msg = commit_error_message(12, "ERROR: insert or update violates foreign key", true);
    assert!(
        msg.starts_with("COMMIT failed, nothing was imported"),
        "{msg}"
    );
    assert!(
        msg.contains("12 statements") && msg.contains("foreign key"),
        "{msg}"
    );
}

#[test]
fn a_commit_that_loses_the_connection_does_not_claim_the_outcome() {
    let msg = commit_error_message(12, "connection closed", false);
    assert!(msg.starts_with("COMMIT failed"), "{msg}");
    assert!(!msg.contains("nothing was imported"), "{msg}");
    assert!(msg.contains("outcome is unknown"), "{msg}");
}

#[test]
fn a_multi_command_rejection_explains_the_splitter_could_not_separate_the_statement() {
    let reason = statement_failure_reason(
        "42601",
        "ERROR: cannot insert multiple commands into a prepared statement",
    );
    assert!(reason.contains("could not be split"), "{reason}");
    assert_eq!(
        statement_failure_reason("23505", "ERROR: duplicate key"),
        "ERROR: duplicate key"
    );
}

#[tokio::test]
async fn a_reader_that_dies_without_the_end_marker_is_an_error_not_a_clean_end() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Item>(4);
    tx.send(Ok(Some(SqlStatement {
        text: "SELECT 1".into(),
        line: 1,
    })))
    .await
    .unwrap();
    drop(tx);
    assert_eq!(next_item(&mut rx).await.unwrap().unwrap().text, "SELECT 1");
    let err = next_item(&mut rx).await.unwrap_err();
    assert!(err.contains("stopped unexpectedly"), "{err}");
}

#[tokio::test]
async fn the_end_marker_is_a_clean_end() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Item>(4);
    tx.send(Ok(None)).await.unwrap();
    assert!(next_item(&mut rx).await.unwrap().is_none());
}

#[tokio::test]
async fn a_reader_error_is_propagated_verbatim() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Item>(4);
    tx.send(Err("Unterminated string literal starting at line 9".into()))
        .await
        .unwrap();
    assert_eq!(
        next_item(&mut rx).await.unwrap_err(),
        "Unterminated string literal starting at line 9"
    );
}

#[test]
fn session_state_is_restored_before_commit_and_the_durability_setting_is_reapplied_last() {
    let steps = session_restore_statements();
    assert_eq!(steps.first(), Some(&"RESET ALL"));
    // RESET ALL deliberately skips role and session authorization.
    assert!(steps.contains(&"RESET SESSION AUTHORIZATION"));
    assert_eq!(steps.last(), Some(&"SET LOCAL synchronous_commit = OFF"));
}

/// Needs `PG_PLUGIN_TEST_HOST` (see `client_live_tests.rs`); skipped otherwise.
#[tokio::test]
async fn one_statement_text_holding_several_commands_is_refused_not_run() {
    let Ok(host) = std::env::var("PG_PLUGIN_TEST_HOST") else {
        return;
    };
    let params = ConnectionParams::from_value(&json!({
        "host": host,
        "port": std::env::var("PG_PLUGIN_TEST_PORT").ok().and_then(|p| p.parse::<u16>().ok()).unwrap_or(54320),
        "database": std::env::var("PG_PLUGIN_TEST_DB").unwrap_or_else(|_| "testdb".into()),
        "username": std::env::var("PG_PLUGIN_TEST_USER").unwrap_or_else(|_| "postgres".into()),
        "password": std::env::var("PG_PLUGIN_TEST_PASSWORD").unwrap_or_else(|_| "password".into()),
    }));
    let client = crate::client::oneshot_pool_client(&params).await.unwrap();
    client.batch_execute("BEGIN").await.unwrap();
    client
        .batch_execute("CREATE TEMP TABLE one_cmd (id int)")
        .await
        .unwrap();

    let err = run_statement(
        &client,
        "INSERT INTO one_cmd VALUES (1); COMMIT; INSERT INTO one_cmd VALUES (2)",
    )
    .await
    .unwrap_err();
    assert_eq!(err.code().map(|c| c.code()), Some("42601"), "{err}");
    assert!(client::format_pg_error(&err).contains("multiple commands"));

    // The transaction is still open (COMMIT never ran) and nothing was inserted.
    let still_open = client.query_one("SELECT count(*) FROM one_cmd", &[]).await;
    assert!(still_open.is_err() || still_open.unwrap().get::<_, i64>(0) == 0);
}
