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

use crate::extract::{
    ArrayValue, BitOrVarBit, Cid, Circle, CompositeValue, EnumLabel, GtsVector, JsonPath, Line,
    Lseg, MacAddr8, Money, MultirangeValue, Path, PgBox, PgBrinBloomSummary, PgDependencies,
    PgHalfVector, PgLsn, PgMcvList, PgNdistinct, PgNodeTree, PgSparseVector, PgVector, Point,
    Polygon, RefCursor, RegClass, RegProc, RegType, Tid, TsQuery, TsVector,
    TxidSnapshotOrPgSnapshot, Xid, Xid8, Xml,
};
use std::collections::HashMap;
use tokio_postgres::types::{Field, FromSql, Kind, Type};

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

// Coverage for a gap found during a thoroughness pass on #82: the array
// decoder (`extract_element_from_bytes`) is a *separate* per-element
// dispatch table from the scalar dispatch (`extract_simple_kind`) — adding
// a type to one does not automatically cover it in the other. The scalar
// fix alone left `xid[]`/`macaddr8[]`/`bit[]`/`regclass[]` (and every other
// new #82 type) still decoding every array element to `null`, confirmed
// live against a real PostgreSQL instance before this fix.

#[test]
fn xid_array_decodes_each_element_as_a_number() {
    let ty = array_type(Type::XID);
    let bytes = array_wire_bytes(
        Type::XID.oid(),
        &[Some(&1_u32.to_be_bytes()), Some(&2_u32.to_be_bytes())],
    );
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(array.0, serde_json::json!([1, 2]));
}

#[test]
fn macaddr8_array_decodes_each_element_as_a_formatted_string() {
    let ty = array_type(Type::MACADDR8);
    let addr = [0x08, 0x00, 0x2b, 0x01, 0x02, 0x03, 0x04, 0x05];
    let bytes = array_wire_bytes(Type::MACADDR8.oid(), &[Some(&addr)]);
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(array.0, serde_json::json!(["08:00:2b:01:02:03:04:05"]));
}

#[test]
fn bit_array_decodes_each_element_as_a_bit_string() {
    let ty = array_type(Type::BIT);
    let bits_a = [0, 0, 0, 4, 0b1010_0000]; // B'1010', padded
    let bits_b = [0, 0, 0, 4, 0b0101_0000]; // B'0101', padded
    let bytes = array_wire_bytes(Type::BIT.oid(), &[Some(&bits_a), Some(&bits_b)]);
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(array.0, serde_json::json!(["1010", "0101"]));
}

#[test]
fn regclass_array_decodes_each_element_as_its_oid() {
    let ty = array_type(Type::REGCLASS);
    let bytes = array_wire_bytes(Type::REGCLASS.oid(), &[Some(&1247_u32.to_be_bytes())]);
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(array.0, serde_json::json!([1247]));
}

