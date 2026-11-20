//! Writes the model back into a copy of the original package.
//!
//! The original bytes are the base. Each modelled shape is compared with its source state, which the same
//! reader produced when the file was opened. Only the differing spans are replaced. Unmodelled content,
//! unchanged paragraphs, and unchanged parts keep their original bytes. New shapes, pages, notes, and
//! pictures are generated from the model. Mapping records supply every shape and page identifier, so the
//! writer allocates only relationship ids.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use designllm_contracts::{
    sha256, AssetRef, AssetStore, Diagnostic, DltError, DltResult, ErrorCode, LineEnd, Node, NodeKind, Page, Rect, Shadow, Style,
};
use designllm_package::opc::{self, ContentTypes, Relationship};
use designllm_package::xml::{self as pxml, Splice};
use designllm_package::Package;
use roxmltree::{Document, Node as XNode};

use crate::editable;
use crate::model::{self, BodyMap, Mappings, PageMap, ShapeMap, ORIGIN_ADDED, ORIGIN_SOURCE};
use crate::ns::{self, NS_A, NS_P, NS_R};
use crate::reader::{align_attr, read_presentation, ParaSpans, ReadNotes, ReadPage, ReadShape, ShapeSpans, UnitKind, UnitSpans};
use crate::text::{ParaProps, Para, RichText, Unit, BREAK_CHAR};
use crate::theme::{normalize_hex, run_style, FALLBACK_FONT};
use crate::xmlx::{self, child, elements, malformed, span};

/// Everything the writer needs for one save.
pub struct WriteInput<'a> {
    /// The original package, from the immutable asset.
    pub base: &'a Package,
    /// Current pages in presentation order.
    pub pages: &'a [Page],
    /// Mapping records keyed by page id and node id.
    pub mappings: &'a Mappings,
    /// Canonical store, used to read the bytes of pictures added in this session.
    pub assets: &'a dyn AssetStore,
}

const FILL_NAMES: [&str; 6] = ["noFill", "solidFill", "gradFill", "blipFill", "pattFill", "grpFill"];
/// Schema successors of the fill slot in `CT_ShapeProperties`.
const SPPR_FILL_SUCCESSORS: [&str; 6] = ["ln", "effectLst", "effectDag", "scene3d", "sp3d", "extLst"];
const SPPR_LN_SUCCESSORS: [&str; 5] = ["effectLst", "effectDag", "scene3d", "sp3d", "extLst"];
/// Schema successors of `a:effectLst` in `CT_ShapeProperties` (`effectDag` is refused where a shadow is written).
const SPPR_EFFECT_SUCCESSORS: [&str; 3] = ["scene3d", "sp3d", "extLst"];
/// Schema successors of the fill slot in `CT_TextCharacterProperties`.
const RPR_FILL_SUCCESSORS: [&str; 15] = [
    "effectLst", "effectDag", "highlight", "uLnTx", "uLn", "uFillTx", "uFill", "latin", "ea", "cs", "sym", "hlinkClick",
    "hlinkMouseOver", "rtl", "extLst",
];
const RPR_LATIN_SUCCESSORS: [&str; 7] = ["ea", "cs", "sym", "hlinkClick", "hlinkMouseOver", "rtl", "extLst"];
const RPR_EA_SUCCESSORS: [&str; 6] = ["cs", "sym", "hlinkClick", "hlinkMouseOver", "rtl", "extLst"];
/// Schema successors of `CT_TextParagraphProperties` children.
const PPR_LNSPC_SUCCESSORS: [&str; 15] = [
    "spcBef", "spcAft", "buClrTx", "buClr", "buSzTx", "buSzPct", "buSzPts", "buFontTx", "buFont", "buNone", "buAutoNum",
    "buChar", "buBlip", "tabLst", "defRPr",
];
const PPR_SPCBEF_SUCCESSORS: [&str; 14] = [
    "spcAft", "buClrTx", "buClr", "buSzTx", "buSzPct", "buSzPts", "buFontTx", "buFont", "buNone", "buAutoNum", "buChar",
    "buBlip", "tabLst", "defRPr",
];
const PPR_SPCAFT_SUCCESSORS: [&str; 13] = [
    "buClrTx", "buClr", "buSzTx", "buSzPct", "buSzPts", "buFontTx", "buFont", "buNone", "buAutoNum", "buChar", "buBlip",
    "tabLst", "defRPr",
];
/// Presentation children that follow `p:sldIdLst` in schema order.
const PRES_AFTER_SLDIDLST: [&str; 11] = [
    "sldSz", "notesSz", "smartTags", "embeddedFontLst", "custShowLst", "photoAlbum", "custDataLst", "kinsoku",
    "defaultTextStyle", "modifyVerifier", "extLst",
];

/// A parsed part whose ranges are absolute byte offsets of the original bytes.
struct Parsed<'a> {
    xml: &'a [u8],
    doc: Document<'a>,
    offset: usize,
}

impl<'a> Parsed<'a> {
    fn new(xml: &'a [u8]) -> DltResult<Self> {
        let (doc, offset) = xmlx::parse(xml)?;
        Ok(Self { xml, doc, offset })
    }

    fn span(&self, node: XNode<'_, '_>) -> Range<usize> {
        span(node, self.offset)
    }

    /// The element whose start tag begins at the absolute offset `start`.
    fn at(&self, start: usize) -> DltResult<XNode<'_, 'a>> {
        self.doc
            .descendants()
            .find(|n| n.is_element() && n.range().start + self.offset == start)
            .ok_or_else(|| malformed(format!("no element starts at byte {start}")))
    }

    /// The original bytes. Absolute offsets index into these bytes.
    fn xml(&self) -> &'a [u8] {
        self.xml
    }

    fn root(&self) -> XNode<'_, 'a> {
        self.doc.root_element()
    }
}

/// Writes the model into a new package. The result is ready for `Package::encode`.
pub fn write_pptx(input: &WriteInput<'_>) -> DltResult<(Package, Vec<Diagnostic>)> {
    // Refused before any part is written: a v1.2 value in the model has no PPTX form yet.
    input.pages.iter().try_for_each(|page| crate::guard::check_page(page, "in PPTX output"))?;
    let hash_put = |bytes: &[u8], mime: &str| -> DltResult<AssetRef> {
        Ok(AssetRef { sha256: sha256(bytes), mime: mime.to_string(), bytes: bytes.len() as u64 })
    };
    let (presentation, sources) = read_presentation(input.base, &hash_put)?;
    let base_pres = input.base.part(ns::PRESENTATION_PART)?.to_vec();
    let base_pres_rels = opc::parse_rels(input.base.part(ns::PRESENTATION_RELS_PART)?)?;
    let base_ct = ContentTypes::parse(input.base.part(opc::CONTENT_TYPES_PART)?)?;
    let mut ct = base_ct.clone();
    let mut out = input.base.clone();
    let mut pres_rels = base_pres_rels.clone();
    let mut losses: Vec<Diagnostic> = Vec::new();

    // Plan the pages in their new order.
    let mut planned: Vec<(&Page, PageMap, Option<&ReadPage>)> = Vec::new();
    let mut seen_sld = BTreeSet::new();
    let mut seen_parts = BTreeSet::new();
    for page in input.pages {
        let map: PageMap = mapping(input.mappings, &page.id)?;
        if !seen_sld.insert(map.sld_id) {
            return Err(malformed(format!("sldId {} is used by two pages", map.sld_id)));
        }
        if !seen_parts.insert(map.part.to_ascii_lowercase()) {
            return Err(malformed(format!("slide part {} is used by two pages", map.part)));
        }
        let source = if map.origin == ORIGIN_SOURCE {
            let found = sources
                .iter()
                .find(|s| s.map.sld_id == map.sld_id)
                .ok_or_else(|| malformed(format!("source page {} is missing from the original package", page.id)))?;
            Some(found)
        } else if map.origin == ORIGIN_ADDED {
            None
        } else {
            return Err(malformed(format!("unknown origin for page {}", page.id)));
        };
        planned.push((page, map, source));
    }

    // Remove source slides that the model no longer contains, with their notes.
    let kept: BTreeSet<u32> = planned.iter().filter(|(_, _, s)| s.is_some()).map(|(_, m, _)| m.sld_id).collect();
    for source in sources.iter().filter(|s| !kept.contains(&s.map.sld_id)) {
        remove_slide(&mut out, &mut ct, &mut pres_rels, source)?;
    }

    // The notes master is shared. Create one only when a page needs a notes slide and none exists.
    let needs_notes = planned
        .iter()
        .any(|(_, map, source)| map.notes_part.is_some() && source.map_or(true, |s| s.notes.is_none()));
    let mut notes_master_part: Option<String> = presentation.notes_master.clone();
    let mut created_notes_master_rid: Option<String> = None;
    if needs_notes && notes_master_part.is_none() {
        let (rid, part) = create_notes_master(&mut out, &mut ct, &mut pres_rels, input.base)?;
        created_notes_master_rid = Some(rid);
        notes_master_part = Some(part);
    }

    let layout = first_layout(input.base)?;
    let mut entries: Vec<(u32, String)> = Vec::with_capacity(planned.len());
    for (page, map, source) in &planned {
        let rid = match source {
            Some(source) => {
                if map.sld_id != source.map.sld_id || map.part != source.map.part {
                    return Err(malformed(format!("page {} changed its slide identity", page.id)));
                }
                source.map.rel_id.clone()
            }
            None => {
                let rid = next_rel_id(&pres_rels);
                pres_rels.push(Relationship {
                    id: rid.clone(),
                    rel_type: ns::REL_SLIDE.to_string(),
                    target: relative(ns::PRESENTATION_PART, &map.part),
                    external: false,
                });
                rid
            }
        };
        entries.push((map.sld_id, rid));
        write_page(&mut out, &mut ct, input, layout.as_deref(), notes_master_part.as_deref(), page, map, *source, &mut losses)?;
    }

    let pres_bytes = presentation_xml(&base_pres, &entries, created_notes_master_rid.as_deref())?;
    out.set_part(ns::PRESENTATION_PART, pres_bytes)?;
    if pres_rels != base_pres_rels {
        out.set_part(ns::PRESENTATION_RELS_PART, opc::rels_to_xml(&pres_rels))?;
    }
    if ct != base_ct {
        out.set_part(opc::CONTENT_TYPES_PART, ct.to_xml())?;
    }
    Ok((out, losses))
}

