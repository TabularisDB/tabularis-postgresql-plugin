//! Unit tests for `ddl.rs`'s pure SQL-builder functions. Sibling test file
//! per repo convention (`.rules/rust.md` #4/#5) — loaded via
//! `#[cfg(test)] #[path = "ddl_tests.rs"] mod ddl_tests;`.

use super::{
    build_add_column_sql, build_alter_column_sql, build_create_foreign_key_sql,
    build_create_index_sql, build_create_table_sql, is_implicit_cast_compatible,
};
use crate::models::ColumnDefinition;

fn column(name: &str, data_type: &str) -> ColumnDefinition {
    ColumnDefinition {
        name: name.to_string(),
        data_type: data_type.to_string(),
        is_nullable: true,
        is_pk: false,
        is_auto_increment: false,
        default_value: None,
    }
}

mod create_table {
    use super::*;

    #[test]
    fn generates_quoted_qualified_table_with_columns() {
        let columns = vec![
            ColumnDefinition {
                name: "id".to_string(),
                data_type: "SERIAL".to_string(),
                is_nullable: false,
                is_pk: true,
                is_auto_increment: true,
                default_value: None,
            },
            column("name", "TEXT"),
        ];
        let sql = build_create_table_sql("users", &columns, "public");
        assert!(sql.contains("CREATE TABLE \"public\".\"users\""));
        assert!(sql.contains("\"id\" SERIAL"));
        assert!(sql.contains("PRIMARY KEY (\"id\")"));
    }

    #[test]
    fn auto_increment_column_skips_not_null_and_default() {
        let columns = vec![ColumnDefinition {
            name: "id".to_string(),
            data_type: "INTEGER".to_string(),
            is_nullable: false,
            is_pk: true,
            is_auto_increment: true,
            default_value: Some("1".to_string()),
        }];
        let sql = build_create_table_sql("t", &columns, "public");
        // is_auto_increment suppresses both NOT NULL and DEFAULT even though
        // is_nullable is false and a default_value is set — matches builtin.
        assert!(!sql.contains("NOT NULL"));
        assert!(!sql.contains("DEFAULT"));
    }

    #[test]
    fn bigint_auto_increment_becomes_bigserial() {
        let columns = vec![ColumnDefinition {
            name: "id".to_string(),
            data_type: "BIGINT".to_string(),
            is_nullable: false,
            is_pk: true,
            is_auto_increment: true,
            default_value: None,
        }];
        let sql = build_create_table_sql("t", &columns, "public");
        assert!(sql.contains("\"id\" BIGSERIAL"));
    }

    #[test]
    fn non_nullable_non_auto_increment_column_gets_not_null() {
        let columns = vec![ColumnDefinition {
            name: "name".to_string(),
            data_type: "TEXT".to_string(),
            is_nullable: false,
            is_pk: false,
            is_auto_increment: false,
            default_value: None,
        }];
        let sql = build_create_table_sql("t", &columns, "public");
        assert!(sql.contains("\"name\" TEXT NOT NULL"));
    }

    #[test]
    fn default_value_is_spliced_in_verbatim() {
        let columns = vec![ColumnDefinition {
            name: "email".to_string(),
            data_type: "VARCHAR(255)".to_string(),
            is_nullable: true,
            is_pk: false,
            is_auto_increment: false,
            default_value: Some("'unknown@example.com'".to_string()),
        }];
        let sql = build_create_table_sql("t", &columns, "public");
        assert!(sql.contains("DEFAULT 'unknown@example.com'"));
    }

    #[test]
    fn no_primary_key_omits_pk_clause() {
        let columns = vec![column("name", "TEXT")];
        let sql = build_create_table_sql("t", &columns, "public");
        assert!(!sql.contains("PRIMARY KEY"));
    }
}

mod add_column {
    use super::*;

    #[test]
    fn generates_alter_table_add_column() {
        let col = ColumnDefinition {
            name: "new_col".to_string(),
            data_type: "INTEGER".to_string(),
            is_nullable: true,
            is_pk: false,
            is_auto_increment: false,
            default_value: Some("0".to_string()),
        };
        let sql = build_add_column_sql("all_types", &col, "test_schema");
        assert!(sql.contains("ALTER TABLE \"test_schema\".\"all_types\" ADD COLUMN \"new_col\" INTEGER"));
        assert!(sql.contains("DEFAULT 0"));
    }
}

mod alter_column {
    use super::*;

