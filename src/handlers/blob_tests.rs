//! Unit tests for `blob.rs`'s pure encoding helper. Sibling test file per
//! repo convention (`.rules/rust.md` #4/#5) — loaded via
//! `#[cfg(test)] #[path = "blob_tests.rs"] mod blob_tests;`.

use super::encode_blob_full;

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
