//! Unit tests for `binding.rs`. Sibling test file per repo convention
//! (`.rules/rust.md` #4/#5) — loaded via `#[cfg(test)] mod binding_tests;`.

use crate::binding::{bind_pg_value, bind_pk_value, BindOptions};
use serde_json::json;

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
        };
        let bound = bind_pg_value(
            json!("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"),
            1,
            &options,
        )
        .unwrap();
        assert_eq!(bound.sql, "CAST($1 AS \"public\".\"status\")");
    }

    #[test]
    fn boolean_column_accepts_common_truthy_strings() {
        let options = BindOptions {
            column_type: Some("boolean"),
            enum_type: None,
            allow_default: false,
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
}
