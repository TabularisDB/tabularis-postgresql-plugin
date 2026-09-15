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

use crate::extract::{ArrayValue, EnumLabel, Money};
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

fn array_type(elem: Type) -> Type {
    Type::new(
        format!("_{}", elem.name()),
        16_433,
        Kind::Array(elem),
        "public".to_string(),
    )
}

/// Builds the 1-D Postgres array wire format: 4-byte dimension count,
/// 4-byte has-nulls flag, 4-byte element type OID, one 8-byte
/// (length, lower_bound) dimension header, then each element as a
/// 4-byte length-prefixed value (-1 length = NULL, no bytes follow).
/// Matches `postgres_protocol::types::array_from_sql`, which
/// `ArrayValue::from_sql` parses directly (#72).
fn array_wire_bytes(element_oid: u32, elements: &[Option<&[u8]>]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&1_i32.to_be_bytes()); // dimensions
    buf.extend_from_slice(&0_i32.to_be_bytes()); // has_nulls (unused by our decoder)
    buf.extend_from_slice(&element_oid.to_be_bytes());
    buf.extend_from_slice(&(elements.len() as i32).to_be_bytes()); // dim length
    buf.extend_from_slice(&1_i32.to_be_bytes()); // lower_bound
    for elem in elements {
        match elem {
            Some(bytes) => {
                buf.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                buf.extend_from_slice(bytes);
            }
            None => buf.extend_from_slice(&(-1_i32).to_be_bytes()),
        }
    }
    buf
}

fn empty_array_wire_bytes() -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&0_i32.to_be_bytes()); // dimensions = 0 -> empty
    buf.extend_from_slice(&0_i32.to_be_bytes());
    buf.extend_from_slice(&Type::INT4.oid().to_be_bytes());
    buf
}

#[test]
fn array_value_accepts_only_array_kinds() {
    assert!(ArrayValue::accepts(&array_type(Type::INT4)));
    assert!(!ArrayValue::accepts(&Type::INT4));
    assert!(!ArrayValue::accepts(&enum_type()));
}

#[test]
fn enum_array_decodes_each_element_as_its_label_string() {
    // enum[] has no hardcoded fast-path in extract.rs (no well-known OID),
    // so it must go through the generic per-element decoder (#72) rather
    // than tokio_postgres's Vec<T>: FromSql (which requires one concrete T).
    let ty = array_type(enum_type());
    let bytes = array_wire_bytes(enum_type().oid(), &[Some(b"happy"), Some(b"sad")]);
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(array.0, serde_json::json!(["happy", "sad"]));
}

#[test]
fn enum_array_null_element_becomes_json_null_not_a_dropped_slot() {
    let ty = array_type(enum_type());
    let bytes = array_wire_bytes(enum_type().oid(), &[Some(b"happy"), None]);
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(array.0, serde_json::json!(["happy", null]));
}

#[test]
fn hstore_array_decodes_each_element_as_a_json_object() {
    let ty = array_type(hstore_type());
    let a = hstore_wire_bytes(&[("a", Some("1"))]);
    let b = hstore_wire_bytes(&[("b", Some("2"))]);
    let bytes = array_wire_bytes(hstore_type().oid(), &[Some(&a), Some(&b)]);
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(array.0, serde_json::json!([{"a": "1"}, {"b": "2"}]));
}

#[test]
fn numeric_array_decodes_via_the_extract_simple_from_bytes_fallback() {
    // INT8 isn't one of extract_element_from_bytes's explicit arms, so this
    // exercises its fallback to extract_simple_from_bytes — proving that
    // shared helper (already covered by range tests) also drives
    // array-element decoding correctly for types beyond enum/hstore.
    let ty = array_type(Type::INT8);
    let elem_bytes = 42_i64.to_be_bytes();
    let bytes = array_wire_bytes(Type::INT8.oid(), &[Some(&elem_bytes)]);
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(array.0, serde_json::json!([42]));
}

#[test]
fn empty_array_decodes_to_an_empty_json_array() {
    let ty = array_type(Type::INT4);
    let array = ArrayValue::from_sql(&ty, &empty_array_wire_bytes()).unwrap();
    assert_eq!(array.0, serde_json::json!([]));
}

#[test]
fn multi_dimensional_array_is_rejected_rather_than_misparsed() {
    let ty = array_type(Type::INT4);
    let mut buf = Vec::new();
    buf.extend_from_slice(&2_i32.to_be_bytes()); // dimensions = 2
    buf.extend_from_slice(&0_i32.to_be_bytes());
    buf.extend_from_slice(&Type::INT4.oid().to_be_bytes());
    assert!(ArrayValue::from_sql(&ty, &buf).is_err());
}

