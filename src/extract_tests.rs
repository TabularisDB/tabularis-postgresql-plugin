//! Unit tests for `extract.rs`. Sibling test file per repo convention
//! (`.rules/rust.md` #4/#5), loaded via `#[cfg(test)] mod extract_tests;`.
//!
//! `extract_value` itself takes a live `tokio_postgres::Row`, which can only
//! be constructed by a real driver round-trip — that end-to-end path is
//! covered by `tests/live_db.rs::execute_query_returns_a_real_enum_value`.
//! These tests exercise the `EnumLabel::FromSql` impl directly (the part
//! that decides whether an enum column comes back as a string or silently
//! nulls out), the same way the builtin driver unit-tests
//! `extract/enum.rs::extract_or_null`.

use crate::extract::{EnumLabel, Money};
use tokio_postgres::types::{FromSql, Kind, Type};

fn enum_type() -> Type {
    Type::new(
        "mood".to_string(),
        16791,
        Kind::Enum(vec!["happy".to_string(), "sad".to_string()]),
        "test_schema".to_string(),
    )
}

#[test]
fn decodes_valid_utf8_label() {
    let ty = enum_type();
    let label = EnumLabel::from_sql(&ty, b"happy").unwrap();
    assert_eq!(label.0, "happy");
}

#[test]
fn rejects_invalid_utf8() {
    let ty = enum_type();
    assert!(EnumLabel::from_sql(&ty, &[0xff, 0xfe, 0xfd]).is_err());
}

#[test]
fn accepts_only_enum_kinds() {
    assert!(EnumLabel::accepts(&enum_type()));
    assert!(!EnumLabel::accepts(&Type::TEXT));
    assert!(!EnumLabel::accepts(&Type::INT4));
}

#[test]
fn money_accepts_only_the_money_type() {
    assert!(Money::accepts(&Type::MONEY));
    assert!(!Money::accepts(&Type::INT8));
    assert!(!Money::accepts(&Type::NUMERIC));
}

#[test]
fn money_decodes_the_same_8_byte_wire_format_as_int8() {
    // MONEY is wire-encoded identically to INT8 (big-endian i64, smallest
    // fractional unit e.g. cents) — 12345 = $123.45.
    let bytes = 12_345_i64.to_be_bytes();
    let money = Money::from_sql(&Type::MONEY, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(money), serde_json::json!(12_345));
}

#[test]
fn money_above_js_safe_integer_becomes_a_string() {
    let above_safe = crate::extract::JS_MAX_SAFE_INTEGER + 1;
    let bytes = above_safe.to_be_bytes();
    let money = Money::from_sql(&Type::MONEY, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(money),
        serde_json::json!(above_safe.to_string())
    );
}
