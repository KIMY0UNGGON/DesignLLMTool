//! `verify_saved`: checks written bytes against the artifact.
//!
//! The checks are: immutable part bytes (SHA-256 of each payload), protected subtrees by owner, id, name,
//! and occurrence, the semantic content of every block in document order (text, atoms, runs and their
//! effective style, paragraph format, table grid and spans, picture bytes and extent, section setup),
//! XML comments and processing instructions of every XML part (by part, element names, and bytes), and package
//! structure through the shared OPC verifier. The output is re-parsed; nothing is trusted from the writer.

use std::collections::{BTreeMap, BTreeSet};

use designllm_contracts::{sha256, Artifact, ArtifactBody, FigureLink, Id, PartFingerprint, PreservationVerification};
use designllm_package::opc;
use designllm_package::parts::{compare_parts, payload_sha256, PartState};
use designllm_package::Package;

use crate::diag;
use crate::model;
use crate::parse;
use crate::styles::StyleTable;
use crate::write::{main_document_part, read_rels, CT_DOCUMENT_MAIN, CT_STYLES, REL_OFFICE_DOCUMENT, REL_STYLES, STYLES_PART};

/// One XML comment or processing instruction: its part, the qualified names of its element ancestors from
/// the root down, and the SHA-256 of its bytes. Sibling positions are not part of the key, so an insertion
/// elsewhere in the document leaves it unchanged. A dropped, added, or altered node always changes it.
pub(crate) type MetadataNode = (String, String, String);

/// Every XML comment and processing instruction of the package's XML parts, in part and document order.
pub(crate) fn xml_metadata(bytes: &[u8]) -> designllm_contracts::DltResult<Vec<MetadataNode>> {
    let package = Package::open(bytes)?;
    let mut result = Vec::new();
    for name in package.names().into_iter().filter(|n| n.ends_with(".xml") || n.ends_with(".rels")) {
        let payload = package.part(&name)?;
        let text = std::str::from_utf8(payload).map_err(|e| designllm_contracts::DltError::invalid(e.to_string()))?;
        let doc = roxmltree::Document::parse(text).map_err(|e| designllm_contracts::DltError::invalid(e.to_string()))?;
        for node in doc.descendants().filter(|n| n.is_comment() || n.is_pi()) {
            let mut ancestry: Vec<String> = node
                .ancestors()
                .filter(|n| n.is_element())
                .map(|e| format!("{{{}}}{}", e.tag_name().namespace().unwrap_or(""), e.tag_name().name()))
                .collect();
            ancestry.reverse();
            result.push((name.clone(), ancestry.join("/"), sha256(&payload[node.range()])));
        }
    }
    Ok(result)
}

/// `None` when `after` holds the same XML comments and processing instructions as `before`. Otherwise the
/// changed parts, each with its count before and after, or a note that a node changed in place.
pub(crate) fn metadata_difference(before: &[MetadataNode], after: &[MetadataNode]) -> Option<String> {
    if before == after {
        return None;
    }
    let parts: BTreeSet<&str> = before.iter().chain(after.iter()).map(|node| node.0.as_str()).collect();
    let changed: Vec<String> = parts
        .into_iter()
        .filter_map(|part| {
            let was: Vec<&MetadataNode> = before.iter().filter(|node| node.0 == part).collect();
            let now: Vec<&MetadataNode> = after.iter().filter(|node| node.0 == part).collect();
            if was == now {
                None
            } else if was.len() != now.len() {
                Some(format!("{part} {} -> {}", was.len(), now.len()))
            } else {
                Some(format!("{part} content or location changed"))
            }
        })
        .collect();
    Some(changed.join(", "))
}

