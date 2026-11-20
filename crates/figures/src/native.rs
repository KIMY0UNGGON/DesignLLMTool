//! Native figure project (`designllm.figure-project`).
//!
//! The file is canonical JSON: object keys sorted by UTF-8 bytes, no whitespace.
//!
//! ```text
//! {"assets":[ASSET,...],"figure":{...},"format":"designllm.figure-project","format_version":2,"schema_version":1}
//! ASSET = {"bytes":<length>,"data_base64":"<standard base64, padded>","mime":"<mime>","sha256":"<hex>"}
//! ```
//!
//! `figure` is the figure document. `assets` lists every source asset the document names, once each, sorted by
//! sha256 (see [`referenced_hashes`]). Version 2 embeds the bytes of each asset, so the file opens in any workspace.
//! Version 3 is written only when the figure uses a persisted feature key: a plot display mode, a common legend, a visual
//! review, or a diagram math node. It adds `feature_keys`, the sorted keys the figure uses. They must match the figure exactly, and builds without
//! the key refuse the file.
//! On open, every payload is decoded and checked against its declared length and sha256 before anything is written
//! to the asset store. A payload that fails the check is an IntegrityError, and nothing is opened.
//!
//! Size limits, the same as the canvas project: at most [`MAX_EMBEDDED_BYTES`] of embedded source bytes, and at most
//! [`MAX_NATIVE_BYTES`] for the whole file. A save above either limit fails with InvalidArgument and writes nothing.
//! Nothing is truncated.
//!
//! Version 1 (written by earlier builds) has no embedded bytes. Its entries are `{bytes, mime, sha256}`. It opens
//! only when each of its assets is already in the store under the recorded reference. A missing asset fails with
//! NotFound, and a differing record fails with IntegrityError. Reading refuses fields this version does not model,
//! so no data is dropped on the way in.

use std::collections::{BTreeMap, BTreeSet};

use designllm_contracts::base64::{self, DecodeError, PayloadError};
use designllm_contracts::native_envelope::{
    self, FeatureKeyError, FeatureUseError, check_feature_key_list, check_feature_key_presence,
};
use designllm_contracts::{
    AssetRef, AssetStore, DltError, DltResult, ErrorCode, FigureDocument, Node, PanelBody, SCHEMA_VERSION, canonical_json, sha256,
};
use serde_json::{Value, json};

use crate::evidence::PDF_EVIDENCE_ORIGIN;

pub const FORMAT: &str = "designllm.figure-project";
/// Version written for a figure without persisted feature keys. Every referenced source asset is embedded.
pub const FORMAT_VERSION: u64 = 2;
/// Version written for a figure that uses a persisted feature key. Older builds refuse it as newer than they are.
pub const FORMAT_VERSION_FEATURE_KEYS: u64 = 3;
/// Newest version this build reads.
pub const NEWEST_READABLE_VERSION: u64 = FORMAT_VERSION_FEATURE_KEYS;
/// Oldest version this build still reads. Version 1 references its assets by hash only.
pub const OLDEST_READABLE_VERSION: u64 = 1;
/// Persisted feature key of a plot with a display mode (`PlotSpec::display`). Store3 records it in `store_features`.
pub const FEATURE_PLOT_ERROR_KEY: &str = "figure.plot_error_key.v1";
/// Persisted feature key of a figure with a common legend (`FigureDocument::common_legend`).
pub const FEATURE_COMMON_LEGEND: &str = "figure.common_legend.v1";
/// Persisted feature key of a figure with a visual review (`FigureDocument::visual_review`).
pub const FEATURE_VISUAL_REVIEW: &str = "figure.visual_review.v1";
/// Persisted feature key of a figure with a Diagram math node (`NodeExt::math`).
pub const FEATURE_MATH: &str = "figure.math.v1";
/// Every persisted feature key this build reads and writes, in sorted order (a file lists its keys sorted).
pub const PERSISTED_FEATURE_KEYS: [&str; 4] = [FEATURE_COMMON_LEGEND, FEATURE_MATH, FEATURE_PLOT_ERROR_KEY, FEATURE_VISUAL_REVIEW];
pub const MIME: &str = "application/vnd.designllm.figure-project+json";
/// Largest total of embedded source bytes in one project (128 MiB). The canvas project uses the same limit.
pub const MAX_EMBEDDED_BYTES: u64 = native_envelope::NATIVE_MAX_EMBEDDED_BYTES;
/// Largest project file this build writes or reads (256 MiB). The canvas project uses the same limit.
pub const MAX_NATIVE_BYTES: u64 = native_envelope::NATIVE_MAX_FILE_BYTES;

