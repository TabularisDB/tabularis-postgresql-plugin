//! Unit tests for `metadata.rs`'s pure helpers — the query-selection
//! helper (`routine_query_for_version`) and the DDL-text builder
//! (`build_table_ddl`). Sibling test file per repo convention
//! (`.rules/rust.md` #4/#5) — loaded via
//! `#[cfg(test)] #[path = "metadata_tests.rs"] mod metadata_tests;`.

use super::{build_table_ddl, routine_query_for_version};
use serde_json::json;

#[test]
fn pg11_and_newer_uses_the_prokind_query() {
    for version in [110_000, 110_001, 120_000, 160_000] {
        let query = routine_query_for_version(version);
        assert!(
            query.contains("prokind"),
            "version {version} should use the prokind query, got: {query}"
        );
        assert!(
            !query.contains("proisagg"),
            "version {version} should not use the legacy proisagg/proiswindow query"
        );
    }
}

#[test]
fn pre_pg11_falls_back_to_proisagg_proiswindow_query() {
    for version in [0, 90_600, 100_000, 100_015, 109_999] {
        let query = routine_query_for_version(version);
        assert!(
            query.contains("proisagg") && query.contains("proiswindow"),
            "version {version} should use the legacy proisagg/proiswindow query, got: {query}"
        );
        assert!(
            !query.contains("prokind IN"),
            "version {version} must not reference the prokind column, which doesn't exist \
             before PostgreSQL 11 (SQLSTATE 42703)"
        );
    }
}

#[test]
fn boundary_is_exactly_110000_inclusive() {
    // 110000 is PostgreSQL 11.0's server_version_num -- the exact version
    // that introduced prokind, so it must take the modern branch.
    assert!(routine_query_for_version(110_000).contains("prokind IN"));
    // One below must take the legacy branch.
    assert!(routine_query_for_version(109_999).contains("proisagg"));
}

#[test]
fn every_branch_selects_and_aliases_prokind_as_a_char_column() {
    // Both queries must produce a `prokind` column the handler can
    // try_get::<i8>() uniformly, regardless of which branch ran.
    for version in [90_600, 160_000] {
        let query = routine_query_for_version(version);
        assert!(query.contains("prokind"), "{query}");
    }
}

fn column(name: &str, data_type: &str, is_nullable: bool, is_pk: bool) -> serde_json::Value {
    json!({
        "name": name,
        "data_type": data_type,
        "is_nullable": is_nullable,
        "is_pk": is_pk,
        "is_auto_increment": false,
    })
}

#[test]
fn builds_create_table_with_not_null_and_primary_key() {
    let columns = vec![
        column("id", "integer", false, true),
        column("email", "text", false, false),
        column("bio", "text", true, false),
    ];

    let ddl = build_table_ddl("public", "users", &columns).unwrap();

    assert_eq!(
        ddl,
        "CREATE TABLE \"public\".\"users\" (\n  \
         \"id\" integer NOT NULL,\n  \
         \"email\" text NOT NULL,\n  \
         \"bio\" text,\n  \
         PRIMARY KEY (\"id\")\n\
         );"
    );
}

#[test]
fn composite_primary_key_lists_every_column_in_order() {
    let columns = vec![
        column("tenant_id", "uuid", false, true),
        column("item_id", "integer", false, true),
        column("label", "text", true, false),
    ];

    let ddl = build_table_ddl("public", "line_items", &columns).unwrap();

    assert!(ddl.contains("PRIMARY KEY (\"tenant_id\", \"item_id\")"));
}

#[test]
fn table_with_no_primary_key_omits_the_clause() {
    let columns = vec![column("note", "text", true, false)];

    let ddl = build_table_ddl("public", "notes", &columns).unwrap();

    assert!(!ddl.contains("PRIMARY KEY"));
}

#[test]
fn no_columns_reports_table_not_found_rather_than_an_empty_create_table() {
    let err = build_table_ddl("public", "ghost", &[]).unwrap_err();
    assert_eq!(err, "Table ghost not found or empty");
}

#[test]
fn schema_and_table_are_quote_escaped_in_the_qualified_name() {
    let columns = vec![column("id", "integer", false, false)];

    let ddl = build_table_ddl("public", "weird\"table", &columns).unwrap();

    assert!(ddl.starts_with("CREATE TABLE \"public\".\"weird\"\"table\" ("));
}