/// Mapping record lookup with a precise error.
pub(crate) fn mapping<T: for<'de> serde::Deserialize<'de>>(mappings: &Mappings, id: &str) -> DltResult<T> {
    let value = mappings
        .get(id)
        .ok_or_else(|| DltError::new(ErrorCode::IntegrityError, format!("no mapping record for {id}")))?;
    model::from_value(id, value)
}

fn remove_slide(out: &mut Package, ct: &mut ContentTypes, pres_rels: &mut Vec<Relationship>, source: &ReadPage) -> DltResult<()> {
    let mut parts = vec![source.map.part.clone()];
    if let Some(notes) = &source.map.notes_part {
        parts.push(notes.clone());
    }
    for part in parts {
        let rels_part = opc::rels_part_for(&part);
        if out.has_part(&rels_part) {
            out.remove_part(&rels_part)?;
        }
        if out.has_part(&part) {
            out.remove_part(&part)?;
        }
        ct.remove_override(&part);
    }
    pres_rels.retain(|r| r.id != source.map.rel_id);
    Ok(())
}

/// Part of the first slide layout of the first master. New slides attach to it.
fn first_layout(package: &Package) -> DltResult<Option<String>> {
    let pres = Parsed::new(package.part(ns::PRESENTATION_PART)?)?;
    let Some(master_id) = child(pres.root(), NS_P, "sldMasterIdLst")
        .and_then(|l| child(l, NS_P, "sldMasterId"))
        .and_then(|e| e.attribute((NS_R, "id")))
    else {
        return Ok(None);
    };
    let pres_rels = opc::parse_rels(package.part(ns::PRESENTATION_RELS_PART)?)?;
    let Some(master_rel) = pres_rels.iter().find(|r| r.id == master_id) else {
        return Ok(None);
    };
    let master = opc::resolve_target(ns::PRESENTATION_PART, &master_rel.target)?;
    let master_rels = match package.part(&opc::rels_part_for(&master)) {
        Ok(bytes) => opc::parse_rels(bytes)?,
        Err(_) => return Ok(None),
    };
    match master_rels.iter().find(|r| r.rel_type == ns::REL_SLIDE_LAYOUT) {
        Some(rel) => Ok(Some(opc::resolve_target(&master, &rel.target)?)),
        None => Ok(None),
    }
}

/// Creates a notes master with a copy of the master theme. Returns the presentation relationship id and the part.
fn create_notes_master(out: &mut Package, ct: &mut ContentTypes, pres_rels: &mut Vec<Relationship>, base: &Package) -> DltResult<(String, String)> {
    let theme_bytes = master_theme_bytes(base)?;
    let master_part = free_part("ppt/notesMasters/notesMaster", ".xml", out);
    let theme_part = free_part("ppt/theme/theme", ".xml", out);
    out.set_part(&theme_part, theme_bytes)?;
    ct.set_override(&theme_part, ns::CT_THEME);
    out.set_part(&master_part, notes_master_xml())?;
    ct.set_override(&master_part, ns::CT_NOTES_MASTER);
    out.set_part(
        &opc::rels_part_for(&master_part),
        opc::rels_to_xml(&[Relationship {
            id: "rId1".into(),
            rel_type: ns::REL_THEME.into(),
            target: relative(&master_part, &theme_part),
            external: false,
        }]),
    )?;
    let rid = next_rel_id(pres_rels);
    pres_rels.push(Relationship {
        id: rid.clone(),
        rel_type: ns::REL_NOTES_MASTER.into(),
        target: relative(ns::PRESENTATION_PART, &master_part),
        external: false,
    });
    Ok((rid, master_part))
}

/// First unused `prefix N suffix` name, starting at 1 for notes masters and at 2 for themes.
fn free_part(prefix: &str, suffix: &str, out: &Package) -> String {
    let mut number = if prefix.contains("theme") { 2 } else { 1 };
    loop {
        let candidate = format!("{prefix}{number}{suffix}");
        if !out.has_part(&candidate) {
            return candidate;
        }
        number += 1;
    }
}

fn master_theme_bytes(base: &Package) -> DltResult<Vec<u8>> {
    let pres = Parsed::new(base.part(ns::PRESENTATION_PART)?)?;
    let pres_rels = opc::parse_rels(base.part(ns::PRESENTATION_RELS_PART)?)?;
    let master_rid = child(pres.root(), NS_P, "sldMasterIdLst")
        .and_then(|l| child(l, NS_P, "sldMasterId"))
        .and_then(|e| e.attribute((NS_R, "id")));
    let master = master_rid
        .and_then(|rid| pres_rels.iter().find(|r| r.id == rid))
        .map(|r| opc::resolve_target(ns::PRESENTATION_PART, &r.target))
        .transpose()?
        .ok_or_else(|| malformed("no slide master to take a theme from"))?;
    let master_rels = opc::parse_rels(base.part(&opc::rels_part_for(&master))?)?;
    let theme = master_rels
        .iter()
        .find(|r| r.rel_type == ns::REL_THEME)
        .ok_or_else(|| malformed("slide master has no theme"))?;
    Ok(base.part(&opc::resolve_target(&master, &theme.target)?)?.to_vec())
}

