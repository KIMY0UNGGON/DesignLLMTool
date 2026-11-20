//! Verifies saved PresentationML bytes against the artifact that was saved.
//!
//! The verifier re-reads the output with the reader. It does not reuse the writer's state. It checks:
//! - structure: package limits, OPC errors (blocking), and OPC warnings (reported only);
//! - immutable parts (themes, masters, layouts, media, unknown parts) by payload hash; with theme edits, the theme
//!   parts by what the edit may change (`verify_bytes_with_theme`);
//! - opaque regions by part, anchor, and hash;
//! - the semantic model: page order, slide identity, notes, shapes, text, styles, geometry, and picture hashes.
//!
//! A mismatch is blocking, so the core refuses the save with UNSAFE_ROUNDTRIP.

use std::collections::BTreeMap;

use designllm_contracts::{
    sha256, AssetRef, Diagnostic, Node, OpaqueRegion, Page, PartFingerprint, PreservationManifest, PreservationVerification, Size,
    ThemePatch,
};
use designllm_package::parts::{compare_parts, payload_sha256, PartState};
use designllm_package::{opc, Package};

use crate::guard;
use crate::theme::{self, Theme};
use crate::model::{Mappings, PageMap, ShapeMap, ORIGIN_ADDED, ORIGIN_SOURCE};
use crate::ns::{self, EXPECTED_TARGET_TYPES};
use crate::reader::{read_presentation, ReadPage, ReadShape};

/// What the verifier compares the output against.
pub struct VerifyInput<'a> {
    pub pages: &'a [Page],
    pub mappings: &'a Mappings,
    pub slide_size: Size,
    pub manifest: &'a PreservationManifest,
    pub opaque: &'a [OpaqueRegion],
}

/// Parts the writer is allowed to change. Everything else must keep its payload hash.
pub fn owned_part(name: &str) -> bool {
    name == ns::PRESENTATION_PART
        || name == ns::PRESENTATION_RELS_PART
        || name == opc::CONTENT_TYPES_PART
        || name.starts_with("ppt/slides/")
        || name.starts_with("ppt/notesSlides/")
}

fn blocking(result: &mut PreservationVerification, code: &str, message: impl Into<String>) {
    result.diagnostics.push(Diagnostic { code: code.to_string(), message: message.into(), node_id: None, blocking: true, params: Default::default() });
}

/// Verifies `bytes` against `input`. Never panics. Failures are diagnostics.
pub fn verify_bytes(input: &VerifyInput<'_>, bytes: &[u8]) -> PreservationVerification {
    match open_saved(bytes) {
        Ok(package) => verify_package(input, &package),
        Err(result) => result,
    }
}

/// The saved package, or the verification that refuses it when it cannot be reopened safely.
fn open_saved(bytes: &[u8]) -> Result<Package, PreservationVerification> {
    guard::open_package(bytes).map_err(|e| {
        let mut result = PreservationVerification::default();
        let code = e
            .diagnostics
            .iter()
            .find(|d| matches!(d.code.as_str(), guard::COMPOUND_CONTAINER_UNSUPPORTED | guard::DIGITAL_SIGNATURE_UNSUPPORTED | guard::OPC_ERROR))
            .map(|d| d.code.as_str())
            .unwrap_or("PPTX_UNREADABLE");
        blocking(&mut result, code, format!("saved package cannot be safely reopened: {}", e.message));
        result
    })
}

/// `verify_bytes` on a saved package that is already open.
fn verify_package(input: &VerifyInput<'_>, package: &Package) -> PreservationVerification {
    let mut result = PreservationVerification::default();
    let report = opc::verify(package, EXPECTED_TARGET_TYPES);
    for error in &report.errors {
        blocking(&mut result, "PPTX_OPC_ERROR", error.clone());
    }
    for warning in &report.warnings {
        result.diagnostics.push(Diagnostic { code: "PPTX_OPC_WARNING".into(), message: warning.clone(), node_id: None, blocking: false, params: Default::default() });
    }

    let put = |bytes: &[u8], mime: &str| -> designllm_contracts::DltResult<AssetRef> {
        Ok(AssetRef { sha256: sha256(bytes), mime: mime.to_string(), bytes: bytes.len() as u64 })
    };
    let (presentation, pages) = match read_presentation(&package, &put) {
        Ok(read) => read,
        Err(e) => {
            blocking(&mut result, "PPTX_REREAD_FAILED", format!("saved presentation cannot be read: {}", e.message));
            return result;
        }
    };

    // Immutable parts keep their payload hash.
    check_parts(&input.manifest.parts, &package, &mut result);

    // Opaque regions: same part, same anchor, same bytes.
    let mut output_regions: BTreeMap<(String, String), String> = BTreeMap::new();
    for region in presentation.opaque.iter().chain(pages.iter().flat_map(|p| p.opaque.iter())) {
        output_regions.insert((region.part.clone(), region.anchor.clone()), region.sha256.clone());
    }
    for region in input.opaque {
        match output_regions.get(&(region.part.clone(), region.anchor.clone())) {
            Some(hash) if *hash == region.sha256 => result.checked_regions += 1,
            Some(_) => blocking(&mut result, "PPTX_OPAQUE_CHANGED", format!("{} {} changed", region.part, region.anchor)),
            None => blocking(&mut result, "PPTX_OPAQUE_MISSING", format!("{} {} is missing", region.part, region.anchor)),
        }
    }

    // Semantic model.
    if presentation.slide_size != input.slide_size {
        blocking(&mut result, "PPTX_SEMANTIC_MISMATCH", "slide size changed");
    }
    if pages.len() != input.pages.len() {
        blocking(&mut result, "PPTX_SEMANTIC_MISMATCH", format!("page count {} differs from {}", pages.len(), input.pages.len()));
        return result;
    }
    for (page, output) in input.pages.iter().zip(&pages) {
        compare_page(input, page, output, &package, &mut result);
    }
    result
}