#[test]
fn xid8_array_decodes_each_element_respecting_the_js_safe_integer_boundary() {
    let ty = array_type(Type::XID8);
    let small = 42_i64.to_be_bytes();
    let large = 9_007_199_254_740_993_i64.to_be_bytes();
    let bytes = array_wire_bytes(Type::XID8.oid(), &[Some(&small), Some(&large)]);
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(array.0, serde_json::json!([42, "9007199254740993"]));
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

// Wire bytes below were captured from a real PostgreSQL 16 instance (not
// hand-derived from the format spec) — see `#82`'s tracking issue: this
// plugin's `extract.rs` had no arm at all for these types, so they
// previously fell to the string-fallback and silently decoded to `null`.

#[test]
fn macaddr8_decodes_all_eight_bytes() {
    // `'08:00:2b:01:02:03:04:05'::macaddr8`
    let bytes = [0x08, 0x00, 0x2b, 0x01, 0x02, 0x03, 0x04, 0x05];
    let v = MacAddr8::from_sql(&Type::MACADDR8, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("08:00:2b:01:02:03:04:05")
    );
}

#[test]
fn macaddr8_rejects_wrong_length() {
    assert!(MacAddr8::from_sql(&Type::MACADDR8, &[1, 2, 3]).is_err());
}

#[test]
fn bit_string_decodes_to_a_string_of_zero_one_characters() {
    // `B'101101101101'::bit(12)` — 4-byte bit count (12), then the packed
    // bits padded to a byte boundary: 0xb6 (10110110) + 0xd0 (1101_0000,
    // only the first 4 bits are real, the trailing zeros are padding).
    let bytes = [0, 0, 0, 12, 0xb6, 0xd0];
    let v = BitOrVarBit::from_sql(&Type::BIT, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("101101101101")
    );
}

#[test]
fn varbit_string_shares_the_bit_decoder() {
    let bytes = [0, 0, 0, 12, 0xb6, 0xd0];
    let v = BitOrVarBit::from_sql(&Type::VARBIT, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("101101101101")
    );
}

#[test]
fn bit_string_with_zero_length_decodes_to_an_empty_string() {
    let bytes = [0, 0, 0, 0];
    let v = BitOrVarBit::from_sql(&Type::BIT, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!(""));
}

#[test]
fn bit_string_exactly_a_byte_multiple_has_no_padding_to_strip() {
    // 8 bits fits exactly one byte — no partial-byte remainder to trim.
    let bytes = [0, 0, 0, 8, 0b1010_1010];
    let v = BitOrVarBit::from_sql(&Type::BIT, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("10101010"));
}

#[test]
fn xid_decodes_as_a_plain_number() {
    // `'123'::xid`
    let bytes = 123_u32.to_be_bytes();
    let v = Xid::from_sql(&Type::XID, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!(123));
}

#[test]
fn cid_decodes_as_a_plain_number() {
    // `'456'::cid`
    let bytes = 456_u32.to_be_bytes();
    let v = Cid::from_sql(&Type::CID, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!(456));
}

#[test]
fn xid_and_cid_accepts_are_distinct_and_do_not_cross_match() {
    // The u32_oid_wrapper! macro substitutes `Type::$pg_type` per invocation
    // — this guards against a copy-paste mixup in the macro expansion
    // routing an XID buffer through Cid's (or vice versa) accepts check.
    assert!(Xid::accepts(&Type::XID));
    assert!(!Xid::accepts(&Type::CID));
    assert!(!Xid::accepts(&Type::OID));
    assert!(Cid::accepts(&Type::CID));
    assert!(!Cid::accepts(&Type::XID));
}

#[test]
fn macaddr8_accepts_rejects_macaddr_and_other_types() {
    assert!(MacAddr8::accepts(&Type::MACADDR8));
    assert!(!MacAddr8::accepts(&Type::MACADDR));
    assert!(!MacAddr8::accepts(&Type::TEXT));
}

#[test]
fn bit_or_varbit_accepts_both_bit_and_varbit_but_nothing_else() {
    assert!(BitOrVarBit::accepts(&Type::BIT));
    assert!(BitOrVarBit::accepts(&Type::VARBIT));
    assert!(!BitOrVarBit::accepts(&Type::TEXT));
    assert!(!BitOrVarBit::accepts(&Type::BOOL));
}

#[test]
fn tid_accepts_rejects_other_types() {
    assert!(Tid::accepts(&Type::TID));
    assert!(!Tid::accepts(&Type::OID));
    assert!(!Tid::accepts(&Type::XID));
}

#[test]
fn xid8_accepts_rejects_int8_despite_sharing_its_wire_format() {
    // XID8 reinterprets INT8's exact wire bytes as unsigned — accepts must
    // still gate on the XID8 OID, not fall open for any 8-byte integer type.
    assert!(Xid8::accepts(&Type::XID8));
    assert!(!Xid8::accepts(&Type::INT8));
}

#[test]
fn tid_decodes_as_a_block_offset_pair_string() {
    // `'(3,7)'::tid` — 4-byte block number, 2-byte offset.
    let bytes = [0, 0, 0, 3, 0, 7];
    let v = Tid::from_sql(&Type::TID, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("(3, 7)"));
}

#[test]
fn tid_rejects_wrong_length() {
    assert!(Tid::from_sql(&Type::TID, &[0, 0, 0, 3]).is_err());
}

#[test]
fn xid8_within_js_safe_range_decodes_as_a_number() {
    let bytes = 42_i64.to_be_bytes();
    let v = Xid8::from_sql(&Type::XID8, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!(42));
}

#[test]
fn xid8_above_js_safe_integer_becomes_a_string() {
    // `'9007199254740993'::xid8` — one past JS_MAX_SAFE_INTEGER (2^53 - 1),
    // captured live to confirm the plugin's XID8 arm and the builtin's
    // agree on the boundary, not just on an in-range value.
    let above_safe = crate::extract::JS_MAX_SAFE_INTEGER as u64 + 2;
    let bytes = (above_safe as i64).to_be_bytes();
    let v = Xid8::from_sql(&Type::XID8, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!(above_safe.to_string())
    );
}

#[test]
fn reg_types_decode_as_their_underlying_oid() {
    // `'now'::regproc`, `'pg_type'::regclass`, `'int4'::regtype` — captured
    // live: these are plain OIDs under the hood (1299, 1247, 23
    // respectively on a stock PostgreSQL 16 instance).
    let regproc = RegProc::from_sql(&Type::REGPROC, &1299_u32.to_be_bytes()).unwrap();
    assert_eq!(serde_json::Value::from(regproc), serde_json::json!(1299));

    let regclass = RegClass::from_sql(&Type::REGCLASS, &1247_u32.to_be_bytes()).unwrap();
    assert_eq!(serde_json::Value::from(regclass), serde_json::json!(1247));

    let regtype = RegType::from_sql(&Type::REGTYPE, &23_u32.to_be_bytes()).unwrap();
    assert_eq!(serde_json::Value::from(regtype), serde_json::json!(23));
}

#[test]
fn reg_type_accepts_rejects_mismatched_types() {
    assert!(RegProc::accepts(&Type::REGPROC));
    assert!(!RegProc::accepts(&Type::REGCLASS));
    assert!(!RegProc::accepts(&Type::OID));
}

// Geometric types — wire bytes below were captured from a real PostgreSQL
// 16 instance (not hand-derived from the format spec), same as the
// BIT/network/system-identifier batch. Before this fix, POINT/LSEG/BOX/
// POLYGON/PATH/LINE/CIRCLE had no dispatch arm at all in extract.rs, so
// they fell to the string-or-null fallback and silently decoded to `null`.

#[test]
fn point_decodes_two_f64_coordinates() {
    // `'(1.5, 2.5)'::point`
    let bytes = [
        63, 248, 0, 0, 0, 0, 0, 0, // 1.5
        64, 4, 0, 0, 0, 0, 0, 0, // 2.5
    ];
    let v = Point::from_sql(&Type::POINT, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("(1.5, 2.5)"));
}

#[test]
fn point_decodes_negative_coordinates() {
    // `'(-1.25, -2.75)'::point`
    let bytes = [
        191, 244, 0, 0, 0, 0, 0, 0, // -1.25
        192, 6, 0, 0, 0, 0, 0, 0, // -2.75
    ];
    let v = Point::from_sql(&Type::POINT, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("(-1.25, -2.75)")
    );
}

#[test]
fn point_rejects_wrong_length() {
    assert!(Point::from_sql(&Type::POINT, &[0; 8]).is_err());
    assert!(Point::from_sql(&Type::POINT, &[0; 24]).is_err());
}

#[test]
fn point_accepts_rejects_other_types() {
    assert!(Point::accepts(&Type::POINT));
    assert!(!Point::accepts(&Type::LSEG));
}

#[test]
fn lseg_decodes_two_consecutive_points() {
    // `'((1,1),(4,4))'::lseg`
    let bytes = [
        63, 240, 0, 0, 0, 0, 0, 0, // 1.0
        63, 240, 0, 0, 0, 0, 0, 0, // 1.0
        64, 16, 0, 0, 0, 0, 0, 0, // 4.0
        64, 16, 0, 0, 0, 0, 0, 0, // 4.0
    ];
    let v = Lseg::from_sql(&Type::LSEG, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("[(1, 1), (4, 4)]")
    );
}

#[test]
fn lseg_rejects_wrong_length() {
    assert!(Lseg::from_sql(&Type::LSEG, &[0; 16]).is_err());
}

#[test]
fn box_decodes_upper_right_and_lower_left_points() {
    // `'((3,3),(1,1))'::box` — PostgreSQL normalizes the corners so the
    // first point is always the upper-right one, regardless of input order.
    let bytes = [
        64, 8, 0, 0, 0, 0, 0, 0, // 3.0
        64, 8, 0, 0, 0, 0, 0, 0, // 3.0
        63, 240, 0, 0, 0, 0, 0, 0, // 1.0
        63, 240, 0, 0, 0, 0, 0, 0, // 1.0
    ];
    let v = PgBox::from_sql(&Type::BOX, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("((3, 3), (1, 1))")
    );
}

#[test]
fn box_rejects_wrong_length() {
    assert!(PgBox::from_sql(&Type::BOX, &[0; 16]).is_err());
}

#[test]
fn polygon_decodes_a_variable_point_count() {
    // `'((0,0),(1,0),(1,1),(0,1))'::polygon`
    let mut bytes = vec![0, 0, 0, 4]; // 4 points
    for (x, y) in [(0.0_f64, 0.0_f64), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
        bytes.extend_from_slice(&x.to_be_bytes());
        bytes.extend_from_slice(&y.to_be_bytes());
    }
    let v = Polygon::from_sql(&Type::POLYGON, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("((0, 0), (1, 0), (1, 1), (0, 1))")
    );
}

#[test]
fn polygon_with_a_single_point_has_no_comma_separator() {
    // PostgreSQL's minimum polygon has 1 point (there is no true "empty
    // polygon" literal) — the join-with-", "-between-elements logic must
    // not emit a leading/trailing separator for a single-element polygon.
    let bytes = [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let v = Polygon::from_sql(&Type::POLYGON, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("((0, 0))"));
}

#[test]
fn polygon_with_zero_points_decodes_to_empty_parens() {
    // Not reachable through a real INSERT (Postgres rejects 0-point
    // polygons), but the decoder is defensive rather than assuming the
    // server always sends a well-formed value.
    let bytes = [0, 0, 0, 0];
    let v = Polygon::from_sql(&Type::POLYGON, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("()"));
}

#[test]
fn polygon_rejects_negative_point_count() {
    let bytes = (-1_i32).to_be_bytes();
    assert!(Polygon::from_sql(&Type::POLYGON, &bytes).is_err());
}

#[test]
fn path_closed_uses_parentheses() {
    // `'((0,0),(1,1),(2,0))'::path` — closed path, flag bit 0 set.
    let mut bytes = vec![1, 0, 0, 0, 3]; // flag=1 (closed), 3 points
    for (x, y) in [(0.0_f64, 0.0_f64), (1.0, 1.0), (2.0, 0.0)] {
        bytes.extend_from_slice(&x.to_be_bytes());
        bytes.extend_from_slice(&y.to_be_bytes());
    }
    let v = Path::from_sql(&Type::PATH, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("((0, 0), (1, 1), (2, 0))")
    );
}

#[test]
fn path_open_uses_square_brackets() {
    // `'[(0,0),(1,1),(2,0)]'::path` — open path, flag bit 0 clear.
    let mut bytes = vec![0, 0, 0, 0, 3]; // flag=0 (open), 3 points
    for (x, y) in [(0.0_f64, 0.0_f64), (1.0, 1.0), (2.0, 0.0)] {
        bytes.extend_from_slice(&x.to_be_bytes());
        bytes.extend_from_slice(&y.to_be_bytes());
    }
    let v = Path::from_sql(&Type::PATH, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("[(0, 0), (1, 1), (2, 0)]")
    );
}

#[test]
fn path_with_a_single_point_has_no_comma_separator() {
    let bytes = [
        1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    let v = Path::from_sql(&Type::PATH, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("((0, 0))"));
}

#[test]
fn path_rejects_negative_point_count() {
    let mut bytes = vec![1];
    bytes.extend_from_slice(&(-1_i32).to_be_bytes());
    assert!(Path::from_sql(&Type::PATH, &bytes).is_err());
}

#[test]
fn line_decodes_three_coefficients() {
    // `'{1,-2,3}'::line`
    let bytes = [
        63, 240, 0, 0, 0, 0, 0, 0, // 1.0
        192, 0, 0, 0, 0, 0, 0, 0, // -2.0
        64, 8, 0, 0, 0, 0, 0, 0, // 3.0
    ];
    let v = Line::from_sql(&Type::LINE, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("{1, -2, 3}"));
}

#[test]
fn line_rejects_wrong_length() {
    assert!(Line::from_sql(&Type::LINE, &[0; 16]).is_err());
}

#[test]
fn circle_decodes_center_point_and_radius() {
    // `'<(1,1),5>'::circle`
    let bytes = [
        63, 240, 0, 0, 0, 0, 0, 0, // center x = 1.0
        63, 240, 0, 0, 0, 0, 0, 0, // center y = 1.0
        64, 20, 0, 0, 0, 0, 0, 0, // radius = 5.0
    ];
    let v = Circle::from_sql(&Type::CIRCLE, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("<(1, 1), 5>"));
}

#[test]
fn circle_rejects_wrong_length() {
    assert!(Circle::from_sql(&Type::CIRCLE, &[0; 16]).is_err());
}

#[test]
fn geometric_types_accepts_do_not_cross_match_each_other() {
    // All seven share the same underlying f64/Point building blocks — this
    // guards against an accepts() copy-paste bug routing one geometric
    // type's wire bytes through another's parser.
    assert!(!Lseg::accepts(&Type::BOX));
    assert!(!PgBox::accepts(&Type::LSEG));
    assert!(!Polygon::accepts(&Type::PATH));
    assert!(!Path::accepts(&Type::POLYGON));
    assert!(!Line::accepts(&Type::CIRCLE));
    assert!(!Circle::accepts(&Type::LINE));
    assert!(!Point::accepts(&Type::CIRCLE));
}

// Array-element coverage for the same seven types (a separate dispatch
// table from the scalar decoder above — see the BIT/network/system-
// identifier batch, where this exact gap was found and fixed after the
// scalar-only fix shipped first).

#[test]
fn point_array_decodes_each_element() {
    let ty = array_type(Type::POINT);
    let p1 = [63, 240, 0, 0, 0, 0, 0, 0, 63, 240, 0, 0, 0, 0, 0, 0]; // (1, 1)
    let p2 = [64, 0, 0, 0, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0]; // (2, 2)
    let bytes = array_wire_bytes(Type::POINT.oid(), &[Some(&p1), Some(&p2)]);
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(array.0, serde_json::json!(["(1, 1)", "(2, 2)"]));
}

#[test]
fn polygon_array_decodes_each_element() {
    let ty = array_type(Type::POLYGON);
    let square = [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]; // single point (0,0)
    let bytes = array_wire_bytes(Type::POLYGON.oid(), &[Some(&square)]);
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(array.0, serde_json::json!(["((0, 0))"]));
}

#[test]
fn circle_array_decodes_each_element() {
    let ty = array_type(Type::CIRCLE);
    let c = [
        63, 240, 0, 0, 0, 0, 0, 0, 63, 240, 0, 0, 0, 0, 0, 0, 64, 20, 0, 0, 0, 0, 0, 0,
    ];
    let bytes = array_wire_bytes(Type::CIRCLE.oid(), &[Some(&c)]);
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(array.0, serde_json::json!(["<(1, 1), 5>"]));
}

// Full-text-search + introspection types — wire bytes below were captured
// from a real PostgreSQL 16 instance (not hand-derived from the format
// spec). Before this fix, none of these had a dispatch arm in extract.rs,
// so they fell to the string-or-null fallback and either silently decoded
// to `null` (most of them) or, for JSONPATH specifically, decoded to a
// *corrupted* string (the fallback's plain-String FromSql happens to
// accept JSONPATH's wire format, but without stripping the leading
// version byte).
//
// ACLITEM is deliberately not covered here: confirmed live that
// PostgreSQL has no binary send function for it at all
// ("no binary output function available for type aclitem", SQLSTATE
// 42883) — the query fails at the server before any client-side
// FromSql ever runs, for both this plugin and the builtin. See the
// doc comment on the (absent) AclItem type in extract.rs.

#[test]
fn xml_decodes_as_plain_utf8_text() {
    // `'<foo>bar</foo>'::xml`
    let bytes = b"<foo>bar</foo>";
    let v = Xml::from_sql(&Type::XML, bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("<foo>bar</foo>")
    );
}

#[test]
fn xml_accepts_rejects_other_types() {
    assert!(Xml::accepts(&Type::XML));
    assert!(!Xml::accepts(&Type::REFCURSOR));
    assert!(!Xml::accepts(&Type::TEXT));
}

#[test]
fn refcursor_decodes_as_plain_utf8_text() {
    let bytes = b"my_cursor_name";
    let v = RefCursor::from_sql(&Type::REFCURSOR, bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("my_cursor_name")
    );
}

#[test]
fn refcursor_accepts_rejects_other_types() {
    assert!(RefCursor::accepts(&Type::REFCURSOR));
    assert!(!RefCursor::accepts(&Type::XML));
}

#[test]
fn pg_node_tree_decodes_as_plain_utf8_text() {
    // Captured from a live `pg_attrdef.adbin` column (a column DEFAULT
    // expression's parsed node tree) — confirmed reachable under the
    // binary protocol, unlike ACLITEM.
    let text = "{CONST :consttype 23 :consttypmod -1 :constcollid 0 \
                 :constlen 4 :constbyval true :constisnull false \
                 :location 45 :constvalue 4 [ 5 0 0 0 0 0 0 0 ]}";
    let v = PgNodeTree::from_sql(&Type::PG_NODE_TREE, text.as_bytes()).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!(text));
}

#[test]
fn json_path_strips_the_leading_version_byte() {
    // `'$.store.book[*].author'::jsonpath` — PostgreSQL normalizes the
    // stored path to quote every key segment. Byte 0 is the version
    // prefix (1), which must NOT appear in the decoded output — that's
    // the exact corruption the string fallback produced before this fix.
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(b"$.\"store\".\"book\"[*].\"author\"");
    let v = JsonPath::from_sql(&Type::JSONPATH, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("$.\"store\".\"book\"[*].\"author\"")
    );
}

#[test]
fn json_path_rejects_an_empty_buffer() {
    assert!(JsonPath::from_sql(&Type::JSONPATH, &[]).is_err());
}

#[test]
fn tsvector_decodes_lexemes_with_positions_and_default_weight() {
    // `to_tsvector('english', 'a fat cat sat on a mat and ate a fat rat')`
    // — 6 distinct lexemes, one ('fat') with two positions. Weight 'D' is
    // the default and must NOT be rendered (matches PostgreSQL's own
    // tsvector text output, which omits the default weight letter).
    let bytes: [u8; 54] = [
        0, 0, 0, 6, 97, 116, 101, 0, 0, 1, 0, 9, 99, 97, 116, 0, 0, 1, 0, 3, 102, 97, 116, 0, 0, 2,
        0, 2, 0, 11, 109, 97, 116, 0, 0, 1, 0, 7, 114, 97, 116, 0, 0, 1, 0, 12, 115, 97, 116, 0, 0,
        1, 0, 4,
    ];
    let v = TsVector::from_sql(&Type::TS_VECTOR, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("'ate':9 'cat':3 'fat':2,11 'mat':7 'rat':12 'sat':4")
    );
}

#[test]
fn tsvector_decodes_a_non_default_weight_letter() {
    // `setweight(to_tsvector('english', 'fat cat'), 'A')` — every lexeme's
    // weight is 'A' here, so both must render the letter (unlike the
    // default-weight test above, where 'D' is always omitted). Weight bits
    // live in the top 2 bits of the position/weight u16: 0xC0 = 0b11______
    // -> weight index 3 -> 'A'.
    let bytes: [u8; 20] = [
        0, 0, 0, 2, 99, 97, 116, 0, 0, 1, 192, 2, 102, 97, 116, 0, 0, 1, 192, 1,
    ];
    let v = TsVector::from_sql(&Type::TS_VECTOR, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("'cat':2A 'fat':1A")
    );
}

#[test]
fn tsvector_decodes_mixed_weights_across_lexemes() {
    // `setweight(..., 'A') || setweight(..., 'B')` — 'cat' carries weight
    // 'B' (weight bits 0b10______ -> index 2), 'fat' carries 'A' (index 3).
    // Exercises every branch of the weight lookup table in one test,
    // beyond the all-default (D) and all-same-non-default (A) cases above.
    let bytes: [u8; 20] = [
        0, 0, 0, 2, 99, 97, 116, 0, 0, 1, 128, 2, 102, 97, 116, 0, 0, 1, 192, 1,
    ];
    let v = TsVector::from_sql(&Type::TS_VECTOR, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("'cat':2B 'fat':1A")
    );
}

#[test]
fn tsvector_with_zero_lexemes_decodes_to_an_empty_string() {
    let bytes = [0, 0, 0, 0];
    let v = TsVector::from_sql(&Type::TS_VECTOR, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!(""));
}

#[test]
fn tsvector_rejects_a_buffer_too_short_for_the_count_header() {
    assert!(TsVector::from_sql(&Type::TS_VECTOR, &[0, 0, 0]).is_err());
}

#[test]
fn tsquery_decodes_a_two_operand_and_expression() {
    // `to_tsquery('english', 'fat & rat')`
    let bytes: [u8; 20] = [
        0, 0, 0, 3, 2, 2, 1, 0, 0, 114, 97, 116, 0, 1, 0, 0, 102, 97, 116, 0,
    ];
    let v = TsQuery::from_sql(&Type::TSQUERY, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("'fat' & 'rat'")
    );
}

// The AND/OR/prefix/weight case above exercises only one shape of the
// recursive tsquery tree. Every remaining operator (NOT, OR, both phrase-
// distance forms), every operand modifier (prefix, weight, prefix+weight
// combined), and the precedence-based parenthesization logic get their
// own dedicated test below — all against wire bytes captured live from
// `to_tsquery('english', ...)`, not hand-derived. This is the highest-risk
// parser in this batch (a stateful recursive binary-tree decoder with
// four distinct operator encodings), so it gets the most exhaustive
// coverage.

#[test]
fn tsquery_decodes_a_not_operand() {
    // `to_tsquery('english', '!fat')` -> `!'fat'`
    let bytes: [u8; 13] = [0, 0, 0, 2, 2, 1, 1, 0, 0, 102, 97, 116, 0];
    let v = TsQuery::from_sql(&Type::TSQUERY, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("!'fat'"));
}

#[test]
fn tsquery_decodes_an_or_expression() {
    // `to_tsquery('english', 'fat | rat')` -> `'fat' | 'rat'`
    let bytes: [u8; 20] = [
        0, 0, 0, 3, 2, 3, 1, 0, 0, 114, 97, 116, 0, 1, 0, 0, 102, 97, 116, 0,
    ];
    let v = TsQuery::from_sql(&Type::TSQUERY, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("'fat' | 'rat'")
    );
}

#[test]
fn tsquery_decodes_adjacent_phrase_operator_as_arrow_no_distance_number() {
    // `to_tsquery('english', 'fat <-> rat')` — distance 1 renders as the
    // bare "<->" arrow, not "<1>".
    let bytes: [u8; 22] = [
        0, 0, 0, 3, 2, 4, 0, 1, 1, 0, 0, 114, 97, 116, 0, 1, 0, 0, 102, 97, 116, 0,
    ];
    let v = TsQuery::from_sql(&Type::TSQUERY, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("'fat' <-> 'rat'")
    );
}

#[test]
fn tsquery_decodes_phrase_operator_with_explicit_distance() {
    // `to_tsquery('english', 'fat <3> rat')` — distance 3 renders as "<3>".
    let bytes: [u8; 22] = [
        0, 0, 0, 3, 2, 4, 0, 3, 1, 0, 0, 114, 97, 116, 0, 1, 0, 0, 102, 97, 116, 0,
    ];
    let v = TsQuery::from_sql(&Type::TSQUERY, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("'fat' <3> 'rat'")
    );
}

#[test]
fn tsquery_decodes_weighted_operand() {
    // `to_tsquery('english', 'fat:A')` -> `'fat':A`
    let bytes: [u8; 11] = [0, 0, 0, 1, 1, 8, 0, 102, 97, 116, 0];
    let v = TsQuery::from_sql(&Type::TSQUERY, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("'fat':A"));
}

#[test]
fn tsquery_decodes_prefix_operand() {
    // `to_tsquery('english', 'fat:*')` -> `'fat':*`
    let bytes: [u8; 11] = [0, 0, 0, 1, 1, 0, 1, 102, 97, 116, 0];
    let v = TsQuery::from_sql(&Type::TSQUERY, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("'fat':*"));
}

#[test]
fn tsquery_decodes_prefix_and_weight_combined() {
    // `to_tsquery('english', 'fat:*A')` -> `'fat':*A` — note the weight
    // letter is appended directly after `*` with no colon when both a
    // prefix marker and a weight are present (differs from the
    // weight-only case, which uses `:A`).
    let bytes: [u8; 11] = [0, 0, 0, 1, 1, 8, 1, 102, 97, 116, 0];
    let v = TsQuery::from_sql(&Type::TSQUERY, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("'fat':*A"));
}

#[test]
fn tsquery_omits_parens_when_and_is_nested_inside_or_at_default_precedence() {
    // `to_tsquery('english', 'fat & cat | rat')` — AND binds tighter than
    // OR by default, so no parens are needed. Captured live: PostgreSQL's
    // own parser produces IDENTICAL bytes for `fat & cat | rat` and
    // `(fat & cat) | rat` (confirmed separately) — the explicit parens in
    // the second form are dropped before the query even reaches the wire,
    // so both inputs exercise this same byte sequence and same expected
    // output.
    let bytes: [u8; 29] = [
        0, 0, 0, 5, 2, 3, 1, 0, 0, 114, 97, 116, 0, 2, 2, 1, 0, 0, 99, 97, 116, 0, 1, 0, 0, 102,
        97, 116, 0,
    ];
    let v = TsQuery::from_sql(&Type::TSQUERY, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("'fat' & 'cat' | 'rat'")
    );
}

#[test]
fn tsquery_adds_parens_when_not_wraps_an_and_expression() {
    // `to_tsquery('english', '!(fat & cat)')` -> the builtin's rendering
    // is `!('fat' & 'cat')` — parens present (this is what distinguishes
    // it from the bare NOT-of-a-single-operand case above), but note this
    // differs from PostgreSQL's own `::text` cast, which additionally
    // pads the parens with spaces: `!( 'fat' & 'cat' )`. That whitespace
    // difference is the builtin's own behavior (verified by reading its
    // source directly — the NOT branch does `format!("!{}", operand)`
    // with no space), not something this port introduces; parity with
    // the builtin, not with psql's `::text` output, is this plugin's
    // contract.
    let bytes: [u8; 22] = [
        0, 0, 0, 4, 2, 1, 2, 2, 1, 0, 0, 99, 97, 116, 0, 1, 0, 0, 102, 97, 116, 0,
    ];
    let v = TsQuery::from_sql(&Type::TSQUERY, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("!('fat' & 'cat')")
    );
}

#[test]
fn tsquery_rejects_a_buffer_too_short_for_the_length_prefix() {
    assert!(TsQuery::from_sql(&Type::TSQUERY, &[0, 0, 0]).is_err());
}

#[test]
fn tsquery_accepts_rejects_tsvector() {
    assert!(TsQuery::accepts(&Type::TSQUERY));
    assert!(!TsQuery::accepts(&Type::TS_VECTOR));
}

#[test]
fn gts_vector_decodes_as_a_blob_string() {
    let bytes = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
    let v = GtsVector::from_sql(&Type::GTS_VECTOR, &bytes).unwrap();
    let serde_json::Value::String(s) = serde_json::Value::from(v) else {
        panic!("expected a string");
    };
    assert!(s.starts_with("BLOB:5:application/octet-stream:"));
}

#[test]
fn gts_vector_rejects_a_buffer_shorter_than_5_bytes() {
    assert!(GtsVector::from_sql(&Type::GTS_VECTOR, &[0; 4]).is_err());
}

#[test]
fn pg_lsn_decodes_as_uppercase_hex_upper_slash_lower() {
    // `'16/B374D848'::pg_lsn`
    let bytes = [0, 0, 0, 22, 179, 116, 216, 72];
    let v = PgLsn::from_sql(&Type::PG_LSN, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("16/B374D848"));
}

#[test]
fn pg_lsn_rejects_wrong_length() {
    assert!(PgLsn::from_sql(&Type::PG_LSN, &[0; 4]).is_err());
}

#[test]
fn pg_snapshot_decodes_xmin_xmax_and_empty_active_xids() {
    // `pg_current_snapshot()` on a connection with no other concurrent
    // transactions — captured live: count=0, xmin=xmax=2679.
    let bytes: [u8; 20] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 10, 119, 0, 0, 0, 0, 0, 0, 10, 119,
    ];
    let v = TxidSnapshotOrPgSnapshot::from_sql(&Type::PG_SNAPSHOT, &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("2679:2679:"));
}

#[test]
fn txid_snapshot_decodes_active_xids_joined_by_comma() {
    let mut bytes = vec![0, 0, 0, 2]; // count = 2
    bytes.extend_from_slice(&100_i64.to_be_bytes()); // xmin
    bytes.extend_from_slice(&200_i64.to_be_bytes()); // xmax
    bytes.extend_from_slice(&150_i64.to_be_bytes()); // active xid 1
    bytes.extend_from_slice(&175_i64.to_be_bytes()); // active xid 2
    let v = TxidSnapshotOrPgSnapshot::from_sql(&Type::TXID_SNAPSHOT, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("100:200:150,175")
    );
}

#[test]
fn txid_snapshot_and_pg_snapshot_share_the_same_accepts() {
    assert!(TxidSnapshotOrPgSnapshot::accepts(&Type::TXID_SNAPSHOT));
    assert!(TxidSnapshotOrPgSnapshot::accepts(&Type::PG_SNAPSHOT));
    assert!(!TxidSnapshotOrPgSnapshot::accepts(&Type::PG_LSN));
}

#[test]
fn txid_snapshot_rejects_a_buffer_too_short_for_the_header() {
    assert!(TxidSnapshotOrPgSnapshot::from_sql(&Type::TXID_SNAPSHOT, &[0; 10]).is_err());
}

#[test]
fn internal_statistics_blob_types_decode_and_accept_correctly() {
    // pg_ndistinct/pg_dependencies bytes captured live from a real
    // pg_statistic_ext_data row (CREATE STATISTICS ... ON a, b). No
    // meaningful text representation, so all five internal-stats types
    // decode via the same opaque-blob pattern as this plugin's existing
    // BYTEA arm.
    let ndistinct_bytes: [u8; 28] = [
        164, 191, 82, 163, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 89, 64, 2, 0, 0, 0, 1, 0, 2, 0,
    ];
    let v = PgNdistinct::from_sql(&Type::PG_NDISTINCT, &ndistinct_bytes).unwrap();
    let serde_json::Value::String(s) = serde_json::Value::from(v) else {
        panic!("expected a string");
    };
    assert!(s.starts_with("BLOB:28:application/octet-stream:"));

    let dependencies_bytes: [u8; 26] = [
        44, 154, 84, 180, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 240, 63, 2, 0, 1, 0, 2, 0,
    ];
    let v = PgDependencies::from_sql(&Type::PG_DEPENDENCIES, &dependencies_bytes).unwrap();
    let serde_json::Value::String(s) = serde_json::Value::from(v) else {
        panic!("expected a string");
    };
    assert!(s.starts_with("BLOB:26:application/octet-stream:"));

    // PG_MCV_LIST and the two BRIN summary types share the identical blob
    // pattern; no live example was practical to capture for the BRIN
    // types (they're only ever materialized inside a BRIN index page, not
    // exposed as an ordinary queryable catalog column) but the decode
    // logic is a byte-identical pass-through, so a synthetic buffer
    // suffices to prove the wiring.
    let synthetic = [1, 2, 3, 4, 5, 6, 7, 8];
    let v = PgMcvList::from_sql(&Type::PG_MCV_LIST, &synthetic).unwrap();
    assert!(matches!(
        serde_json::Value::from(v),
        serde_json::Value::String(_)
    ));
    let v = PgBrinBloomSummary::from_sql(&Type::PG_BRIN_BLOOM_SUMMARY, &synthetic).unwrap();
    assert!(matches!(
        serde_json::Value::from(v),
        serde_json::Value::String(_)
    ));

    assert!(PgNdistinct::accepts(&Type::PG_NDISTINCT));
    assert!(!PgNdistinct::accepts(&Type::PG_DEPENDENCIES));
    assert!(PgDependencies::accepts(&Type::PG_DEPENDENCIES));
    assert!(!PgDependencies::accepts(&Type::PG_NDISTINCT));
    assert!(PgMcvList::accepts(&Type::PG_MCV_LIST));
    assert!(PgBrinBloomSummary::accepts(&Type::PG_BRIN_BLOOM_SUMMARY));
    assert!(!PgBrinBloomSummary::accepts(
        &Type::PG_BRIN_MINMAX_MULTI_SUMMARY
    ));
}

#[test]
fn tsvector_array_decodes_each_element() {
    let ty = array_type(Type::TS_VECTOR);
    let single_lexeme: [u8; 12] = [0, 0, 0, 1, 104, 105, 0, 0, 1, 0, 1, 0]; // 'hi':1
    let bytes = array_wire_bytes(Type::TS_VECTOR.oid(), &[Some(&single_lexeme)]);
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(array.0, serde_json::json!(["'hi':1"]));
}

#[test]
fn pg_lsn_array_decodes_each_element() {
    let ty = array_type(Type::PG_LSN);
    let lsn = [0, 0, 0, 22, 179, 116, 216, 72];
    let bytes = array_wire_bytes(Type::PG_LSN.oid(), &[Some(&lsn)]);
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(array.0, serde_json::json!(["16/B374D848"]));
}

// Final #82 batch: pgvector + Kind::Composite/Domain/Multirange + arrays
// of ranges/composites. Wire bytes below were captured live from a real
// PostgreSQL 16 instance (a pgvector/pgvector:pg16 image for the pgvector
// cases specifically, since the stock postgres:16 image doesn't bundle
// the extension) — not hand-derived from the format spec, same discipline
// as every prior #82 batch.

fn multirange_type(subtype: Type) -> Type {
    Type::new(
        format!("{}multirange", subtype.name()),
        16_500,
        Kind::Multirange(subtype),
        "public".to_string(),
    )
}

#[test]
fn multirange_decodes_two_ranges_joined_by_bare_comma() {
    // `'{[1,5),[10,20)}'::int4multirange` — captured live.
    let bytes: [u8; 46] = [
        0, 0, 0, 2, 0, 0, 0, 17, 2, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 4, 0, 0, 0, 5, 0, 0, 0, 17, 2,
        0, 0, 0, 4, 0, 0, 0, 10, 0, 0, 0, 4, 0, 0, 0, 20,
    ];
    let v = MultirangeValue::from_sql(&multirange_type(Type::INT4), &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("{[1, 5),[10, 20)}")
    );
}

#[test]
fn multirange_with_zero_ranges_decodes_to_empty_braces() {
    let bytes = [0, 0, 0, 0];
    let v = MultirangeValue::from_sql(&multirange_type(Type::INT4), &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("{}"));
}

#[test]
fn multirange_with_a_single_range() {
    // Adapted from the builtin's own `test_single_range_multirange`
    // (`extract/multi_range.rs`) as a cross-check, not just this port's
    // own hand-derived bytes: flag=RANGE_LB_INC (bit 1), bounds [1, 5).
    let mut bytes = vec![0, 0, 0, 1]; // count = 1
    let range = [2u8, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 4, 0, 0, 0, 5]; // [1, 5)
    bytes.extend_from_slice(&(range.len() as i32).to_be_bytes());
    bytes.extend_from_slice(&range);
    let v = MultirangeValue::from_sql(&multirange_type(Type::INT4), &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("{[1, 5)}"));
}

#[test]
fn multirange_with_mixed_bound_inclusivity_across_ranges() {
    // Adapted from the builtin's `test_mixed_bounds`: three ranges with
    // different inclusivity flags on each — proves the per-range flag
    // byte isn't accidentally reused/cached across iterations of the loop.
    const RANGE_LB_INC: u8 = 1 << 1;
    const RANGE_UB_INC: u8 = 1 << 2;
    fn build_range(flag: u8, lower: i32, upper: i32) -> Vec<u8> {
        let mut r = vec![flag];
        r.extend_from_slice(&4i32.to_be_bytes());
        r.extend_from_slice(&lower.to_be_bytes());
        r.extend_from_slice(&4i32.to_be_bytes());
        r.extend_from_slice(&upper.to_be_bytes());
        r
    }
    let r1 = build_range(RANGE_LB_INC, 1, 5); // [1, 5)
    let r2 = build_range(0x00, 10, 20); // (10, 20)
    let r3 = build_range(RANGE_UB_INC, 100, 200); // (100, 200]
    let mut bytes = vec![0, 0, 0, 3]; // count = 3
    for r in [&r1, &r2, &r3] {
        bytes.extend_from_slice(&(r.len() as i32).to_be_bytes());
        bytes.extend_from_slice(r);
    }
    let v = MultirangeValue::from_sql(&multirange_type(Type::INT4), &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("{[1, 5),(10, 20),(100, 200]}")
    );
}

#[test]
fn multirange_accepts_rejects_a_plain_range() {
    assert!(MultirangeValue::accepts(&multirange_type(Type::INT4)));
    assert!(!MultirangeValue::accepts(&Type::INT4_RANGE));
}

#[test]
fn multirange_with_truncated_range_length_prefix_stops_early() {
    // count says 2 ranges, but only 1 fits. Verified this exact scenario
    // against the builtin's own loop structure (`for _ in 0..count - 1 {
    // ...push a comma... } // then the last range separately`, in
    // `extract/multi_range.rs`) before writing this assertion: with
    // count=2, the loop body runs once (for range 0, which succeeds and
    // pushes its trailing comma expecting range 1 to follow), then the
    // "final range" code detects the truncation and closes with `}`
    // immediately — producing a trailing comma before the closing brace.
    // This is the builtin's own real, shared behavior for this specific
    // truncation shape (a range's bytes present but its length prefix
    // missing when it is NOT the very first range), not a bug this port
    // introduced — parity with the builtin is the contract here, not an
    // "improvement" on an edge case the builtin itself doesn't handle
    // more gracefully.
    let mut bytes = vec![0, 0, 0, 2]; // count = 2
    let range = [2u8, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 4, 0, 0, 0, 5];
    bytes.extend_from_slice(&(range.len() as i32).to_be_bytes());
    bytes.extend_from_slice(&range);
    // no second range's bytes follow
    let v = MultirangeValue::from_sql(&multirange_type(Type::INT4), &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("{[1, 5),}"));
}

fn composite_type(name: &str, fields: Vec<Field>) -> Type {
    Type::new(
        name.to_string(),
        16_600,
        Kind::Composite(fields),
        "public".to_string(),
    )
}

/// Builds the composite wire format: 4-byte field count (its value is
/// unused by the decoder — the real field list comes from `Type::fields()`
/// — but PostgreSQL always sends one, so the fixture includes it for
/// realism), then per field a 4-byte type OID + a 4-byte length-prefixed
/// value (-1 length = NULL).
fn composite_wire_bytes(field_values: &[(u32, Option<&[u8]>)]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(field_values.len() as i32).to_be_bytes());
    for (oid, val) in field_values {
        buf.extend_from_slice(&oid.to_be_bytes());
        match val {
            Some(bytes) => {
                buf.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                buf.extend_from_slice(bytes);
            }
            None => buf.extend_from_slice(&(-1_i32).to_be_bytes()),
        }
    }
    buf
}

