//! Shared BLOB (bytea) wire-format encoding, used by both the read path
//! (`extract.rs`'s `Type::BYTEA` arm) and the file-export path
//! (`handlers/blob.rs::fetch_blob_as_data_url`).
//!
//! Matches `src-tauri/src/drivers/common/blob.rs` exactly.

/// Maximum number of bytes base64-encoded into a read/preview response.
/// Larger values are truncated to this many bytes before encoding — the
/// `BLOB:` header still reports the true, untruncated size, so the UI knows
/// the real length without paying to transfer it.
pub const MAX_BLOB_PREVIEW_SIZE: usize = 10_240;

/// Encode raw bytes into the canonical BLOB wire format for the read/preview
/// path: `"BLOB:<total_size>:<mime_type>:<base64_data>"`. Truncates the
/// encoded payload to [`MAX_BLOB_PREVIEW_SIZE`] bytes (MIME is sniffed from
/// that same truncated preview, matching the builtin), while `total_size`
/// always reports the untruncated length.
pub fn encode_blob(data: &[u8]) -> String {
    let total_size = data.len();
    let preview = if total_size > MAX_BLOB_PREVIEW_SIZE {
        &data[..MAX_BLOB_PREVIEW_SIZE]
    } else {
        data
    };

    let mime_type = infer::get(preview)
        .map(|k| k.mime_type())
        .unwrap_or("application/octet-stream");
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, preview);

    format!("BLOB:{}:{}:{}", total_size, mime_type, b64)
}

/// Encode raw bytes into the canonical BLOB wire format, preserving the
/// complete data with no truncation — used by upload/write/export paths so
/// files aren't silently truncated.
pub fn encode_blob_full(data: &[u8]) -> String {
    let mime_type = infer::get(data)
        .map(|k| k.mime_type())
        .unwrap_or("application/octet-stream");
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data);

    format!("BLOB:{}:{}:{}", data.len(), mime_type, b64)
}

#[cfg(test)]
#[path = "blob_tests.rs"]
mod blob_tests;
