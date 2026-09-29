//! Unit tests for `metadata.rs`'s pure helpers — the query-selection
//! helper (`routine_query_for_version`), the DDL-text builder
//! (`build_table_ddl`, backing #118's `get_table_ddl`), and the
//! batch-composition helpers (`build_schema_snapshot`, `group_rows_by_table`,
//! backing #121's `get_schema_snapshot`/`get_all_columns_batch`/
//! `get_all_foreign_keys_batch`). Sibling test file per repo convention
//! (`.rules/rust.md` #4/#5) — loaded via
//! `#[cfg(test)] #[path = "metadata_tests.rs"] mod metadata_tests;`.

use super::{build_table_ddl, column_default_value, routine_query_for_version};
use serde_json::{json, Map, Value};

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

/// Helper for the `build_table_ddl` tests: a `row_to_table_column`-shaped
/// column object with the fields `build_table_ddl` reads.
fn column(name: &str, data_type: &str, is_nullable: bool, is_pk: bool) -> Value {
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

#[test]
fn column_names_with_embedded_quotes_are_escaped_not_truncated() {
    // A column named `we"ird` is legal PostgreSQL (CREATE TABLE t
    // ("we""ird" int)). Bare `"{name}"` interpolation would terminate the
    // identifier early and corrupt the statement; it must come out escaped
    // as `"we""ird"`, exactly like the schema/table name is.
    let columns = vec![column("we\"ird", "integer", false, true)];

    let ddl = build_table_ddl("public", "t", &columns).unwrap();

    assert!(
        ddl.contains("\"we\"\"ird\" integer NOT NULL"),
        "column def not escaped correctly: {ddl}"
    );
    assert!(
        ddl.contains("PRIMARY KEY (\"we\"\"ird\")"),
        "primary key column not escaped correctly: {ddl}"
    );
}

/// Helper: build a `HashMap<String, Vec<_>>`-shaped JSON object (the wire
/// shape `get_all_columns_batch`/`get_all_foreign_keys_batch` return) from
/// `(table, value)` pairs, preserving insertion order per table.
fn batch_map(pairs: &[(&str, Value)]) -> Map<String, Value> {
    let mut map = Map::new();
    for (table, value) in pairs {
        map.entry(table.to_string())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .unwrap()
            .push(value.clone());
    }
    map
}

#[test]
fn build_schema_snapshot_zips_tables_with_their_columns_and_foreign_keys() {
    // Mirrors the builtin's `get_schema_snapshot`: each table from
    // `get_tables` is paired with its columns/FKs from the batch maps,
    // falling back to empty arrays when a table has none.
    let tables = json!([{"name": "users"}, {"name": "orders"}, {"name": "empty"}]);
    let columns_map = batch_map(&[
        (
            "users",
            json!({"name": "id", "data_type": "integer", "is_pk": true}),
        ),
        (
            "orders",
            json!({"name": "user_id", "data_type": "integer", "is_pk": false}),
        ),
    ]);
    let fks_map = batch_map(&[(
        "orders",
        json!({"name": "orders_user_id_fkey", "column_name": "user_id", "ref_table": "users"}),
    )]);

    let snapshot =
        super::build_schema_snapshot(tables.as_array().unwrap().clone(), columns_map, fks_map);

    assert_eq!(
        json!(snapshot),
        json!([
            {"name": "users", "columns": [{"name": "id", "data_type": "integer", "is_pk": true}], "foreign_keys": []},
            {"name": "orders", "columns": [{"name": "user_id", "data_type": "integer", "is_pk": false}], "foreign_keys": [{"name": "orders_user_id_fkey", "column_name": "user_id", "ref_table": "users"}]},
            {"name": "empty", "columns": [], "foreign_keys": []},
        ]),
        "each table carries its columns and FKs, with empty arrays when absent"
    );
}

#[test]
fn build_schema_snapshot_preserves_table_order_from_get_tables() {
    // `get_tables` orders by name; the snapshot must keep that order even
    // when the batch maps group by a different key order.
    let tables = json!([{"name": "zeta"}, {"name": "alpha"}]);
    let columns_map = batch_map(&[("alpha", json!({"name": "id"}))]);
    let fks_map = Map::new();

    let snapshot =
        super::build_schema_snapshot(tables.as_array().unwrap().clone(), columns_map, fks_map);

    let names: Vec<&str> = snapshot
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["zeta", "alpha"],
        "table order follows get_tables, not the batch maps"
    );
}

