//! Unit tests for the streaming SQL statement splitter.

use super::*;
use std::io::Cursor;

fn split(sql: &str) -> Vec<String> {
    split_full(sql).into_iter().map(|s| s.text).collect()
}

fn split_full(sql: &str) -> Vec<SqlStatement> {
    try_split(sql).expect("splitting should succeed")
}

fn try_split(sql: &str) -> Result<Vec<SqlStatement>, String> {
    let mut splitter = StatementSplitter::new(Cursor::new(sql.as_bytes()));
    let mut out = Vec::new();
    while let Some(statement) = splitter.next_statement()? {
        out.push(statement);
    }
    Ok(out)
}

#[test]
fn splits_on_semicolons_and_drops_the_terminator() {
    assert_eq!(split("SELECT 1; SELECT 2;"), ["SELECT 1", "SELECT 2"]);
}

#[test]
fn emits_a_final_statement_that_has_no_terminator() {
    assert_eq!(split("SELECT 1;\nSELECT 2"), ["SELECT 1", "SELECT 2"]);
}

#[test]
fn several_statements_on_one_line() {
    assert_eq!(
        split("SELECT 1;SELECT 2 ; SELECT 3;"),
        ["SELECT 1", "SELECT 2", "SELECT 3"]
    );
}

#[test]
fn empty_and_whitespace_only_input_yields_nothing() {
    assert!(split("").is_empty());
    assert!(split("  \n\t\n").is_empty());
    assert!(split(";;\n ; ;").is_empty());
}

#[test]
fn semicolon_inside_a_single_quoted_string_does_not_split() {
    assert_eq!(
        split("INSERT INTO t VALUES ('a;b'); SELECT 1;"),
        ["INSERT INTO t VALUES ('a;b')", "SELECT 1"]
    );
}

#[test]
fn doubled_single_quote_is_an_escape_not_a_terminator() {
    assert_eq!(
        split("INSERT INTO t VALUES ('it''s; fine'); SELECT 1;"),
        ["INSERT INTO t VALUES ('it''s; fine')", "SELECT 1"]
    );
}

#[test]
fn backslash_is_literal_in_a_standard_string() {
    // standard_conforming_strings is on by default: the quote after the
    // backslash closes the string.
    assert_eq!(
        split(r"SELECT 'a\'; SELECT 2;"),
        [r"SELECT 'a\'", "SELECT 2"]
    );
}

#[test]
fn backslash_escapes_the_quote_in_an_e_string() {
    assert_eq!(
        split(r"SELECT E'it\'s; here'; SELECT 2;"),
        [r"SELECT E'it\'s; here'", "SELECT 2"]
    );
    assert_eq!(
        split(r"SELECT e'a\\'; SELECT 2;"),
        [r"SELECT e'a\\'", "SELECT 2"]
    );
}

