//! Unit tests for `metadata.rs`'s pure query-selection helper
//! (`routine_query_for_version`) and the pure batch-composition helpers
//! (`build_schema_snapshot`, `group_rows_by_table`) backing #121's
//! `get_schema_snapshot`/`get_all_columns_batch`/`get_all_foreign_keys_batch`.
//! Sibling test file per repo convention (`.rules/rust.md` #4/#5) — loaded
//! via `#[cfg(test)] #[path = "metadata_tests.rs"] mod metadata_tests;`.

use super::routine_query_for_version;
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