/// Size limits for one project. Product code uses [`Limits::DEFAULT`]. Tests pass smaller values.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) max_embedded: u64,
    pub(crate) max_file: u64,
}

impl Limits {
    pub(crate) const DEFAULT: Limits = Limits { max_embedded: MAX_EMBEDDED_BYTES, max_file: MAX_NATIVE_BYTES };
}

/// Source assets the document names, sorted and unique: panel images, dataset sources, PDF evidence sources,
/// source-coverage sources, and images on diagram nodes. Version 2 embeds each of them.
pub fn referenced_hashes(doc: &FigureDocument) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for p in &doc.panels {
        match &p.body {
            PanelBody::Image(img) => {
                set.insert(img.asset.sha256.clone());
            }
            PanelBody::Diagram(dg) => {
                for node in &dg.nodes {
                    collect_node_asset(node, &mut set);
                }
            }
            PanelBody::Plot(_) => {}
        }
    }
    for ds in &doc.datasets {
        set.insert(ds.original_asset_hash.clone());
    }
    for ev in &doc.evidence {
        // user_statement evidence carries the hash of its own text. That is not a stored asset.
        if ev.origin == PDF_EVIDENCE_ORIGIN {
            set.insert(ev.source_hash.clone());
        }
    }
    for scope in &doc.source_scopes {
        set.insert(scope.source_hash.clone());
    }
    set.into_iter().collect()
}

fn collect_node_asset(node: &Node, set: &mut BTreeSet<String>) {
    if let Some(asset) = &node.asset {
        set.insert(asset.sha256.clone());
    }
    for child in &node.children {
        collect_node_asset(child, set);
    }
}

/// The reference rule of format_version 1: panel images, dataset sources, PDF evidence. Version 1 files list
/// exactly this set, so they are checked against it.
fn referenced_hashes_v1(doc: &FigureDocument) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for p in &doc.panels {
        if let PanelBody::Image(img) = &p.body {
            set.insert(img.asset.sha256.clone());
        }
    }
    for ds in &doc.datasets {
        set.insert(ds.original_asset_hash.clone());
    }
    for ev in &doc.evidence {
        if ev.origin == PDF_EVIDENCE_ORIGIN {
            set.insert(ev.source_hash.clone());
        }
    }
    set.into_iter().collect()
}

/// Persisted feature keys the figure uses, sorted (the order of PERSISTED_FEATURE_KEYS). A plot with a display mode needs
/// `figure.plot_error_key.v1`, a common legend needs `figure.common_legend.v1`, a diagram math node needs `figure.math.v1`,
/// and a visual review needs `figure.visual_review.v1`.
pub fn feature_keys_of(doc: &FigureDocument) -> Vec<&'static str> {
    let uses_display = doc.panels.iter().any(|p| matches!(&p.body, PanelBody::Plot(plot) if plot.display.is_some()));
    // guard.rs accepts math on diagram nodes only (not on their children), so the nodes of each diagram are all there is.
    let uses_math =
        doc.panels.iter().any(|p| matches!(&p.body, PanelBody::Diagram(dg) if dg.nodes.iter().any(|n| n.ext.math.is_some())));
    let mut keys = Vec::new();
    if doc.common_legend.is_some() {
        keys.push(FEATURE_COMMON_LEGEND);
    }
    if uses_math {
        keys.push(FEATURE_MATH);
    }
    if uses_display {
        keys.push(FEATURE_PLOT_ERROR_KEY);
    }
    if doc.visual_review.is_some() {
        keys.push(FEATURE_VISUAL_REVIEW);
    }
    keys
}

/// Encodes the document with every referenced asset embedded. Each referenced asset must be in the store, and its
/// bytes must still match the reference. Nothing is returned unless the whole project is consistent.
pub fn encode(doc: &FigureDocument, assets: &dyn AssetStore) -> DltResult<Vec<u8>> {
    encode_with(doc, assets, Limits::DEFAULT)
}