/// Verifies the output bytes of `artifact`.
pub fn verify_output(artifact: &Artifact, bytes: &[u8]) -> PreservationVerification {
    let mut result = PreservationVerification::default();
    let Some(native) = artifact.native.as_ref() else {
        result.diagnostics.push(diag::blocking(diag::PACKAGE_STRUCTURE, "the artifact has no native source to verify against", None));
        return result;
    };
    let ArtifactBody::Flow(model_document) = &artifact.body else {
        result.diagnostics.push(diag::blocking(diag::VERIFY_MISMATCH, "the artifact body is not a flow document", None));
        return result;
    };

    let package = match Package::open(bytes) {
        Ok(package) => package,
        Err(error) => {
            result.diagnostics.push(diag::blocking(diag::PACKAGE_STRUCTURE, format!("the written package cannot be read: {}", error.message), None));
            return result;
        }
    };
    match opc::has_digital_signature(&package) {
        Ok(false) => {}
        Ok(true) => {
            result.diagnostics.push(diag::blocking(
                diag::DIGITAL_SIGNATURE_UNSUPPORTED,
                "the written package has digital-signature markers that this engine cannot validate",
                None,
            ));
            return result;
        }
        Err(error) => {
            result.diagnostics.push(diag::blocking(
                diag::PACKAGE_STRUCTURE,
                format!("signature markers cannot be checked: {}", error.message),
                None,
            ));
            return result;
        }
    }

    // Immutable parts must match byte for byte. Mutable parts are checked semantically below.
    check_parts(&native.preservation.parts, &package, &mut result);

    let doc_part = match main_document_part(&package) {
        Ok(part) => part,
        Err(error) => {
            result.diagnostics.push(diag::blocking(diag::PACKAGE_STRUCTURE, error.message, None));
            return result;
        }
    };
    let doc_bytes = match package.part(&doc_part) {
        Ok(bytes) => bytes.to_vec(),
        Err(error) => {
            result.diagnostics.push(diag::blocking(diag::PACKAGE_STRUCTURE, error.message, None));
            return result;
        }
    };
    let rels = read_rels(&package, &doc_part).unwrap_or_default();
    let rels_map: std::collections::BTreeMap<String, String> = rels
        .iter()
        .map(|r| (r.id.clone(), opc::resolve_target(&doc_part, &r.target).unwrap_or_default()))
        .collect();
    let styles_name = rels_map
        .values()
        .find(|t| t.ends_with("styles.xml"))
        .cloned()
        .unwrap_or_else(|| STYLES_PART.to_string());
    let styles_bytes = package.part(&styles_name).ok().map(|b| b.to_vec());
    let styles = match StyleTable::parse(styles_bytes.as_deref()) {
        Ok(styles) => styles,
        Err(error) => {
            result.diagnostics.push(diag::blocking(diag::VERIFY_MISMATCH, format!("styles cannot be read: {}", error.message), None));
            StyleTable::default()
        }
    };
    let read = |name: &str| package.part(name).ok().map(|b| b.to_vec());
    let parsed = match parse::parse_document(&doc_bytes, &styles, &rels_map, &read) {
        Ok(parsed) => parsed,
        Err(error) => {
            result.diagnostics.push(diag::blocking(diag::VERIFY_MISMATCH, format!("the written document cannot be read: {}", error.message), None));
            return result;
        }
    };

    // Semantic content, in document order, aligned by position.
    let model_flat = model::flatten(model_document);
    let out_flat = model::flatten(&parsed.document);
    if model_flat.len() != out_flat.len() {
        result.diagnostics.push(diag::blocking(
            diag::VERIFY_MISMATCH,
            format!("the written document has {} blocks, the model has {}", out_flat.len(), model_flat.len()),
            None,
        ));
    }
    for (index, (expected, actual)) in model_flat.iter().zip(out_flat.iter()).enumerate() {
        if expected.canon != actual.canon {
            result.diagnostics.push(diag::blocking(
                diag::VERIFY_MISMATCH,
                format!("block {index} differs between the model and the written document"),
                expected.id.as_ref(),
            ));
        }
    }

    // Figure links: the linked picture must hold the render asset in the model and in the written file.
    for link in &artifact.figure_links {
        check_figure_link(link, &model_flat, &out_flat, &mut result);
    }

    // Protected subtrees: each region names its owner, its qualified name, and its occurrence.
    for region in &native.preservation.opaque_regions {
        let (owner, selector) = match region.anchor.split_once('#') {
            Some(parts) => parts,
            None => {
                result.diagnostics.push(diag::blocking(diag::PROTECTED_CHANGED, format!("malformed region anchor {}", region.anchor), None));
                continue;
            }
        };
        result.checked_regions += 1;
        let Some(index) = model_flat.iter().position(|f| f.id.as_deref() == Some(owner)) else {
            result.diagnostics.push(diag::blocking(diag::PROTECTED_CHANGED, format!("protected owner {owner} is missing"), None));
            continue;
        };
        let owner_id: Option<Id> = out_flat.get(index).and_then(|f| f.id.clone());
        let (qname, occurrence) = parse_selector(selector);
        let actual = owner_id
            .as_ref()
            .and_then(|id| parsed.atoms.get(id))
            .and_then(|atoms| atoms.iter().find(|a| a.qname == qname && Some(a.occ) == occurrence));
        match actual {
            Some(atom) if atom.sha == region.sha256 => {}
            _ => result.diagnostics.push(diag::blocking(
                diag::PROTECTED_CHANGED,
                format!("protected subtree {qname}[{}] of {owner} changed or is missing", occurrence.unwrap_or(0)),
                Some(&owner.to_string()),
            )),
        }
    }

    // Package structure: content types, relationship targets, document and styles types.
    let report = opc::verify(&package, &[(REL_OFFICE_DOCUMENT, CT_DOCUMENT_MAIN), (REL_STYLES, CT_STYLES)]);
    for error in report.errors {
        result.diagnostics.push(diag::blocking(diag::PACKAGE_STRUCTURE, error, None));
    }
    result
}