/// Writes one slide: its XML, its relationships, its notes, and its pictures.
#[allow(clippy::too_many_arguments)]
fn write_page(
    out: &mut Package,
    ct: &mut ContentTypes,
    input: &WriteInput<'_>,
    layout: Option<&str>,
    notes_master: Option<&str>,
    page: &Page,
    map: &PageMap,
    source: Option<&ReadPage>,
    losses: &mut Vec<Diagnostic>,
) -> DltResult<()> {
    let base_slide: Vec<u8> = match source {
        Some(s) => input.base.part(&s.map.part)?.to_vec(),
        None => new_slide_skeleton(),
    };
    let mut rels: Vec<Relationship> = match source {
        Some(s) => s.relationships.clone(),
        None => Vec::new(),
    };
    if source.is_none() {
        if let Some(layout) = layout {
            rels.push(Relationship {
                id: "rId1".into(),
                rel_type: ns::REL_SLIDE_LAYOUT.into(),
                target: relative(&map.part, layout),
                external: false,
            });
        }
    }

    // Node identity and z-order. Source nodes must keep their order, and new nodes must come after them.
    let mut node_maps: Vec<(&Node, ShapeMap)> = Vec::with_capacity(page.nodes.len());
    for node in &page.nodes {
        node_maps.push((node, mapping(input.mappings, &node.id)?));
    }
    let source_position: BTreeMap<u32, usize> = source
        .map(|s| s.shapes.iter().enumerate().map(|(i, sh)| (sh.map.cnvpr_id, i)).collect())
        .unwrap_or_default();
    let mut previous: Option<usize> = None;
    let mut seen_source = false;
    let mut seen_cnvpr = BTreeSet::new();
    for (node, node_map) in &node_maps {
        if !seen_cnvpr.insert(node_map.cnvpr_id) {
            return Err(malformed(format!("two nodes share shape id {}", node_map.cnvpr_id)));
        }
        match node_map.origin.as_str() {
            ORIGIN_SOURCE => {
                if seen_source && previous.is_none() {
                    return Err(malformed("source shapes out of order"));
                }
                let position = *source_position
                    .get(&node_map.cnvpr_id)
                    .ok_or_else(|| malformed(format!("node {} has no source shape", node.id)))?;
                if previous.is_some_and(|p| position <= p) {
                    return Err(DltError::unsupported("reordering shapes within a slide is not supported"));
                }
                previous = Some(position);
                seen_source = true;
            }
            ORIGIN_ADDED => {}
            other => return Err(malformed(format!("unknown origin {other} for node {}", node.id))),
        }
    }
    let kept_source: BTreeSet<u32> = node_maps.iter().filter(|(_, m)| m.origin == ORIGIN_SOURCE).map(|(_, m)| m.cnvpr_id).collect();
    let first_added = node_maps.iter().position(|(_, m)| m.origin == ORIGIN_ADDED);
    if let Some(index) = first_added {
        if node_maps[index..].iter().any(|(_, m)| m.origin == ORIGIN_SOURCE) {
            return Err(DltError::unsupported("new shapes must come after existing shapes in z-order"));
        }
    }

    let parsed = Parsed::new(&base_slide)?;
    let decls = xmlx::in_scope_declarations(parsed.root());
    let c_sld = child(parsed.root(), NS_P, "cSld").ok_or_else(|| malformed("slide has no cSld"))?;
    let sp_tree = child(c_sld, NS_P, "spTree").ok_or_else(|| malformed("slide has no spTree"))?;
    let tree_range = parsed.span(sp_tree);

    let mut splices: Vec<Splice> = Vec::new();
    if let Some(source) = source {
        let node_by_cnvpr: BTreeMap<u32, (&Node, &ShapeMap)> = node_maps
            .iter()
            .filter(|(_, m)| m.origin == ORIGIN_SOURCE)
            .map(|(n, m)| (m.cnvpr_id, (*n, m)))
            .collect();
        for shape in &source.shapes {
            match node_by_cnvpr.get(&shape.map.cnvpr_id) {
                None => splices.push(Splice { range: shape.spans.element.clone(), replacement: Vec::new() }),
                Some((node, node_map)) => {
                    if node.kind == NodeKind::Image && node.asset != shape.node.asset {
                        replace_picture(out, ct, input, &base_slide, &mut rels, &map.part, shape, node, node_map)?;
                    }
                    splices.extend(diff_shape(&base_slide, &decls, shape, node, node_map, losses)?);
                }
            }
        }
        for id in &kept_source {
            if !source.shapes.iter().any(|s| s.map.cnvpr_id == *id) {
                return Err(malformed(format!("kept shape {id} is not in the source slide")));
            }
        }
    }

    // New shapes. A picture takes a new relationship that points at its media part.
    let mut new_xml = String::new();
    for (node, node_map) in node_maps.iter().filter(|(_, m)| m.origin == ORIGIN_ADDED) {
        match node.kind {
            NodeKind::Path => new_xml.push_str(&path_shape_xml(node, node_map, losses)?),
            NodeKind::Video => return Err(DltError::unsupported("NodeKind::Video is not written to PPTX yet")),
            NodeKind::Image => {
                let asset = node.asset.as_ref().ok_or_else(|| malformed("picture node has no asset"))?;
                let media = node_map.media_part.clone().ok_or_else(|| malformed("picture node has no media part"))?;
                let bytes = input.assets.read(asset)?;
                if sha256(&bytes) != asset.sha256 || !ns::magic_matches(&asset.mime, &bytes) {
                    return Err(DltError::invalid("picture bytes do not match their declared type or hash"));
                }
                if !out.has_part(&media) {
                    out.set_part(&media, bytes)?;
                }
                let extension = ns::extension_for_mime(&asset.mime)
                    .ok_or_else(|| DltError::unsupported("this picture format cannot be written"))?;
                if ct.defaults.get(extension).is_none() {
                    ct.set_default(extension, &asset.mime);
                }
                let rid = next_rel_id(&rels);
                rels.push(Relationship { id: rid.clone(), rel_type: ns::REL_IMAGE.into(), target: relative(&map.part, &media), external: false });
                new_xml.push_str(&picture_xml(node, node_map, &rid)?);
            }
            NodeKind::Line => new_xml.push_str(&connector_xml(node, node_map)?),
            NodeKind::Text | NodeKind::Rectangle | NodeKind::Ellipse => new_xml.push_str(&shape_xml(node, node_map, losses)?),
            NodeKind::Group => return Err(DltError::unsupported("group nodes cannot be created in this release")),
        }
    }
    if !new_xml.is_empty() {
        let content = pxml::content_range(&base_slide, &tree_range)?.ok_or_else(|| malformed("spTree has no content"))?;
        splices.push(pxml::insert_at(content.end, &new_xml));
    }
    let slide_xml = pxml::apply_checked(&base_slide, splices)?;

    // Pictures that the slide no longer references lose their relationship.
    let referenced = referenced_ids(&slide_xml);
    rels.retain(|r| !(r.rel_type == ns::REL_IMAGE && !referenced.contains(&r.id)));

    // Notes.
    if let Some(notes_part) = map.notes_part.as_deref() {
        match source.and_then(|s| s.notes.as_ref()) {
            Some(existing) => {
                if existing.part != notes_part {
                    return Err(malformed("notes part changed identity"));
                }
                if source.is_some_and(|s| s.page.notes != page.notes) {
                    let bytes = input.base.part(&existing.part)?.to_vec();
                    out.set_part(&existing.part, patch_notes(&bytes, existing, &page.notes)?)?;
                }
            }
            None => {
                let master = notes_master.ok_or_else(|| malformed("notes need a notes master"))?;
                if !rels.iter().any(|r| r.rel_type == ns::REL_NOTES_SLIDE) {
                    let rid = next_rel_id(&rels);
                    rels.push(Relationship { id: rid, rel_type: ns::REL_NOTES_SLIDE.into(), target: relative(&map.part, notes_part), external: false });
                }
                out.set_part(notes_part, new_notes_xml(&page.notes)?)?;
                out.set_part(
                    &opc::rels_part_for(notes_part),
                    opc::rels_to_xml(&[
                        Relationship { id: "rId1".into(), rel_type: ns::REL_SLIDE.into(), target: relative(notes_part, &map.part), external: false },
                        Relationship { id: "rId2".into(), rel_type: ns::REL_NOTES_MASTER.into(), target: relative(notes_part, master), external: false },
                    ]),
                )?;
                ct.set_override(notes_part, ns::CT_NOTES_SLIDE);
            }
        }
    }

    out.set_part(&map.part, slide_xml)?;
    let rels_part = opc::rels_part_for(&map.part);
    let original_rels: Vec<Relationship> = source.map(|s| s.relationships.clone()).unwrap_or_default();
    if rels.is_empty() {
        if out.has_part(&rels_part) {
            out.remove_part(&rels_part)?;
        }
    } else if source.is_none() || rels != original_rels {
        out.set_part(&rels_part, opc::rels_to_xml(&rels))?;
    }
    if source.is_none() {
        ct.set_override(&map.part, ns::CT_SLIDE);
    }
    Ok(())
}

/// Points an existing picture at the media part of its new asset. The picture keeps its relationship id, so its
/// XML bytes and their preservation regions do not change. The previous media part stays in the package, as a
/// removed picture's does, because the preservation manifest still lists it.
#[allow(clippy::too_many_arguments)]
fn replace_picture(
    out: &mut Package,
    ct: &mut ContentTypes,
    input: &WriteInput<'_>,
    base_slide: &[u8],
    rels: &mut [Relationship],
    slide_part: &str,
    source: &ReadShape,
    node: &Node,
    node_map: &ShapeMap,
) -> DltResult<()> {
    let asset = node.asset.as_ref().ok_or_else(|| malformed("picture node has no asset"))?;
    let media = node_map.media_part.clone().ok_or_else(|| malformed("picture node has no media part"))?;
    let rid = source.map.rel_id.clone().ok_or_else(|| malformed(format!("picture {} has no relationship", node.id)))?;
    let bytes = input.assets.read(asset)?;
    if sha256(&bytes) != asset.sha256 || !ns::magic_matches(&asset.mime, &bytes) {
        return Err(DltError::invalid("picture bytes do not match their declared type or hash"));
    }
    let extension = ns::extension_for_mime(&asset.mime).ok_or_else(|| DltError::unsupported("this picture format cannot be written"))?;
    // Retargeting changes every object that uses the relationship, so only this picture may use it.
    let parsed = Parsed::new(base_slide)?;
    let users = parsed
        .doc
        .descendants()
        .filter(|n| n.is_element() && n.attributes().any(|a| a.namespace() == Some(NS_R) && a.value() == rid.as_str()))
        .count();
    if users != 1 {
        return Err(DltError::unsupported(format!(
            "picture {} shares its image relationship with another object; replacing it is not supported",
            node.id
        )));
    }
    if out.has_part(&media) {
        if out.part(&media)? != bytes.as_slice() {
            return Err(malformed(format!("media part {media} already holds different content")));
        }
    } else {
        out.set_part(&media, bytes)?;
    }
    if ct.defaults.get(extension).is_none() {
        ct.set_default(extension, &asset.mime);
    }
    let rel = rels
        .iter_mut()
        .find(|r| r.id == rid && r.rel_type == ns::REL_IMAGE)
        .ok_or_else(|| malformed(format!("picture {} has no image relationship {rid}", node.id)))?;
    rel.target = relative(slide_part, &media);
    Ok(())
}