pub(crate) fn encode_with(doc: &FigureDocument, assets: &dyn AssetStore, limits: Limits) -> DltResult<Vec<u8>> {
    let mut refs: Vec<AssetRef> = Vec::new();
    let mut declared_total: u64 = 0;
    for h in referenced_hashes(doc) {
        let reference = assets
            .lookup(&h)
            .map_err(|e| DltError::new(ErrorCode::NotFound, format!("source asset {h} is missing: {}", e.message)))?;
        declared_total = declared_total.saturating_add(reference.bytes);
        refs.push(reference);
    }
    if declared_total > limits.max_embedded {
        return Err(DltError::invalid(format!(
            "the source assets total {declared_total} bytes, above the {} byte limit for a self-contained project; nothing was written",
            limits.max_embedded
        )));
    }

    let figure = serde_json::to_value(doc).map_err(|e| DltError::invalid(e.to_string()))?;
    let keys = feature_keys_of(doc);
    let version = if keys.is_empty() { FORMAT_VERSION } else { FORMAT_VERSION_FEATURE_KEYS };
    let mut envelope = json!({
        "figure": figure,
        "format": FORMAT,
        "format_version": version,
        "schema_version": SCHEMA_VERSION,
    });
    // Written only when used, so a figure without feature keys keeps its version 2 bytes.
    if !keys.is_empty() {
        envelope["feature_keys"] = json!(keys);
    }
    // "assets" sorts before every other top-level key, so the rest of the canonical object follows it directly.
    let head = canonical_json(&envelope)?;
    let mut out = String::with_capacity(head.len() + (declared_total.div_ceil(3) * 4) as usize + 128 * refs.len());
    out.push_str("{\"assets\":[");
    for (i, reference) in refs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let data = assets.read(reference).map_err(|e| {
            DltError::new(e.code.clone(), format!("source asset {} cannot be read: {}", reference.sha256, e.message))
        })?;
        if data.len() as u64 != reference.bytes || sha256(&data) != reference.sha256 {
            return Err(DltError::new(
                ErrorCode::IntegrityError,
                format!("source asset {} does not match its reference; nothing was written", reference.sha256),
            ));
        }
        push_asset(&mut out, reference, &data)?;
    }
    out.push_str("],");
    out.push_str(&head[1..]);
    if out.len() as u64 > limits.max_file {
        return Err(DltError::invalid(format!(
            "the project file would be {} bytes, above the {} byte limit; nothing was written",
            out.len(),
            limits.max_file
        )));
    }
    Ok(out.into_bytes())
}

/// Writes one asset with its keys in canonical order.
fn push_asset(out: &mut String, reference: &AssetRef, data: &[u8]) -> DltResult<()> {
    out.push_str("{\"bytes\":");
    out.push_str(&reference.bytes.to_string());
    out.push_str(",\"data_base64\":\"");
    base64::encode_into(data, out);
    out.push_str("\",\"mime\":");
    out.push_str(&json_string(&reference.mime)?);
    out.push_str(",\"sha256\":");
    out.push_str(&json_string(&reference.sha256)?);
    out.push('}');
    Ok(())
}

fn json_string(text: &str) -> DltResult<String> {
    serde_json::to_string(text).map_err(|e| DltError::invalid(e.to_string()))
}

/// A project file after its structure, figure model, and payloads were checked. Nothing is stored yet.
pub(crate) struct Parsed {
    pub(crate) doc: FigureDocument,
    /// Sorted by sha256. Version 2 carries the verified bytes. Version 1 carries none.
    pub(crate) assets: Vec<(AssetRef, Option<Vec<u8>>)>,
}

