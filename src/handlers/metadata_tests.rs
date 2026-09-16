//! Unit tests for `metadata.rs`'s pure query-selection helper
//! (`routine_query_for_version`). Sibling test file per repo convention
//! (`.rules/rust.md` #4/#5) — loaded via
//! `#[cfg(test)] #[path = "metadata_tests.rs"] mod metadata_tests;`.

use super::routine_query_for_version;

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