#[test]
fn composite_decodes_every_field_by_name() {
    // `ROW(1, 2, 'hello')::point3d` where point3d is `(x int, y int, z text)`
    // — captured live.
    let fields = vec![
        Field::new("x".to_string(), Type::INT4),
        Field::new("y".to_string(), Type::INT4),
        Field::new("z".to_string(), Type::TEXT),
    ];
    let bytes = composite_wire_bytes(&[
        (Type::INT4.oid(), Some(&1_i32.to_be_bytes())),
        (Type::INT4.oid(), Some(&2_i32.to_be_bytes())),
        (Type::TEXT.oid(), Some(b"hello")),
    ]);
    let ty = composite_type("point3d", fields);
    let v = CompositeValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!({"x": 1, "y": 2, "z": "hello"})
    );
}

#[test]
fn composite_preserves_a_null_field_at_its_own_position() {
    // `ROW(1, NULL, 'x')::point3d` — captured live. The NULL field must
    // decode to JSON null WITHOUT causing every subsequent field to also
    // become null (that's the truncated-buffer recovery path, a different
    // case, tested separately below).
    let fields = vec![
        Field::new("x".to_string(), Type::INT4),
        Field::new("y".to_string(), Type::INT4),
        Field::new("z".to_string(), Type::TEXT),
    ];
    let bytes = composite_wire_bytes(&[
        (Type::INT4.oid(), Some(&1_i32.to_be_bytes())),
        (Type::INT4.oid(), None),
        (Type::TEXT.oid(), Some(b"x")),
    ]);
    let ty = composite_type("point3d", fields);
    let v = CompositeValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!({"x": 1, "y": null, "z": "x"})
    );
}

