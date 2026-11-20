//! Chunked asset retrieval. The core returns `asset.read` chunks of at most 4 MiB (raw bytes); this
//! module assembles them, checks the offset chain and the total length, and verifies the SHA-256 of the
//! whole asset before the bytes are used. A mismatch is INTEGRITY_ERROR and nothing is returned.

use base64::Engine;
use designllm_contracts::{sha256, DltError, DltResult, ErrorCode};
use serde_json::{json, Value};

use crate::mcp::ToolBackend;

/// Chunk size requested from `asset.read`. The core never returns more than 4 MiB per chunk.
pub const ASSET_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

/// A fully assembled and verified asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedAsset {
    pub sha256: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

fn integrity(message: impl Into<String>) -> DltError {
    DltError::new(ErrorCode::IntegrityError, message)
}

/// Fetches `sha256` through `backend` (`asset.read` chunks). Refuses assets larger than `max_bytes`
/// before downloading more than one chunk.
pub fn fetch_asset(backend: &dyn ToolBackend, sha256_hex: &str, max_bytes: u64) -> DltResult<FetchedAsset> {
    let mut offset = 0u64;
    let mut total: Option<u64> = None;
    let mut mime = String::new();
    let mut data: Vec<u8> = Vec::new();
    loop {
        let chunk: Value = backend.call(
            "asset.read",
            json!({ "sha256": sha256_hex, "offset": offset, "max_bytes": ASSET_CHUNK_BYTES }),
        )?;
        let total_bytes = chunk
            .get("total_bytes")
            .and_then(Value::as_u64)
            .ok_or_else(|| integrity("asset.read response has no total_bytes"))?;
        if total_bytes > max_bytes {
            return Err(DltError::invalid(format!(
                "asset of {total_bytes} bytes exceeds the inline limit of {max_bytes} bytes; read it in the runtime with asset.read"
            )));
        }
        match total {
            None => total = Some(total_bytes),
            Some(known) if known != total_bytes => return Err(integrity("asset size changed between chunks")),
            Some(_) => {}
        }
        if chunk.get("offset").and_then(Value::as_u64) != Some(offset) {
            return Err(integrity("asset.read returned an unexpected offset"));
        }
        if let Some(m) = chunk.get("mime").and_then(Value::as_str) {
            mime = m.to_string();
        }
        let encoded = chunk
            .get("base64")
            .and_then(Value::as_str)
            .ok_or_else(|| integrity("asset.read response has no base64 data"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| integrity(format!("asset chunk is not valid base64: {e}")))?;
        let eof = chunk.get("eof").and_then(Value::as_bool).unwrap_or(false);
        if bytes.is_empty() && !eof {
            return Err(integrity("asset.read made no progress"));
        }
        offset += bytes.len() as u64;
        data.extend_from_slice(&bytes);
        if eof {
            break;
        }
    }
    let expected = total.unwrap_or(0);
    if data.len() as u64 != expected {
        return Err(integrity(format!("assembled {} bytes but the asset has {expected}", data.len())));
    }
    let actual = sha256(&data);
    if actual != sha256_hex {
        return Err(integrity("assembled asset does not match the requested sha256"));
    }
    Ok(FetchedAsset { sha256: sha256_hex.to_string(), mime, bytes: data })
}

/// Base64 for MCP `image` and text transports.
pub fn base64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serves the given bytes in chunks of `chunk` bytes, optionally corrupting the advertised hash input.
    struct Serve {
        data: Vec<u8>,
        chunk: usize,
        calls: Mutex<Vec<(u64, u64)>>,
    }

    impl ToolBackend for Serve {
        fn call(&self, method: &str, params: Value) -> DltResult<Value> {
            assert_eq!(method, "asset.read");
            let offset = params["offset"].as_u64().unwrap();
            let max = params["max_bytes"].as_u64().unwrap().min(self.chunk as u64);
            self.calls.lock().unwrap().push((offset, max));
            let start = offset as usize;
            let end = (start + max as usize).min(self.data.len());
            let slice = &self.data[start..end];
            Ok(json!({
                "mime": "image/png",
                "base64": base64_encode(slice),
                "offset": offset,
                "eof": end == self.data.len(),
                "total_bytes": self.data.len(),
            }))
        }
    }

    fn digest(bytes: &[u8]) -> String {
        sha256(bytes)
    }

    #[test]
    fn chunks_are_assembled_and_verified() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let backend = Serve { data: data.clone(), chunk: 4096, calls: Mutex::new(vec![]) };
        let fetched = fetch_asset(&backend, &digest(&data), 1 << 20).expect("fetch");
        assert_eq!(fetched.bytes, data);
        assert_eq!(fetched.mime, "image/png");
        assert_eq!(backend.calls.lock().unwrap().len(), 3, "10000 bytes in 4096-byte chunks");
    }

    #[test]
    fn wrong_hash_is_integrity_error_and_nothing_is_returned() {
        let data = vec![1u8, 2, 3, 4];
        let backend = Serve { data, chunk: 4096, calls: Mutex::new(vec![]) };
        let err = fetch_asset(&backend, &digest(b"other"), 1 << 20).unwrap_err();
        assert_eq!(err.code, ErrorCode::IntegrityError);
    }

    #[test]
    fn oversized_asset_is_refused_after_the_first_chunk_only() {
        let data = vec![7u8; 5000];
        let backend = Serve { data: data.clone(), chunk: 1000, calls: Mutex::new(vec![]) };
        let err = fetch_asset(&backend, &digest(&data), 4000).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn empty_asset_is_one_eof_chunk() {
        let backend = Serve { data: vec![], chunk: 4096, calls: Mutex::new(vec![]) };
        let fetched = fetch_asset(&backend, &digest(&[]), 1024).expect("empty asset");
        assert!(fetched.bytes.is_empty());
    }
}
