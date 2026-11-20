//! Typed request parsing that refuses keys the model does not have.
//!
//! serde ignores unknown keys inside nested structs (the contracts deny them only on ChangeSet and
//! Operation), so a request carrying `layout.wrap` would succeed with the key silently gone.
//! `parse_at` deserializes the request, serializes the typed value back and names every key of the
//! request that has no place in that round trip, as JSON pointers. Maps are data and come back
//! with their keys; scalars are never compared, so numbers and nulls cannot cause a refusal.
//!
//! Empty values (contract v1.2, docs/P0_CONTRACT_V1_2.md §3): a key sent as `null`, `[]` or `{}`
//! that is absent from the round trip is accepted. The v1.2 fields are skipped while unset, so an
//! explicit "unset" value never comes back. The cost: a misspelled key whose value is null/[]/{}
//! (e.g. `"colour": null`) is no longer named; it carried no value, so nothing is lost by it.
//!
//! An explicit `false` for a bool skipped while false (P1 `RenderRequest.include_node_frames`)
//! is accepted when the key comes back once it is sent as `true` (every dropped `false` is
//! flipped in one copy and parsed again). A misspelled key sent as `false` is still named.
use crate::errors::{err, invalid, ErrExt};
use designllm_contracts::{Diagnostic, DltError, DltResult, ErrorCode};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

/// Pointers named in one refusal; the message also gives the total.
pub(crate) const MAX_LISTED: usize = 20;

/// `params` as `T`, refused (INVALID_ARGUMENT) when it has keys `T` would drop. `at` is the JSON
/// pointer of `params` inside the whole request ("" for the request itself).
pub(crate) fn parse_at<T: DeserializeOwned + Serialize>(params: &Value, at: &str) -> DltResult<T> {
    let typed = T::deserialize(params).map_err(|e| invalid(if at.is_empty() { format!("invalid params: {e}") } else { format!("invalid params at {at}: {e}") }))?;
    let kept = serde_json::to_value(&typed).map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
    let mut dropped = dropped_keys(params, &kept);
    if dropped.iter().any(|p| is_false(params, p)) { dropped = without_skipped_false::<T>(params, dropped); }
    if !dropped.is_empty() { return Err(refusal(&dropped, at)); }
    Ok(typed)
}

/// Pointers (relative to `sent`) of keys in `sent` without a counterpart in `kept`.
fn dropped_keys(sent: &Value, kept: &Value) -> Vec<String> {
    let mut dropped = Vec::new();
    walk(sent, kept, &mut String::new(), &mut dropped);
    dropped
}

fn is_false(params: &Value, pointer: &str) -> bool { params.pointer(pointer) == Some(&Value::Bool(false)) }

/// Drops from `dropped` the keys sent as `false` that come back as `true` once sent as `true`:
/// bool fields skipped while false. Unknown keys stay ignored by serde and stay dropped.
fn without_skipped_false<T: DeserializeOwned + Serialize>(params: &Value, dropped: Vec<String>) -> Vec<String> {
    let mut flipped = params.clone();
    for pointer in dropped.iter().filter(|p| is_false(params, p)) {
        if let Some(value) = flipped.pointer_mut(pointer) { *value = Value::Bool(true); }
    }
    let back = T::deserialize(&flipped).ok().and_then(|typed| serde_json::to_value(&typed).ok());
    dropped.into_iter().filter(|p| !(is_false(params, p) && back.as_ref().and_then(|b| b.pointer(p)) == Some(&Value::Bool(true)))).collect()
}

fn walk(sent: &Value, kept: &Value, path: &mut String, dropped: &mut Vec<String>) {
    match (sent, kept) {
        (Value::Object(sent), Value::Object(kept)) => {
            for (key, value) in sent {
                let len = path.len();
                path.push('/');
                for c in key.chars() {
                    match c { '~' => path.push_str("~0"), '/' => path.push_str("~1"), c => path.push(c) }
                }
                match kept.get(key) {
                    Some(back) => walk(value, back, path, dropped),
                    // An unset field (skipped when serialized) or a valueless typo; see the module doc.
                    None if is_empty(value) => {}
                    // An AssetRef copied from asset.import/lookup carries its pixel size; derived, dropped.
                    None if key == "image" && is_asset_ref(kept) => {}
                    None => dropped.push(path.clone()),
                }
                path.truncate(len);
            }
        }
        (Value::Array(sent), Value::Array(kept)) => {
            for (index, (value, back)) in sent.iter().zip(kept).enumerate() {
                let len = path.len();
                path.push('/');
                path.push_str(&index.to_string());
                walk(value, back, path, dropped);
                path.truncate(len);
            }
        }
        _ => {}
    }
}

/// Exactly the keys of an AssetRef.
fn is_asset_ref(kept: &serde_json::Map<String, Value>) -> bool {
    kept.len() == 3 && ["sha256", "mime", "bytes"].iter().all(|k| kept.contains_key(*k))
}

/// `null`, `[]` or `{}`: carries no value.
fn is_empty(value: &Value) -> bool {
    match value { Value::Null => true, Value::Array(a) => a.is_empty(), Value::Object(o) => o.is_empty(), _ => false }
}

