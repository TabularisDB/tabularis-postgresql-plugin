//! Unit tests for `blob.rs`'s pure helper (`validate_writable_file_path`).
//! `encode_blob`/`encode_blob_full` moved to `crate::utils::blob` (#87) —
//! their tests live in `utils/blob.rs`'s own sibling test module now.
//! Sibling test file per repo convention (`.rules/rust.md` #4/#5) — loaded
//! via `#[cfg(test)] #[path = "blob_tests.rs"] mod blob_tests;`.

mod validate_writable_file_path_tests {
    use super::super::validate_writable_file_path;

    #[test]
    fn rejects_empty_path() {
        assert!(validate_writable_file_path("").is_err());
        assert!(validate_writable_file_path("   ").is_err());
    }

    #[test]
    fn rejects_existing_directory() {
        let err = validate_writable_file_path("/tmp")
            .expect_err("an existing directory must be rejected");
        assert!(err.contains("directory"));
    }

    #[test]
    fn rejects_nonexistent_parent_directory() {
        let err = validate_writable_file_path("/this-dir-should-not-exist-xyz/out.bin")
            .expect_err("a missing parent directory must be rejected");
        assert!(err.contains("parent directory"));
    }

    #[test]
    fn accepts_writable_path_in_existing_directory() {
        // /tmp always exists in the test environment; the target file itself
        // need not exist yet (that's the whole point of a "save to" path).
        assert!(validate_writable_file_path("/tmp/some-file-that-need-not-exist.bin").is_ok());
    }

    #[test]
    fn accepts_relative_path_with_no_directory_component() {
        // A bare filename (no parent) is valid — writes to the plugin's CWD.
        assert!(validate_writable_file_path("output.bin").is_ok());
    }
}
