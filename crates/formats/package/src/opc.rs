//! Open Packaging Conventions helpers: content types, relationship parts, target resolution, and a
//! structural verifier.
//!
//! The verifier checks: every part has a content type; every Override names an existing part; every
//! relationship part has an existing owner and unique ids; every internal relationship target exists;
//! relationship types listed by the caller have the expected target content type; every `r:*` attribute
//! names a relationship of its own part; and unreachable parts are reported as warnings.
//! It is not an ECMA-376 conformance suite.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use designllm_contracts::{sha256, DltError, DltResult, ErrorCode};
use roxmltree::{Document, Node};

use crate::package::{error, Package, DIAG_UNSAFE_NAME};
use crate::xml::text_and_offset;

pub const CONTENT_TYPES_PART: &str = "[Content_Types].xml";
pub const NS_CONTENT_TYPES: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
pub const NS_PACKAGE_RELATIONSHIPS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
pub const NS_OFFICE_RELATIONSHIPS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
pub const REL_DIGITAL_SIGNATURE_ORIGIN: &str = "http://schemas.openxmlformats.org/package/2006/relationships/digital-signature/origin";

// OOXML names shared by more than one format engine (DOCX, PPTX). Each value is defined only here and the engines
// re-export it; format-specific names stay in their engine.
pub const NS_A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
pub const NS_MC: &str = "http://schemas.openxmlformats.org/markup-compatibility/2006";
pub const REL_OFFICE_DOCUMENT: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
/// Relationship type of images in a package.
pub const REL_IMAGE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";
pub const REL_CORE_PROPS: &str = "http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties";
pub const REL_EXT_PROPS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties";
pub const CT_CORE_PROPS: &str = "application/vnd.openxmlformats-package.core-properties+xml";
pub const CT_EXT_PROPS: &str = "application/vnd.openxmlformats-officedocument.extended-properties+xml";

/// Content type defaults (by extension) and overrides (by part name, without the leading slash).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContentTypes {
    /// Lowercase extension without a dot, to content type.
    pub defaults: BTreeMap<String, String>,
    /// Part name without a leading slash, to content type.
    pub overrides: BTreeMap<String, String>,
}

impl ContentTypes {
    pub fn parse(xml: &[u8]) -> DltResult<Self> {
        let (text, _) = text_and_offset(xml)?;
        let doc = Document::parse(text).map_err(|e| malformed(format!("{CONTENT_TYPES_PART}: {e}")))?;
        let root = doc.root_element();
        if !root.has_tag_name((NS_CONTENT_TYPES, "Types")) {
            return Err(malformed(format!("{CONTENT_TYPES_PART} root is not Types")));
        }
        let mut out = Self::default();
        let mut override_keys_lower = BTreeSet::new();
        for node in root.children().filter(|n| n.is_element()) {
            if node.has_tag_name((NS_CONTENT_TYPES, "Default")) {
                let extension = required_attr(node, "Extension")?.to_ascii_lowercase();
                let content_type = required_attr(node, "ContentType")?.to_string();
                if out.defaults.insert(extension.clone(), content_type).is_some() {
                    return Err(malformed(format!("duplicate Default for extension {extension}")));
                }
            } else if node.has_tag_name((NS_CONTENT_TYPES, "Override")) {
                let part = required_attr(node, "PartName")?.trim_start_matches('/').to_string();
                let content_type = required_attr(node, "ContentType")?.to_string();
                if !override_keys_lower.insert(part.to_ascii_lowercase()) {
                    return Err(malformed(format!("duplicate Override for {part}")));
                }
                out.overrides.insert(part, content_type);
            } else {
                return Err(malformed(format!("unexpected element {} in {CONTENT_TYPES_PART}", node.tag_name().name())));
            }
        }
        Ok(out)
    }