/// Shape-level differences between a source shape and its model node.
fn diff_shape(
    xml: &[u8],
    decls: &str,
    source: &ReadShape,
    node: &Node,
    node_map: &ShapeMap,
    losses: &mut Vec<Diagnostic>,
) -> DltResult<Vec<Splice>> {
    if node.kind != source.node.kind {
        return Err(DltError::unsupported("changing the kind of an existing shape is not supported"));
    }
    if node_map.geometry != source.map.geometry {
        return Err(DltError::unsupported("changing the geometry preset of an existing shape is not supported"));
    }
    // A picture's asset is replaced by `replace_picture` before this function runs.
    if node.kind != NodeKind::Image && node.asset != source.node.asset {
        return Err(DltError::unsupported("an image asset can only belong to a picture"));
    }
    if node_map.rotation != source.map.rotation || node_map.flip_h != source.map.flip_h || node_map.flip_v != source.map.flip_v {
        return Err(DltError::unsupported("rotation and flip cannot be edited in this release"));
    }
    if node.kind == NodeKind::Path && node.ext.path != source.node.ext.path {
        return Err(DltError::unsupported(format!("changing the path of the vector shape {} is not supported", node.id)));
    }
    let parsed = Parsed::new(xml)?;
    let mut splices = Vec::new();
    if node.bounds != source.node.bounds {
        let off = source.spans.off.clone().ok_or_else(|| malformed("shape has no offset element"))?;
        let ext = source.spans.ext.clone().ok_or_else(|| malformed("shape has no extent element"))?;
        splices.push(pxml::set_attribute(xml, &off, "x", &node.bounds.x.to_string())?);
        splices.push(pxml::set_attribute(xml, &off, "y", &node.bounds.y.to_string())?);
        splices.push(pxml::set_attribute(xml, &ext, "cx", &node.bounds.width.to_string())?);
        splices.push(pxml::set_attribute(xml, &ext, "cy", &node.bounds.height.to_string())?);
    }
    if node.style.fill != source.node.style.fill {
        let fragment = fill_fragment(&node.style.fill)?;
        match &source.spans.fill {
            Some(existing) => splices.push(pxml::replace_element(existing, &fragment)),
            None => {
                let sp_pr = source.spans.sp_pr.clone().ok_or_else(|| malformed("shape has no spPr"))?;
                splices.push(insert_before_successor(&parsed, &sp_pr, &SPPR_FILL_SUCCESSORS, &fragment)?);
            }
        }
    }
    let outline_changed = node.style.stroke != source.node.style.stroke || node.style.stroke_width != source.node.style.stroke_width;
    let legacy_marker_mapping = node.kind == NodeKind::Line
        && node_map.arrow
        && node.style.stroke_start.is_none()
        && node.style.stroke_end.is_none()
        && !node_map.unmapped_head_end
        && !node_map.unmapped_tail_end
        && !node_map.custom_head_end_size
        && !node_map.custom_tail_end_size;
    let start_changed = !legacy_marker_mapping
        && (node.style.stroke_start != source.node.style.stroke_start
            || node_map.unmapped_head_end != source.map.unmapped_head_end);
    let end_changed = !legacy_marker_mapping
        && (node.style.stroke_end != source.node.style.stroke_end
            || node_map.unmapped_tail_end != source.map.unmapped_tail_end);
    if outline_changed || start_changed || end_changed {
        match &source.spans.ln {
            None => splices.extend(line_splices(&parsed, source, &node.style)?),
            Some(ln) => {
                let mut ln_splices = Vec::new();
                if outline_changed {
                    ln_splices.extend(line_splices(&parsed, source, &node.style)?);
                }
                if start_changed {
                    ln_splices.extend(line_end_splices(&parsed, source, "headEnd", source.node.style.stroke_start, source.map.unmapped_head_end, node.style.stroke_start, node_map.unmapped_head_end)?);
                }
                if end_changed {
                    ln_splices.extend(line_end_splices(&parsed, source, "tailEnd", source.node.style.stroke_end, source.map.unmapped_tail_end, node.style.stroke_end, node_map.unmapped_tail_end)?);
                }
                splices.extend(open_empty_line(parsed.xml(), ln, ln_splices)?);
            }
        }
    }
    if node.style.shadows != source.node.style.shadows {
        splices.extend(effect_splices(&parsed, source, &node.style.shadows, &node.id, losses)?);
    }
    let source_rich = RichText::from_parts(&source.node.text, &source.map.paragraphs)?;
    let target_rich = RichText::from_parts(&node.text, &node_map.paragraphs)?;
    if source_rich != target_rich {
        splices.extend(shape_text_splices(xml, decls, &parsed, source, &source_rich, &target_rich)?);
    }
    Ok(splices)
}

/// Outline changes: the `a:ln` colour and width, or a new `a:ln` when the shape has none. An existing `a:ln` changes only what
/// the model changed: its fill when the colour changed, and `w` when the width changed. An unchanged width keeps the
/// original attribute bytes, whatever they are (out of range, negative, or absent). A new `a:ln` takes a checked width.
fn line_splices(parsed: &Parsed<'_>, source: &ReadShape, style: &Style) -> DltResult<Vec<Splice>> {
    let stroke = style.stroke.as_str();
    if stroke.starts_with("unsupported:") {
        return Err(DltError::unsupported("this outline cannot be written back exactly"));
    }
    match &source.spans.ln {
        Some(ln) => {
            let mut out = Vec::new();
            if style.stroke != source.node.style.stroke {
                out.extend(outline_fill_splices(parsed, ln, stroke)?);
            }
            if style.stroke_width != source.node.style.stroke_width {
                let width = crate::v12::checked_line_width(style.stroke_width, &source.node.id)?;
                out.push(pxml::set_attribute(parsed.xml(), ln, "w", &width.to_string())?);
            }
            Ok(out)
        }
        None => {
            let sp_pr = source.spans.sp_pr.clone().ok_or_else(|| malformed("shape has no spPr"))?;
            let width = crate::v12::checked_line_width(style.stroke_width, &source.node.id)?;
            let fill = line_fill(stroke)?;
            let markers = crate::v12::line_end_xml(style);
            let fragment = format!("<a:ln w=\"{width}\">{fill}{markers}</a:ln>");
            Ok(vec![insert_before_successor(parsed, &sp_pr, &SPPR_LN_SUCCESSORS, &fragment)?])
        }
    }
}

/// The fill of an existing `a:ln` whose colour changed. A fill child is replaced. Without one, the fill goes before the first
/// child, or at the end of the content when there is none. An empty element (`<a:ln .../>`) has no content: the fill is
/// inserted at the end of its `/>`, and `open_empty_line` opens the element. Only the splices here touch the element, so a
/// width splice in the start tag never overlaps them.
fn outline_fill_splices(parsed: &Parsed<'_>, ln: &Range<usize>, stroke: &str) -> DltResult<Vec<Splice>> {
    let fill = line_fill(stroke)?;
    let xml = parsed.xml();
    let ln_node = parsed.at(ln.start)?;
    let existing = elements(ln_node)
        .into_iter()
        .find(|c| c.tag_name().namespace() == Some(NS_A) && FILL_NAMES.contains(&c.tag_name().name()));
    if let Some(current) = existing {
        return Ok(vec![pxml::replace_element(&parsed.span(current), &fill)]);
    }
    if pxml::is_self_closing(xml, ln)? {
        return Ok(vec![pxml::insert_at(ln.end, &fill)]);
    }
    let first = elements(ln_node).first().map(|n| parsed.span(*n).start);
    Ok(vec![pxml::insert_child(xml, ln, first, &fill)?])
}

/// Opens a self-closing `a:ln` once for all the children added to it. The children are insertions at the end of its `/>`
/// (`outline_fill_splices`, `line_end_splices`). The `/>` becomes `>`, and `</a:ln>` closes the children after them. Two
/// splices over the same `/>` would overlap, so every change to the element goes through here.
fn open_empty_line(xml: &[u8], ln: &Range<usize>, splices: Vec<Splice>) -> DltResult<Vec<Splice>> {
    if !pxml::is_self_closing(xml, ln)? || !splices.iter().any(|s| s.range == (ln.end..ln.end)) {
        return Ok(splices);
    }
    let slash = ln.end.checked_sub(2).ok_or_else(|| malformed("an empty element is shorter than '/>'"))?;
    if xml.get(slash..ln.end) != Some(&b"/>"[..]) {
        return Err(malformed("an empty element does not end with '/>'"));
    }
    let name = pxml::qname(xml, ln.start)?;
    let mut out = vec![pxml::replace_element(&(slash..ln.end), ">")];
    out.extend(splices);
    out.push(pxml::insert_at(ln.end, &format!("</{name}>")));
    Ok(out)
}

/// The fill element of an outline: `noFill` for no stroke, otherwise a solid fill of the colour.
fn line_fill(stroke: &str) -> DltResult<String> {
    if stroke == "none" {
        return Ok("<a:noFill/>".to_string());
    }
    solid_fill(stroke)
}

fn line_end_splices(
    parsed: &Parsed<'_>,
    source: &ReadShape,
    element_name: &str,
    old_value: Option<LineEnd>,
    old_unmapped: bool,
    new_value: Option<LineEnd>,
    new_unmapped: bool,
) -> DltResult<Vec<Splice>> {
    if old_value == new_value && old_unmapped == new_unmapped {
        return Ok(Vec::new());
    }
    // An unmapped marker is preserved unless the user explicitly changes or clears that endpoint.
    if new_unmapped {
        return Ok(Vec::new());
    }
    let Some(line) = source.spans.ln.as_ref() else { return Ok(Vec::new()) };
    let ln = parsed.at(line.start)?;
    let existing = elements(ln)
        .into_iter()
        .find(|child| child.tag_name().namespace() == Some(NS_A) && child.tag_name().name() == element_name);
    let type_name = |value: LineEnd| match value { LineEnd::Open => "open", LineEnd::Filled => "triangle" };
    match new_value {
        Some(value) => {
            if let Some(marker) = existing {
                Ok(vec![pxml::set_attribute(parsed.xml(), &parsed.span(marker), "type", type_name(value))?])
            } else {
                let fragment = format!("<a:{element_name} type=\"{}\"/>", type_name(value));
                match pxml::content_range(parsed.xml(), line)? {
                    Some(content) => {
                        let ext = child(ln, NS_A, "extLst").map(|node| parsed.span(node).start);
                        let other_end = (element_name == "headEnd")
                            .then(|| child(ln, NS_A, "tailEnd").map(|node| parsed.span(node).start))
                            .flatten();
                        let at = other_end.or(ext).unwrap_or(content.end);
                        Ok(vec![pxml::insert_at(at, &fragment)])
                    }
                    // An empty `a:ln` (`<a:ln/>`) has no content: the marker goes at the end of its `/>`, see `open_empty_line`.
                    None => Ok(vec![pxml::insert_at(line.end, &fragment)]),
                }
            }
        }
        None => Ok(existing.map(|marker| Splice { range: parsed.span(marker), replacement: Vec::new() }).into_iter().collect()),
    }
}