/// Parses and checks a project file without touching any store.
pub(crate) fn parse(bytes: &[u8], limits: Limits) -> DltResult<Parsed> {
    if bytes.len() as u64 > limits.max_file {
        return Err(DltError::invalid(format!("project file is {} bytes; the limit is {} bytes", bytes.len(), limits.max_file)));
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|e| DltError::invalid(format!("project file is not JSON: {e}")))?;
    let obj = value.as_object().ok_or_else(|| DltError::invalid("project file must be a JSON object"))?;
    let allowed = ["format", "format_version", "schema_version", "figure", "assets", "feature_keys"];
    if let Some(k) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(DltError::invalid(format!("project file has unknown field \"{k}\"")));
    }
    if obj.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err(DltError::invalid("project file is not a figure project"));
    }
    let version = obj.get("format_version").and_then(Value::as_u64).unwrap_or(0);
    if version > NEWEST_READABLE_VERSION {
        return Err(DltError::new(ErrorCode::SchemaUnsupported, format!("figure project format_version {version} is newer than this build")));
    }
    if version < OLDEST_READABLE_VERSION {
        return Err(DltError::invalid(format!("figure project format_version {version} is not supported")));
    }
    let schema = obj.get("schema_version").and_then(Value::as_u64).unwrap_or(0);
    if schema > u64::from(SCHEMA_VERSION) {
        return Err(DltError::new(ErrorCode::SchemaUnsupported, format!("schema_version {schema} is newer than this build")));
    }
    let figure_value = obj.get("figure").ok_or_else(|| DltError::invalid("project file has no figure"))?;
    // The declared keys are checked before the figure is read, as canvas native.rs does: a file that uses a key this build
    // does not know is refused by name, whatever else the figure holds.
    let declared = check_feature_keys_declared(obj.get("feature_keys"), version)?;
    let doc: FigureDocument =
        serde_json::from_value(figure_value.clone()).map_err(|e| DltError::invalid(format!("figure rejected: {e}")))?;
    let modelled = serde_json::to_value(&doc).map_err(|e| DltError::invalid(e.to_string()))?;
    if canonical_json(&modelled)? != canonical_json(figure_value)? {
        let reason = model_difference(figure_value, &modelled, "").unwrap_or_else(|| "figure contains fields this version does not model".to_owned());
        return Err(DltError::invalid(format!("{reason}; nothing was read")));
    }
    // Refused before any asset payload is decoded or written to the store (guard.rs).
    crate::guard::require_document(&doc)?;
    check_feature_keys_used(&declared, version, &doc)?;

    // Entries first: field set, duplicates, and the declared total. Payloads are decoded only after the reference
    // set matches, so a bogus file costs at most one pass over its text.
    let embedded = version >= 2;
    let listed_values = obj.get("assets").and_then(Value::as_array).ok_or_else(|| DltError::invalid("project file has no asset list"))?;
    let mut listed: BTreeMap<String, (AssetRef, Option<&str>)> = BTreeMap::new();
    let mut declared_total: u64 = 0;
    for entry in listed_values {
        let (reference, payload) = asset_entry(entry, embedded)?;
        if listed.contains_key(&reference.sha256) {
            return Err(DltError::new(ErrorCode::IntegrityError, format!("asset {} is listed twice", reference.sha256)));
        }
        declared_total = declared_total.saturating_add(reference.bytes);
        listed.insert(reference.sha256.clone(), (reference, payload));
    }
    if embedded && declared_total > limits.max_embedded {
        return Err(DltError::invalid(format!("embedded assets total {declared_total} bytes; the limit is {} bytes", limits.max_embedded)));
    }
    let referenced_list = if embedded { referenced_hashes(&doc) } else { referenced_hashes_v1(&doc) };
    let referenced: BTreeSet<String> = referenced_list.into_iter().collect();
    for h in &referenced {
        if !listed.contains_key(h) {
            return Err(DltError::new(ErrorCode::IntegrityError, format!("referenced asset {h} is not listed in the project")));
        }
    }
    if let Some(extra) = listed.keys().find(|h| !referenced.contains(*h)) {
        return Err(DltError::new(ErrorCode::IntegrityError, format!("asset {extra} is listed but the figure does not reference it")));
    }

    let mut assets = Vec::with_capacity(listed.len());
    for (reference, payload) in listed.into_values() {
        let data = match payload {
            Some(text) => Some(verified_payload(&reference, text)?),
            None => None,
        };
        assets.push((reference, data));
    }
    Ok(Parsed { doc, assets })
}

/// The first place where the file's figure and this build's model of it differ, named by its field path. A field that the
/// model writes but the file lacks is missing (a default filled it in on read); a field that the file has and the model does
/// not write is not modelled. `None` when the two are equal in every named place.
fn model_difference(file: &Value, model: &Value, path: &str) -> Option<String> {
    match (file, model) {
        (Value::Object(f), Value::Object(m)) => {
            if let Some(key) = m.keys().find(|key| !f.contains_key(*key)) {
                return Some(format!("figure field \"{}\" is missing from the project file", child_path(path, key)));
            }
            if let Some(key) = f.keys().find(|key| !m.contains_key(*key)) {
                let name = child_path(path, key);
                return Some(if f[key].is_null() {
                    format!("figure field \"{name}\" is null in the project file; leave the field out instead")
                } else {
                    format!("figure field \"{name}\" is a field this version does not model")
                });
            }
            f.iter().find_map(|(key, value)| model_difference(value, &m[key], &child_path(path, key)))
        }
        (Value::Array(f), Value::Array(m)) => {
            if f.len() != m.len() {
                return Some(format!("figure field \"{path}\" has {} items; this version reads {}", f.len(), m.len()));
            }
            f.iter().zip(m).enumerate().find_map(|(i, (a, b))| model_difference(a, b, &format!("{path}[{i}]")))
        }
        _ if file == model => None,
        _ => Some(format!("figure field \"{path}\" differs from this version's model")),
    }
}

fn child_path(path: &str, key: &str) -> String {
    if path.is_empty() { key.to_owned() } else { format!("{path}.{key}") }
}

