//! Unit tests for `blob.rs`'s pure encoding helper. Sibling test file per
//! repo convention (`.rules/rust.md` #4/#5) — loaded via
//! `#[cfg(test)] #[path = "blob_tests.rs"] mod blob_tests;`.

use super::{encode_blob_full, validate_writable_file_path};

#[test]
fn encodes_size_mime_and_base64() {
    // 4 bytes (0xCA 0xFE 0xBA 0xBE) — not a recognized magic-byte format, so
    // infer falls back to application/octet-stream.
    let bytes = [0xCA, 0xFE, 0xBA, 0xBE];
    let wire = encode_blob_full(&bytes);
    assert_eq!(wire, "BLOB:4:application/octet-stream:yv66vg==");
}

#[test]
fn empty_input_encodes_zero_size() {
    let wire = encode_blob_full(&[]);
    assert_eq!(wire, "BLOB:0:application/octet-stream:");
}

#[test]
fn sniffs_recognized_magic_bytes() {
    // PNG signature.
    let bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let wire = encode_blob_full(&bytes);
    assert!(wire.starts_with("BLOB:8:image/png:"));
}

mod validate_writable_file_path_tests {
    use super::validate_writable_file_path;

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