#[test]
fn semicolon_inside_a_quoted_identifier_does_not_split() {
    assert_eq!(
        split(r#"CREATE TABLE "a;b" (id int); SELECT 1;"#),
        [r#"CREATE TABLE "a;b" (id int)"#, "SELECT 1"]
    );
    assert_eq!(
        split(r#"SELECT 1 AS "x""y;z"; SELECT 2;"#),
        [r#"SELECT 1 AS "x""y;z""#, "SELECT 2"]
    );
}

#[test]
fn dollar_quoted_function_body_is_one_statement() {
    let body = "CREATE FUNCTION f() RETURNS int AS $$\nBEGIN\n  PERFORM 1;\n  RETURN 2;\nEND;\n$$ LANGUAGE plpgsql";
    assert_eq!(split(&format!("{body}; SELECT 1;")), [body, "SELECT 1"]);
}

#[test]
fn tagged_dollar_quotes_only_close_on_the_same_tag() {
    let body = "DO $fn$ BEGIN PERFORM $$; $$; END $fn$";
    assert_eq!(split(&format!("{body}; SELECT 1;")), [body, "SELECT 1"]);
}

#[test]
fn dollar_parameters_and_identifier_dollars_are_not_dollar_quotes() {
    assert_eq!(
        split("SELECT $1; SELECT $2, $3;"),
        ["SELECT $1", "SELECT $2, $3"]
    );
    assert_eq!(
        split("SELECT 1 AS a$b$c; SELECT 2;"),
        ["SELECT 1 AS a$b$c", "SELECT 2"]
    );
}

#[test]
fn line_comment_hides_semicolons_and_quotes() {
    assert_eq!(
        split("-- it's; a comment\nSELECT 1; -- trailing ' ;\nSELECT 2;"),
        ["SELECT 1", "SELECT 2"]
    );
}

#[test]
fn block_comments_nest_and_hide_semicolons() {
    assert_eq!(
        split("/* a; /* nested; */ still; */ SELECT 1; SELECT /* ; */ 2;"),
        ["SELECT 1", "SELECT /* ; */ 2"]
    );
}

#[test]
fn comment_markers_inside_strings_are_data() {
    assert_eq!(
        split("SELECT '-- x', '/* y'; SELECT 2;"),
        ["SELECT '-- x', '/* y'", "SELECT 2"]
    );
}

#[test]
fn comment_only_fragments_are_not_statements() {
    assert!(split("-- just a comment\n/* and a block */\n").is_empty());
    assert_eq!(split("-- header\nSELECT 1;\n-- footer\n"), ["SELECT 1"]);
}

#[test]
fn multi_line_statements_keep_their_newlines() {
    assert_eq!(
        split("CREATE TABLE t (\n  id int,\n  n text\n);\n"),
        ["CREATE TABLE t (\n  id int,\n  n text\n)"]
    );
}

#[test]
fn crlf_line_endings_are_handled() {
    assert_eq!(
        split("SELECT 1;\r\nSELECT 'a;\r\nb';\r\n"),
        ["SELECT 1", "SELECT 'a;\r\nb'"]
    );
}

#[test]
fn a_leading_byte_order_mark_is_ignored() {
    assert_eq!(split("\u{feff}SELECT 1;"), ["SELECT 1"]);
}

#[test]
fn non_ascii_text_survives_intact() {
    assert_eq!(
        split("INSERT INTO t VALUES ('héllo; 日本語 🎉');"),
        ["INSERT INTO t VALUES ('héllo; 日本語 🎉')"]
    );
}

#[test]
fn reports_the_line_each_statement_starts_on() {
    let lines: Vec<usize> = split_full("-- c\nSELECT 1;\n\n  SELECT\n 2;\nSELECT 3;")
        .iter()
        .map(|s| s.line)
        .collect();
    assert_eq!(lines, [2, 4, 6]);
}

#[test]
fn a_psql_meta_command_line_is_its_own_statement() {
    assert_eq!(
        split("\\connect other\nSELECT 1;\n\\restrict abc123\nSELECT 2;"),
        [
            "\\connect other",
            "SELECT 1",
            "\\restrict abc123",
            "SELECT 2"
        ]
    );
}

#[test]
fn a_backslash_inside_a_statement_is_not_a_meta_command() {
    assert_eq!(split("SELECT 1 \\\n + 1;"), ["SELECT 1 \\\n + 1"]);
}

#[test]
fn unterminated_constructs_are_errors_naming_the_start_line() {
    for (sql, what) in [
        ("SELECT 1;\nSELECT 'oops;", "string"),
        ("SELECT \"oops;", "identifier"),
        ("SELECT $$oops;", "dollar"),
        ("SELECT 1; /* oops", "comment"),
    ] {
        let err = try_split(sql).unwrap_err();
        assert!(err.to_lowercase().contains(what), "{sql:?} -> {err}");
        assert!(err.contains("line"), "{sql:?} -> {err}");
    }
}

#[test]
fn invalid_utf8_is_an_error_not_silent_corruption() {
    let mut splitter = StatementSplitter::new(Cursor::new(b"SELECT '\xff';".to_vec()));
    assert!(splitter.next_statement().is_err());
}

#[test]
fn works_through_a_tiny_buffered_reader() {
    let reader = std::io::BufReader::with_capacity(
        1,
        Cursor::new("SELECT 'a;b';\nSELECT $$;$$;".as_bytes()),
    );
    let mut splitter = StatementSplitter::new(reader);
    let mut out = Vec::new();
    while let Some(s) = splitter.next_statement().unwrap() {
        out.push(s.text);
    }
    assert_eq!(out, ["SELECT 'a;b'", "SELECT $$;$$"]);
}

#[test]
fn semicolons_inside_parentheses_do_not_split() {
    let rule = "CREATE RULE r AS ON INSERT TO t DO ALSO (INSERT INTO a VALUES (1); INSERT INTO b VALUES (2))";
    assert_eq!(split(&format!("{rule}; SELECT 1;")), [rule, "SELECT 1"]);
}

#[test]
fn an_unbalanced_close_paren_does_not_suppress_later_splits() {
    assert_eq!(split("SELECT 1); SELECT 2;"), ["SELECT 1)", "SELECT 2"]);
}

#[test]
fn begin_atomic_function_bodies_are_one_statement() {
    let f =
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql\nBEGIN ATOMIC\n  SELECT 1;\n  SELECT 2;\nEND";
    assert_eq!(split(&format!("{f}; SELECT 3;")), [f, "SELECT 3"]);
    let p = "create or replace procedure p() language sql begin atomic select 1; select 2; end";
    assert_eq!(split(&format!("{p}; SELECT 3;")), [p, "SELECT 3"]);
}

#[test]
fn case_inside_a_begin_atomic_body_does_not_end_it_early() {
    let f = "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT CASE WHEN true THEN 1 END; SELECT 2; END";
    assert_eq!(split(&format!("{f}; SELECT 3;")), [f, "SELECT 3"]);
}

#[test]
fn a_bare_begin_statement_is_not_a_function_body() {
    assert_eq!(
        split("BEGIN; SELECT 1; COMMIT;"),
        ["BEGIN", "SELECT 1", "COMMIT"]
    );
}

#[test]
fn begin_and_end_outside_a_create_function_do_not_suppress_splits() {
    assert_eq!(
        split("CREATE TABLE begin_t (id int); SELECT 1; CREATE TRIGGER t BEFORE INSERT ON x EXECUTE FUNCTION f(); SELECT 2;"),
        [
            "CREATE TABLE begin_t (id int)",
            "SELECT 1",
            "CREATE TRIGGER t BEFORE INSERT ON x EXECUTE FUNCTION f()",
            "SELECT 2"
        ]
    );
}

#[test]
fn begin_inside_a_quoted_function_body_is_not_counted() {
    let f = "CREATE FUNCTION f() RETURNS int AS 'begin return 1; end' LANGUAGE plpgsql";
    assert_eq!(split(&format!("{f}; SELECT 2;")), [f, "SELECT 2"]);
    let g = "CREATE FUNCTION g() RETURNS int AS $$ begin return 1; end $$ LANGUAGE plpgsql";
    assert_eq!(split(&format!("{g}; SELECT 2;")), [g, "SELECT 2"]);
}

#[test]
fn state_resets_between_statements() {
    let f = "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; END";
    assert_eq!(
        split(&format!("{f}; SELECT (1; SELECT 2;")),
        [f, "SELECT (1; SELECT 2;"]
    );
    assert_eq!(
        split(&format!("{f}; CREATE TABLE t (a int); SELECT 3;")),
        [f, "CREATE TABLE t (a int)", "SELECT 3"]
    );
}

#[test]
fn a_large_dollar_quoted_body_splits_in_linear_time() {
    let body = "x;\n".repeat(200_000);
    let sql = format!(
        "CREATE FUNCTION f() RETURNS void AS $tag$\n{body}$tag$ LANGUAGE plpgsql; SELECT 1;"
    );
    let started = std::time::Instant::now();
    let out = split(&sql);
    assert_eq!(out.len(), 2);
    assert!(
        started.elapsed().as_secs() < 5,
        "dollar-quote scanning is not linear"
    );
}

#[test]
fn begin_as_a_parameter_or_column_name_does_not_open_an_atomic_body() {
    let tail = " INSERT INTO t VALUES (1); COMMIT; INSERT INTO t VALUES (2);";
    for head in [
        "CREATE FUNCTION f() RETURNS TABLE(begin int) LANGUAGE sql AS 'select 1';",
        "CREATE FUNCTION f(begin date) RETURNS int LANGUAGE sql AS 'select 1';",
        "CREATE FUNCTION begin() RETURNS int LANGUAGE sql AS 'select 1';",
        "CREATE PROCEDURE p(IN begin int) LANGUAGE sql AS 'select 1';",
    ] {
        let out = split(&format!("{head}{tail}"));
        assert_eq!(out.len(), 4, "{head} -> {out:?}");
        assert_eq!(out[2], "COMMIT");
    }
}

#[test]
fn begin_atomic_is_recognised_across_whitespace_newlines_and_comments() {
    for f in [
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN\n  ATOMIC SELECT 1; END",
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN /* c */ ATOMIC SELECT 1; END",
        "create function f() returns int language sql begin -- c\n atomic select 1; select 2; end",
    ] {
        assert_eq!(split(&format!("{f}; SELECT 3;")), [f, "SELECT 3"], "{f}");
    }
}

#[test]
fn a_carriage_return_ends_a_line_comment() {
    assert_eq!(split("SELECT 1; -- c\rSELECT 2;"), ["SELECT 1", "SELECT 2"]);
    assert_eq!(split("-- header\rSELECT 1;"), ["SELECT 1"]);
}

#[test]
fn a_runaway_statement_is_rejected_at_the_size_limit_not_buffered_to_the_end() {
    let mut splitter = StatementSplitter::with_max_statement_bytes(
        Cursor::new(
            "SELECT 1;\nSELECT 'aaaaaaaaaaaaaaaaaaaaaaaa\nbbbbbbbbbbbbbbbbbbbbb\n".as_bytes(),
        ),
        20,
    );
    assert_eq!(splitter.next_statement().unwrap().unwrap().text, "SELECT 1");
    let err = splitter.next_statement().unwrap_err();
    assert!(err.contains("exceeds") && err.contains("line 2"), "{err}");
    assert!(
        splitter.next_statement().unwrap().is_none(),
        "splitter must stay stopped"
    );
}

#[test]
fn a_statement_exactly_at_the_limit_is_accepted() {
    let mut splitter =
        StatementSplitter::with_max_statement_bytes(Cursor::new("SELECT 12345;".as_bytes()), 12);
    assert_eq!(
        splitter.next_statement().unwrap().unwrap().text,
        "SELECT 12345"
    );
}