/// The declared `feature_keys`, checked before the figure is read, in canvas native.rs order: the shape, sorted and unique,
/// and membership in PERSISTED_FEATURE_KEYS, all SchemaUnsupported. Version 3 must declare keys, and an older file must not.
/// A key this build does not know is refused by name, so a file with a future feature never reaches the figure model.
/// A present list, even an empty one, is refused below version 3 before its items are read.
fn check_feature_keys_declared(declared: Option<&Value>, version: u64) -> DltResult<Vec<String>> {
    check_feature_key_presence(declared.is_some(), version == FORMAT_VERSION_FEATURE_KEYS).map_err(feature_key_refusal)?;
    let Some(declared) = declared else {
        return Ok(Vec::new());
    };
    let items = declared.as_array().ok_or_else(|| DltError::invalid("feature_keys must be an array of strings"))?;
    let mut names: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        names.push(item.as_str().ok_or_else(|| DltError::invalid("feature_keys must be an array of strings"))?.to_owned());
    }
    check_feature_key_list(&names, &PERSISTED_FEATURE_KEYS).map_err(feature_key_refusal)?;
    Ok(names)
}

fn feature_key_refusal(error: FeatureKeyError<'_>) -> DltError {
    let message = match error {
        FeatureKeyError::NotAllowed => format!("feature_keys are only valid on figure project format_version {FORMAT_VERSION_FEATURE_KEYS}"),
        FeatureKeyError::Required => format!("figure project format_version {FORMAT_VERSION_FEATURE_KEYS} requires feature_keys"),
        FeatureKeyError::Duplicate(_) | FeatureKeyError::Unsorted => "figure project feature_keys must be sorted and unique".to_owned(),
        FeatureKeyError::Unsupported(unknown) => format!("figure project feature key {unknown:?} is not supported"),
    };
    DltError::new(ErrorCode::SchemaUnsupported, message)
}

/// The declared keys against the figure that was read. Version 3 lists exactly the keys the figure uses. An older file
/// uses none, and a figure that uses a key needs version 3. Only the document says which keys it uses, so this runs after it.
fn check_feature_keys_used(declared: &[String], version: u64, doc: &FigureDocument) -> DltResult<()> {
    let needed = feature_keys_of(doc);
    native_envelope::check_feature_keys_used(declared, &needed, version == FORMAT_VERSION_FEATURE_KEYS, &[]).map_err(|error| {
        let message = match error {
            FeatureUseError::NeedsKeysVersion => format!(
                "figure project uses persisted feature keys {needed:?}, which require format_version {FORMAT_VERSION_FEATURE_KEYS}"
            ),
            FeatureUseError::Mismatch => {
                let names: Vec<&str> = declared.iter().map(String::as_str).collect();
                format!("declared feature keys {names:?} do not match the feature keys the figure uses {needed:?}")
            }
        };
        DltError::new(ErrorCode::SchemaUnsupported, message)
    })
}

/// One entry of `assets`. The field set depends on the version. The payload text is returned unchecked.
fn asset_entry(entry: &Value, embedded: bool) -> DltResult<(AssetRef, Option<&str>)> {
    let obj = entry.as_object().ok_or_else(|| DltError::invalid("asset entry must be a JSON object"))?;
    for key in obj.keys() {
        let known = matches!(key.as_str(), "bytes" | "mime" | "sha256") || (embedded && key.as_str() == "data_base64");
        if !known {
            return Err(DltError::invalid(format!("asset entry has unknown field \"{key}\"")));
        }
    }
    let sha = obj.get("sha256").and_then(Value::as_str).ok_or_else(|| DltError::invalid("asset entry has no sha256"))?;
    let mime = obj.get("mime").and_then(Value::as_str).ok_or_else(|| DltError::invalid(format!("asset {sha} has no mime")))?;
    let bytes = obj.get("bytes").and_then(Value::as_u64).ok_or_else(|| DltError::invalid(format!("asset {sha} has no byte length")))?;
    let payload = if embedded {
        let text = obj
            .get("data_base64")
            .and_then(Value::as_str)
            .ok_or_else(|| DltError::new(ErrorCode::IntegrityError, format!("asset {sha} is listed without its embedded bytes")))?;
        Some(text)
    } else {
        None
    };
    Ok((AssetRef { sha256: sha.to_owned(), mime: mime.to_owned(), bytes }, payload))
}

/// Decodes an embedded payload and checks its length and sha256 against the reference.
fn verified_payload(reference: &AssetRef, text: &str) -> DltResult<Vec<u8>> {
    let h = &reference.sha256;
    // The text length follows from the declared length, so a payload of the wrong size is rejected before decoding.
    if text.len() as u64 != base64::encoded_len(reference.bytes) {
        return Err(DltError::new(
            ErrorCode::IntegrityError,
            format!("embedded bytes of asset {h} do not match its declared length {}", reference.bytes),
        ));
    }
    base64::decode_verified(text, reference.bytes, h).map_err(|error| {
        let message = match error {
            PayloadError::Base64(why) => format!("embedded bytes of asset {h} are not valid base64: {}", base64_reason(why)),
            PayloadError::Length { actual } => {
                format!("embedded bytes of asset {h} are {actual} bytes, not the declared {}", reference.bytes)
            }
            PayloadError::Hash => format!("embedded bytes of asset {h} do not match its sha256"),
        };
        DltError::new(ErrorCode::IntegrityError, message)
    })
}

