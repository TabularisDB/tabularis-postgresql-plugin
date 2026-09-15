//! Unit tests for `binding.rs`. Sibling test file per repo convention
//! (`.rules/rust.md` #4/#5) — loaded via `#[cfg(test)] mod binding_tests;`.

use crate::binding::{bind_pg_value, bind_pk_value, build_pk_map_predicate, BindOptions};
use serde_json::{json, Value};
use std::collections::HashMap;

mod bind_pg_value_tests {
    use super::*;

    #[test]
    fn number_binds_as_bigint_cast() {
        let bound = bind_pg_value(json!(42), 1, &BindOptions::default()).unwrap();
        assert_eq!(bound.sql, "CAST($1 AS bigint)");
        assert!(bound.param.is_some());
    }

    #[test]
    fn float_number_binds_as_double_precision_cast() {
        let bound = bind_pg_value(json!(1.5), 1, &BindOptions::default()).unwrap();
        assert_eq!(bound.sql, "CAST($1 AS double precision)");
    }

    #[test]
    fn bool_binds_natively_without_cast() {
        let bound = bind_pg_value(json!(true), 1, &BindOptions::default()).unwrap();
        assert_eq!(bound.sql, "$1");
        assert!(bound.param.is_some());
    }

    #[test]
    fn null_binds_as_inline_keyword_with_no_parameter() {
        let bound = bind_pg_value(json!(null), 1, &BindOptions::default()).unwrap();
        assert_eq!(bound.sql, "NULL");
        assert!(bound.param.is_none());
    }

    #[test]
    fn array_binds_as_inline_literal_with_no_parameter() {
        let bound = bind_pg_value(json!([1, 2, 3]), 1, &BindOptions::default()).unwrap();
        assert_eq!(bound.sql, "ARRAY[1, 2, 3]");
        assert!(bound.param.is_none());
    }

    #[test]
    fn nested_array_binds_recursively() {
        let bound = bind_pg_value(json!([[1, 2], [3, 4]]), 1, &BindOptions::default()).unwrap();
        assert_eq!(bound.sql, "ARRAY[ARRAY[1, 2], ARRAY[3, 4]]");
    }

    #[test]
    fn string_array_escapes_single_quotes() {
        let bound = bind_pg_value(json!(["it's", "ok"]), 1, &BindOptions::default()).unwrap();
        assert_eq!(bound.sql, "ARRAY['it''s', 'ok']");
    }

    #[test]
    fn object_without_json_column_type_is_rejected() {
        let err = bind_pg_value(json!({"a": 1}), 1, &BindOptions::default()).unwrap_err();
        assert!(err.contains("Cannot bind a JSON object"));
    }

    #[test]
    fn object_with_jsonb_column_type_binds_natively() {
        let options = BindOptions {
            column_type: Some("jsonb"),
            enum_type: None,
            allow_default: false,
            hstore_oid: None,
        };
        let bound = bind_pg_value(json!({"a": 1}), 1, &options).unwrap();
        assert_eq!(bound.sql, "$1");
        assert!(bound.param.is_some());
    }

    #[test]
    fn json_string_value_does_not_take_native_json_path() {
        // A JSON *string* (not object/array) still goes through the generic
        // string cascade even when the column is jsonb — matches the builtin's
        // "value is neither String nor Null" gate.
        let options = BindOptions {
            column_type: Some("jsonb"),
            enum_type: None,
            allow_default: false,
            hstore_oid: None,
        };
        let bound = bind_pg_value(json!("{\"a\":1}"), 1, &options).unwrap();
        assert_eq!(bound.sql, "$1");
    }

    #[test]
    fn default_sentinel_only_honored_when_allow_default_is_true() {
        let options = BindOptions {
            column_type: None,
            enum_type: None,
            allow_default: true,
            hstore_oid: None,
        };
        let bound = bind_pg_value(json!("__USE_DEFAULT__"), 1, &options).unwrap();
        assert_eq!(bound.sql, "DEFAULT");
        assert!(bound.param.is_none());
    }

    #[test]
    fn default_sentinel_ignored_on_insert_allow_default_false() {
        let options = BindOptions {
            column_type: None,
            enum_type: None,
            allow_default: false,
            hstore_oid: None,
        };
        let bound = bind_pg_value(json!("__USE_DEFAULT__"), 1, &options).unwrap();
        // Falls through to the plain TEXT fallback, not treated as DEFAULT.
        assert_eq!(bound.sql, "$1");
    }