    #[test]
    fn rename_only_when_names_differ() {
        let old = column("old_name", "TEXT");
        let new = column("new_name", "TEXT");
        let stmts = build_alter_column_sql("t", &old, &new, "public").unwrap();
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].contains("RENAME COLUMN \"old_name\" TO \"new_name\""));
    }

    #[test]
    fn compatible_type_change_omits_using_clause() {
        let old = column("col_text", "TEXT");
        let new = column("col_text", "VARCHAR(500)");
        let stmts = build_alter_column_sql("t", &old, &new, "public").unwrap();
        assert!(stmts.iter().any(|s| s.contains("TYPE VARCHAR(500)") && !s.contains("USING")));
    }

    #[test]
    fn incompatible_type_change_adds_using_clause() {
        let old = column("col", "TEXT");
        let new = column("col", "INTEGER");
        let stmts = build_alter_column_sql("t", &old, &new, "public").unwrap();
        assert!(stmts.iter().any(|s| s.contains("USING \"col\"::INTEGER")));
    }

    #[test]
    fn nullable_to_not_nullable_sets_not_null() {
        let mut old = column("col", "TEXT");
        old.is_nullable = true;
        let mut new = column("col", "TEXT");
        new.is_nullable = false;
        let stmts = build_alter_column_sql("t", &old, &new, "public").unwrap();
        assert!(stmts.iter().any(|s| s.contains("SET NOT NULL")));
    }

    #[test]
    fn not_nullable_to_nullable_drops_not_null() {
        let mut old = column("col", "TEXT");
        old.is_nullable = false;
        let mut new = column("col", "TEXT");
        new.is_nullable = true;
        let stmts = build_alter_column_sql("t", &old, &new, "public").unwrap();
        assert!(stmts.iter().any(|s| s.contains("DROP NOT NULL")));
    }

    #[test]
    fn default_value_added_sets_default() {
        let old = column("col", "TEXT");
        let mut new = column("col", "TEXT");
        new.default_value = Some("'x'".to_string());
        let stmts = build_alter_column_sql("t", &old, &new, "public").unwrap();
        assert!(stmts.iter().any(|s| s.contains("SET DEFAULT 'x'")));
    }

    #[test]
    fn default_value_removed_drops_default() {
        let mut old = column("col", "TEXT");
        old.default_value = Some("'x'".to_string());
        let new = column("col", "TEXT");
        let stmts = build_alter_column_sql("t", &old, &new, "public").unwrap();
        assert!(stmts.iter().any(|s| s.contains("DROP DEFAULT")));
    }

    #[test]
    fn no_changes_is_an_error() {
        let old = column("col", "TEXT");
        let new = column("col", "TEXT");
        let err = build_alter_column_sql("t", &old, &new, "public").unwrap_err();
        assert_eq!(err, "No changes detected");
    }
}

mod cast_compatibility {
    use super::*;

    #[test]
    fn identical_types_are_compatible() {
        assert!(is_implicit_cast_compatible("TEXT", "TEXT"));
    }

    #[test]
    fn integer_family_is_compatible() {
        assert!(is_implicit_cast_compatible("INTEGER", "BIGINT"));
    }

    #[test]
    fn string_family_is_compatible() {
        assert!(is_implicit_cast_compatible("VARCHAR", "TEXT"));
    }

    #[test]
    fn cross_family_is_incompatible() {
        assert!(!is_implicit_cast_compatible("TEXT", "INTEGER"));
    }
}

mod create_index {
    use super::*;

    #[test]
    fn multi_column_index() {
        let sql = build_create_index_sql(
            "all_types",
            "idx_test",
            &["col_text".to_string(), "col_int".to_string()],
            false,
            "test_schema",
        );
        assert_eq!(
            sql,
            "CREATE INDEX \"idx_test\" ON \"test_schema\".\"all_types\" (\"col_text\", \"col_int\")"
        );
    }

    #[test]
    fn unique_index_adds_unique_keyword() {
        let sql = build_create_index_sql("t", "idx", &["c".to_string()], true, "public");
        assert!(sql.starts_with("CREATE UNIQUE INDEX"));
    }
}

mod create_foreign_key {
    use super::*;

    #[test]
    fn basic_foreign_key_without_actions() {
        let sql = build_create_foreign_key_sql(
            "crud_scratch",
            "fk_test",
            "value",
            "all_types",
            "id",
            None,
            None,
            "test_schema",
        );
        assert_eq!(
            sql,
            "ALTER TABLE \"test_schema\".\"crud_scratch\" ADD CONSTRAINT \"fk_test\" FOREIGN KEY (\"value\") REFERENCES \"test_schema\".\"all_types\" (\"id\")"
        );
    }

    #[test]
    fn on_delete_and_on_update_actions_are_appended() {
        let sql = build_create_foreign_key_sql(
            "t", "fk", "c", "ref_t", "ref_c", Some("CASCADE"), Some("RESTRICT"), "public",
        );
        assert!(sql.ends_with("ON DELETE CASCADE ON UPDATE RESTRICT"));
    }
}