fn base64_reason(error: DecodeError) -> &'static str {
    match error {
        DecodeError::Length => "length is not a multiple of 4",
        DecodeError::MisplacedPadding => "misplaced padding",
        DecodeError::DataAfterPadding => "data after padding",
        DecodeError::InvalidCharacter => "invalid character",
    }
}

/// Decodes and checks a project file. Every embedded payload is verified before the first store write. Then the
/// payloads are stored, and every listed asset must resolve to exactly its recorded reference.
pub fn decode(bytes: &[u8], assets: &dyn AssetStore) -> DltResult<FigureDocument> {
    decode_with(bytes, assets, Limits::DEFAULT)
}

pub(crate) fn decode_with(bytes: &[u8], assets: &dyn AssetStore, limits: Limits) -> DltResult<FigureDocument> {
    let parsed = parse(bytes, limits)?;
    for (reference, data) in &parsed.assets {
        if let Some(data) = data {
            let stored = assets.put(data, &reference.mime)?;
            if stored != *reference {
                return Err(DltError::new(
                    ErrorCode::IntegrityError,
                    format!("the store recorded asset {} with a different reference", reference.sha256),
                ));
            }
        }
    }
    resolve(&parsed.assets, assets)?;
    Ok(parsed.doc)
}

/// Every listed asset must be in the store under exactly its recorded reference.
pub(crate) fn resolve(listed: &[(AssetRef, Option<Vec<u8>>)], assets: &dyn AssetStore) -> DltResult<()> {
    for (reference, _) in listed {
        let h = &reference.sha256;
        let current = assets
            .lookup(h)
            .map_err(|e| DltError::new(ErrorCode::NotFound, format!("source asset {h} is missing: {}", e.message)))?;
        if current != *reference {
            return Err(DltError::new(ErrorCode::IntegrityError, format!("asset {h} differs from its recorded reference")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use designllm_contracts::{Diagram, Evidence, FigureImage, FigurePanel, Rect, Size, Style};

    use super::*;

    /// Content-addressed store that counts writes.
    #[derive(Default)]
    struct Store {
        items: Mutex<BTreeMap<String, (AssetRef, Vec<u8>)>>,
        writes: Mutex<usize>,
    }

    impl Store {
        fn with(bytes: &[u8], mime: &str) -> (Self, AssetRef) {
            let store = Self::default();
            let reference = store.put(bytes, mime).unwrap();
            *store.writes.lock().unwrap() = 0;
            (store, reference)
        }
    }

    impl AssetStore for Store {
        fn put(&self, bytes: &[u8], mime: &str) -> DltResult<AssetRef> {
            *self.writes.lock().unwrap() += 1;
            let reference = AssetRef { sha256: sha256(bytes), mime: mime.to_owned(), bytes: bytes.len() as u64 };
            self.items.lock().unwrap().insert(reference.sha256.clone(), (reference.clone(), bytes.to_vec()));
            Ok(reference)
        }
        fn read(&self, asset: &AssetRef) -> DltResult<Vec<u8>> {
            self.items
                .lock()
                .unwrap()
                .get(&asset.sha256)
                .map(|(_, b)| b.clone())
                .ok_or_else(|| DltError::new(ErrorCode::NotFound, format!("asset {} not found", asset.sha256)))
        }
        fn lookup(&self, hash: &str) -> DltResult<AssetRef> {
            self.items
                .lock()
                .unwrap()
                .get(hash)
                .map(|(r, _)| r.clone())
                .ok_or_else(|| DltError::new(ErrorCode::NotFound, format!("asset {hash} not found")))
        }
    }

    fn b64(bytes: &[u8]) -> String {
        base64::encode(bytes)
    }

    fn image_panel(id: &str, asset: AssetRef) -> FigurePanel {
        FigurePanel {
            id: id.into(),
            bounds: Rect { x: 0, y: 0, width: 1, height: 1 },
            label: id.into(),
            body: PanelBody::Image(FigureImage::new(asset, "experimental_original", 1, 1)),
            style: Style::default(),
        }
    }

    fn figure_with_images(assets: &[AssetRef]) -> FigureDocument {
        FigureDocument {
            panels: assets.iter().enumerate().map(|(i, a)| image_panel(&format!("p{i}"), a.clone())).collect(),
            ..FigureDocument::new(Size::a4())
        }
    }

    #[test]
    fn persisted_feature_keys_are_sorted_and_each_is_listed() {
        assert!(PERSISTED_FEATURE_KEYS.windows(2).all(|pair| pair[0] < pair[1]), "native files require sorted keys");
        assert!(PERSISTED_FEATURE_KEYS.contains(&FEATURE_COMMON_LEGEND));
        assert!(PERSISTED_FEATURE_KEYS.contains(&FEATURE_MATH));
        assert!(PERSISTED_FEATURE_KEYS.contains(&FEATURE_PLOT_ERROR_KEY));
        assert!(PERSISTED_FEATURE_KEYS.contains(&FEATURE_VISUAL_REVIEW));
    }

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        let cases = [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")];
        for (plain, text) in cases {
            assert_eq!(b64(plain.as_bytes()), text);
            assert_eq!(base64::decode(text).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn base64_decode_refuses_malformed_text() {
        for bad in ["Zg=", "Z===", "Zg==Zg==", "Zm9*", "Zm=v", "Zg=A"] {
            assert!(base64::decode(bad).is_err(), "{bad} must be refused");
        }
        assert_eq!(base64_reason(DecodeError::Length), "length is not a multiple of 4");
    }

    #[test]
    fn referenced_hashes_are_sorted_and_unique_and_cover_every_asset_kind() {
        let image = AssetRef { sha256: "c".repeat(64), mime: "image/png".into(), bytes: 1 };
        let node_asset = AssetRef { sha256: "a".repeat(64), mime: "image/png".into(), bytes: 1 };
        let child_asset = AssetRef { sha256: "e".repeat(64), mime: "image/png".into(), bytes: 1 };
        let source = "b".repeat(64);
        let statement = sha256(b"A causes B");
        let node = |id: &str, asset: Option<AssetRef>, children: Vec<Node>| Node {
            id: id.into(),
            kind: designllm_contracts::NodeKind::Rectangle,
            bounds: Rect { x: 0, y: 0, width: 1, height: 1 },
            text: String::new(),
            style: Style::default(),
            asset,
            children,
            layout: None,
            interactions: vec![],
            variants: BTreeMap::new(),
            ext: Default::default(),
        };
        let diagram = FigurePanel {
            id: "d".into(),
            bounds: Rect { x: 0, y: 0, width: 1, height: 1 },
            label: "D".into(),
            body: PanelBody::Diagram(Diagram {
                nodes: vec![node("n", Some(node_asset.clone()), vec![node("child", Some(child_asset.clone()), vec![])])],
                edges: vec![],
                evidence_ids: BTreeMap::new(),
                node_origins: BTreeMap::new(),
            }),
            style: Style::default(),
        };
        let evidence = |id: &str, origin: &str, hash: &str| Evidence {
            id: id.into(),
            source_hash: hash.into(),
            origin: origin.into(),
            physical_page_index: None,
            region: None,
            pdf_geometry: None,
            text: "A causes B".into(),
            review_state: "user_provided".into(),
        };
        let doc = FigureDocument {
            panels: vec![image_panel("img", image.clone()), diagram],
            evidence: vec![evidence("ev-pdf", PDF_EVIDENCE_ORIGIN, &source), evidence("ev-user", "user_statement", &statement)],
            source_scopes: vec![designllm_contracts::SourceCoverage { source_hash: source.clone(), physical_page_indices: vec![0] }],
            ..FigureDocument::new(Size::a4())
        };
        let mut expected = vec![node_asset.sha256, child_asset.sha256, source, image.sha256];
        expected.sort();
        assert_eq!(referenced_hashes(&doc), expected, "the statement hash is text, not an asset, and the PDF source is listed once");
    }

    #[test]
    fn encoded_envelope_is_canonical_json() {
        let (store, reference) = Store::with(b"x,y\n1,2\n", "text/csv");
        let doc = figure_with_images(std::slice::from_ref(&reference));
        let bytes = encode_with(&doc, &store, Limits::DEFAULT).unwrap();
        let expected = canonical_json(&json!({
            "assets": [{"bytes": reference.bytes, "data_base64": b64(b"x,y\n1,2\n"), "mime": "text/csv", "sha256": reference.sha256}],
            "figure": serde_json::to_value(&doc).unwrap(),
            "format": FORMAT,
            "format_version": FORMAT_VERSION,
            "schema_version": SCHEMA_VERSION,
        }))
        .unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), expected);
    }

    #[test]
    fn save_refuses_above_the_embedded_limit_and_the_file_limit() {
        let (store, reference) = Store::with(b"abcd", "text/plain");
        let doc = figure_with_images(std::slice::from_ref(&reference));
        let embedded = Limits { max_embedded: 3, max_file: MAX_NATIVE_BYTES };
        let err = encode_with(&doc, &store, embedded).expect_err("over the embedded limit");
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(err.message.contains("nothing was written"), "{}", err.message);
        let file = Limits { max_embedded: MAX_EMBEDDED_BYTES, max_file: 64 };
        assert_eq!(encode_with(&doc, &store, file).expect_err("over the file limit").code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn open_refuses_a_file_or_an_embedded_total_above_its_limit() {
        let (store, reference) = Store::with(b"abcd", "text/plain");
        let doc = figure_with_images(std::slice::from_ref(&reference));
        let bytes = encode_with(&doc, &store, Limits::DEFAULT).unwrap();
        let empty = Store::default();
        let small_file = Limits { max_embedded: MAX_EMBEDDED_BYTES, max_file: 16 };
        assert_eq!(decode_with(&bytes, &empty, small_file).expect_err("file limit").code, ErrorCode::InvalidArgument);
        let small_total = Limits { max_embedded: 3, max_file: MAX_NATIVE_BYTES };
        let err = decode_with(&bytes, &empty, small_total).expect_err("embedded total limit");
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(err.message.contains("embedded assets total"), "{}", err.message);
        assert_eq!(*empty.writes.lock().unwrap(), 0, "a refused file stores nothing");
    }

    #[test]
    fn every_payload_is_verified_before_any_store_write() {
        let (store, first) = Store::with(b"alpha", "text/plain");
        let second = store.put(b"bravo", "text/plain").unwrap();
        *store.writes.lock().unwrap() = 0;
        let doc = figure_with_images(&[first, second]);
        let bytes = encode_with(&doc, &store, Limits::DEFAULT).unwrap();

        // Flip the first character of the second payload. Its text stays valid base64 with the same length.
        let marker = "\"data_base64\":\"";
        let index = bytes.windows(marker.len()).enumerate().filter(|(_, w)| *w == marker.as_bytes()).nth(1).unwrap().0 + marker.len();
        let mut tampered = bytes.clone();
        tampered[index] = if tampered[index] == b'A' { b'B' } else { b'A' };

        let empty = Store::default();
        let err = decode_with(&tampered, &empty, Limits::DEFAULT).expect_err("corrupt payload");
        assert_eq!(err.code, ErrorCode::IntegrityError);
        assert!(err.message.contains("sha256"), "{}", err.message);
        assert_eq!(*empty.writes.lock().unwrap(), 0, "the valid first payload must not be stored when the file is refused");
    }

    fn edit_entries(bytes: &[u8], edit: impl FnOnce(&mut Value)) -> Vec<u8> {
        let mut value: Value = serde_json::from_slice(bytes).unwrap();
        edit(&mut value);
        serde_json::to_vec(&value).unwrap()
    }

    #[test]
    fn asset_entries_must_match_the_version_and_the_figure() {
        let (store, reference) = Store::with(b"alpha", "text/plain");
        let doc = figure_with_images(std::slice::from_ref(&reference));
        let bytes = encode_with(&doc, &store, Limits::DEFAULT).unwrap();
        let open = |data: &[u8]| decode_with(data, &Store::default(), Limits::DEFAULT);

        let duplicate = edit_entries(&bytes, |v| {
            let first = v["assets"][0].clone();
            v["assets"].as_array_mut().unwrap().push(first);
        });
        assert_eq!(open(&duplicate).expect_err("duplicate").code, ErrorCode::IntegrityError);

        let no_payload = edit_entries(&bytes, |v| {
            v["assets"][0].as_object_mut().unwrap().remove("data_base64");
        });
        assert_eq!(open(&no_payload).expect_err("no payload").code, ErrorCode::IntegrityError);

        let unknown = edit_entries(&bytes, |v| {
            v["assets"][0]["note"] = json!("x");
        });
        assert_eq!(open(&unknown).expect_err("unknown field").code, ErrorCode::InvalidArgument);

        let version_one_with_bytes = edit_entries(&bytes, |v| {
            v["format_version"] = json!(1);
        });
        assert_eq!(open(&version_one_with_bytes).expect_err("version 1 has no payloads").code, ErrorCode::InvalidArgument);

        let unreferenced = edit_entries(&bytes, |v| {
            let empty = json!({"bytes": 0, "data_base64": "", "mime": "application/octet-stream", "sha256": sha256(b"")});
            v["assets"].as_array_mut().unwrap().push(empty);
        });
        let err = open(&unreferenced).expect_err("unreferenced asset");
        assert_eq!(err.code, ErrorCode::IntegrityError);
        assert!(err.message.contains("does not reference"), "{}", err.message);
    }
}