    #[test]
    fn blob_wire_format_decodes_to_bytea_before_other_heuristics() {
        // "yv66vg==" is base64 for [0xCA, 0xFE, 0xBA, 0xBE].
        let bound = bind_pg_value(
            json!("BLOB:4:application/octet-stream:yv66vg=="),
            1,
            &BindOptions::default(),
        )
        .unwrap();
        assert_eq!(bound.sql, "$1");
        assert!(bound.param.is_some());
    }

    #[test]
    fn enum_column_binds_with_qualified_cast() {
        let options = BindOptions {
            column_type: None,
            enum_type: Some("\"test_schema\".\"mood\""),
            allow_default: false,
            hstore_oid: None,
        };
        let bound = bind_pg_value(json!("sad"), 1, &options).unwrap();
        assert_eq!(bound.sql, "CAST($1 AS \"test_schema\".\"mood\")");
        assert!(bound.param.is_some());
    }

    #[test]
    fn enum_column_takes_precedence_over_uuid_shape() {
        // A value that happens to look like a UUID must still bind through
        // the enum CAST if the column is an enum — the enum step runs before
        // the UUID-shape heuristic in the cascade.
        let options = BindOptions {
            column_type: None,
            enum_type: Some("\"public\".\"status\""),
            allow_default: false,
            hstore_oid: None,
        };
        let bound =
            bind_pg_value(json!("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"), 1, &options).unwrap();
        assert_eq!(bound.sql, "CAST($1 AS \"public\".\"status\")");
    }

    #[test]
    fn boolean_column_accepts_common_truthy_strings() {
        let options = BindOptions {
            column_type: Some("boolean"),
            enum_type: None,
            allow_default: false,
            hstore_oid: None,
        };
        for truthy in ["true", "t", "yes", "y", "on", "1", "TRUE"] {
            let bound = bind_pg_value(json!(truthy), 1, &options).unwrap();
            assert_eq!(bound.sql, "$1", "input: {truthy}");
        }
    }

    #[test]
    fn boolean_column_rejects_invalid_string() {
        let options = BindOptions {
            column_type: Some("boolean"),
            enum_type: None,
            allow_default: false,
            hstore_oid: None,
        };
        let err = bind_pg_value(json!("maybe"), 1, &options).unwrap_err();
        assert!(err.contains("boolean"));
    }

    #[test]
    fn integer_column_string_binds_as_bigint_cast() {
        let options = BindOptions {
            column_type: Some("integer"),
            enum_type: None,
            allow_default: false,
            hstore_oid: None,
        };
        let bound = bind_pg_value(json!("42"), 1, &options).unwrap();
        assert_eq!(bound.sql, "CAST($1 AS bigint)");
    }

    #[test]
    fn integer_column_rejects_non_numeric_string() {
        let options = BindOptions {
            column_type: Some("integer"),
            enum_type: None,
            allow_default: false,
            hstore_oid: None,
        };
        let err = bind_pg_value(json!("not-a-number"), 1, &options).unwrap_err();
        assert!(err.contains("integer"));
    }

    #[test]
    fn numeric_column_string_binds_as_numeric_cast() {
        let options = BindOptions {
            column_type: Some("numeric"),
            enum_type: None,
            allow_default: false,
            hstore_oid: None,
        };
        let bound = bind_pg_value(json!("12345.67"), 1, &options).unwrap();
        assert_eq!(bound.sql, "CAST($1 AS numeric)");
    }

    #[test]
    fn timestamp_column_string_binds_with_timestamp_cast() {
        let options = BindOptions {
            column_type: Some("timestamp"),
            enum_type: None,
            allow_default: false,
            hstore_oid: None,
        };
        let bound = bind_pg_value(json!("2026-01-15 14:30:00"), 1, &options).unwrap();
        assert_eq!(bound.sql, "CAST($1 AS timestamp)");
    }

    #[test]
    fn timestamptz_column_string_binds_with_timestamptz_cast() {
        let options = BindOptions {
            column_type: Some("timestamptz"),
            enum_type: None,
            allow_default: false,
            hstore_oid: None,
        };
        let bound = bind_pg_value(json!("2026-01-15 14:30:00+00"), 1, &options).unwrap();
        assert_eq!(bound.sql, "CAST($1 AS timestamptz)");
    }

    #[test]
    fn uuid_shaped_string_binds_with_uuid_cast_regardless_of_column_type() {
        let bound = bind_pg_value(
            json!("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"),
            1,
            &BindOptions::default(),
        )
        .unwrap();
        assert_eq!(bound.sql, "CAST($1 AS uuid)");
    }

    #[test]
    fn array_literal_embedded_in_string_is_parsed_as_pg_array() {
        let bound = bind_pg_value(json!("[1,2,3]"), 1, &BindOptions::default()).unwrap();
        assert_eq!(bound.sql, "ARRAY[1, 2, 3]");
        assert!(bound.param.is_none());
    }