/// A theme part that differs from the theme edit the model asks for (blocking).
pub const PPTX_THEME_MISMATCH: &str = "PPTX_THEME_MISMATCH";

/// `verify_bytes` for a deck with theme edits. Each theme part of the saved deck (`theme::theme_parts`) must be byte for
/// byte the original part with `patch` written in (`theme::splice_theme`), and must read back with the values of
/// `patch`; a theme part the save created (a new notes master) must be the edited slide master theme. Any other
/// difference is blocking (PPTX_THEME_MISMATCH). Without a patch this is `verify_bytes`, where a changed theme part is
/// PPTX_PART_CHANGED.
pub fn verify_bytes_with_theme(input: &VerifyInput<'_>, original: &Package, patch: Option<&ThemePatch>, bytes: &[u8]) -> PreservationVerification {
    let Some(patch) = patch else { return verify_bytes(input, bytes) };
    let saved = match open_saved(bytes) {
        Ok(package) => package,
        Err(result) => return result,
    };
    let parts = match theme::theme_parts(original) {
        Ok(parts) => parts,
        Err(e) => {
            let mut result = PreservationVerification::default();
            blocking(&mut result, PPTX_THEME_MISMATCH, format!("the edited theme cannot be located: {}", e.message));
            return result;
        }
    };
    // The theme parts are checked below against the edited theme, not by their original hash.
    let mut manifest = input.manifest.clone();
    for part in &parts {
        manifest.parts.remove(part);
    }
    let unhashed = VerifyInput { pages: input.pages, mappings: input.mappings, slide_size: input.slide_size, manifest: &manifest, opaque: input.opaque };
    let mut result = verify_package(&unhashed, &saved);
    let saved_parts = match theme::theme_parts(&saved) {
        Ok(saved_parts) => saved_parts,
        Err(e) => {
            blocking(&mut result, PPTX_THEME_MISMATCH, format!("the saved theme cannot be located: {}", e.message));
            Vec::new()
        }
    };
    // Each original theme part, then each theme part the save added, against the slide master theme it copies.
    let created = saved_parts.iter().filter(|part| !original.has_part(part));
    for (part, source) in parts.iter().map(|part| (part, part)).chain(created.map(|part| (part, &parts[0]))) {
        let checked = match (original.part(source), saved.part(part)) {
            (Ok(before), Ok(after)) => check_theme_part(before, after, patch),
            (Ok(_), Err(_)) => Err("is missing".to_string()),
            (Err(_), _) => Err("is missing from the original package".to_string()),
        };
        match checked {
            Ok(()) => result.checked_parts += 1,
            Err(message) => blocking(&mut result, PPTX_THEME_MISMATCH, format!("theme part {part} {message}")),
        }
    }
    result
}

/// The saved theme part is exactly the original part with the patch written in, and reads back as the original theme
/// with the patch values (a second check, independent of the splice).
/// Exact bytes on purpose: the writer saves the theme only through this splice, whose output is deterministic.
fn check_theme_part(before: &[u8], after: &[u8], patch: &ThemePatch) -> Result<(), String> {
    let expected = theme::splice_theme(before, patch).map_err(|e| format!("cannot take the edit: {}", e.message))?;
    if after != expected.as_slice() {
        return Err("differs from the original theme with the edited colours and fonts".into());
    }
    let values = theme::edited_theme(Theme::parse(before).map_err(|e| e.message)?, patch);
    if Theme::parse(after).map_err(|e| format!("cannot be read: {}", e.message))? != values {
        return Err("does not hold the edited colours and fonts".into());
    }
    Ok(())
}