#[test]
fn composite_truncated_after_first_field_fills_the_rest_with_null() {
    // Adapted from the builtin's own
    // `test_truncated_composite_fills_remaining_with_nulls`
    // (`extract/composite.rs`): a buffer cut off mid-second-field must
    // decode the first field normally and fill every remaining field
    // (including the one that was mid-read) with null, not error out or
    // panic on the out-of-bounds read.
    let fields = vec![
        Field::new("first".to_string(), Type::INT4),
        Field::new("second".to_string(), Type::INT4),
    ];
    let full = composite_wire_bytes(&[
        (Type::INT4.oid(), Some(&1_i32.to_be_bytes())),
        (Type::INT4.oid(), Some(&2_i32.to_be_bytes())),
    ]);
    let truncated = &full[..16];
    let ty = composite_type("pair", fields);
    let v = CompositeValue::from_sql(&ty, truncated).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!({"first": 1, "second": null})
    );
}

#[test]
fn composite_with_an_empty_buffer_decodes_to_null_not_an_empty_object() {
    // Matches the builtin's `test_empty_buffer_returns_null`: an empty
    // buffer means the composite value itself is NULL (the length-
    // prefixed value framing at the call site already handles this in
    // practice, since a NULL composite column never reaches from_sql at
    // all — but the decoder is defensive about it anyway, matching the
    // builtin's own defensiveness).
    let fields = vec![Field::new("id".to_string(), Type::INT4)];
    let ty = composite_type("single", fields);
    let v = CompositeValue::from_sql(&ty, &[]).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::Value::Null);
}