/// INVALID_ARGUMENT naming at most [`MAX_LISTED`] of the `dropped` pointers (prefixed with `at`), with the total.
fn refusal(dropped: &[String], at: &str) -> DltError {
    let total = dropped.len();
    let listed: Vec<String> = dropped.iter().take(MAX_LISTED).map(|p| format!("{at}{p}")).collect();
    let more = if total > listed.len() { format!(" and {} more", total - listed.len()) } else { String::new() };
    invalid(format!("{total} key(s) are not part of the request model and would be dropped; nothing was done: {}{more}", listed.join(", ")))
        .diags(listed.iter().map(|p| Diagnostic { code: "unknown_field".into(), message: p.clone(), node_id: None, blocking: true, params: Default::default() }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[derive(Debug, Serialize, Deserialize)]
    struct Inner { a: i64, #[serde(default)] b: Option<String> }
    #[derive(Debug, Serialize, Deserialize)]
    struct Outer { inner: Inner, list: Vec<Inner>, map: BTreeMap<String, Inner>, #[serde(default)] free: Value }

    #[test]
    fn pointers_name_every_dropped_key_and_nothing_else() {
        let ok = json!({ "inner": { "a": -1, "b": null }, "list": [{ "a": 0 }], "map": { "k/1": { "a": 2 } }, "free": { "any": [1.5, null] } });
        parse_at::<Outer>(&ok, "").expect("numbers, nulls, map keys and free JSON are kept");
        let bad = json!({ "inner": { "a": 1, "x": 1 }, "list": [{ "a": 0 }, { "a": 1, "y~": 2 }], "map": { "k/1": { "a": 2, "z": 3 } }, "top": true });
        let e = parse_at::<Outer>(&bad, "/change_set").unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        let named: Vec<&str> = e.diagnostics.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(named, ["/change_set/inner/x", "/change_set/list/1/y~0", "/change_set/map/k~11/z", "/change_set/top"]);
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct Skips {
        a: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")] opt: Option<i64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")] list: Vec<i64>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")] map: BTreeMap<String, i64>,
        #[serde(default, skip_serializing_if = "unset")] flag: bool,
    }
    fn unset(value: &bool) -> bool { !*value }
    #[derive(Debug, Serialize, Deserialize)]
    struct Items { items: Vec<Skips> }

    #[test]
    fn an_explicit_false_of_a_bool_skipped_while_false_passes_and_false_typos_are_named() {
        parse_at::<Skips>(&json!({ "a": 1, "flag": false }), "").expect("explicit false of a skipped bool");
        assert!(parse_at::<Skips>(&json!({ "a": 1, "flag": true }), "").unwrap().flag);
        let e = parse_at::<Skips>(&json!({ "a": 1, "flag": false, "colour": false }), "/p").unwrap_err();
        assert_eq!(e.diagnostics.iter().map(|d| d.message.as_str()).collect::<Vec<_>>(), ["/p/colour"]);
        let e = parse_at::<Items>(&json!({ "items": [{ "a": 1, "flag": false }, { "a": 2, "flag": false, "flg": false }] }), "").unwrap_err();
        assert_eq!(e.diagnostics.iter().map(|d| d.message.as_str()).collect::<Vec<_>>(), ["/items/1/flg"]);
        assert!(e.message.starts_with("1 key(s)"), "{}", e.message);
    }

    #[test]
    fn empty_values_of_skipped_fields_pass_and_valueless_typos_are_the_documented_cost() {
        // v1.2 fields are skipped while unset: an explicit null/[]/{} for them never comes back.
        parse_at::<Skips>(&json!({ "a": 1, "opt": null, "list": [], "map": {} }), "").expect("unset v1.2 fields");
        // The cost (module doc): a misspelled key without a value is no longer named.
        parse_at::<Skips>(&json!({ "a": 1, "colour": null, "shadow": [], "extra": {} }), "").expect("valueless typos pass");
        // Any value is still refused, empty or not inside.
        for (bad, pointer) in [(json!({ "a": 1, "colour": 0 }), "/colour"), (json!({ "a": 1, "colour": "" }), "/colour"), (json!({ "a": 1, "colour": false }), "/colour"),
                               (json!({ "a": 1, "shadow": [null] }), "/shadow"), (json!({ "a": 1, "extra": { "x": null } }), "/extra")] {
            let e = parse_at::<Skips>(&bad, "").unwrap_err();
            assert_eq!(e.diagnostics.iter().map(|d| d.message.as_str()).collect::<Vec<_>>(), [pointer], "{bad}");
        }
    }

    #[test]
    fn the_list_is_capped_and_the_total_is_given() {
        let keys: serde_json::Map<String, Value> = (0..30).map(|i| (format!("k{i:02}"), json!(i))).collect();
        let mut inner = json!({ "a": 1 });
        inner.as_object_mut().unwrap().extend(keys);
        let e = parse_at::<Inner>(&inner, "").unwrap_err();
        assert_eq!(e.diagnostics.len(), MAX_LISTED);
        assert!(e.message.starts_with("30 key(s)") && e.message.ends_with(" and 10 more"), "{}", e.message);
    }
}