    #[test]
    fn plain_string_falls_through_to_text_binding() {
        let bound = bind_pg_value(json!("hello world"), 1, &BindOptions::default()).unwrap();
        assert_eq!(bound.sql, "$1");
        assert!(bound.param.is_some());
    }

    #[test]
    fn hstore_object_bound_as_value_with_correct_type_name() {
        let options = BindOptions {
            column_type: Some("hstore"),
            enum_type: None,
            allow_default: false,
            hstore_oid: Some(16_500),
        };
        let bound = bind_pg_value(json!({"key": "value", "other": "thing"}), 1, &options).unwrap();

        assert_eq!(bound.sql, "$1");
        let (_, pg_type) = bound.param.unwrap();
        assert_eq!(pg_type.name(), "hstore");
        assert_eq!(pg_type.oid(), 16_500);
    }

    #[test]
    fn hstore_object_with_null_value_bound_correctly() {
        let options = BindOptions {
            column_type: Some("hstore"),
            enum_type: None,
            allow_default: false,
            hstore_oid: Some(16_500),
        };
        let bound = bind_pg_value(json!({"key": null}), 1, &options).unwrap();

        assert_eq!(bound.sql, "$1");
        assert!(bound.param.is_some());
    }

    #[test]
    fn hstore_null_value_stays_sql_null() {
        let options = BindOptions {
            column_type: Some("hstore"),
            enum_type: None,
            allow_default: false,
            hstore_oid: Some(16_500),
        };
        let bound = bind_pg_value(Value::Null, 1, &options).unwrap();

        assert_eq!(bound.sql, "NULL");
        assert!(bound.param.is_none());
    }

    #[test]
    fn hstore_json_encoded_string_is_accepted_as_a_fallback() {
        // The plain-text cell editor doesn't distinguish hstore from other
        // types, so it may round-trip a value as a JSON-encoded string.
        let options = BindOptions {
            column_type: Some("hstore"),
            enum_type: None,
            allow_default: false,
            hstore_oid: Some(16_500),
        };
        let bound = bind_pg_value(json!("{\"key\": \"value\"}"), 1, &options).unwrap();

        assert_eq!(bound.sql, "$1");
        assert!(bound.param.is_some());
    }

    #[test]
    fn hstore_non_string_value_in_object_returns_clear_error() {
        let options = BindOptions {
            column_type: Some("hstore"),
            enum_type: None,
            allow_default: false,
            hstore_oid: Some(16_500),
        };
        let err = bind_pg_value(json!({"key": 42}), 1, &options).unwrap_err();

        assert!(err.contains("key"));
        assert!(err.contains("string or null"));
    }

    #[test]
    fn hstore_non_object_value_returns_clear_error() {
        let options = BindOptions {
            column_type: Some("hstore"),
            enum_type: None,
            allow_default: false,
            hstore_oid: Some(16_500),
        };
        let err = bind_pg_value(json!(42), 1, &options).unwrap_err();

        assert!(err.contains("JSON object"));
    }

    #[test]
    fn hstore_object_without_resolved_oid_returns_clear_error() {
        let options = BindOptions {
            column_type: Some("hstore"),
            enum_type: None,
            allow_default: false,
            hstore_oid: None,
        };
        let err = bind_pg_value(json!({"key": "value"}), 1, &options).unwrap_err();

        assert!(err.contains("hstore"));
    }
}

mod bind_pk_value_tests {
    use super::*;

    #[test]
    fn integer_pk_binds_as_bigint_cast() {
        let bound = bind_pk_value(&json!(42), 1, None).unwrap();
        assert_eq!(bound.sql, "CAST($1 AS bigint)");
    }

    #[test]
    fn uuid_string_pk_binds_natively_when_column_type_confirmed_uuid() {
        let bound = bind_pk_value(
            &json!("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"),
            1,
            Some("uuid"),
        )
        .unwrap();
        assert_eq!(bound.sql, "$1");
    }

    #[test]
    fn uuid_shaped_string_pk_binds_as_text_when_column_type_is_not_uuid() {
        // Stricter than the general bind_pg_value cascade: a uuid-*shaped*
        // string targeting a confirmed non-uuid column must bind as TEXT.
        let bound = bind_pk_value(
            &json!("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"),
            1,
            Some("varchar"),
        )
        .unwrap();
        assert_eq!(bound.sql, "$1");
        // (still bound as TEXT — no CAST — since the column type is known
        // and confirmed not to be uuid)
    }

    #[test]
    fn integer_shaped_string_pk_binds_as_bigint_when_column_type_confirmed_integer() {
        let bound = bind_pk_value(&json!("42"), 1, Some("integer")).unwrap();
        assert_eq!(bound.sql, "CAST($1 AS bigint)");
    }