    /// Content type of a part: Override first (case-insensitive), then Default by extension.
    pub fn content_type_for(&self, part: &str) -> Option<&str> {
        if let Some((_, value)) = self.overrides.iter().find(|(name, _)| name.eq_ignore_ascii_case(part)) {
            return Some(value.as_str());
        }
        let file = part.rsplit('/').next().unwrap_or(part);
        let (_, extension) = file.rsplit_once('.')?;
        self.defaults.get(&extension.to_ascii_lowercase()).map(String::as_str)
    }

    pub fn set_override(&mut self, part: &str, content_type: &str) {
        let key = part.trim_start_matches('/').to_string();
        self.overrides.retain(|name, _| !name.eq_ignore_ascii_case(&key));
        self.overrides.insert(key, content_type.to_string());
    }

    pub fn remove_override(&mut self, part: &str) {
        self.overrides.retain(|name, _| !name.eq_ignore_ascii_case(part.trim_start_matches('/')));
    }

    pub fn set_default(&mut self, extension: &str, content_type: &str) {
        self.defaults.insert(extension.to_ascii_lowercase(), content_type.to_string());
    }

    /// Serializes in canonical order (defaults, then overrides, both sorted).
    pub fn to_xml(&self) -> Vec<u8> {
        let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n");
        out.push_str(&format!("<Types xmlns=\"{NS_CONTENT_TYPES}\">"));
        for (extension, content_type) in &self.defaults {
            out.push_str(&format!(
                "<Default Extension=\"{}\" ContentType=\"{}\"/>",
                escape_attr(extension),
                escape_attr(content_type)
            ));
        }
        for (part, content_type) in &self.overrides {
            out.push_str(&format!(
                "<Override PartName=\"/{}\" ContentType=\"{}\"/>",
                escape_attr(part),
                escape_attr(content_type)
            ));
        }
        out.push_str("</Types>");
        out.into_bytes()
    }
}

/// One relationship entry of a `.rels` part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relationship {
    pub id: String,
    pub rel_type: String,
    pub target: String,
    pub external: bool,
}

/// Parses a `.rels` part. Duplicate ids and unexpected elements are errors.
pub fn parse_rels(xml: &[u8]) -> DltResult<Vec<Relationship>> {
    let (text, _) = text_and_offset(xml)?;
    let doc = Document::parse(text).map_err(|e| malformed(format!("relationship part: {e}")))?;
    let root = doc.root_element();
    if !root.has_tag_name((NS_PACKAGE_RELATIONSHIPS, "Relationships")) {
        return Err(malformed("relationship part root is not Relationships"));
    }
    let mut out = Vec::new();
    let mut ids = BTreeSet::new();
    for node in root.children().filter(|n| n.is_element()) {
        if !node.has_tag_name((NS_PACKAGE_RELATIONSHIPS, "Relationship")) {
            return Err(malformed(format!("unexpected element {} in relationships", node.tag_name().name())));
        }
        let rel = Relationship {
            id: required_attr(node, "Id")?.to_string(),
            rel_type: required_attr(node, "Type")?.to_string(),
            target: required_attr(node, "Target")?.to_string(),
            external: node.attribute("TargetMode") == Some("External"),
        };
        if !ids.insert(rel.id.clone()) {
            return Err(malformed(format!("duplicate relationship id {}", rel.id)));
        }
        out.push(rel);
    }
    Ok(out)
}

/// Detects OPC digital-signature markers without assuming a conventional signature part path.
/// This detects presence only; it does not validate a signature.
pub fn has_digital_signature(package: &Package) -> DltResult<bool> {
    if package.has_part("_rels/.rels") {
        let rels = parse_rels(package.part("_rels/.rels")?)?;
        if rels.iter().any(|r| r.rel_type == REL_DIGITAL_SIGNATURE_ORIGIN || r.rel_type.ends_with("/digital-signature/origin")) {
            return Ok(true);
        }
    }

    if !package.has_part(CONTENT_TYPES_PART) {
        return Ok(false);
    }
    let types = ContentTypes::parse(package.part(CONTENT_TYPES_PART)?)?;
    Ok(package.names().iter().filter(|name| !name.ends_with('/')).any(|name| {
        types.content_type_for(name).is_some_and(|kind| kind.to_ascii_lowercase().contains("digital-signature"))
    }))
}