#[test]
fn composite_with_a_nested_composite_field() {
    // A field whose own type is itself Kind::Composite — proves the
    // recursive dispatch (extract_kind_from_bytes -> Kind::Composite ->
    // extract_composite_fields -> recurse) terminates correctly rather
    // than looping or panicking on nested structures.
    let inner_fields = vec![
        Field::new("a".to_string(), Type::INT4),
        Field::new("b".to_string(), Type::INT4),
    ];
    let inner_ty = composite_type("inner", inner_fields);
    let inner_bytes = composite_wire_bytes(&[
        (Type::INT4.oid(), Some(&10_i32.to_be_bytes())),
        (Type::INT4.oid(), Some(&20_i32.to_be_bytes())),
    ]);

    let outer_fields = vec![
        Field::new("name".to_string(), Type::TEXT),
        Field::new("nested".to_string(), inner_ty),
    ];
    let outer_bytes = composite_wire_bytes(&[
        (Type::TEXT.oid(), Some(b"outer")),
        (16_600, Some(&inner_bytes)),
    ]);
    let outer_ty = composite_type("outer", outer_fields);
    let v = CompositeValue::from_sql(&outer_ty, &outer_bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!({"name": "outer", "nested": {"a": 10, "b": 20}})
    );
}

#[test]
fn composite_with_an_array_typed_field() {
    // A field whose type is Kind::Array — proves extract_kind_from_bytes's
    // Kind::Array arm (added in this batch specifically to cover this
    // case, which previously fell to `extract_simple_from_bytes` and
    // decoded to null) is reachable from inside a composite.
    let int4_array_ty = Type::new(
        "_int4".to_string(),
        1007,
        Kind::Array(Type::INT4),
        "pg_catalog".to_string(),
    );
    let array_bytes = array_wire_bytes(
        Type::INT4.oid(),
        &[Some(&1_i32.to_be_bytes()), Some(&2_i32.to_be_bytes())],
    );

    let fields = vec![Field::new("nums".to_string(), int4_array_ty)];
    let bytes = composite_wire_bytes(&[(1007, Some(&array_bytes))]);
    let ty = composite_type("with_array", fields);
    let v = CompositeValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!({"nums": [1, 2]})
    );
}

