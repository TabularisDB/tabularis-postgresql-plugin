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

// The caller clamps `limit` and `page` to `u32::MAX`, so the arithmetic has to
// hold there (#202): a wrapped `page_size + 1` became `LIMIT 0` and an
// overflowing offset panicked the worker.
#[test]
fn the_largest_page_size_does_not_overflow() {
    let sql = build_paginated_query("SELECT * FROM t", u32::MAX, 1);
    assert_eq!(sql, "SELECT * FROM t LIMIT 4294967296 OFFSET 0");
}

#[test]
fn a_page_offset_beyond_u32_does_not_overflow() {
    let sql = build_paginated_query("SELECT * FROM t", 100_000, 100_000);
    assert_eq!(sql, "SELECT * FROM t LIMIT 100001 OFFSET 9999900000");
    let sql = build_paginated_query("SELECT * FROM t", 2, 3_000_000_000);
    assert_eq!(sql, "SELECT * FROM t LIMIT 3 OFFSET 5999999998");
}

#[test]
fn the_largest_page_and_page_size_stay_within_a_bigint() {
    let sql = build_paginated_query("SELECT * FROM t", u32::MAX, u32::MAX);
    assert_eq!(
        sql,
        "SELECT * FROM t LIMIT 4294967296 OFFSET 9223372036854775807"
    );
}

#[test]
fn limit_offset_does_not_overflow_at_the_extremes() {
    assert_eq!(limit_offset(u32::MAX, u32::MAX).0, u64::from(u32::MAX));
    assert_eq!(limit_offset(u32::MAX, u32::MAX).1, i64::MAX as u64);
    assert_eq!(limit_offset(0, 10), (10, 0));
}

#[test]
fn a_user_limit_beyond_u32_is_kept_not_silently_dropped() {
    // 5_000_000_000 does not fit in a u32; dropping it would return more rows than asked for.
    let sql = build_paginated_query("SELECT * FROM t LIMIT 5000000000", 10, 1);
    assert_eq!(sql, "SELECT * FROM t LIMIT 11 OFFSET 0");
    let sql = build_paginated_query("SELECT * FROM t LIMIT 5 OFFSET 5000000000", 10, 1);
    assert_eq!(sql, "SELECT * FROM t LIMIT 5 OFFSET 5000000000");
    let sql = build_paginated_query("SELECT * FROM t LIMIT 5000000000 OFFSET 5000000000", 10, 2);
    assert_eq!(sql, "SELECT * FROM t LIMIT 11 OFFSET 5000000010");
}

#[test]
fn a_user_limit_beyond_u32_still_caps_a_deep_page() {
    // Page 1_000_000_000 of 5 starts at row 4_999_999_995; a user LIMIT of
    // 5_000_000_000 leaves five rows. Reading the limit as a u32 would drop it
    // and ask for six.
    let sql = build_paginated_query("SELECT * FROM t LIMIT 5000000000", 5, 1_000_000_000);
    assert_eq!(sql, "SELECT * FROM t LIMIT 5 OFFSET 4999999995");
}

#[test]
fn a_user_offset_added_to_a_page_offset_stays_within_a_bigint() {
    let sql = build_paginated_query("SELECT * FROM t OFFSET 9223372036854775807", 10, 2);
    assert_eq!(sql, "SELECT * FROM t LIMIT 11 OFFSET 9223372036854775807");
}

// Issue #204: the clause used to be appended inside a trailing `--` comment,
// where PostgreSQL ignores it and every page returns the same rows.

#[test]
fn a_trailing_line_comment_does_not_swallow_the_page_clause() {
    let sql = build_paginated_query("SELECT g FROM t ORDER BY g -- export", 1000, 2);
    assert_eq!(
        sql,
        "SELECT g FROM t ORDER BY g LIMIT 1001 OFFSET 1000 -- export"
    );
}

#[test]
fn a_comment_on_its_own_trailing_line_is_kept_after_the_clause() {
    let sql = build_paginated_query("SELECT g FROM t ORDER BY g\n-- export", 1000, 2);
    assert_eq!(
        sql,
        "SELECT g FROM t ORDER BY g LIMIT 1001 OFFSET 1000 -- export"
    );
}

#[test]
fn a_trailing_block_comment_is_kept_after_the_clause() {
    let sql = build_paginated_query(
        "SELECT g FROM t ORDER BY g /* export /* nested */ */",
        10,
        1,
    );
    assert_eq!(
        sql,
        "SELECT g FROM t ORDER BY g LIMIT 11 OFFSET 0 /* export /* nested */ */"
    );
}