/// Serializes relationships in the given order.
pub fn rels_to_xml(rels: &[Relationship]) -> Vec<u8> {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n");
    out.push_str(&format!("<Relationships xmlns=\"{NS_PACKAGE_RELATIONSHIPS}\">"));
    for rel in rels {
        out.push_str(&format!(
            "<Relationship Id=\"{}\" Type=\"{}\" Target=\"{}\"{}/>",
            escape_attr(&rel.id),
            escape_attr(&rel.rel_type),
            escape_attr(&rel.target),
            if rel.external { " TargetMode=\"External\"" } else { "" }
        ));
    }
    out.push_str("</Relationships>");
    out.into_bytes()
}

/// Relationship part name for an owner part. The package root owner is the empty string.
pub fn rels_part_for(owner: &str) -> String {
    if owner.is_empty() {
        return "_rels/.rels".to_string();
    }
    match owner.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{owner}.rels"),
    }
}

/// Owner part of a relationship part name, or `None` when the name is not a relationship part.
pub fn owner_for_rels(name: &str) -> Option<String> {
    if name == "_rels/.rels" {
        return Some(String::new());
    }
    let (dir, file) = name.rsplit_once('/')?;
    let base = file.strip_suffix(".rels")?;
    if dir == "_rels" {
        return Some(base.to_string());
    }
    let parent = dir.strip_suffix("/_rels")?;
    Some(format!("{parent}/{base}"))
}

/// Resolves a relationship target against its owner. The result has no leading slash.
/// A target that climbs above the package root is an error, not a clamp.
pub fn resolve_target(owner: &str, target: &str) -> DltResult<String> {
    let target = target.split(['#', '?']).next().unwrap_or(target);
    let joined = if let Some(absolute) = target.strip_prefix('/') {
        absolute.to_string()
    } else {
        let base = owner.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
        if base.is_empty() {
            target.to_string()
        } else {
            format!("{base}/{target}")
        }
    };
    normalize(&joined, target)
}

fn normalize(path: &str, original: &str) -> DltResult<String> {
    let mut out: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if out.pop().is_none() {
                    return Err(error(
                        ErrorCode::InvalidArgument,
                        DIAG_UNSAFE_NAME,
                        format!("relationship target {original:?} leaves the package"),
                    ));
                }
            }
            other => out.push(other),
        }
    }
    Ok(out.join("/"))
}

/// Result of [`verify`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpcReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub parts: Vec<OpcPart>,
    pub relationships: Vec<OpcRelationship>,
}