/// Every recorded part counts as checked. An immutable part must keep its payload hash; a changed or a missing one
/// is reported with the same code.
fn check_parts(parts: &BTreeMap<String, PartFingerprint>, package: &Package, result: &mut PreservationVerification) {
    result.checked_parts += parts.len() as u32;
    let immutable = parts.iter().filter(|(_, fp)| fp.immutable).map(|(name, fp)| (name.as_str(), fp.sha256.as_str()));
    for (name, state) in compare_parts(immutable, |name| payload_sha256(package, name)) {
        let message = match state {
            PartState::Kept => continue,
            PartState::Changed => format!("immutable part {name} changed"),
            PartState::Missing => format!("immutable part {name} is missing"),
        };
        result.diagnostics.push(diag::blocking(diag::IMMUTABLE_PART_CHANGED, message, None));
    }
}

/// A figure link names a picture whose bytes, in the model and in the written file, are the render asset.
fn check_figure_link(link: &FigureLink, model: &[model::Flat], written: &[model::Flat], result: &mut PreservationVerification) {
    let Some(index) = model.iter().position(|f| f.id.as_deref() == Some(link.target_node_id.as_str())) else {
        result.diagnostics.push(diag::blocking(
            diag::VERIFY_MISMATCH,
            format!("figure link target {} is not a block of the document", link.target_node_id),
            Some(&link.target_node_id),
        ));
        return;
    };
    let expected = (link.render_hash.as_str(), "image/png");
    let in_model = model::picture_asset(&model[index].canon) == Some(expected);
    let in_file = written.get(index).and_then(|f| model::picture_asset(&f.canon)) == Some(expected);
    if link.representation != "png" || !in_model || !in_file {
        result.diagnostics.push(diag::blocking(
            diag::VERIFY_MISMATCH,
            format!(
                "figure link to {} does not match its render {}: the picture must hold that image/png in the model and in the saved file",
                link.target_node_id, link.render_hash
            ),
            Some(&link.target_node_id),
        ));
    }
}

fn parse_selector(selector: &str) -> (String, Option<u32>) {
    match selector.strip_suffix(']').and_then(|s| s.rsplit_once('[')) {
        Some((name, occurrence)) => (name.to_string(), occurrence.parse::<u32>().ok()),
        None => (selector.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(part: &str, ancestry: &str, sha: &str) -> MetadataNode {
        (part.to_string(), ancestry.to_string(), sha.to_string())
    }

    fn fingerprint(sha: String, immutable: bool) -> PartFingerprint {
        PartFingerprint { sha256: sha, content_type: None, immutable }
    }

    #[test]
    fn every_recorded_part_is_counted_and_immutable_changes_share_one_code() {
        let mut package = Package::new();
        for (name, bytes) in [("a.xml", b"<a/>"), ("b.xml", b"<b/>"), ("m.xml", b"<m/>")] {
            package.set_part(name, bytes.to_vec()).expect("set");
        }
        let parts: BTreeMap<String, PartFingerprint> = [
            ("a.xml", fingerprint(sha256(b"<a/>"), true)),
            ("b.xml", fingerprint(sha256(b"<x/>"), true)),
            ("c.xml", fingerprint(sha256(b"<c/>"), true)),
            ("m.xml", fingerprint(sha256(b"<x/>"), false)),
            ("n.xml", fingerprint(sha256(b"<n/>"), false)),
        ]
        .into_iter()
        .map(|(name, fp)| (name.to_string(), fp))
        .collect();
        let mut result = PreservationVerification::default();
        check_parts(&parts, &package, &mut result);
        assert_eq!(result.checked_parts, 5, "mutable and missing parts are counted too");
        let found: Vec<(&str, &str, bool)> = result.diagnostics.iter().map(|d| (d.code.as_str(), d.message.as_str(), d.blocking)).collect();
        assert_eq!(
            found,
            vec![
                (diag::IMMUTABLE_PART_CHANGED, "immutable part b.xml changed", true),
                (diag::IMMUTABLE_PART_CHANGED, "immutable part c.xml is missing", true),
            ]
        );
    }

    #[test]
    fn metadata_difference_names_lost_nodes_and_in_place_changes() {
        let before = vec![node("word/document.xml", "a/b", "1"), node("word/styles.xml", "c", "2")];
        assert_eq!(metadata_difference(&before, &before), None);
        let lost = vec![node("word/styles.xml", "c", "2")];
        assert_eq!(metadata_difference(&before, &lost).as_deref(), Some("word/document.xml 1 -> 0"));
        let altered = vec![node("word/document.xml", "a/b", "9"), node("word/styles.xml", "c", "2")];
        assert_eq!(metadata_difference(&before, &altered).as_deref(), Some("word/document.xml content or location changed"));
    }
}