#[test]
fn a_comment_marker_inside_a_string_or_identifier_is_not_a_comment() {
    for (query, expected) in [
        (
            "SELECT '--' AS a FROM t",
            "SELECT '--' AS a FROM t LIMIT 11 OFFSET 0",
        ),
        (
            "SELECT '/*' AS a FROM t",
            "SELECT '/*' AS a FROM t LIMIT 11 OFFSET 0",
        ),
        (
            "SELECT \"a--b\" FROM t",
            "SELECT \"a--b\" FROM t LIMIT 11 OFFSET 0",
        ),
        (
            "SELECT $$ -- x $$ FROM t",
            "SELECT $$ -- x $$ FROM t LIMIT 11 OFFSET 0",
        ),
        (
            "SELECT $q$ ' -- $q$ FROM t",
            "SELECT $q$ ' -- $q$ FROM t LIMIT 11 OFFSET 0",
        ),
        // A backslash escapes the quote only in an `E'..'` string.
        (
            "SELECT E'it\\'s -- x' FROM t",
            "SELECT E'it\\'s -- x' FROM t LIMIT 11 OFFSET 0",
        ),
        (
            "SELECT 'a\\' FROM t -- c",
            "SELECT 'a\\' FROM t LIMIT 11 OFFSET 0 -- c",
        ),
    ] {
        assert_eq!(build_paginated_query(query, 10, 1), expected, "{query}");
    }
}

#[test]
fn a_user_limit_and_offset_are_found_through_trailing_comments() {
    let sql = build_paginated_query("SELECT * FROM t LIMIT 5 OFFSET 3 -- keep it small", 100, 1);
    assert_eq!(sql, "SELECT * FROM t LIMIT 5 OFFSET 3 -- keep it small");
    let sql = build_paginated_query("SELECT * FROM t LIMIT 5 /* a */ -- b", 100, 1);
    assert_eq!(sql, "SELECT * FROM t LIMIT 5 OFFSET 0 /* a */\n-- b");
    // The user's limit still caps the rows across pages.
    let sql = build_paginated_query("SELECT * FROM t LIMIT 15 -- cap", 10, 2);
    assert_eq!(sql, "SELECT * FROM t LIMIT 5 OFFSET 10 -- cap");
}

#[test]
fn a_comment_before_a_stripped_clause_does_not_swallow_the_page_clause() {
    let sql = build_paginated_query("SELECT * FROM t -- note\nLIMIT 5", 10, 1);
    assert_eq!(sql, "SELECT * FROM t -- note\nLIMIT 5 OFFSET 0");
    let sql = build_paginated_query("SELECT * FROM t -- a\nLIMIT 5 OFFSET 2 -- b", 10, 1);
    assert_eq!(sql, "SELECT * FROM t -- a\nLIMIT 5 OFFSET 2 -- b");
}

#[test]
fn a_carriage_return_ends_a_line_comment() {
    let sql = build_paginated_query("SELECT 1 -- a\rLIMIT 5", 10, 1);
    assert_eq!(sql, "SELECT 1 -- a\nLIMIT 5 OFFSET 0");
}

#[test]
fn limit_and_offset_inside_a_comment_or_string_are_not_the_users() {
    let sql = build_paginated_query("SELECT 1 -- LIMIT 5", 10, 1);
    assert_eq!(sql, "SELECT 1 LIMIT 11 OFFSET 0 -- LIMIT 5");
    let sql = build_paginated_query("SELECT 'x LIMIT 5'", 10, 1);
    assert_eq!(sql, "SELECT 'x LIMIT 5' LIMIT 11 OFFSET 0");
    let sql = build_paginated_query("SELECT \"limit\" FROM t", 10, 1);
    assert_eq!(sql, "SELECT \"limit\" FROM t LIMIT 11 OFFSET 0");
}

#[test]
fn trailing_semicolons_and_unterminated_constructs_are_handled() {
    assert_eq!(
        build_paginated_query("SELECT 1; -- done", 10, 1),
        "SELECT 1 LIMIT 11 OFFSET 0 -- done"
    );
    assert_eq!(
        build_paginated_query("SELECT 1;;", 10, 1),
        "SELECT 1 LIMIT 11 OFFSET 0"
    );
    // An unterminated block comment stays unterminated, so the server reports it.
    assert_eq!(
        build_paginated_query("SELECT 1 /* oops", 10, 1),
        "SELECT 1 LIMIT 11 OFFSET 0 /* oops"
    );
}

#[test]
fn limit_and_offset_are_matched_in_any_case() {
    let sql = build_paginated_query("select * from t limit 5 offset 3", 100, 1);
    assert_eq!(sql, "select * from t LIMIT 5 OFFSET 3");
    let sql = build_paginated_query("SELECT * FROM t Limit 15", 10, 2);
    assert_eq!(sql, "SELECT * FROM t LIMIT 5 OFFSET 10");
}

#[test]
fn a_dollar_tag_cannot_start_with_a_digit() {
    // `$1$` is not a dollar quote, so the `--` after it is a real comment.
    let sql = build_paginated_query("SELECT $1$ -- x $1$ y", 10, 1);
    assert_eq!(sql, "SELECT $1$ LIMIT 11 OFFSET 0 -- x $1$ y");
}
