//! HWPX container policy over the shared package layer.
//!
//! The shared `Package` owns ZIP handling and raw copies of unmodified entries. This module decides
//! what a valid HWPX is, which parts an engine may change, and which parts are derived previews.
use designllm_contracts::{DltResult, PartFingerprint, PreservationManifest};
use designllm_package::Package;

use crate::diag;
use crate::header::{child_el, child_els, is_el};
use crate::xmlx::{Xml, NS_OPF};

pub const MIMETYPE: &str = "application/hwp+zip";
pub const CONTAINER: &str = "META-INF/container.xml";
pub const CONTAINER_RDF: &str = "META-INF/container.rdf";
const META_MANIFEST: &str = "META-INF/manifest.xml";
const NS_ODF_MANIFEST: &str = "urn:oasis:names:tc:opendocument:xmlns:manifest:1.0";
pub const CONTENT_HPF: &str = "Contents/content.hpf";
pub const HEADER: &str = "Contents/header.xml";
pub const SETTINGS: &str = "settings.xml";
pub const PREVIEW_TEXT: &str = "Preview/PrvText.txt";
pub const PREVIEW_IMAGE: &str = "Preview/PrvImage.png";

/// Parts that an encrypted or signed package carries. Their presence blocks editing.
const PROTECTION_PARTS: &[&str] = &["META-INF/encryption.xml", "META-INF/signatures.xml", "META-INF/digitalsignature.xml"];

/// How an engine may treat a part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Written only through the model (sections, header, manifest).
    Editable,
    /// Regenerated or removed when the content changes (preview parts).
    Derived,
    /// Must stay byte-identical to the source payload.
    Immutable,
}

pub fn role_of(name: &str) -> Role {
    if name == HEADER || name == CONTENT_HPF || section_index(name).is_some() {
        Role::Editable
    } else if name == PREVIEW_TEXT || name == PREVIEW_IMAGE {
        Role::Derived
    } else {
        Role::Immutable
    }
}