/// Text differences in a shape. A shape without a text body gets one; otherwise paragraphs are rewritten.
fn shape_text_splices(
    xml: &[u8],
    decls: &str,
    parsed: &Parsed<'_>,
    source: &ReadShape,
    source_rich: &RichText,
    target_rich: &RichText,
) -> DltResult<Vec<Splice>> {
    match &source.spans.tx_body {
        Some(_) => text_body_splices(xml, decls, &source.spans, source_rich, target_rich),
        None => {
            if target_rich.paragraphs.is_empty() {
                return Ok(vec![]);
            }
            let fragment = tx_body_xml(target_rich, &source.map.body)?;
            let sp = parsed.at(source.spans.element.start)?;
            // The text body goes before the extension list, or at the end of the content. The end tag is not assumed to be
            // `</p:sp>`: its prefix and spacing come from the document.
            let at = match child(sp, NS_P, "extLst") {
                Some(ext) => parsed.span(ext).start,
                None => pxml::content_range(xml, &source.spans.element)?
                    .ok_or_else(|| malformed("the shape has no content to hold a text body"))?
                    .end,
            };
            Ok(vec![pxml::insert_at(at, &fragment)])
        }
    }
}

/// Splices that turn the source text body into the target. Paragraphs keep their bytes when unchanged.
fn text_body_splices(xml: &[u8], decls: &str, spans: &ShapeSpans, source: &RichText, target: &RichText) -> DltResult<Vec<Splice>> {
    if source == target {
        return Ok(vec![]);
    }
    if source.paragraphs.len() == target.paragraphs.len() {
        let mut out = Vec::new();
        for (index, (src_para, tgt_para)) in source.paragraphs.iter().zip(&target.paragraphs).enumerate() {
            if src_para == tgt_para {
                continue;
            }
            let para_spans = spans.paragraphs.get(index).ok_or_else(|| malformed("paragraph span missing"))?;
            let fragment = paragraph_xml(xml, decls, Some((src_para, para_spans)), tgt_para)?;
            out.push(pxml::replace_element(&para_spans.element, &fragment));
        }
        return Ok(out);
    }
    // The paragraph count changed: rewrite the whole paragraph list in one span.
    let first = spans
        .paragraphs
        .first()
        .ok_or_else(|| DltError::unsupported("a text body without paragraphs cannot gain paragraphs in this release"))?;
    let last = spans.paragraphs.last().expect("a first paragraph implies a last one");
    let mut body = String::new();
    for (index, tgt_para) in target.paragraphs.iter().enumerate() {
        let src = source.paragraphs.get(index).zip(spans.paragraphs.get(index));
        body.push_str(&paragraph_xml(xml, decls, src, tgt_para)?);
    }
    Ok(vec![pxml::replace_element(&(first.element.start..last.element.end), &body)])
}

/// XML for one paragraph. `src` supplies the template run properties and the paragraph properties.
fn paragraph_xml(xml: &[u8], decls: &str, src: Option<(&Para, &ParaSpans)>, target: &Para) -> DltResult<String> {
    let mut out = String::from("<a:p>");
    let ppr = match src.and_then(|(src_para, spans)| spans.ppr.clone().map(|range| (src_para, range))) {
        Some((src_para, range)) => {
            if src_para.props == target.props {
                slice_str(xml, &range)?
            } else {
                patch_ppr(&slice_str(xml, &range)?, decls, &src_para.props, &target.props)?
            }
        }
        None => generated_ppr(&target.props),
    };
    out.push_str(&ppr);

    let src_units: &[Unit] = src.map(|(p, _)| p.units.as_slice()).unwrap_or(&[]);
    let src_spans: &[UnitSpans] = src.map(|(_, s)| s.units.as_slice()).unwrap_or(&[]);
    let mapping = match_units(src_units, &target.units);

    let mut index = 0usize;
    while index < target.units.len() {
        match &target.units[index] {
            Unit::Char { style, .. } => {
                let mut text = String::new();
                let mut end = index;
                while end < target.units.len() {
                    match &target.units[end] {
                        Unit::Char { ch, style: s } if s == style => {
                            text.push(*ch);
                            end += 1;
                        }
                        _ => break,
                    }
                }
                let rpr = run_rpr(xml, decls, src_units, src_spans, &mapping, index, style)?;
                out.push_str("<a:r>");
                out.push_str(&rpr);
                out.push_str("<a:t>");
                out.push_str(&pxml::escape_text(&text)?);
                out.push_str("</a:t></a:r>");
                index = end;
            }
            Unit::Break => {
                let verbatim = match mapping[index] {
                    Some(si) if src_spans[si].kind == UnitKind::Break => Some(src_spans[si].element.clone()),
                    _ => None,
                };
                match verbatim {
                    Some(range) => out.push_str(&slice_str(xml, &range)?),
                    None => out.push_str("<a:br/>"),
                }
                index += 1;
            }
            Unit::Atom { element, display, .. } => {
                let si = mapping[index].ok_or_else(|| {
                    DltError::unsupported(format!(
                        "inline element {element} with text {display:?} cannot be created or removed by a text edit"
                    ))
                })?;
                if src_spans[si].kind != UnitKind::Atom {
                    return Err(malformed("protected inline element is not an atom in the source"));
                }
                out.push_str(&slice_str(xml, &src_spans[si].element)?);
                index += 1;
            }
        }
    }
    if let Some((_, spans)) = src {
        if let Some(end_rpr) = &spans.end_rpr {
            out.push_str(&slice_str(xml, end_rpr)?);
        }
    }
    out.push_str("</a:p>");
    Ok(out)
}

fn slice_str(xml: &[u8], range: &Range<usize>) -> DltResult<String> {
    let bytes = xml
        .get(range.clone())
        .ok_or_else(|| malformed(format!("span {range:?} is outside the part")))?;
    String::from_utf8(bytes.to_vec()).map_err(|e| malformed(format!("span is not UTF-8: {e}")))
}