#[test]
fn composite_accepts_rejects_a_plain_simple_type() {
    let fields = vec![Field::new("id".to_string(), Type::INT4)];
    let ty = composite_type("single", fields);
    assert!(CompositeValue::accepts(&ty));
    assert!(!CompositeValue::accepts(&Type::INT4));
}

// Arrays of ranges/multirange/composite: a gap found live (int4range[]
// decoded every element to null) during this batch's investigation —
// extract_element_from_bytes (renamed extract_kind_from_bytes internally)
// had no arms for these Kinds, so every array element of these types fell
// to the same "unsupported, return null" path the scalar columns used to
// hit before Kind::Multirange/Composite existed at all.

#[test]
fn int4range_array_decodes_each_element() {
    let ty = array_type(Type::INT4_RANGE);
    let r1 = [2u8, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 4, 0, 0, 0, 5]; // [1, 5)
    let r2 = [2u8, 0, 0, 0, 4, 0, 0, 0, 10, 0, 0, 0, 4, 0, 0, 0, 20]; // [10, 20)
    let bytes = array_wire_bytes(Type::INT4_RANGE.oid(), &[Some(&r1), Some(&r2)]);
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(array.0, serde_json::json!(["[1, 5)", "[10, 20)"]));
}

#[test]
fn composite_array_decodes_each_element() {
    let fields = vec![
        Field::new("x".to_string(), Type::INT4),
        Field::new("y".to_string(), Type::INT4),
    ];
    let point_ty = composite_type("point2d", fields);
    let ty = array_type(point_ty.clone());
    let p1 = composite_wire_bytes(&[
        (Type::INT4.oid(), Some(&1_i32.to_be_bytes())),
        (Type::INT4.oid(), Some(&2_i32.to_be_bytes())),
    ]);
    let p2 = composite_wire_bytes(&[
        (Type::INT4.oid(), Some(&3_i32.to_be_bytes())),
        (Type::INT4.oid(), Some(&4_i32.to_be_bytes())),
    ]);
    let bytes = array_wire_bytes(16_600, &[Some(&p1), Some(&p2)]);
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(
        array.0,
        serde_json::json!([{"x": 1, "y": 2}, {"x": 3, "y": 4}])
    );
}

