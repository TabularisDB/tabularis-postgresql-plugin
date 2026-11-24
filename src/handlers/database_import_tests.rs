//! Unit tests for the pure parts of `import_database_sql`: request parsing,
//! statement classification and error formatting. The transactional behavior
//! itself is covered against a live database in `tests/live_db.rs`.

use super::*;
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