/// Immutable parts that the writer does not own keep their payload hash. Only a kept part counts as checked.
fn check_parts(parts: &BTreeMap<String, PartFingerprint>, package: &Package, result: &mut PreservationVerification) {
    let expected = parts
        .iter()
        .filter(|(name, fp)| fp.immutable && !owned_part(name))
        .map(|(name, fp)| (name.as_str(), fp.sha256.as_str()));
    for (name, state) in compare_parts(expected, |name| payload_sha256(package, name)) {
        match state {
            PartState::Kept => result.checked_parts += 1,
            PartState::Changed => blocking(result, "PPTX_PART_CHANGED", format!("part {name} changed")),
            PartState::Missing => blocking(result, "PPTX_PART_MISSING", format!("part {name} is missing")),
        }
    }
}

fn compare_page(input: &VerifyInput<'_>, page: &Page, output: &ReadPage, package: &Package, result: &mut PreservationVerification) {
    let Some(map_value) = input.mappings.get(&page.id) else {
        blocking(result, "PPTX_SEMANTIC_MISMATCH", format!("page {} has no mapping", page.id));
        return;
    };
    let Ok(map) = serde_json::from_value::<PageMap>(map_value.clone()) else {
        blocking(result, "PPTX_SEMANTIC_MISMATCH", format!("page {} mapping is malformed", page.id));
        return;
    };
    if map.sld_id != output.map.sld_id || map.part != output.map.part {
        blocking(result, "PPTX_SEMANTIC_MISMATCH", format!("page {} has a different slide identity", page.id));
    }
    if map.notes_part != output.map.notes_part {
        blocking(result, "PPTX_SEMANTIC_MISMATCH", format!("page {} has a different notes part", page.id));
    }
    if page.notes != output.page.notes {
        blocking(result, "PPTX_SEMANTIC_MISMATCH", format!("notes of page {} differ from the model", page.id));
    }
    if page.nodes.len() != output.shapes.len() {
        blocking(
            result,
            "PPTX_SEMANTIC_MISMATCH",
            format!("page {} has {} modelled shapes, saved {}", page.id, page.nodes.len(), output.shapes.len()),
        );
        return;
    }
    for (node, shape) in page.nodes.iter().zip(&output.shapes) {
        compare_node(input, node, shape, package, result);
    }
}

fn compare_node(input: &VerifyInput<'_>, node: &Node, shape: &ReadShape, package: &Package, result: &mut PreservationVerification) {
    let Some(value) = input.mappings.get(&node.id) else {
        blocking(result, "PPTX_SEMANTIC_MISMATCH", format!("node {} has no mapping", node.id));
        return;
    };
    let Ok(expected) = serde_json::from_value::<ShapeMap>(value.clone()) else {
        blocking(result, "PPTX_SEMANTIC_MISMATCH", format!("node {} mapping is malformed", node.id));
        return;
    };
    if expected.cnvpr_id != shape.map.cnvpr_id {
        blocking(result, "PPTX_SEMANTIC_MISMATCH", format!("node {} is saved as shape {}", node.id, shape.map.cnvpr_id));
        return;
    }
    if expected.origin != ORIGIN_SOURCE && expected.origin != ORIGIN_ADDED {
        blocking(result, "PPTX_SEMANTIC_MISMATCH", format!("node {} has an unknown origin", node.id));
        return;
    }
    let legacy_marker_mapping = node.kind == designllm_contracts::NodeKind::Line
        && expected.arrow
        && node.style.stroke_start.is_none()
        && node.style.stroke_end.is_none()
        && !expected.unmapped_head_end
        && !expected.unmapped_tail_end
        && !expected.custom_head_end_size
        && !expected.custom_tail_end_size;
    let (mut left, mut right) = (normalized_shape(&expected), normalized_shape(&shape.map));
    if legacy_marker_mapping {
        for value in [&mut left, &mut right] {
            if let Some(map) = value.as_object_mut() {
                map.remove("unmapped_head_end");
                map.remove("unmapped_tail_end");
                map.remove("custom_head_end_size");
                map.remove("custom_tail_end_size");
            }
        }
    }
    if left != right {
        let keys = differing_keys(&left, &right);
        blocking(result, "PPTX_SEMANTIC_MISMATCH", format!("shape {} differs from the model in {keys:?}", expected.cnvpr_id));
    }
    if node.kind != shape.node.kind || node.text != shape.node.text || node.bounds != shape.node.bounds {
        blocking(result, "PPTX_SEMANTIC_MISMATCH", format!("shape {} text or geometry differs", expected.cnvpr_id));
    }
    if node.ext.path != shape.node.ext.path {
        blocking(result, "PPTX_SEMANTIC_MISMATCH", format!("shape {} vector path differs from the model", expected.cnvpr_id));
    }
    // Font fields are stored only as run formatting. A shape without text has none, so they are not compared.
    let mut left = serde_json::to_value(&node.style).unwrap_or(serde_json::Value::Null);
    let mut right = serde_json::to_value(&shape.node.style).unwrap_or(serde_json::Value::Null);
    if legacy_marker_mapping {
        // Old workspace snapshots only saved the coarse arrow bit. The source XML remains authoritative until the
        // user reopens the PPTX to get directional endpoint fields into the model.
        for key in ["stroke_start", "stroke_end"] {
            if let Some(map) = left.as_object_mut() { map.remove(key); }
            if let Some(map) = right.as_object_mut() { map.remove(key); }
        }
    }
    if node.text.is_empty() {
        for key in ["font_family", "font_size", "color", "bold", "italic", "align"] {
            if let Some(map) = left.as_object_mut() {
                map.remove(key);
            }
            if let Some(map) = right.as_object_mut() {
                map.remove(key);
            }
        }
    }
    if left != right {
        blocking(result, "PPTX_SEMANTIC_MISMATCH", format!("shape {} style differs", expected.cnvpr_id));
    }
    if let Some(asset) = &node.asset {
        let media_ok = match &shape.map.media_part {
            Some(part) => package.part(part).map(|bytes| sha256(bytes) == asset.sha256).unwrap_or(false),
            None => false,
        };
        if shape.node.asset.as_ref() != Some(asset) || !media_ok {
            blocking(result, "PPTX_MEDIA_MISMATCH", format!("picture {} does not match its media hash", expected.cnvpr_id));
        }
    }
}

