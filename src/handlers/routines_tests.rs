//! Unit tests for `handlers/routines.rs`'s pure SQL builders. Sibling test
//! file per repo convention (`#[cfg(test)] #[path = ...] mod routines_tests;`
//! in `routines.rs`) — mirrors the builtin driver's own
//! `routine_management` test module
//! (`src-tauri/src/drivers/postgres/tests.rs`) so the two stay provably in
//! sync on expected output.

use super::{drop_routine_sql, render_sql_literal, routine_call_sql, routine_template};
use crate::models::RoutineCallArg;

fn arg(name: &str, mode: &str, value: Option<&str>, is_raw: bool) -> RoutineCallArg {
    RoutineCallArg {
        name: name.to_string(),
        mode: mode.to_string(),
        value: value.map(|v| v.to_string()),
        is_raw,
    }
}

#[test]
fn function_call_uses_select_star_from() {
    let sql = routine_call_sql(
        "fn_report",
        "FUNCTION",
        &[arg("p_year", "IN", Some("2026"), true)],
        "public",
    );
    assert_eq!(sql, "SELECT * FROM \"public\".\"fn_report\"(2026);");
}

#[test]
fn function_call_excludes_out_params() {
    // PostgreSQL functions do not accept pure OUT parameters in the call
    // signature; only IN/INOUT are passed.
    let sql = routine_call_sql(
        "fn_split",
        "FUNCTION",
        &[
            arg("p_in", "IN", Some("5"), true),
            arg("p_lo", "OUT", None, false),
            arg("p_hi", "OUT", None, false),
        ],
        "public",
    );
    assert_eq!(sql, "SELECT * FROM \"public\".\"fn_split\"(5);");
}

#[test]
fn function_call_keeps_inout_params() {
    let sql = routine_call_sql(
        "fn_adjust",
        "FUNCTION",
        &[
            arg("p_val", "INOUT", Some("10"), true),
            arg("p_out", "OUT", None, false),
        ],
        "public",
    );
    assert_eq!(sql, "SELECT * FROM \"public\".\"fn_adjust\"(10);");
}

#[test]
fn procedure_call_renders_out_params_as_null() {
    let sql = routine_call_sql(
        "sp_test",
        "PROCEDURE",
        &[
            arg("p_in", "IN", Some("it's"), false),
            arg("p_out", "OUT", None, false),
        ],
        "public",
    );
    assert_eq!(sql, "CALL \"public\".\"sp_test\"('it''s', NULL);");
}

#[test]
fn procedure_call_keeps_out_params_in_argument_list() {
    // Unlike functions, procedures require every OUT parameter in the call
    // signature — this is the "OUT args ARE required" half of the contract.
    let sql = routine_call_sql(
        "sp_split",
        "PROCEDURE",
        &[
            arg("p_in", "IN", Some("5"), true),
            arg("p_out", "OUT", None, false),
        ],
        "public",
    );
    assert_eq!(sql, "CALL \"public\".\"sp_split\"(5, NULL);");
}

#[test]
fn render_sql_literal_quotes_and_escapes_non_raw_values() {
    assert_eq!(
        render_sql_literal(&arg("p", "IN", Some("it's"), false)),
        "'it''s'"
    );
}

#[test]
fn render_sql_literal_passes_raw_values_verbatim() {
    assert_eq!(
        render_sql_literal(&arg("p", "IN", Some("now()"), true)),
        "now()"
    );
}

#[test]
fn render_sql_literal_renders_missing_value_as_null() {
    assert_eq!(render_sql_literal(&arg("p", "OUT", None, false)), "NULL");
}

#[test]
fn create_templates_are_schema_qualified_or_replace() {
    let tpl = routine_template("FUNCTION", "app");
    assert!(
        tpl.starts_with("CREATE OR REPLACE FUNCTION \"app\"."),
        "{tpl}"
    );
    let tpl = routine_template("PROCEDURE", "");
    assert!(
        tpl.starts_with("CREATE OR REPLACE PROCEDURE my_procedure"),
        "{tpl}"
    );
}

#[test]
fn function_template_is_valid_plpgsql_with_dollar_quoting() {
    let tpl = routine_template("FUNCTION", "public");
    assert!(tpl.contains("LANGUAGE plpgsql"));
    assert!(tpl.contains("AS $$"));
    assert!(tpl.contains("RETURNS integer"));
}

#[test]
fn drop_sql_includes_identity_signature() {
    assert_eq!(
        drop_routine_sql("fn_add", "FUNCTION", "integer, integer", "public"),
        "DROP FUNCTION \"public\".\"fn_add\"(integer, integer)"
    );
    assert_eq!(
        drop_routine_sql("sp", "PROCEDURE", "", "public"),
        "DROP PROCEDURE \"public\".\"sp\"()"
    );
}
