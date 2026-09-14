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
use std::collections::HashMap;
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

fn hstore_type() -> Type {
    Type::new(
        "hstore".to_string(),
        16_432,
        Kind::Simple,
        "public".to_string(),
    )
}

/// Builds the HSTORE wire format: 4-byte big-endian entry count, then per
/// entry a 4-byte key length + key bytes, and a 4-byte value length (-1 for
/// NULL) + value bytes. Matches `postgres_protocol::types::hstore_from_sql`,
/// which `HashMap<String, Option<String>>`'s `FromSql` impl delegates to.
fn hstore_wire_bytes(entries: &[(&str, Option<&str>)]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(entries.len() as i32).to_be_bytes());
    for (key, value) in entries {
        buf.extend_from_slice(&(key.len() as i32).to_be_bytes());
        buf.extend_from_slice(key.as_bytes());
        match value {
            Some(v) => {
                buf.extend_from_slice(&(v.len() as i32).to_be_bytes());
                buf.extend_from_slice(v.as_bytes());
            }
            None => buf.extend_from_slice(&(-1_i32).to_be_bytes()),
        }
    }
    buf
}

#[test]
fn hstore_type_is_matched_by_name_not_a_well_known_oid() {
    // hstore is an extension type with no fixed OID (#68/#69) — dispatch in
    // extract.rs must match on `ty.name()`, not a `Type::` constant.
    assert_eq!(hstore_type().name(), "hstore");
    assert!(HashMap::<String, Option<String>>::accepts(&hstore_type()));
}

#[test]
fn hstore_decodes_to_a_json_object_with_string_and_null_values() {
    let bytes = hstore_wire_bytes(&[("comment", Some("This is a test")), ("s156", Some("1"))]);
    let map = HashMap::<String, Option<String>>::from_sql(&hstore_type(), &bytes).unwrap();
    let json = serde_json::to_value(map).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"comment": "This is a test", "s156": "1"})
    );
}

#[test]
fn hstore_null_value_decodes_to_json_null_not_a_dropped_key() {
    let bytes = hstore_wire_bytes(&[("key", None)]);
    let map = HashMap::<String, Option<String>>::from_sql(&hstore_type(), &bytes).unwrap();
    let json = serde_json::to_value(map).unwrap();
    assert_eq!(json, serde_json::json!({"key": null}));
}

#[test]
fn empty_hstore_decodes_to_an_empty_json_object() {
    let bytes = hstore_wire_bytes(&[]);
    let map = HashMap::<String, Option<String>>::from_sql(&hstore_type(), &bytes).unwrap();
    let json = serde_json::to_value(map).unwrap();
    assert_eq!(json, serde_json::json!({}));
}