/// Mapping JSON with the fields that legitimately differ between the model and the saved reading removed.
fn normalized_shape(map: &ShapeMap) -> serde_json::Value {
    let mut copy = map.clone();
    copy.origin = String::new();
    copy.anchor = String::new();
    copy.render_notes = Vec::new();
    // Relationship ids are allocated by the writer. The picture is verified through its media part and hash.
    copy.rel_id = None;
    serde_json::to_value(copy).unwrap_or(serde_json::Value::Null)
}

/// Shape ids that the reader reports for a page, in order (used by the open path and tests).
pub fn modelled_shape_ids(page: &ReadPage) -> Vec<u32> {
    page.shapes.iter().map(|s| s.map.cnvpr_id).collect()
}

/// Top-level keys whose values differ between two mapping records (used in diagnostics).
fn differing_keys(left: &serde_json::Value, right: &serde_json::Value) -> Vec<String> {
    match (left.as_object(), right.as_object()) {
        (Some(a), Some(b)) => a
            .keys()
            .chain(b.keys())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .filter(|k| a.get(*k) != b.get(*k))
            .cloned()
            .collect(),
        _ => vec!["(not an object)".to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(sha: String, immutable: bool) -> PartFingerprint {
        PartFingerprint { sha256: sha, content_type: None, immutable }
    }

    #[test]
    fn only_kept_immutable_parts_outside_the_writer_count() {
        let mut package = Package::new();
        for (name, bytes) in [("ppt/media/a.png", b"<a/>"), ("ppt/media/b.png", b"<b/>"), ("ppt/slides/slide1.xml", b"<s/>")] {
            package.set_part(name, bytes.to_vec()).expect("set");
        }
        let parts: BTreeMap<String, PartFingerprint> = [
            ("ppt/media/a.png", fingerprint(sha256(b"<a/>"), true)),
            ("ppt/media/b.png", fingerprint(sha256(b"<x/>"), true)),
            ("ppt/media/c.png", fingerprint(sha256(b"<c/>"), true)),
            ("ppt/media/d.png", fingerprint(sha256(b"<d/>"), false)),
            ("ppt/slides/slide1.xml", fingerprint(sha256(b"<x/>"), true)),
            (ns::PRESENTATION_PART, fingerprint(sha256(b"<p/>"), true)),
        ]
        .into_iter()
        .map(|(name, fp)| (name.to_string(), fp))
        .collect();
        let mut result = PreservationVerification::default();
        check_parts(&parts, &package, &mut result);
        assert_eq!(result.checked_parts, 1, "changed, missing, mutable, and writer-owned parts are not counted");
        let found: Vec<(&str, &str, bool)> = result.diagnostics.iter().map(|d| (d.code.as_str(), d.message.as_str(), d.blocking)).collect();
        assert_eq!(
            found,
            vec![
                ("PPTX_PART_CHANGED", "part ppt/media/b.png changed", true),
                ("PPTX_PART_MISSING", "part ppt/media/c.png is missing", true),
            ]
        );
    }
}