impl OpcReport {
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpcPart {
    pub name: String,
    pub content_type: Option<String>,
    pub size: usize,
    pub sha256: String,
    pub reachable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpcRelationship {
    pub owner: String,
    pub id: String,
    pub rel_type: String,
    pub target: String,
    pub resolved: Option<String>,
    pub external: bool,
}

/// Verifies package structure. `expected_target_types` lists relationship type URIs that must point to a
/// part with the given content type (for example, a format's office document and slide types).
pub fn verify(package: &Package, expected_target_types: &[(&str, &str)]) -> OpcReport {
    let mut report = OpcReport::default();
    let part_names: BTreeSet<String> = package
        .names()
        .into_iter()
        .filter(|n| !n.ends_with('/'))
        .collect();

    let content_types = match package.part(CONTENT_TYPES_PART) {
        Err(_) => {
            report.errors.push(format!("missing {CONTENT_TYPES_PART}"));
            None
        }
        Ok(bytes) => match ContentTypes::parse(bytes) {
            Ok(ct) => Some(ct),
            Err(e) => {
                report.errors.push(format!("{CONTENT_TYPES_PART}: {}", e.message));
                None
            }
        },
    };

    let mut rels_by_owner: BTreeMap<String, Vec<Relationship>> = BTreeMap::new();
    for name in &part_names {
        let Some(owner) = owner_for_rels(name) else { continue };
        let bytes = package.part(name).unwrap_or(&[]);
        match parse_rels(bytes) {
            Ok(rels) => {
                if !owner.is_empty() && !part_names.contains(&owner) {
                    report.errors.push(format!("{name}: owner part {owner} is missing"));
                }
                rels_by_owner.insert(owner, rels);
            }
            Err(e) => report.errors.push(format!("{name}: {}", e.message)),
        }
    }

    for (owner, rels) in &rels_by_owner {
        for rel in rels {
            if rel.external {
                report.relationships.push(OpcRelationship {
                    owner: owner_label(owner).to_string(),
                    id: rel.id.clone(),
                    rel_type: rel.rel_type.clone(),
                    target: rel.target.clone(),
                    resolved: None,
                    external: true,
                });
                continue;
            }
            let resolved = match resolve_target(owner, &rel.target) {
                Ok(path) => path,
                Err(e) => {
                    report.errors.push(format!("unsafe relationship {} {}: {}", owner_label(owner), rel.id, e.message));
                    continue;
                }
            };
            if !part_names.contains(&resolved) {
                report.errors.push(format!("dangling relationship {} {} -> {resolved}", owner_label(owner), rel.id));
            } else if let Some((_, expected)) = expected_target_types.iter().find(|(t, _)| *t == rel.rel_type) {
                let actual = content_types.as_ref().and_then(|ct| ct.content_type_for(&resolved));
                if actual != Some(*expected) {
                    report.errors.push(format!(
                        "content type mismatch for {resolved}: expected {expected}, found {}",
                        actual.unwrap_or("(none)")
                    ));
                }
            }
            report.relationships.push(OpcRelationship {
                owner: owner_label(owner).to_string(),
                id: rel.id.clone(),
                rel_type: rel.rel_type.clone(),
                target: rel.target.clone(),
                resolved: Some(resolved),
                external: false,
            });
        }
    }

    let mut reachable: BTreeSet<String> = BTreeSet::new();
    let mut visited_owners: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::from([String::new()]);
    while let Some(owner) = queue.pop_front() {
        if !visited_owners.insert(owner.clone()) {
            continue;
        }
        if let Some(rels) = rels_by_owner.get(&owner) {
            for rel in rels.iter().filter(|r| !r.external) {
                if let Ok(target) = resolve_target(&owner, &rel.target) {
                    if part_names.contains(&target) {
                        reachable.insert(target.clone());
                        queue.push_back(target);
                    }
                }
            }
        }
    }

    for name in &part_names {
        let bytes = package.part(name).unwrap_or(&[]);
        let is_rels = owner_for_rels(name).is_some();
        let is_content_types = name == CONTENT_TYPES_PART;
        let content_type = if is_content_types {
            None
        } else {
            content_types.as_ref().and_then(|ct| ct.content_type_for(name)).map(str::to_string)
        };
        if !is_content_types && content_type.is_none() {
            report.errors.push(format!("no content type for part {name}"));
        }
        if !is_rels && !is_content_types && !reachable.contains(name) {
            report.warnings.push(format!("unreachable part {name}"));
        }
        let xml_part = content_type.as_deref().is_some_and(is_xml_content_type);
        if !is_rels && !is_content_types && xml_part {
            check_xml_part(name, bytes, rels_by_owner.get(name), &mut report);
        }
        report.parts.push(OpcPart {
            name: name.clone(),
            content_type,
            size: bytes.len(),
            sha256: sha256(bytes),
            reachable: reachable.contains(name),
        });
    }

    let office_documents = rels_by_owner
        .get("")
        .map(|rels| rels.iter().filter(|r| r.rel_type.ends_with("/officeDocument")).count())
        .unwrap_or(0);
    if office_documents != 1 {
        report.errors.push(format!(
            "package root must have exactly one officeDocument relationship, found {office_documents}"
        ));
    }
    report
}

fn check_xml_part(name: &str, bytes: &[u8], rels: Option<&Vec<Relationship>>, report: &mut OpcReport) {
    let text = match std::str::from_utf8(bytes) {
        Ok(t) => t,
        Err(e) => {
            report.errors.push(format!("{name}: not UTF-8: {e}"));
            return;
        }
    };
    let doc = match Document::parse(text) {
        Ok(d) => d,
        Err(e) => {
            report.errors.push(format!("{name}: malformed XML: {e}"));
            return;
        }
    };
    let ids: BTreeSet<&str> = rels.map(|r| r.iter().map(|rel| rel.id.as_str()).collect()).unwrap_or_default();
    for node in doc.descendants().filter(|n| n.is_element()) {
        for attr in node.attributes() {
            if attr.namespace() == Some(NS_OFFICE_RELATIONSHIPS) && !ids.contains(attr.value()) {
                report.errors.push(format!(
                    "{name}: r:{} = {:?} names no relationship of this part",
                    attr.name(),
                    attr.value()
                ));
            }
        }
    }
}

fn is_xml_content_type(content_type: &str) -> bool {
    content_type == "application/xml" || content_type.ends_with("+xml")
}

fn owner_label(owner: &str) -> &str {
    if owner.is_empty() {
        "/"
    } else {
        owner
    }
}

fn required_attr<'a>(node: Node<'a, '_>, name: &str) -> DltResult<&'a str> {
    node.attribute(name).ok_or_else(|| malformed(format!("missing attribute {name}")))
}

fn malformed(message: impl Into<String>) -> DltError {
    error(ErrorCode::InvalidArgument, crate::package::DIAG_MALFORMED, message)
}

/// Escapes an attribute value for double-quoted output.
fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared OOXML names keep the exact strings the DOCX and PPTX engines defined before task 49k.
    #[test]
    fn shared_ooxml_names_keep_their_values() {
        assert_eq!(CONTENT_TYPES_PART, "[Content_Types].xml");
        assert_eq!(NS_OFFICE_RELATIONSHIPS, "http://schemas.openxmlformats.org/officeDocument/2006/relationships");
        assert_eq!(NS_A, "http://schemas.openxmlformats.org/drawingml/2006/main");
        assert_eq!(NS_MC, "http://schemas.openxmlformats.org/markup-compatibility/2006");
        assert_eq!(REL_OFFICE_DOCUMENT, "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument");
        assert_eq!(REL_IMAGE, "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image");
        assert_eq!(REL_CORE_PROPS, "http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties");
        assert_eq!(REL_EXT_PROPS, "http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties");
        assert_eq!(CT_CORE_PROPS, "application/vnd.openxmlformats-package.core-properties+xml");
        assert_eq!(CT_EXT_PROPS, "application/vnd.openxmlformats-officedocument.extended-properties+xml");
    }

    #[test]
    fn rels_part_and_owner_are_inverse() {
        assert_eq!(rels_part_for(""), "_rels/.rels");
        assert_eq!(rels_part_for("ppt/presentation.xml"), "ppt/_rels/presentation.xml.rels");
        assert_eq!(owner_for_rels("ppt/_rels/presentation.xml.rels").as_deref(), Some("ppt/presentation.xml"));
        assert_eq!(owner_for_rels("_rels/.rels").as_deref(), Some(""));
        assert_eq!(owner_for_rels("ppt/presentation.xml"), None);
    }

    #[test]
    fn resolve_target_handles_relative_absolute_and_escapes() {
        assert_eq!(resolve_target("ppt/slides/slide1.xml", "../slideLayouts/slideLayout1.xml").unwrap(), "ppt/slideLayouts/slideLayout1.xml");
        assert_eq!(resolve_target("ppt/presentation.xml", "slides/slide1.xml").unwrap(), "ppt/slides/slide1.xml");
        assert_eq!(resolve_target("ppt/presentation.xml", "/docProps/core.xml").unwrap(), "docProps/core.xml");
        assert_eq!(resolve_target("", "ppt/presentation.xml").unwrap(), "ppt/presentation.xml");
        assert_eq!(resolve_target("ppt/slides/slide1.xml", "../../x.xml").unwrap(), "x.xml");
        assert!(resolve_target("ppt/slides/slide1.xml", "../../../x.xml").is_err());
        assert!(resolve_target("", "../x.xml").is_err());
    }

    #[test]
    fn content_types_prefer_override_then_default_and_roundtrip() {
        let mut ct = ContentTypes::default();
        ct.set_default("xml", "application/xml");
        ct.set_override("/ppt/slides/slide1.xml", "S");
        assert_eq!(ct.content_type_for("ppt/slides/slide1.xml"), Some("S"));
        assert_eq!(ct.content_type_for("PPT/Slides/Slide1.XML"), Some("S"));
        assert_eq!(ct.content_type_for("customXml/item1.xml"), Some("application/xml"));
        assert_eq!(ct.content_type_for("misc/blob.bin"), None);
        let reparsed = ContentTypes::parse(&ct.to_xml()).unwrap();
        assert_eq!(reparsed, ct);
    }

    #[test]
    fn rels_roundtrip_preserves_external_flag_and_rejects_duplicate_ids() {
        let rels = vec![
            Relationship { id: "rId1".into(), rel_type: "urn:t".into(), target: "a/b.xml".into(), external: false },
            Relationship { id: "rId2".into(), rel_type: "urn:h".into(), target: "https://example.invalid/?a=1&b=2".into(), external: true },
        ];
        let xml = rels_to_xml(&rels);
        assert_eq!(parse_rels(&xml).unwrap(), rels);
        let dup = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="r" Type="t" Target="a"/><Relationship Id="r" Type="t" Target="b"/></Relationships>"#;
        assert!(parse_rels(dup).is_err());
    }

    fn package_for_signature_test() -> Package {
        let mut package = Package::new();
        let mut types = ContentTypes::default();
        types.set_default("xml", "application/xml");
        types.set_default("rels", "application/vnd.openxmlformats-package.relationships+xml");
        types.set_override("word/document.xml", "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml");
        package.set_part(CONTENT_TYPES_PART, types.to_xml()).unwrap();
        package
            .set_part(
                "_rels/.rels",
                rels_to_xml(&[Relationship {
                    id: "rIdDoc".into(),
                    rel_type: format!("{NS_OFFICE_RELATIONSHIPS}/officeDocument"),
                    target: "word/document.xml".into(),
                    external: false,
                }]),
            )
            .unwrap();
        package.set_part("word/document.xml", b"<w:document/>".to_vec()).unwrap();
        package
    }

    #[test]
    fn detects_signature_origin_relationship_at_a_nonstandard_part_path() {
        let mut package = package_for_signature_test();
        let mut rels = parse_rels(package.part("_rels/.rels").unwrap()).unwrap();
        rels.push(Relationship {
            id: "rIdSignature".into(),
            rel_type: REL_DIGITAL_SIGNATURE_ORIGIN.into(),
            target: "metadata/owner-record.bin".into(),
            external: false,
        });
        package.set_part("_rels/.rels", rels_to_xml(&rels)).unwrap();
        let mut types = ContentTypes::parse(package.part(CONTENT_TYPES_PART).unwrap()).unwrap();
        types.set_override("metadata/owner-record.bin", "application/vnd.openxmlformats-package.digital-signature-origin");
        package.set_part(CONTENT_TYPES_PART, types.to_xml()).unwrap();
        package.set_part("metadata/owner-record.bin", b"synthetic origin marker".to_vec()).unwrap();
        assert!(has_digital_signature(&package).unwrap());
    }

    #[test]
    fn detects_digital_signature_content_type_without_a_signature_origin_relationship() {
        let mut package = package_for_signature_test();
        let mut types = ContentTypes::parse(package.part(CONTENT_TYPES_PART).unwrap()).unwrap();
        types.set_override("vault/arbitrary-name.xml", "application/vnd.openxmlformats-package.digital-signature-xmlsignature+xml");
        package.set_part(CONTENT_TYPES_PART, types.to_xml()).unwrap();
        package.set_part("vault/arbitrary-name.xml", b"<Signature/>".to_vec()).unwrap();
        assert!(has_digital_signature(&package).unwrap());
    }

    #[test]
    fn unsigned_opc_package_has_no_digital_signature_marker() {
        assert!(!has_digital_signature(&package_for_signature_test()).unwrap());
    }

    #[test]
    fn missing_content_types_without_a_signature_origin_is_not_a_signature_marker() {
        let mut package = package_for_signature_test();
        package.remove_part(CONTENT_TYPES_PART).unwrap();
        assert!(!has_digital_signature(&package).unwrap());
    }

    #[test]
    fn verifier_reports_missing_content_type_and_dangling_relationship() {
        let mut pkg = Package::new();
        pkg.set_part(CONTENT_TYPES_PART, ContentTypes { defaults: BTreeMap::from([("xml".into(), "application/xml".into()), ("rels".into(), "application/vnd.openxmlformats-package.relationships+xml".into())]), overrides: BTreeMap::from([("ppt/presentation.xml".into(), "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml".into())]) }.to_xml()).unwrap();
        pkg.set_part("_rels/.rels", rels_to_xml(&[Relationship { id: "rId1".into(), rel_type: "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument".into(), target: "ppt/presentation.xml".into(), external: false }])).unwrap();
        pkg.set_part("ppt/presentation.xml", b"<p/>".to_vec()).unwrap();
        pkg.set_part("ppt/extra.bin", b"x".to_vec()).unwrap();
        let clean = verify(&pkg, &[]);
        assert_eq!(clean.errors, vec!["no content type for part ppt/extra.bin".to_string()]);
        assert!(clean.warnings.iter().any(|w| w == "unreachable part ppt/extra.bin"));

        pkg.set_part("_rels/.rels", rels_to_xml(&[Relationship { id: "rId1".into(), rel_type: "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument".into(), target: "ppt/missing.xml".into(), external: false }])).unwrap();
        let broken = verify(&pkg, &[]);
        assert!(broken.errors.iter().any(|e| e.starts_with("dangling relationship / rId1")), "{:?}", broken.errors);
    }

    #[test]
    fn verifier_flags_xml_part_referencing_unknown_relationship_id() {
        let mut pkg = Package::new();
        pkg.set_part(CONTENT_TYPES_PART, ContentTypes { defaults: BTreeMap::from([("xml".into(), "application/xml".into()), ("rels".into(), "application/vnd.openxmlformats-package.relationships+xml".into())]), overrides: BTreeMap::from([("ppt/presentation.xml".into(), "application/xml".into())]) }.to_xml()).unwrap();
        pkg.set_part("_rels/.rels", rels_to_xml(&[Relationship { id: "rId1".into(), rel_type: "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument".into(), target: "ppt/presentation.xml".into(), external: false }])).unwrap();
        pkg.set_part("ppt/presentation.xml", br#"<p xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><c r:id="rId9"/></p>"#.to_vec()).unwrap();
        let report = verify(&pkg, &[]);
        assert!(report.errors.iter().any(|e| e.contains("rId9")), "{:?}", report.errors);
    }
}