/// Source unit index for each target unit. Equal prefixes and suffixes match. The middle is new.
fn match_units(source: &[Unit], target: &[Unit]) -> Vec<Option<usize>> {
    let mut result = vec![None; target.len()];
    let limit = source.len().min(target.len());
    let mut prefix = 0;
    while prefix < limit && same_unit(&source[prefix], &target[prefix]) {
        result[prefix] = Some(prefix);
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < limit - prefix && same_unit(&source[source.len() - 1 - suffix], &target[target.len() - 1 - suffix]) {
        result[target.len() - 1 - suffix] = Some(source.len() - 1 - suffix);
        suffix += 1;
    }
    result
}

fn same_unit(a: &Unit, b: &Unit) -> bool {
    match (a, b) {
        (Unit::Char { ch: x, .. }, Unit::Char { ch: y, .. }) => x == y,
        (Unit::Break, Unit::Break) => true,
        (Unit::Atom { element: e1, display: d1, .. }, Unit::Atom { element: e2, display: d2, .. }) => e1 == e2 && d1 == d2,
        _ => false,
    }
}

/// `a:rPr` bytes for a run that starts at target unit `index`. The template is the matched source unit, or the
/// nearest matched source unit before the run for inserted characters.
fn run_rpr(
    xml: &[u8],
    decls: &str,
    src_units: &[Unit],
    src_spans: &[UnitSpans],
    mapping: &[Option<usize>],
    index: usize,
    style: &Style,
) -> DltResult<String> {
    let template = mapping[index].or_else(|| mapping[..index].iter().rev().find_map(|m| *m));
    if let Some(si) = template {
        if let Some(rpr) = &src_spans[si].rpr {
            let source_style = src_units[si].style().cloned().unwrap_or_default();
            if &source_style == style {
                return slice_str(xml, rpr);
            }
            return patch_rpr(&slice_str(xml, rpr)?, decls, &source_style, style);
        }
    }
    generated_rpr(style)
}

/// Hex digits of a `#RRGGBB` colour, for DrawingML `val` attributes.
pub(crate) fn hex6(color: &str) -> DltResult<String> {
    normalize_hex(color)
        .map(|v| v[1..].to_string())
        .ok_or_else(|| DltError::new(ErrorCode::IntegrityError, format!("colour {color:?} is not #RRGGBB")))
}

/// Font size in hundredths of a point. The model stores EMU in multiples of 127 (prepare quantizes).
pub(crate) fn centipoints(font_size_emu: i64) -> DltResult<String> {
    if font_size_emu <= 0 || font_size_emu % 127 != 0 {
        return Err(DltError::new(ErrorCode::IntegrityError, format!("font size {font_size_emu} EMU is not on the 1/100 pt grid")));
    }
    Ok((font_size_emu / 127).to_string())
}

fn centi_from_emu(value: i64) -> DltResult<String> {
    if value < 0 || value % 127 != 0 {
        return Err(DltError::new(ErrorCode::IntegrityError, format!("spacing {value} EMU is not on the 1/100 pt grid")));
    }
    Ok((value / 127).to_string())
}

/// New `a:rPr` from a style.
pub(crate) fn generated_rpr(style: &Style) -> DltResult<String> {
    let font = pxml::escape_attr(&style.font_family)?;
    Ok(format!(
        "<a:rPr lang=\"en-US\" sz=\"{}\" b=\"{}\" i=\"{}\" dirty=\"0\"><a:solidFill><a:srgbClr val=\"{}\"/></a:solidFill><a:latin typeface=\"{font}\"/><a:ea typeface=\"{font}\"/></a:rPr>",
        centipoints(style.font_size)?,
        u8::from(style.bold),
        u8::from(style.italic),
        hex6(&style.color)?
    ))
}

/// Patches `a:rPr` bytes so that the font properties equal `to`. Unrelated children stay as they are.
fn patch_rpr(rpr: &str, decls: &str, from: &Style, to: &Style) -> DltResult<String> {
    let bytes = xmlx::patch_fragment(rpr.as_bytes(), decls, |wrapper, fragment| {
        let parsed = Parsed::new(wrapper)?;
        let element = parsed.at(fragment.start)?;
        let mut splices = Vec::new();
        if from.font_size != to.font_size {
            splices.push(pxml::set_attribute(wrapper, &fragment, "sz", &centipoints(to.font_size)?)?);
        }
        if from.bold != to.bold {
            splices.push(pxml::set_attribute(wrapper, &fragment, "b", if to.bold { "1" } else { "0" })?);
        }
        if from.italic != to.italic {
            splices.push(pxml::set_attribute(wrapper, &fragment, "i", if to.italic { "1" } else { "0" })?);
        }
        if from.color != to.color {
            let fill = solid_fill(&to.color)?;
            splices.push(fill_splice(&parsed, &element, &fill, &RPR_FILL_SUCCESSORS)?);
        }
        if from.font_family != to.font_family {
            splices.push(typeface_splice(wrapper, &element, "latin", &to.font_family, &RPR_LATIN_SUCCESSORS)?);
            splices.push(typeface_splice(wrapper, &element, "ea", &to.font_family, &RPR_EA_SUCCESSORS)?);
        }
        Ok(splices)
    })?;
    String::from_utf8(bytes).map_err(|e| malformed(format!("patched run properties are not UTF-8: {e}")))
}

/// Patches `a:pPr` bytes so that alignment and spacing equal `to`.
fn patch_ppr(ppr: &str, decls: &str, from: &ParaProps, to: &ParaProps) -> DltResult<String> {
    let bytes = xmlx::patch_fragment(ppr.as_bytes(), decls, |wrapper, fragment| {
        let parsed = Parsed::new(wrapper)?;
        let element = parsed.at(fragment.start)?;
        let mut splices = Vec::new();
        if from.align != to.align {
            splices.push(pxml::set_attribute(wrapper, &fragment, "algn", align_attr(&to.align))?);
        }
        if from.line_spacing_pct != to.line_spacing_pct {
            let fragment_xml = format!("<a:lnSpc><a:spcPct val=\"{}\"/></a:lnSpc>", to.line_spacing_pct * 1000);
            splices.push(child_splice(&parsed, &element, "lnSpc", &fragment_xml, &PPR_LNSPC_SUCCESSORS)?);
        }
        if from.space_before != to.space_before {
            let fragment_xml = format!("<a:spcBef><a:spcPts val=\"{}\"/></a:spcBef>", centi_from_emu(to.space_before)?);
            splices.push(child_splice(&parsed, &element, "spcBef", &fragment_xml, &PPR_SPCBEF_SUCCESSORS)?);
        }
        if from.space_after != to.space_after {
            let fragment_xml = format!("<a:spcAft><a:spcPts val=\"{}\"/></a:spcAft>", centi_from_emu(to.space_after)?);
            splices.push(child_splice(&parsed, &element, "spcAft", &fragment_xml, &PPR_SPCAFT_SUCCESSORS)?);
        }
        Ok(splices)
    })?;
    String::from_utf8(bytes).map_err(|e| malformed(format!("patched paragraph properties are not UTF-8: {e}")))
}

/// Generated `a:pPr` for a paragraph without a source property element. The alignment is always explicit,
/// because an inherited alignment would otherwise apply.
fn generated_ppr(props: &ParaProps) -> String {
    let mut inner = String::new();
    if props.line_spacing_pct != 100 {
        inner.push_str(&format!("<a:lnSpc><a:spcPct val=\"{}\"/></a:lnSpc>", props.line_spacing_pct * 1000));
    }
    if props.space_before != 0 {
        inner.push_str(&format!("<a:spcBef><a:spcPts val=\"{}\"/></a:spcBef>", props.space_before / 127));
    }
    if props.space_after != 0 {
        inner.push_str(&format!("<a:spcAft><a:spcPts val=\"{}\"/></a:spcAft>", props.space_after / 127));
    }
    format!("<a:pPr algn=\"{}\">{inner}</a:pPr>", align_attr(&props.align))
}

/// Replaces an existing child `local` of `element`, or inserts `fragment` before its schema successors.
fn child_splice(parsed: &Parsed<'_>, element: &XNode<'_, '_>, local: &str, fragment: &str, successors: &[&str]) -> DltResult<Splice> {
    match child(*element, NS_A, local) {
        Some(existing) => Ok(pxml::replace_element(&parsed.span(existing), fragment)),
        None => {
            let parent = parsed.span(*element);
            let before = successor_start(parsed, element, successors);
            pxml::insert_child(parsed.xml(), &parent, before, fragment)
        }
    }
}

fn typeface_splice(wrapper: &[u8], element: &XNode<'_, '_>, local: &str, face: &str, successors: &[&str]) -> DltResult<Splice> {
    let parsed = Parsed::new(wrapper)?;
    match child(*element, NS_A, local) {
        Some(existing) => pxml::set_attribute(wrapper, &parsed.span(existing), "typeface", face),
        None => {
            let parent = parsed.span(*element);
            let before = successor_start(&parsed, element, successors);
            let fragment = format!("<a:{local} typeface=\"{}\"/>", pxml::escape_attr(face)?);
            pxml::insert_child(wrapper, &parent, before, &fragment)
        }
    }
}

/// Replaces the fill child of `element`, or inserts the fill before its schema successors.
fn fill_splice(parsed: &Parsed<'_>, element: &XNode<'_, '_>, fragment: &str, successors: &[&str]) -> DltResult<Splice> {
    let existing = elements(*element)
        .into_iter()
        .find(|c| c.tag_name().namespace() == Some(NS_A) && FILL_NAMES.contains(&c.tag_name().name()));
    match existing {
        Some(fill) => Ok(pxml::replace_element(&parsed.span(fill), fragment)),
        None => {
            let parent = parsed.span(*element);
            let before = successor_start(parsed, element, successors);
            pxml::insert_child(parsed.xml(), &parent, before, fragment)
        }
    }
}

fn successor_start(parsed: &Parsed<'_>, element: &XNode<'_, '_>, successors: &[&str]) -> Option<usize> {
    elements(*element)
        .into_iter()
        .find(|c| c.tag_name().namespace() == Some(NS_A) && successors.contains(&c.tag_name().name()))
        .map(|c| parsed.span(c).start)
}

/// Inserts `fragment` before the first schema successor among the children of `parent`, or at the content end.
fn insert_before_successor(parsed: &Parsed<'_>, parent: &Range<usize>, successors: &[&str], fragment: &str) -> DltResult<Splice> {
    let element = parsed.at(parent.start)?;
    let before = elements(element)
        .into_iter()
        .find(|c| successors.contains(&c.tag_name().name()) && c.tag_name().namespace() == Some(NS_A))
        .map(|c| parsed.span(c).start);
    match before {
        Some(position) => Ok(pxml::insert_at(position, fragment)),
        None => {
            let content = pxml::content_range(parsed.xml(), parent)?.ok_or_else(|| malformed("parent has no content"))?;
            Ok(pxml::insert_at(content.end, fragment))
        }
    }
}

/// Solid fill element for a `#RRGGBB` colour.
pub(crate) fn solid_fill(color: &str) -> DltResult<String> {
    Ok(format!("<a:solidFill><a:srgbClr val=\"{}\"/></a:solidFill>", hex6(color)?))
}

pub(crate) fn fill_fragment(fill: &str) -> DltResult<String> {
    if fill == "none" {
        return Ok("<a:noFill/>".into());
    }
    if fill.starts_with("unsupported:") {
        return Err(DltError::unsupported("this fill cannot be written back exactly"));
    }
    solid_fill(fill)
}

/// Relationship ids referenced by `r:` attributes in a part.
fn referenced_ids(xml: &[u8]) -> BTreeSet<String> {
    let text = String::from_utf8_lossy(xml);
    let mut out = BTreeSet::new();
    for marker in ["r:embed=\"", "r:id=\"", "r:link=\""] {
        let mut rest: &str = &text;
        while let Some(at) = rest.find(marker) {
            let after = &rest[at + marker.len()..];
            match after.find('"') {
                Some(end) => {
                    out.insert(after[..end].to_string());
                    rest = &after[end..];
                }
                None => break,
            }
        }
    }
    out
}

/// Next free `rIdN` in a relationship list.
pub(crate) fn next_rel_id(rels: &[Relationship]) -> String {
    let max = rels
        .iter()
        .filter_map(|r| r.id.strip_prefix("rId").and_then(|n| n.parse::<u32>().ok()))
        .max()
        .unwrap_or(0);
    format!("rId{}", max + 1)
}

/// Relative target from one part to another, using `/` separators.
pub(crate) fn relative(from_part: &str, to_part: &str) -> String {
    let from: Vec<&str> = from_part.split('/').collect();
    let from_dir = &from[..from.len() - 1];
    let to: Vec<&str> = to_part.split('/').collect();
    let common = from_dir.iter().zip(to.iter()).take_while(|(a, b)| a == b).count();
    let mut out = String::new();
    for _ in common..from_dir.len() {
        out.push_str("../");
    }
    out.push_str(&to[common..].join("/"));
    out
}

/// A new slide with an empty shape tree. Shapes are appended to it.
fn new_slide_skeleton() -> Vec<u8> {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<p:sld xmlns:a=\"{NS_A}\" xmlns:r=\"{NS_R}\" xmlns:p=\"{NS_P}\"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"
    )
    .into_bytes()
}

