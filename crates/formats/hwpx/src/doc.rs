//! Loads a HWPX package into the owned model used by open, save, and verification.
//!
//! The loader is the single reader. Writing and verifying call it on the bytes they produce, so
//! a model that the writer intends is compared with the model that a fresh read yields.
use designllm_contracts::{
    sha256, AssetStore, DltResult, FlowDocument, OpaqueRegion, PreservationManifest,
};
use designllm_package::Package;

use crate::diag;
use crate::header::{self, is_el, PHeader};
use crate::package::{self, Manifest, CONTENT_HPF, HEADER};
use crate::section::{self, Bins, BinRes, PSection};
use crate::xmlx::{Xml, NS_HH};

/// A byte-identical native region: part, anchor, and payload hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub part: String,
    pub anchor: String,
    pub sha256: String,
}

/// Everything read from one package.
pub struct Loaded {
    pub manifest: Manifest,
    pub header: PHeader,
    pub bins: Bins,
    pub sections: Vec<PSection>,
    pub flow: FlowDocument,
    pub regions: Vec<Region>,
    pub fingerprints: PreservationManifest,
}

fn utf8<'a>(bytes: &'a [u8], part: &str) -> DltResult<&'a str> {
    std::str::from_utf8(bytes).map_err(|e| diag::malformed(format!("{part} is not UTF-8: {e}")))
}

fn parse_error(part: &str, message: String) -> designllm_contracts::DltError {
    diag::malformed(format!("{part}: {message}"))
}

/// Native regions of the header that must stay byte-identical. Style and font entries are excluded
/// because a style may be repointed and a font table may gain entries; both are checked semantically.
fn header_regions(xml: &Xml<'_>) -> Vec<Region> {
    let mut out = Vec::new();
    let root = xml.doc.root_element();
    let mut push = |node: roxmltree::Node<'_, '_>| {
        if let (Some(key), true) = (xml.key(node), node.is_element()) {
            out.push(Region {
                part: HEADER.to_string(),
                anchor: key.to_string(),
                sha256: sha256(xml.raw(node).as_bytes()),
            });
        }
    };
    for child in root.children() {
        if is_el(child, NS_HH, "refList") {
            for list in child.children().filter(|c| c.is_element()) {
                let local = list.tag_name().name();
                match local {
                    "fontfaces" | "styles" => {}
                    "charProperties" | "paraProperties" | "borderFills" | "tabProperties" | "numberings" => {
                        for entry in list.children().filter(|c| c.is_element()) {
                            push(entry);
                        }
                    }
                    _ => push(list),
                }
            }
        } else if child.is_element() {
            push(child);
        }
    }
    out
}

/// Loads the package. Images are put into `assets` so that layout and export can read them.
pub fn load_package(pkg: &Package, assets: &dyn AssetStore) -> DltResult<Loaded> {
    let hpf = utf8(pkg.part(CONTENT_HPF)?, CONTENT_HPF)?;
    let manifest = {
        let xml = Xml::parse(hpf).map_err(|e| parse_error(CONTENT_HPF, e))?;
        package::parse_manifest(&xml).map_err(|e| parse_error(CONTENT_HPF, e))?
    };
    package::check_manifest_targets(pkg, &manifest).map_err(diag::malformed)?;

    let mut bins: Bins = Bins::new();
    for item in &manifest.items {
        if item.href.starts_with("BinData/") || item.embedded {
            let bytes = pkg.part(&item.href)?;
            let asset = assets.put(bytes, &item.media_type)?;
            bins.insert(
                item.id.clone(),
                BinRes { asset },
            );
        }
    }

    let header_src = utf8(pkg.part(HEADER)?, HEADER)?;
    let mut regions = Vec::new();
    let header = {
        let xml = Xml::parse(header_src).map_err(|e| parse_error(HEADER, e))?;
        let header = header::parse(&xml).map_err(|e| parse_error(HEADER, e))?;
        regions.extend(header_regions(&xml));
        header
    };

    let mut sections = Vec::new();
    for part in package::section_parts(pkg) {
        let src = utf8(pkg.part(&part)?, &part)?;
        let xml = Xml::parse(src).map_err(|e| parse_error(&part, e))?;
        let parsed = section::parse_section(&xml, &part, &header, &bins).map_err(|e| parse_error(&part, e))?;
        for rec in &parsed.opaque {
            let node = xml
                .node(&rec.key)
                .ok_or_else(|| parse_error(&part, format!("opaque anchor {} lost", rec.key)))?;
            regions.push(Region { part: part.clone(), anchor: rec.key.clone(), sha256: sha256(xml.raw(node).as_bytes()) });
        }
        sections.push(parsed);
    }
    if sections.is_empty() {
        return Err(diag::malformed("package has no section part"));
    }

    let flow = FlowDocument {
        sections: sections.iter().map(section::flow_section).collect(),
        styles: header.named_styles(),
    };
    let mut fingerprints = package::fingerprints(pkg, &manifest);
    fingerprints.opaque_regions = regions
        .iter()
        .map(|r| OpaqueRegion { part: r.part.clone(), anchor: r.anchor.clone(), sha256: r.sha256.clone() })
        .collect();
    Ok(Loaded { manifest, header, bins, sections, flow, regions, fingerprints })
}

/// Opens bytes as HWPX and loads them.
pub fn load_bytes(bytes: &[u8], assets: &dyn AssetStore) -> DltResult<(Package, Loaded)> {
    let pkg = package::open(bytes)?;
    let loaded = load_package(&pkg, assets)?;
    Ok((pkg, loaded))
}