#[test]
fn build_schema_snapshot_empty_schema_is_an_empty_array() {
    // A schema with no base tables: empty `get_tables` result, empty
    // batch maps. The snapshot is an empty array — *not* an error and
    // *not* null — so the host reads it as "no tables", not "no endpoint".
    let snapshot = super::build_schema_snapshot(vec![], Map::new(), Map::new());
    assert!(snapshot.is_empty());
}

#[test]
fn build_schema_snapshot_drops_table_comments_to_match_builtin_table_schema() {
    // The builtin's `get_schema_snapshot` builds `TableSchema { name,
    // columns, foreign_keys }` — no `comment` field — so the plugin must
    // drop the optional `comment` that `get_tables` carries. Carrying it
    // would be a wire-shape divergence (the host's `TableInfo` has
    // `comment`, but `TableSchema` does not), and parity is byte-for-byte
    // here: behavioral differences are regressions, not fixes.
    let tables = json!([{"name": "with_comment", "comment": "a table comment"}]);
    let snapshot =
        super::build_schema_snapshot(tables.as_array().unwrap().clone(), Map::new(), Map::new());
    assert_eq!(
        snapshot[0],
        json!({"name": "with_comment", "columns": [], "foreign_keys": []}),
        "snapshot entry must be exactly the builtin's TableSchema shape (name/columns/foreign_keys), dropping comment"
    );
}

#[test]
fn column_default_value_drops_a_lowercase_null_default_to_match_builtin() {
    // #122: the builtin filters NULL defaults case-insensitively
    // (eq_ignore_ascii_case("null")), so `DEFAULT null` (lowercase, which
    // PostgreSQL accepts) must produce no default_value — same as
    // `DEFAULT NULL`. The plugin's filter was case-sensitive (`== "NULL"`),
    // so it emitted a spurious `default_value: "null"` here. This is the
    // divergence the test proves before the fix.
    assert_eq!(
        column_default_value(Some("null"), "NO"),
        None,
        "lowercase `null` default must be dropped, matching the builtin's case-insensitive filter"
    );
    assert_eq!(
        column_default_value(Some("NULL"), "NO"),
        None,
        "uppercase `NULL` default must be dropped (this case already worked)"
    );
    assert_eq!(
        column_default_value(Some("Null"), "NO"),
        None,
        "mixed-case `Null` default must be dropped, matching the builtin's case-insensitive filter"
    );
}

#[test]
fn column_default_value_drops_null_cast_prefixes() {
    // `NULL::<type>` casts are PostgreSQL's representation of a nullable
    // column with no real default; the builtin drops these too. The
    // `NULL::` prefix check stays case-sensitive in the builtin (only the
    // bare-null check is eq_ignore_ascii_case), so match that exactly.
    assert_eq!(column_default_value(Some("NULL::text"), "NO"), None);
    assert_eq!(column_default_value(Some("NULL::integer"), "NO"), None);
}

#[test]
fn column_default_value_surfaces_real_defaults_unchanged() {
    // Non-NULL defaults pass through verbatim — the value the host shows
    // in the column's default cell.
    assert_eq!(column_default_value(Some("0"), "NO"), Some("0".to_string()));
    assert_eq!(
        column_default_value(Some("'neutral'"), "NO"),
        Some("'neutral'".to_string())
    );
    assert_eq!(
        column_default_value(Some("now()"), "NO"),
        Some("now()".to_string())
    );
}

#[test]
fn column_default_value_drops_auto_increment_defaults() {
    // SERIAL/IDENTITY columns carry a `nextval(...)` default or an
    // is_identity of YES; the host surfaces those as is_auto_increment,
    // not as default_value, so the raw default must not leak through.
    assert_eq!(
        column_default_value(Some("nextval('users_id_seq'::regclass)"), "NO"),
        None,
        "nextval default must be dropped (auto-increment)"
    );
    assert_eq!(
        column_default_value(Some("42"), "YES"),
        None,
        "is_identity=YES must drop the default regardless of its value"
    );
}

#[test]
fn column_default_value_drops_empty_defaults() {
    // An empty string is not a meaningful default — drop it rather than
    // surfacing an empty default_value field.
    assert_eq!(column_default_value(Some(""), "NO"), None);
    assert_eq!(column_default_value(None, "NO"), None, "no default at all -> None");
}