/// Notes slide with a slide-image placeholder and a body placeholder that holds `text`.
fn new_notes_xml(text: &str) -> DltResult<Vec<u8>> {
    let rich = notes_rich(text, None, &[]);
    let body = paragraphs_xml(&rich)?;
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<p:notes xmlns:a=\"{NS_A}\" xmlns:r=\"{NS_R}\" xmlns:p=\"{NS_P}\"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr><p:sp><p:nvSpPr><p:cNvPr id=\"2\" name=\"Slide Image Placeholder 1\"/><p:cNvSpPr><a:spLocks noGrp=\"1\" noRot=\"1\" noChangeAspect=\"1\"/></p:cNvSpPr><p:nvPr><p:ph type=\"sldImg\"/></p:nvPr></p:nvSpPr><p:spPr/></p:sp><p:sp><p:nvSpPr><p:cNvPr id=\"3\" name=\"Notes Placeholder 2\"/><p:cNvSpPr><a:spLocks noGrp=\"1\"/></p:cNvSpPr><p:nvPr><p:ph type=\"body\" idx=\"1\"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/>{body}</p:txBody></p:sp></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:notes>"
    )
    .into_bytes())
}

/// Replaces the body placeholder paragraphs of an existing notes slide with `text`.
fn patch_notes(base: &[u8], notes: &ReadNotes, text: &str) -> DltResult<Vec<u8>> {
    let body = notes
        .body
        .as_ref()
        .ok_or_else(|| DltError::unsupported("the notes slide has no body placeholder to edit"))?;
    let parsed = Parsed::new(base)?;
    let decls = xmlx::in_scope_declarations(parsed.root());
    let template = notes.rich.paragraphs.iter().flat_map(|p| p.units.iter()).find_map(|u| u.style().cloned());
    let target = notes_rich(text, template, &notes.rich.paragraphs);
    let splices = text_body_splices(base, &decls, body, &notes.rich, &target)?;
    pxml::apply_checked(base, splices)
}

/// Rich text for notes text: one paragraph per line. Alignment and spacing follow the paragraph at the same
/// index in `source_paragraphs`. Vertical tab marks a line break.
fn notes_rich(text: &str, template: Option<Style>, source_paragraphs: &[Para]) -> RichText {
    let style = template.unwrap_or_else(|| run_style(FALLBACK_FONT.into(), 1200 * 127, "#000000".into(), false, false, "left"));
    let lines: Vec<&str> = if text.is_empty() { vec![""] } else { text.split('\n').collect() };
    let paragraphs = lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let props = source_paragraphs.get(index).map(|p| p.props.clone()).unwrap_or_default();
            let units = line
                .chars()
                .map(|ch| if ch == BREAK_CHAR { Unit::Break } else { Unit::Char { ch, style: style.clone() } })
                .collect();
            Para { props, units }
        })
        .collect();
    RichText { paragraphs }
}

/// Paragraph XML for generated text. Rich text is always rebuilt from the model here, never from a template.
fn paragraphs_xml(rich: &RichText) -> DltResult<String> {
    let mut out = String::new();
    for para in &rich.paragraphs {
        out.push_str(&paragraph_xml(&[], "", None, para)?);
    }
    Ok(out)
}

fn notes_master_xml() -> Vec<u8> {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<p:notesMaster xmlns:a=\"{NS_A}\" xmlns:r=\"{NS_R}\" xmlns:p=\"{NS_P}\"><p:cSld><p:bg><p:bgRef idx=\"1001\"><a:schemeClr val=\"bg1\"/></p:bgRef></p:bg><p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr><p:sp><p:nvSpPr><p:cNvPr id=\"2\" name=\"Slide Image Placeholder 1\"/><p:cNvSpPr><a:spLocks noGrp=\"1\" noRot=\"1\" noChangeAspect=\"1\"/></p:cNvSpPr><p:nvPr><p:ph type=\"sldImg\" idx=\"2\"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x=\"685800\" y=\"1143000\"/><a:ext cx=\"5486400\" cy=\"3086100\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom><a:noFill/><a:ln w=\"12700\"><a:solidFill><a:prstClr val=\"black\"/></a:solidFill></a:ln></p:spPr></p:sp><p:sp><p:nvSpPr><p:cNvPr id=\"3\" name=\"Notes Placeholder 2\"/><p:cNvSpPr><a:spLocks noGrp=\"1\"/></p:cNvSpPr><p:nvPr><p:ph type=\"body\" sz=\"quarter\" idx=\"3\"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x=\"685800\" y=\"4400550\"/><a:ext cx=\"5486400\" cy=\"3600450\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></p:spPr><p:txBody><a:bodyPr vert=\"horz\" lIns=\"91440\" tIns=\"45720\" rIns=\"91440\" bIns=\"45720\" rtlCol=\"0\"/><a:lstStyle/><a:p><a:pPr lvl=\"0\"/><a:r><a:rPr lang=\"en-US\"/><a:t>Click to edit Master text styles</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld><p:clrMap bg1=\"lt1\" tx1=\"dk1\" bg2=\"lt2\" tx2=\"dk2\" accent1=\"accent1\" accent2=\"accent2\" accent3=\"accent3\" accent4=\"accent4\" accent5=\"accent5\" accent6=\"accent6\" hlink=\"hlink\" folHlink=\"folHlink\"/><p:notesStyle><a:lvl1pPr marL=\"0\" algn=\"l\" defTabSz=\"914400\" rtl=\"0\" eaLnBrk=\"1\" latinLnBrk=\"0\" hangingPunct=\"1\"><a:defRPr sz=\"1200\" kern=\"1200\"><a:solidFill><a:schemeClr val=\"tx1\"/></a:solidFill><a:latin typeface=\"+mn-lt\"/><a:ea typeface=\"+mn-ea\"/><a:cs typeface=\"+mn-cs\"/></a:defRPr></a:lvl1pPr></p:notesStyle></p:notesMaster>"
    )
    .into_bytes()
}

/// Shape XML for a new text box, rectangle, or ellipse.
fn shape_xml(node: &Node, node_map: &ShapeMap, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    let cnvpr = node_map.cnvpr_id;
    let rich = RichText::from_parts(&node.text, &node_map.paragraphs)?;
    let label = if node_map.text_box { "TextBox" } else { "Shape" };
    let name = pxml::escape_attr(&format!("{label} {cnvpr}"))?;
    let cnv_sp = if node_map.text_box { "<p:cNvSpPr txBox=\"1\"/>" } else { "<p:cNvSpPr/>" };
    let fill = fill_fragment(&node.style.fill)?;
    let width = crate::v12::checked_line_width(node.style.stroke_width, &node.id)?;
    let ln = line_fragment(&node.style.stroke, width)?;
    let effects = editable::effect_list_xml(&node.style.shadows, &node.id, losses)?;
    let body = if node_map.has_text_body || !rich.paragraphs.is_empty() { tx_body_xml(&rich, &node_map.body)? } else { String::new() };
    Ok(format!(
        "<p:sp><p:nvSpPr><p:cNvPr id=\"{cnvpr}\" name=\"{name}\"/>{cnv_sp}<p:nvPr/></p:nvSpPr><p:spPr>{}<a:prstGeom prst=\"{}\"><a:avLst/></a:prstGeom>{fill}{ln}{effects}</p:spPr>{body}</p:sp>",
        xfrm_xml(&node.bounds, false, false),
        pxml::escape_attr(&node_map.geometry)?
    ))
}

