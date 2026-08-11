//! Unit tests for `pagination.rs`. Sibling test file per repo convention
//! (`.rules/rust.md` #4/#5) — loaded via `#[cfg(test)] mod pagination_tests;`.

use crate::utils::pagination::{build_paginated_query, limit_offset};

#[test]
fn limit_offset_computes_zero_based_offset_from_one_indexed_page() {
    assert_eq!(limit_offset(1, 10), (10, 0));
    assert_eq!(limit_offset(2, 10), (10, 10));
    assert_eq!(limit_offset(3, 5), (5, 10));
}

#[test]
fn build_paginated_query_appends_limit_offset_when_none_present() {
    let sql = build_paginated_query("SELECT * FROM t ORDER BY id", 10, 1);
    assert_eq!(sql, "SELECT * FROM t ORDER BY id LIMIT 11 OFFSET 0");
}

#[test]
fn build_paginated_query_page_two_uses_correct_offset() {
    let sql = build_paginated_query("SELECT * FROM t ORDER BY id", 10, 2);
    assert_eq!(sql, "SELECT * FROM t ORDER BY id LIMIT 11 OFFSET 10");
}

#[test]
fn build_paginated_query_strips_existing_trailing_limit() {
    // Without stripping, this would produce two LIMIT clauses (a syntax
    // error) — this is the regression this module exists to prevent.
    let sql = build_paginated_query("SELECT * FROM t ORDER BY id LIMIT 5", 100, 1);
    assert_eq!(
        sql.matches("LIMIT").count(),
        1,
        "must not contain two LIMIT clauses: {sql}"
    );
}

#[test]
fn build_paginated_query_honors_user_limit_as_a_cap_across_pages() {
    // User asked for at most 5 rows total. Page 1 with page_size=100 should
    // fetch min(5, 101) = 5, not 101.
    let sql = build_paginated_query("SELECT * FROM t ORDER BY id LIMIT 5", 100, 1);
    assert_eq!(sql, "SELECT * FROM t ORDER BY id LIMIT 5 OFFSET 0");
}

#[test]
fn build_paginated_query_user_limit_cap_shrinks_on_later_pages() {
    // User LIMIT 5, page_size 2, page 3 -> offset 4, remaining = 5-4 = 1.
    let sql = build_paginated_query("SELECT * FROM t ORDER BY id LIMIT 5", 2, 3);
    assert_eq!(sql, "SELECT * FROM t ORDER BY id LIMIT 1 OFFSET 4");
}

#[test]
fn build_paginated_query_strips_existing_limit_and_offset() {
    let sql = build_paginated_query("SELECT * FROM t ORDER BY id LIMIT 5 OFFSET 3", 100, 1);
    // User OFFSET 3 is preserved and added to the page offset (0 on page 1).
    assert_eq!(sql, "SELECT * FROM t ORDER BY id LIMIT 5 OFFSET 3");
}

#[test]
fn build_paginated_query_adds_user_offset_to_page_offset() {
    let sql = build_paginated_query("SELECT * FROM t ORDER BY id OFFSET 3", 10, 2);
    // page 2 offset = 10, plus user offset 3 = 13.
    assert_eq!(sql, "SELECT * FROM t ORDER BY id LIMIT 11 OFFSET 13");
}

#[test]
fn build_paginated_query_ignores_trailing_semicolon() {
    let sql = build_paginated_query("SELECT * FROM t;", 10, 1);
    assert_eq!(sql, "SELECT * FROM t LIMIT 11 OFFSET 0");
}
