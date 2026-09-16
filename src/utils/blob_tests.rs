//! Unit tests for `blob.rs`'s BLOB wire-format encoders. Sibling test file
//! per repo convention (`.rules/rust.md` #4/#5) — loaded via
//! `#[cfg(test)] #[path = "blob_tests.rs"] mod blob_tests;`.

use super::{encode_blob, encode_blob_full, MAX_BLOB_PREVIEW_SIZE};

mod encode_blob_tests {
    use super::*;

    #[test]
    fn encodes_size_mime_and_base64() {
        // 4 bytes (0xCA 0xFE 0xBA 0xBE) — not a recognized magic-byte format,
        // so infer falls back to application/octet-stream.
        let bytes = [0xCA, 0xFE, 0xBA, 0xBE];
        let wire = encode_blob(&bytes);
        assert_eq!(wire, "BLOB:4:application/octet-stream:yv66vg==");
    }

    #[test]
    fn empty_input_encodes_zero_size() {
        let wire = encode_blob(&[]);
        assert_eq!(wire, "BLOB:0:application/octet-stream:");
    }

    #[test]
    fn sniffs_recognized_magic_bytes() {
        // PNG signature.
        let bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let wire = encode_blob(&bytes);
        assert!(wire.starts_with("BLOB:8:image/png:"));
    }

    #[test]
    fn small_blob_under_preview_cap_encodes_in_full() {
        let bytes = vec![0x42u8; 100];
        let wire = encode_blob(&bytes);
        let header = format!("BLOB:{}:application/octet-stream:", bytes.len());
        assert!(wire.starts_with(&header));
        let b64_payload = &wire[header.len()..];
        let decoded =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64_payload)
                .unwrap();
        assert_eq!(
            decoded, bytes,
            "under the cap, the full payload must round-trip"
        );
    }

    #[test]
    fn large_blob_reports_true_size_but_truncates_payload_to_preview_cap() {
        // 20 KB input -- outlasts MAX_BLOB_PREVIEW_SIZE (10 KB), per #87.
        let total_size = 20 * 1024;
        let bytes = vec![0x41u8; total_size];
        let wire = encode_blob(&bytes);

        let header_prefix = format!("BLOB:{}:", total_size);
        assert!(
            wire.starts_with(&header_prefix),
            "the BLOB: header must report the TRUE total size (20480), not the truncated \
             preview size, so the UI knows the real length: {wire}"
        );

        let mime_and_b64 = &wire[header_prefix.len()..];
        let (_, b64_payload) = mime_and_b64.split_once(':').expect("mime:base64 shape");
        let decoded =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64_payload)
                .unwrap();
        assert_eq!(
            decoded.len(),
            MAX_BLOB_PREVIEW_SIZE,
            "the base64 payload must only cover the first {MAX_BLOB_PREVIEW_SIZE} bytes, \
             not the full 20480-byte input"
        );
        assert_eq!(decoded, bytes[..MAX_BLOB_PREVIEW_SIZE]);
    }

    #[test]
    fn blob_exactly_at_preview_cap_is_not_truncated() {
        let bytes = vec![0x99u8; MAX_BLOB_PREVIEW_SIZE];
        let wire = encode_blob(&bytes);
        let header_prefix = format!("BLOB:{}:", MAX_BLOB_PREVIEW_SIZE);
        assert!(wire.starts_with(&header_prefix));
        let mime_and_b64 = &wire[header_prefix.len()..];
        let (_, b64_payload) = mime_and_b64.split_once(':').unwrap();
        let decoded =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64_payload)
                .unwrap();
        assert_eq!(
            decoded.len(),
            MAX_BLOB_PREVIEW_SIZE,
            "the boundary itself must not truncate"
        );
    }
}

mod encode_blob_full_tests {
    use super::*;

    #[test]
    fn encodes_size_mime_and_base64() {
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
        let bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let wire = encode_blob_full(&bytes);
        assert!(wire.starts_with("BLOB:8:image/png:"));
    }

    #[test]
    fn large_blob_is_never_truncated() {
        // Unlike encode_blob, the full-fidelity encoder must preserve every
        // byte regardless of size -- this is what the file-export path
        // (fetch_blob_as_data_url) relies on to not silently corrupt files.
        let total_size = 20 * 1024;
        let bytes = vec![0x41u8; total_size];
        let wire = encode_blob_full(&bytes);

        let header_prefix = format!("BLOB:{}:", total_size);
        let mime_and_b64 = &wire[header_prefix.len()..];
        let (_, b64_payload) = mime_and_b64.split_once(':').unwrap();
        let decoded =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64_payload)
                .unwrap();
        assert_eq!(decoded, bytes, "encode_blob_full must never truncate");
    }
}
