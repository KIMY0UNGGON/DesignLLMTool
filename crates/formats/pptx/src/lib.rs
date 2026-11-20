//! PresentationML (PPTX) codec for DesignLLMTool.
//!
//! The codec opens an existing `.pptx` into slide pages with mapping records, writes model changes back into a
//! copy of the original package with raw preserving edits, verifies saved bytes by re-reading them, and creates
//! new decks, including the figure export `encode_scene`.
//!
//! Support in this release, in short: text boxes and preset shapes with text, straight lines, PNG/JPEG/GIF/BMP
//! pictures, slide order and insertion/removal, notes text, and theme colours and fonts. An outer shadow of a
//! rectangle or ellipse and a vector path are editable in their canonical forms (see `editable`). Charts, tables,
//! groups, SmartArt, media, comments, other custom geometry, and other objects are preserved byte for byte and are
//! not editable or rendered.

#![forbid(unsafe_code)]

pub mod create;
pub mod editable;
pub mod guard;
pub mod model;
pub mod ns;
pub mod reader;
pub mod text;
pub mod theme;
pub mod v12;
pub mod verify;
pub mod writer;
mod xmlx;

use std::collections::BTreeSet;

use designllm_contracts::{
    AssetRef, Diagnostic, DltError, DltResult, ErrorCode, Node, OpaqueRegion, Page, PreservationManifest, Size,
};
use designllm_package::opc;
use designllm_package::Package;

pub use create::{blank_deck, encode_scene, SCENE_PRESETS};
pub use model::{BodyMap, Mappings, PageMap, ParagraphMap, SegmentMap, ShapeMap, ORIGIN_ADDED, ORIGIN_SOURCE};
pub use text::{ParaProps, RichText, Unit};
pub use verify::{owned_part, verify_bytes, VerifyInput};
pub use writer::WriteInput;

/// Everything `open_deck` produces for the engine to store.
#[derive(Clone, Debug)]
pub struct OpenedDeck {
    pub pages: Vec<Page>,
    /// Page and node mapping records, keyed by `Page.id` and `Node.id`.
    pub mappings: Mappings,
    pub slide_size: Size,
    /// Preserved parts and elements: part, anchor, and hash.
    pub opaque: Vec<OpaqueRegion>,
    /// Payload hashes of every part of the original package.
    pub manifest: PreservationManifest,
    /// Non-blocking findings from opening (unmodelled objects, unsupported render features, and so on).
    pub diagnostics: Vec<Diagnostic>,
}

/// Opens a PresentationML package. Malformed, unsafe, or structurally broken packages are refused. Objects
/// that the model does not cover are preserved and reported.
pub fn open_deck(package: &Package, put: &dyn Fn(&[u8], &str) -> DltResult<AssetRef>) -> DltResult<OpenedDeck> {
    guard::check_package(package)?;
    let report = opc::verify(package, ns::EXPECTED_TARGET_TYPES);
    if !report.errors.is_empty() {
        let mut err = DltError::new(ErrorCode::InvalidArgument, "the package structure is broken; it cannot be opened safely");
        err.diagnostics = report
            .errors
            .iter()
            .map(|e| Diagnostic { code: "PPTX_OPC_ERROR".into(), message: e.clone(), node_id: None, blocking: true, params: Default::default() })
            .collect();
        return Err(err);
    }
    let (presentation, read_pages) = reader::read_presentation(package, put)?;
    let mut diagnostics = presentation.diagnostics.clone();
    diagnostics.extend(report.warnings.iter().map(|w| Diagnostic {
        code: "PPTX_OPC_WARNING".into(),
        message: w.clone(),
        node_id: None,
        blocking: false,
        params: Default::default(),
    }));
    let mut mappings = Mappings::new();
    let mut opaque = presentation.opaque.clone();
    let mut pages = Vec::with_capacity(read_pages.len());
    for read in read_pages {
        mappings.insert(read.page.id.clone(), model::to_value(&read.map)?);
        let mut nodes = Vec::with_capacity(read.shapes.len());
        for shape in &read.shapes {
            mappings.insert(shape.node.id.clone(), model::to_value(&shape.map)?);
            nodes.push(shape.node.clone());
        }
        opaque.extend(read.opaque.iter().cloned());
        diagnostics.extend(read.diagnostics.iter().cloned());
        pages.push(Page { nodes, notes: read.page.notes, name: read.page.name, ..Page::new(read.page.id, read.page.size) });
    }
    Ok(OpenedDeck {
        pages,
        mappings,
        slide_size: presentation.slide_size,
        opaque,
        manifest: package.preservation_manifest(),
        diagnostics,
    })
}

/// Writes the model into a copy of `base` and returns the losses of the objects it wrote: non-blocking `PPTX_LOSS`
/// entries for values that PowerPoint draws differently. The result is ready to encode and then verify.
pub fn write_deck_reporting(
    base: &Package,
    pages: &[Page],
    mappings: &Mappings,
    assets: &dyn designllm_contracts::AssetStore,
) -> DltResult<(Package, Vec<Diagnostic>)> {
    writer::write_pptx(&WriteInput { base, pages, mappings, assets })
}

/// Writes the model into a copy of `base`. The losses are not returned; an engine that saves a document uses
/// `write_deck_reporting`.
pub fn write_deck(base: &Package, pages: &[Page], mappings: &Mappings, assets: &dyn designllm_contracts::AssetStore) -> DltResult<Package> {
    write_deck_reporting(base, pages, mappings, assets).map(|(package, _)| package)
}