// pgvector: dynamic OIDs, matched by name (like hstore) rather than a
// `Type::` constant. Wire bytes captured live from pgvector/pgvector:pg16.

fn pgvector_type(name: &str) -> Type {
    Type::new(name.to_string(), 16_700, Kind::Simple, "public".to_string())
}

#[test]
fn pgvector_decodes_three_dimensions() {
    // `'[1,2,3.5]'::vector(3)` — captured live.
    let bytes: [u8; 16] = [0, 3, 0, 0, 63, 128, 0, 0, 64, 0, 0, 0, 64, 96, 0, 0];
    let v = PgVector::from_sql(&pgvector_type("vector"), &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("[1,2,3.5]"));
}

#[test]
fn pgvector_accepts_matches_only_by_name() {
    assert!(PgVector::accepts(&pgvector_type("vector")));
    assert!(!PgVector::accepts(&pgvector_type("halfvec")));
    assert!(!PgVector::accepts(&Type::INT4));
}

#[test]
fn pgvector_rejects_a_buffer_shorter_than_the_header() {
    assert!(PgVector::from_sql(&pgvector_type("vector"), &[0; 2]).is_err());
}

#[test]
fn pgvector_rejects_a_buffer_too_short_for_its_declared_dimension() {
    // dim=3 declared but only 1 float4 worth of payload follows.
    let bytes = [0, 3, 0, 0, 63, 128, 0, 0];
    assert!(PgVector::from_sql(&pgvector_type("vector"), &bytes).is_err());
}

