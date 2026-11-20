//! Receipt identity of mutating requests (G0_DECISIONS #1): lowercase canonical UUID request_id,
//! SHA-256 of canonical JSON of `{method, request-without-request_id}`; sets sorted, operation
//! order kept. Shape-invalid requests never get a key and are never recorded.
use crate::errors::invalid;
use designllm_contracts::{canonical_json, sha256, ChangeSet, DltResult, JobRef};
use serde::Serialize;
use serde_json::Value;

/// Stored in workspace meta; a build with a different value refuses to open the workspace
/// instead of mis-reporting REQUEST_ID_MISMATCH.
pub const REQUEST_HASH_ALG: &str = "sha256:canonical-json:v1:method+request-minus-request_id";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestKey { pub request_id: String, pub method: &'static str, pub content_hash: String }

/// Hyphenated 8-4-4-4-12 hex UUID, case-insensitive input, lowercase canonical form.
pub fn normalize_request_id(id: &str) -> DltResult<String> {
    let lower = id.to_ascii_lowercase();
    let shape = lower.len() == 36 && lower.bytes().enumerate().all(|(i, c)| match i { 8 | 13 | 18 | 23 => c == b'-', _ => c.is_ascii_hexdigit() });
    if !shape || uuid::Uuid::parse_str(&lower).is_err() { return Err(invalid(format!("request_id is not a hyphenated UUID: {id:?}"))); }
    Ok(lower)
}

pub fn key_for<T: Serialize>(method: &'static str, request_id: &str, request: &T) -> DltResult<RequestKey> {
    let request_id = normalize_request_id(request_id)?;
    let mut value = serde_json::to_value(request).map_err(|e| invalid(e.to_string()))?;
    if let Value::Object(map) = &mut value { map.remove("request_id"); }
    crate::caller::fingerprint_actors(&mut value);
    let canonical = canonical_json(&serde_json::json!({ "method": method, "request": value }))?;
    Ok(RequestKey { request_id, method, content_hash: sha256(canonical.as_bytes()) })
}

/// ChangeSet key with the effective job binding and dependency set order normalized.
pub fn change_key(method: &'static str, cs: &ChangeSet, effective_job: Option<&JobRef>) -> DltResult<RequestKey> {
    let mut value = serde_json::to_value(cs).map_err(|e| invalid(e.to_string()))?;
    if let Value::Object(map) = &mut value {
        map.insert("job_ref".into(), serde_json::to_value(effective_job).map_err(|e| invalid(e.to_string()))?);
        if let Some(Value::Array(deps)) = map.get_mut("read_dependencies") {
            let mut keyed: Vec<(String, Value)> = deps.drain(..).map(|d| (canonical_json(&d).unwrap_or_default(), d)).collect();
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
            if keyed.windows(2).any(|w| w[0].0 == w[1].0) { return Err(invalid("read_dependencies contains duplicates")); }
            *deps = keyed.into_iter().map(|(_, d)| d).collect();
        }
    }
    key_for(method, &cs.request_id, &value)
}

/// JSON arrays that represent sets (allowed_intents) must not repeat values; serde would merge
/// them silently into a BTreeSet.
pub fn reject_duplicate_set(params: &Value, field: &str) -> DltResult<()> {
    if let Some(Value::Array(items)) = params.get(field) {
        let mut seen: Vec<String> = items.iter().map(|v| v.to_string()).collect();
        seen.sort();
        if seen.windows(2).any(|w| w[0] == w[1]) { return Err(invalid(format!("{field} contains duplicates"))); }
        if items.is_empty() { return Err(invalid(format!("{field} must not be empty"))); }
    }
    Ok(())
}