/// A new vector path: custom geometry in its canonical form, with the fill and the outline of the node and no text body.
/// Losses of the geometry, such as overlapping subpaths, are added to `losses`.
fn path_shape_xml(node: &Node, node_map: &ShapeMap, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    let cnvpr = node_map.cnvpr_id;
    let path = node.ext.path.as_ref().ok_or_else(|| malformed(format!("path {} has no geometry", node.id)))?;
    let geometry = crate::v12::cust_geom_xml(path, &node.id, losses)?;
    let fill = fill_fragment(&node.style.fill)?;
    let width = crate::v12::checked_line_width(node.style.stroke_width, &node.id)?;
    let ln = line_fragment(&node.style.stroke, width)?;
    let name = pxml::escape_attr(&format!("Freeform {cnvpr}"))?;
    Ok(format!(
        "<p:sp><p:nvSpPr><p:cNvPr id=\"{cnvpr}\" name=\"{name}\"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr>{}{geometry}{fill}{ln}</p:spPr></p:sp>",
        xfrm_xml(&node.bounds, false, false)
    ))
}

/// The outer shadow of a preset shape changes. A modelled `a:effectLst` is replaced, or removed when no shadow is left. A
/// shape without one gets a new `a:effectLst` before its schema successors. An `a:effectLst` that the reader kept as an
/// opaque region is not rewritten, so a change to its shadow is refused.
fn effect_splices(
    parsed: &Parsed<'_>,
    source: &ReadShape,
    shadows: &[Shadow],
    node_id: &str,
    losses: &mut Vec<Diagnostic>,
) -> DltResult<Vec<Splice>> {
    match &source.spans.effect_list {
        Some(range) if shadows.is_empty() => Ok(vec![Splice { range: range.clone(), replacement: Vec::new() }]),
        Some(range) => {
            let fragment = editable::effect_list_xml(shadows, node_id, losses)?;
            Ok(vec![pxml::replace_element(range, &fragment)])
        }
        None => {
            let sp_pr = source.spans.sp_pr.as_ref().ok_or_else(|| malformed("shape has no spPr"))?;
            if has_unmodelled_effects(parsed, sp_pr)? {
                return Err(DltError::unsupported(format!(
                    "the shadow of {node_id} is kept in the file as written and cannot be changed exactly"
                )));
            }
            let fragment = editable::effect_list_xml(shadows, node_id, losses)?;
            Ok(vec![insert_before_successor(parsed, sp_pr, &SPPR_EFFECT_SUCCESSORS, &fragment)?])
        }
    }
}

/// Whether the `spPr` at `sp_pr` holds an `a:effectLst` or `a:effectDag` element.
fn has_unmodelled_effects(parsed: &Parsed<'_>, sp_pr: &Range<usize>) -> DltResult<bool> {
    let element = parsed.at(sp_pr.start)?;
    Ok(elements(element)
        .iter()
        .any(|c| c.tag_name().namespace() == Some(NS_A) && matches!(c.tag_name().name(), "effectLst" | "effectDag")))
}

/// `a:ln` for a new shape. `width` must already be checked by `v12::checked_line_width`.
pub(crate) fn line_fragment(stroke: &str, width: i64) -> DltResult<String> {
    if stroke == "none" {
        return Ok(format!("<a:ln w=\"{width}\"><a:noFill/></a:ln>"));
    }
    Ok(format!("<a:ln w=\"{width}\">{}</a:ln>", solid_fill(stroke)?))
}

pub(crate) fn xfrm_xml(bounds: &Rect, flip_h: bool, flip_v: bool) -> String {
    let mut attrs = String::new();
    if flip_h {
        attrs.push_str(" flipH=\"1\"");
    }
    if flip_v {
        attrs.push_str(" flipV=\"1\"");
    }
    format!(
        "<a:xfrm{attrs}><a:off x=\"{}\" y=\"{}\"/><a:ext cx=\"{}\" cy=\"{}\"/></a:xfrm>",
        bounds.x, bounds.y, bounds.width, bounds.height
    )
}

fn picture_xml(node: &Node, node_map: &ShapeMap, rid: &str) -> DltResult<String> {
    let cnvpr = node_map.cnvpr_id;
    Ok(format!(
        "<p:pic><p:nvPicPr><p:cNvPr id=\"{cnvpr}\" name=\"Picture {cnvpr}\"/><p:cNvPicPr><a:picLocks noChangeAspect=\"1\"/></p:cNvPicPr><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed=\"{rid}\"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr>{}<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></p:spPr></p:pic>",
        xfrm_xml(&node.bounds, false, false)
    ))
}

fn connector_xml(node: &Node, node_map: &ShapeMap) -> DltResult<String> {
    let cnvpr = node_map.cnvpr_id;
    let width = crate::v12::checked_line_width(node.style.stroke_width, &node.id)?;
    let markers = crate::v12::line_end_xml(&node.style);
    let legacy_tail = if markers.is_empty() && node_map.arrow { "<a:tailEnd type=\"triangle\"/>" } else { "" };
    let ln = if node.style.stroke == "none" {
        format!("<a:ln w=\"{width}\"><a:noFill/>{markers}{legacy_tail}</a:ln>")
    } else {
        format!("<a:ln w=\"{width}\">{}{markers}{legacy_tail}</a:ln>", solid_fill(&node.style.stroke)?)
    };
    Ok(format!(
        "<p:cxnSp><p:nvCxnSpPr><p:cNvPr id=\"{cnvpr}\" name=\"Line {cnvpr}\"/><p:cNvCxnSpPr/><p:nvPr/></p:nvCxnSpPr><p:spPr>{}<a:prstGeom prst=\"{}\"><a:avLst/></a:prstGeom>{ln}</p:spPr></p:cxnSp>",
        xfrm_xml(&node.bounds, node_map.flip_h, node_map.flip_v),
        pxml::escape_attr(&node_map.geometry)?
    ))
}

/// A complete `p:txBody` for generated text.
fn tx_body_xml(rich: &RichText, body: &BodyMap) -> DltResult<String> {
    let wrap = if body.wrap_square { "square" } else { "none" };
    let rich = if rich.paragraphs.is_empty() {
        RichText { paragraphs: vec![Para { props: ParaProps::default(), units: vec![] }] }
    } else {
        rich.clone()
    };
    let paragraphs = paragraphs_xml(&rich)?;
    Ok(format!(
        "<p:txBody><a:bodyPr wrap=\"{wrap}\" lIns=\"{}\" tIns=\"{}\" rIns=\"{}\" bIns=\"{}\" anchor=\"{}\" rtlCol=\"0\"/><a:lstStyle/>{paragraphs}</p:txBody>",
        body.insets[0], body.insets[1], body.insets[2], body.insets[3], body.anchor
    ))
}

/// Presentation part with the new slide list. Adds the notes master id list when the notes master is new.
fn presentation_xml(base: &[u8], entries: &[(u32, String)], created_notes_master_rid: Option<&str>) -> DltResult<Vec<u8>> {
    let parsed = Parsed::new(base)?;
    let root = parsed.root();
    let mut list = String::from("<p:sldIdLst>");
    for (sld_id, rid) in entries {
        list.push_str(&format!("<p:sldId id=\"{sld_id}\" r:id=\"{}\"/>", pxml::escape_attr(rid)?));
    }
    list.push_str("</p:sldIdLst>");
    let mut splices = Vec::new();
    match child(root, NS_P, "sldIdLst") {
        Some(existing) => splices.push(pxml::replace_element(&parsed.span(existing), &list)),
        None => {
            let at = match first_child_matching(&parsed, &PRES_AFTER_SLDIDLST) {
                Some(at) => at,
                None => root_content_end(&parsed)?,
            };
            splices.push(pxml::insert_at(at, &list));
        }
    }
    if let Some(rid) = created_notes_master_rid {
        if child(root, NS_P, "notesMasterIdLst").is_none() {
            let fragment = format!("<p:notesMasterIdLst><p:notesMasterId r:id=\"{}\"/></p:notesMasterIdLst>", pxml::escape_attr(rid)?);
            let mut before: Vec<&str> = vec!["sldIdLst"];
            before.extend_from_slice(&PRES_AFTER_SLDIDLST);
            let at = match first_child_matching(&parsed, &before) {
                Some(at) => at,
                None => root_content_end(&parsed)?,
            };
            splices.push(pxml::insert_at(at, &fragment));
        }
    }
    pxml::apply_checked(base, splices)
}

/// Start offset of the first presentation child whose name is in `names`.
fn first_child_matching(parsed: &Parsed<'_>, names: &[&str]) -> Option<usize> {
    parsed
        .root()
        .children()
        .find(|c| c.is_element() && c.tag_name().namespace() == Some(NS_P) && names.contains(&c.tag_name().name()))
        .map(|c| parsed.span(c).start)
}

/// Offset just before the presentation end tag. The end tag is found from the document, not assumed to be `</p:presentation>`.
/// A presentation with no content has nowhere to take a new child, so it is refused.
fn root_content_end(parsed: &Parsed<'_>) -> DltResult<usize> {
    let root = parsed.span(parsed.root());
    let content = pxml::content_range(parsed.xml(), &root)?.ok_or_else(|| malformed("the presentation has no content"))?;
    Ok(content.end)
}

/// Relative target helper for callers outside this module.
pub fn part_relative(from_part: &str, to_part: &str) -> String {
    relative(from_part, to_part)
}

/// Next free relationship id helper for callers outside this module.
pub fn free_rel_id(rels: &[Relationship]) -> String {
    next_rel_id(rels)
}