/// Stores rich text on a node and its mapping record, and keeps the node's font fields in step with its first
/// character. The paragraph structure is rebuilt from `rich`.
pub fn store_rich_text(node: &mut Node, map: &mut ShapeMap, rich: &RichText) {
    let (text, paragraphs) = rich.to_parts();
    node.text = text;
    map.paragraphs = paragraphs;
    let first_char = rich.paragraphs.iter().flat_map(|p| p.units.iter()).find_map(|u| match u {
        Unit::Char { style, .. } => Some(style.clone()),
        _ => None,
    });
    if let Some(style) = first_char {
        node.style.font_family = style.font_family;
        node.style.font_size = style.font_size;
        node.style.color = style.color;
        node.style.bold = style.bold;
        node.style.italic = style.italic;
    }
    if let Some(first) = rich.paragraphs.first() {
        node.style.align = first.props.align.clone();
    }
}

/// Sorted `p:sldId` values of an existing presentation, so new ids never collide with them.
pub fn existing_sld_ids(base: &Package) -> DltResult<BTreeSet<u32>> {
    let (doc, _) = xmlx::parse(base.part(ns::PRESENTATION_PART)?)?;
    let root = doc.root_element();
    let mut out = BTreeSet::new();
    if let Some(list) = xmlx::child(root, ns::NS_P, "sldIdLst") {
        for entry in xmlx::children(list, ns::NS_P, "sldId") {
            if let Some(id) = xmlx::attr_i64(entry, "id").and_then(|v| u32::try_from(v).ok()) {
                out.insert(id);
            }
        }
    }
    Ok(out)
}

/// Identifiers for a new slide: `p:sldId` (at least 256 and above every existing id), the slide part, and
/// the notes part when the page has notes. Names avoid every part of `base` and every part recorded in mappings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageAllocation {
    pub sld_id: u32,
    pub part: String,
    pub notes_part: Option<String>,
}

pub fn allocate_page(base: &Package, mappings: &Mappings, with_notes: bool) -> DltResult<PageAllocation> {
    let mut used_ids = existing_sld_ids(base)?;
    let mut used_parts: BTreeSet<String> = base.names().into_iter().map(|n| n.to_ascii_lowercase()).collect();
    for value in mappings.values() {
        if let Ok(page) = serde_json::from_value::<PageMap>(value.clone()) {
            used_ids.insert(page.sld_id);
            used_parts.insert(page.part.to_ascii_lowercase());
            if let Some(notes) = page.notes_part {
                used_parts.insert(notes.to_ascii_lowercase());
            }
        }
    }
    let sld_id = used_ids.iter().next_back().map_or(256, |max| (*max + 1).max(256));
    let part = first_free_part("ppt/slides/slide", ".xml", &used_parts);
    let notes_part = with_notes.then(|| first_free_part("ppt/notesSlides/notesSlide", ".xml", &used_parts));
    Ok(PageAllocation { sld_id, part, notes_part })
}

fn first_free_part(prefix: &str, suffix: &str, used: &BTreeSet<String>) -> String {
    let mut number = 1usize;
    loop {
        let candidate = format!("{prefix}{number}{suffix}");
        if !used.contains(&candidate.to_ascii_lowercase()) {
            return candidate;
        }
        number += 1;
    }
}

/// Media part for a picture. An identical picture already in `base` is reused. Otherwise the name is derived
/// from the content hash, so the same picture always gets the same name.
pub fn allocate_media(base: &Package, asset: &AssetRef) -> DltResult<String> {
    let extension = ns::extension_for_mime(&asset.mime)
        .ok_or_else(|| DltError::unsupported(format!("picture type {} cannot be written to PPTX", asset.mime)))?;
    for name in base.names() {
        if name.starts_with("ppt/media/") {
            if let Ok(bytes) = base.part(&name) {
                if designllm_contracts::sha256(bytes) == asset.sha256 {
                    return Ok(name);
                }
            }
        }
    }
    let name = format!("ppt/media/image-{}.{extension}", &asset.sha256[..16]);
    if base.has_part(&name) {
        return Err(DltError::new(ErrorCode::IntegrityError, format!("media name {name} is already used by different content")));
    }
    Ok(name)
}

/// Mapping record for a shape the editor adds. The shape id follows the page's current largest id.
pub fn new_shape_map(kind: &str, cnvpr_id: u32, geometry: &str, text_box: bool, rich: &RichText, body: BodyMap) -> ShapeMap {
    ShapeMap {
        kind: kind.to_string(),
        origin: ORIGIN_ADDED.to_string(),
        cnvpr_id,
        geometry: geometry.to_string(),
        text_box,
        has_text_body: !rich.paragraphs.is_empty() || kind == model::KIND_SHAPE,
        paragraphs: rich.to_parts().1,
        body,
        media_part: None,
        rel_id: None,
        flip_h: false,
        flip_v: false,
        rotation: 0,
        arrow: false,
        unmapped_head_end: false,
        unmapped_tail_end: false,
        custom_head_end_size: false,
        custom_tail_end_size: false,
        placeholder: None,
        render_notes: Vec::new(),
        anchor: String::new(),
    }
}

/// Verifies saved bytes with a mapping-based input. Convenience for callers that hold the artifact pieces.
pub fn verify_saved(pages: &[Page], mappings: &Mappings, slide_size: Size, manifest: &PreservationManifest, opaque: &[OpaqueRegion], bytes: &[u8]) -> designllm_contracts::PreservationVerification {
    verify_bytes(&VerifyInput { pages, mappings, slide_size, manifest, opaque }, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_free_part_skips_used_names_case_insensitively() {
        let used: BTreeSet<String> = ["ppt/slides/slide1.xml".to_string()].into_iter().collect();
        assert_eq!(first_free_part("ppt/slides/slide", ".xml", &used), "ppt/slides/slide2.xml");
    }
}