    #[test]
    fn plain_string_pk_falls_back_to_text() {
        let bound = bind_pk_value(&json!("abc"), 1, None).unwrap();
        assert_eq!(bound.sql, "$1");
    }

    #[test]
    fn object_pk_is_rejected() {
        let err = bind_pk_value(&json!({"a": 1}), 1, None).unwrap_err();
        assert!(err.contains("Unsupported PK type"));
    }

    // Keyless tables identify rows by every column, so the WHERE predicate
    // can target numeric/temporal columns whose values arrive as JSON
    // strings (numeric serializes as string to preserve arbitrary
    // precision). Ported from the builtin driver's parity fix
    // (TabularisDB/tabularis#618): a plain TEXT bind trips SQLSTATE 42883,
    // "operator does not exist: numeric = text". These mirror #618's own
    // test cases for `build_pk_predicate`, adapted to `bind_pk_value`'s
    // signature.

    #[test]
    fn numeric_column_string_value_casts_to_numeric() {
        let bound = bind_pk_value(&json!("1500.00"), 2, Some("numeric")).unwrap();
        assert_eq!(bound.sql, "CAST($2 AS numeric)");
        let (_, pg_type) = bound.param.unwrap();
        assert_eq!(pg_type, tokio_postgres::types::Type::NUMERIC);
    }

    #[test]
    fn double_precision_column_string_value_casts_to_double() {
        let bound = bind_pk_value(&json!("1.5"), 1, Some("double precision")).unwrap();
        assert_eq!(bound.sql, "CAST($1 AS double precision)");
        let (_, pg_type) = bound.param.unwrap();
        assert_eq!(pg_type, tokio_postgres::types::Type::FLOAT8);
    }

    #[test]
    fn numeric_column_unparsable_string_is_rejected() {
        assert!(bind_pk_value(&json!("abc"), 1, Some("numeric")).is_err());
    }

    #[test]
    fn timestamp_column_string_value_casts_through_text() {
        let bound = bind_pk_value(
            &json!("2024-05-01 10:30:00"),
            3,
            Some("timestamp without time zone"),
        )
        .unwrap();
        assert_eq!(bound.sql, "CAST($3 AS timestamp)");
        let (_, pg_type) = bound.param.unwrap();
        assert_eq!(pg_type, tokio_postgres::types::Type::TEXT);
    }

    // Keyless tables identify rows by every column, so a `pk_map` entry can
    // legitimately be `null` (a column whose value is NULL). `= NULL` never
    // matches in SQL — the predicate must be `IS NULL`, and no parameter is
    // bound so the placeholder index is not consumed. Mirrors the builtin
    // driver's `build_pk_predicate` `Null` arm
    // (`TabularisDB/tabularis` `binding.rs`).

    #[test]
    fn null_pk_binds_as_is_null() {
        let bound = bind_pk_value(&Value::Null, 1, None).unwrap();
        assert_eq!(bound.sql, "IS NULL");
        assert!(
            bound.param.is_none(),
            "IS NULL must bind no parameter so the placeholder index is not consumed"
        );
    }
}

mod build_pk_map_predicate_tests {
    use super::*;

    fn empty_types() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn null_pk_entry_emits_is_null_predicate_without_consuming_placeholder() {
        // A keyless-table row with a NULL column: `{"a": 1, "b": null}` must
        // produce `"a" = CAST($1 AS bigint) AND "b" IS NULL` with exactly one
        // parameter. The builtin builds this; the plugin previously errored
        // with "Unsupported PK type" (#78).
        let mut pk_map = serde_json::Map::new();
        pk_map.insert("a".to_string(), json!(1));
        pk_map.insert("b".to_string(), Value::Null);
        let types = empty_types();

        let (predicate, params) = build_pk_map_predicate(&pk_map, &types, 1).unwrap();

        // Keys are sorted alphabetically: "a" before "b". The number binds as
        // a bigint cast (one param); the null binds as `IS NULL` (no param).
        assert_eq!(predicate, r#""a" = CAST($1 AS bigint) AND "b" IS NULL"#);
        assert_eq!(params.len(), 1, "IS NULL must not consume a placeholder");
    }

    #[test]
    fn null_pk_entry_alone_emits_is_null_with_no_parameters() {
        let mut pk_map = serde_json::Map::new();
        pk_map.insert("nullable_col".to_string(), Value::Null);

        let (predicate, params) = build_pk_map_predicate(&pk_map, &empty_types(), 1).unwrap();

        assert_eq!(predicate, r#""nullable_col" IS NULL"#);
        assert!(params.is_empty(), "IS NULL binds no parameters");
    }
}