#[test]
fn huge_claimed_length_with_truncated_buffer_does_not_attempt_unbounded_allocation() {
    // A malformed/truncated array buffer could claim a huge element count
    // (e.g. i32::MAX) while actually containing far fewer bytes. This must
    // error out cheaply rather than pre-allocating a Vec sized to the
    // claimed (attacker/corruption-controlled) length.
    let ty = array_type(Type::INT4);
    let mut buf = Vec::new();
    buf.extend_from_slice(&1_i32.to_be_bytes()); // dimensions = 1
    buf.extend_from_slice(&0_i32.to_be_bytes());
    buf.extend_from_slice(&Type::INT4.oid().to_be_bytes());
    buf.extend_from_slice(&i32::MAX.to_be_bytes()); // claimed length: ~2.1 billion
    buf.extend_from_slice(&1_i32.to_be_bytes()); // lower_bound
                                                 // No element bytes follow — buffer is truncated relative to the claim.
    let result = ArrayValue::from_sql(&ty, &buf);
    assert!(result.is_err());
}

// --- #73: hardcoded array fast-paths must preserve a NULL element instead
// of nulling out the whole array. extract_value dispatches these types via
// `Vec<Option<T>>: FromSql`, not `Vec<T>: FromSql` (which errors — and
// therefore whole-column-nulls via try_extract's catch-all — the moment any
// element is absent, since plain T has no "missing" representation). These
// tests exercise that FromSql impl directly with hand-built wire bytes,
// reusing `array_wire_bytes` (the same 1-D array wire format `ArrayValue`
// parses) since tokio_postgres's own array decoders parse the identical
// layout.

#[test]
fn int4_array_with_null_element_preserves_the_null_slot() {
    let bytes = array_wire_bytes(Type::INT4.oid(), &[Some(&1_i32.to_be_bytes()), None]);
    let v = Vec::<Option<i32>>::from_sql(&array_type(Type::INT4), &bytes).unwrap();
    assert_eq!(v, vec![Some(1), None]);
}

#[test]
fn int2_array_with_null_element_preserves_the_null_slot() {
    let bytes = array_wire_bytes(Type::INT2.oid(), &[Some(&1_i16.to_be_bytes()), None]);
    let v = Vec::<Option<i16>>::from_sql(&array_type(Type::INT2), &bytes).unwrap();
    assert_eq!(v, vec![Some(1), None]);
}

#[test]
fn int8_array_with_null_element_preserves_the_null_slot() {
    let bytes = array_wire_bytes(Type::INT8.oid(), &[Some(&1_i64.to_be_bytes()), None]);
    let v = Vec::<Option<i64>>::from_sql(&array_type(Type::INT8), &bytes).unwrap();
    assert_eq!(v, vec![Some(1), None]);
}

#[test]
fn text_array_with_null_element_preserves_the_null_slot() {
    let bytes = array_wire_bytes(Type::TEXT.oid(), &[Some(b"a" as &[u8]), None]);
    let v = Vec::<Option<String>>::from_sql(&array_type(Type::TEXT), &bytes).unwrap();
    assert_eq!(v, vec![Some("a".to_string()), None]);
}

#[test]
fn float4_array_with_null_element_preserves_the_null_slot() {
    let bytes = array_wire_bytes(Type::FLOAT4.oid(), &[Some(&1.5_f32.to_be_bytes()), None]);
    let v = Vec::<Option<f32>>::from_sql(&array_type(Type::FLOAT4), &bytes).unwrap();
    assert_eq!(v, vec![Some(1.5), None]);
}

#[test]
fn float8_array_with_null_element_preserves_the_null_slot() {
    let bytes = array_wire_bytes(Type::FLOAT8.oid(), &[Some(&1.5_f64.to_be_bytes()), None]);
    let v = Vec::<Option<f64>>::from_sql(&array_type(Type::FLOAT8), &bytes).unwrap();
    assert_eq!(v, vec![Some(1.5), None]);
}

#[test]
fn bool_array_with_null_element_preserves_the_null_slot() {
    let bytes = array_wire_bytes(Type::BOOL.oid(), &[Some(&[1u8]), None]);
    let v = Vec::<Option<bool>>::from_sql(&array_type(Type::BOOL), &bytes).unwrap();
    assert_eq!(v, vec![Some(true), None]);
}

#[test]
fn array_with_no_null_elements_still_decodes_every_value() {
    // Regression guard: switching from Vec<T> to Vec<Option<T>> must not
    // change behavior for the (much more common) all-present case.
    let bytes = array_wire_bytes(
        Type::INT4.oid(),
        &[
            Some(&1_i32.to_be_bytes()),
            Some(&2_i32.to_be_bytes()),
            Some(&3_i32.to_be_bytes()),
        ],
    );
    let v = Vec::<Option<i32>>::from_sql(&array_type(Type::INT4), &bytes).unwrap();
    assert_eq!(v, vec![Some(1), Some(2), Some(3)]);
}