#[test]
fn pg_halfvec_decodes_three_dimensions_via_f16_conversion() {
    // `'[1,2,3.5]'::halfvec(3)` — captured live. 0x3C00/0x4000/0x4300 are
    // the IEEE 754 binary16 encodings of 1.0/2.0/3.5 — verified against
    // the f16_bits_to_f32 algorithm independently before writing this
    // test, not just trusted because the live query happened to match.
    let bytes: [u8; 10] = [0, 3, 0, 0, 60, 0, 64, 0, 67, 0];
    let v = PgHalfVector::from_sql(&pgvector_type("halfvec"), &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("[1,2,3.5]"));
}

#[test]
fn pg_halfvec_decodes_special_values_zero_and_negative() {
    // 0x0000 = +0.0, 0x8000 = -0.0 (renders as "0" either way via Rust's
    // f32 Display), 0xBC00 = -1.0 — exercises the sign-bit and the
    // subnormal/zero branch of f16_bits_to_f32 that the "happy path"
    // 1/2/3.5 values above never touch.
    let bytes: [u8; 8] = [0, 2, 0, 0, 0, 0, 188, 0];
    let v = PgHalfVector::from_sql(&pgvector_type("halfvec"), &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("[0,-1]"));
}

#[test]
fn pg_sparsevec_decodes_and_renders_one_based_indices() {
    // `'{1:1.5,3:2.25}/5'::sparsevec(5)` — captured live. The wire format
    // stores indices 0-based (0 and 2 here); the text form is 1-based (1
    // and 3) — this is the one place pgvector's wire and text
    // representations disagree on indexing, so it gets a dedicated test
    // rather than trusting the +1 arithmetic by inspection.
    let bytes: [u8; 28] = [
        0, 0, 0, 5, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 63, 192, 0, 0, 64, 16, 0, 0,
    ];
    let v = PgSparseVector::from_sql(&pgvector_type("sparsevec"), &bytes).unwrap();
    assert_eq!(
        serde_json::Value::from(v),
        serde_json::json!("{1:1.5,3:2.25}/5")
    );
}

#[test]
fn pg_sparsevec_with_zero_entries_decodes_to_an_empty_body() {
    let bytes: [u8; 12] = [0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0];
    let v = PgSparseVector::from_sql(&pgvector_type("sparsevec"), &bytes).unwrap();
    assert_eq!(serde_json::Value::from(v), serde_json::json!("{}/5"));
}

#[test]
fn pg_sparsevec_rejects_a_buffer_too_short_for_the_header() {
    assert!(PgSparseVector::from_sql(&pgvector_type("sparsevec"), &[0; 8]).is_err());
}

#[test]
fn pgvector_array_decodes_each_element() {
    let ty = array_type(pgvector_type("vector"));
    // dim=1, header(4 bytes) + 1 float4(4 bytes) = 8 bytes per element.
    let v1: [u8; 8] = [0, 1, 0, 0, 63, 128, 0, 0]; // dim=1, value=1.0
    let v2: [u8; 8] = [0, 1, 0, 0, 64, 0, 0, 0]; // dim=1, value=2.0
    let bytes = array_wire_bytes(16_700, &[Some(&v1), Some(&v2)]);
    let array = ArrayValue::from_sql(&ty, &bytes).unwrap();
    assert_eq!(array.0, serde_json::json!(["[1]", "[2]"]));
}