/// `N` of `Contents/sectionN.xml`.
pub fn section_index(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("Contents/section")?.strip_suffix(".xml")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Checks the first local file header on the raw bytes: `mimetype`, stored, exact payload.
/// The shared package does not expose compression methods, so this reads the header directly.
pub fn check_mimetype(bytes: &[u8]) -> DltResult<()> {
    let bad = |m: &str| diag::invalid(diag::MIMETYPE_INVALID, format!("mimetype: {m}"));
    if bytes.len() < 30 || bytes[0..4] != [0x50, 0x4b, 0x03, 0x04] {
        return Err(bad("first local header is missing"));
    }
    let u16le = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32le = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let method = u16le(8);
    let flags = u16le(6);
    let name_len = u16le(26) as usize;
    let extra_len = u16le(28) as usize;
    let size = u32le(22) as usize;
    let name_end = 30 + name_len;
    if bytes.len() < name_end + extra_len {
        return Err(bad("first local header is truncated"));
    }
    if &bytes[30..name_end] != b"mimetype" {
        return Err(bad("the first entry is not mimetype"));
    }
    if method != 0 {
        return Err(bad("mimetype must be stored, without compression"));
    }
    if flags & 0x08 != 0 {
        return Err(bad("mimetype must not use a data descriptor"));
    }
    let start = name_end + extra_len;
    match bytes.get(start..start + size) {
        Some(payload) if payload == MIMETYPE.as_bytes() => Ok(()),
        _ => Err(bad("payload is not application/hwp+zip")),
    }
}

/// Opens a package as HWPX: container checks, protection checks, and required parts.
pub fn open(bytes: &[u8]) -> DltResult<Package> {
    check_mimetype(bytes)?;
    let pkg = Package::open(bytes)?;
    if pkg.names().first().map(String::as_str) != Some("mimetype") {
        return Err(diag::invalid(diag::MIMETYPE_INVALID, "mimetype must be the first archive entry"));
    }
    for name in pkg.names() {
        if PROTECTION_PARTS.contains(&name.as_str()) {
            return Err(diag::error(
                designllm_contracts::ErrorCode::UnsupportedOperation,
                diag::ENCRYPTED_OR_SIGNED,
                format!("{name}: encrypted or digitally signed HWPX cannot be edited"),
            ));
        }
    }
    check_odf_manifest_protection(&pkg)?;
    for required in [CONTAINER, CONTENT_HPF, HEADER] {
        if !pkg.has_part(required) {
            return Err(diag::malformed(format!("required part {required} is missing")));
        }
    }
    if !pkg.names().iter().any(|n| section_index(n).is_some()) {
        return Err(diag::malformed("package has no section part"));
    }
    Ok(pkg)
}

/// Refuses ODF manifest data that identifies encryption or whose protection meaning is unknown.
fn check_odf_manifest_protection(pkg: &Package) -> DltResult<()> {
    if !pkg.has_part(META_MANIFEST) {
        return Ok(());
    }
    let bytes = pkg.part(META_MANIFEST)?;
    let source = std::str::from_utf8(bytes)
        .map_err(|e| diag::malformed(format!("{META_MANIFEST} is not UTF-8: {e}")))?;
    let xml = Xml::parse(source).map_err(|e| diag::malformed(format!("{META_MANIFEST}: {e}")))?;
    let root = xml.doc.root_element();
    if !is_el(root, NS_ODF_MANIFEST, "manifest") {
        return Err(diag::malformed(format!(
            "{META_MANIFEST} root must be odf:manifest in the ODF manifest namespace"
        )));
    }

    let unknown = || {
        diag::error(
            designllm_contracts::ErrorCode::UnsupportedOperation,
            diag::ENCRYPTED_OR_SIGNED,
            format!("{META_MANIFEST} contains unrecognized protection metadata; protection status cannot be established"),
        )
    };
    for child in root.children() {
        if child.is_comment() || (child.is_text() && child.text().unwrap_or_default().trim().is_empty()) {
            continue;
        }
        if !child.is_element() || !is_el(child, NS_ODF_MANIFEST, "file-entry") {
            return Err(unknown());
        }
        for entry_child in child.children() {
            if entry_child.is_comment()
                || (entry_child.is_text() && entry_child.text().unwrap_or_default().trim().is_empty())
            {
                continue;
            }
            if !entry_child.is_element() {
                return Err(unknown());
            }
            if is_el(entry_child, NS_ODF_MANIFEST, "encryption-data") {
                return Err(diag::error(
                    designllm_contracts::ErrorCode::UnsupportedOperation,
                    diag::ENCRYPTED_OR_SIGNED,
                    format!("{META_MANIFEST} declares encrypted HWPX content"),
                ));
            }
            return Err(unknown());
        }
    }
    Ok(())
}

/// One manifest item from `Contents/content.hpf`.
#[derive(Debug, Clone)]
pub struct Item {
    pub key: String,
    pub id: String,
    pub href: String,
    pub media_type: String,
    pub embedded: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Manifest {
    pub key: String,
    pub items: Vec<Item>,
    pub spine: Vec<String>,
}

pub fn parse_manifest(xml: &Xml<'_>) -> Result<Manifest, String> {
    let root = xml.doc.root_element();
    if !is_el(root, NS_OPF, "package") {
        return Err("content.hpf root is not opf:package".into());
    }
    let list = child_el(root, NS_OPF, "manifest").ok_or("content.hpf has no opf:manifest")?;
    let mut manifest = Manifest { key: xml.key(list).unwrap_or_default().to_string(), ..Manifest::default() };
    for item in child_els(list, NS_OPF, "item") {
        manifest.items.push(Item {
            key: xml.key(item).unwrap_or_default().to_string(),
            id: item.attribute("id").unwrap_or("").to_string(),
            href: item.attribute("href").unwrap_or("").to_string(),
            media_type: item.attribute("media-type").unwrap_or("").to_string(),
            embedded: item.attribute("isEmbeded") == Some("1"),
        });
    }
    if let Some(spine) = child_el(root, NS_OPF, "spine") {
        manifest.spine = child_els(spine, NS_OPF, "itemref")
            .iter()
            .filter_map(|n| n.attribute("idref").map(str::to_string))
            .collect();
    }
    Ok(manifest)
}

/// Manifest items that name a part of the package. Items naming missing parts are an error.
pub fn check_manifest_targets(pkg: &Package, manifest: &Manifest) -> Result<(), String> {
    for item in &manifest.items {
        if !pkg.has_part(&item.href) {
            return Err(format!("manifest item {} names missing part {}", item.id, item.href));
        }
    }
    Ok(())
}

/// Preservation fingerprints of every part, with immutability decided by [`role_of`].
pub fn fingerprints(pkg: &Package, manifest: &Manifest) -> PreservationManifest {
    let mut pm = pkg.preservation_manifest();
    for (name, fp) in pm.parts.iter_mut() {
        let media = manifest.items.iter().find(|i| &i.href == name).map(|i| i.media_type.clone());
        *fp = PartFingerprint {
            sha256: fp.sha256.clone(),
            content_type: media.or_else(|| fp.content_type.clone()),
            immutable: role_of(name) == Role::Immutable,
        };
    }
    pm
}

/// Section part names in document order (numeric order of their index).
pub fn section_parts(pkg: &Package) -> Vec<String> {
    let mut parts: Vec<(u32, String)> = pkg
        .names()
        .into_iter()
        .filter_map(|n| section_index(&n).map(|i| (i, n)))
        .collect();
    parts.sort();
    parts.into_iter().map(|(_, n)| n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_names_are_recognised_and_ordered() {
        assert_eq!(section_index("Contents/section0.xml"), Some(0));
        assert_eq!(section_index("Contents/section12.xml"), Some(12));
        assert_eq!(section_index("Contents/sectionX.xml"), None);
        assert_eq!(section_index("Contents/section.xml"), None);
        assert_eq!(role_of("settings.xml"), Role::Immutable);
        assert_eq!(role_of("Contents/section0.xml"), Role::Editable);
        assert_eq!(role_of("Preview/PrvText.txt"), Role::Derived);
    }

    #[test]
    fn renamed_plain_zip_fails_the_mimetype_check() {
        assert!(check_mimetype(b"PK\x03\x04garbage-not-hwpx-at-all-and-long-enough-to-pass-length").is_err());
        assert!(check_mimetype(&[]).is_err());
    }
}
