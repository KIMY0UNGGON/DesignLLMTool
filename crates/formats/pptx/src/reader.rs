//! Reads a PresentationML package into pages, nodes, mapping records, and byte spans.
//!
//! The reader is the single source of truth for what a slide means. `open`, the writer's diff, and the
//! verifier all call it, so a value the reader produces from the output is the value the writer must have
//! written.
//!
//! Modelled: slide size and order, text boxes and preset shapes with text, straight lines, pictures in PNG,
//! JPEG, GIF, and BMP, notes text, and theme colours and fonts. Everything else is preserved as an opaque
//! element. Its bytes are hashed at part + anchor, and the verifier checks each hash.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use designllm_contracts::{
    AssetRef, Diagnostic, DltResult, LineEnd, Node, NodeExt, NodeKind, OpaqueRegion, Page, Rect, Size, Style,
};
use designllm_package::opc::{self, ContentTypes, Relationship};
use designllm_package::xml as pxml;
use designllm_package::Package;
use roxmltree::Node as XNode;

use crate::editable;
use crate::model::{BodyMap, PageMap, ShapeMap, KIND_CONNECTOR, KIND_PAGE, KIND_PICTURE, KIND_SHAPE, ORIGIN_SOURCE};
use crate::ns::{self, NS_A, NS_P, NS_R};
use crate::text::{ParaProps, Para, RichText, Unit};
use crate::theme::{
    font_from_theme, parse_color_element, ColorContext, ColorMap, ColorSpec, FillStyle, Theme, FALLBACK_FONT,
};
use crate::xmlx::{attr_i64, bool_attr, child, children, direct_text, elements, is_el, malformed, span};

/// Slide size, theme, and default-text information shared by all pages.
#[derive(Clone, Debug)]
pub struct ReadPresentation {
    pub presentation_part: String,
    pub slide_size: Size,
    pub notes_master: Option<String>,
    pub theme: Theme,
    pub color_map: ColorMap,
    pub defaults: StyleDefaults,
    /// Presentation-level elements, except the slide list, that the writer must keep byte-identical.
    pub opaque: Vec<OpaqueRegion>,
    pub diagnostics: Vec<Diagnostic>,
}

/// One slide, in presentation order.
#[derive(Clone, Debug)]
pub struct ReadPage {
    pub page: Page,
    pub map: PageMap,
    /// One entry per node of `page.nodes`, in the same order.
    pub shapes: Vec<ReadShape>,
    pub relationships: Vec<Relationship>,
    pub notes: Option<ReadNotes>,
    /// Opaque regions of the slide part and of its notes part.
    pub opaque: Vec<OpaqueRegion>,
    pub diagnostics: Vec<Diagnostic>,
}

/// A modelled shape with the spans the writer needs.
#[derive(Clone, Debug)]
pub struct ReadShape {
    pub node: Node,
    pub map: ShapeMap,
    pub spans: ShapeSpans,
}

/// A notes slide: part name, plain text, and body placeholder spans.
#[derive(Clone, Debug)]
pub struct ReadNotes {
    pub part: String,
    pub text: String,
    pub rich: RichText,
    pub body: Option<ShapeSpans>,
}

/// Byte ranges of one modelled shape inside its part.
#[derive(Clone, Debug, Default)]
pub struct ShapeSpans {
    pub element: Range<usize>,
    pub xfrm: Option<Range<usize>>,
    pub off: Option<Range<usize>>,
    pub ext: Option<Range<usize>>,
    pub sp_pr: Option<Range<usize>>,
    /// Direct fill element of `spPr` (`noFill`, `solidFill`, ...).
    pub fill: Option<Range<usize>>,
    /// `a:ln` of `spPr`.
    pub ln: Option<Range<usize>>,
    /// `a:effectLst` of `spPr` when its outer shadow is modelled. The writer replaces or removes this element.
    pub effect_list: Option<Range<usize>>,
    pub tx_body: Option<Range<usize>>,
    pub paragraphs: Vec<ParaSpans>,
}

/// Byte ranges of one paragraph.
#[derive(Clone, Debug)]
pub struct ParaSpans {
    pub element: Range<usize>,
    pub ppr: Option<Range<usize>>,
    pub end_rpr: Option<Range<usize>>,
    /// One entry per unit of the paragraph, in order.
    pub units: Vec<UnitSpans>,
}

/// Source kind of one unit, used when the writer rebuilds a paragraph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitKind {
    /// A character inside `a:r`.
    Char,
    /// A child `a:br`. Its bytes are copied when the paragraph is rebuilt.
    Break,
    /// A raw newline inside `a:t`, read as a break. The writer emits `a:br` for it.
    RawBreak,
    /// `a:fld` or another paragraph child. Its bytes are copied when the paragraph is rebuilt.
    Atom,
}

#[derive(Clone, Debug)]
pub struct UnitSpans {
    pub kind: UnitKind,
    /// `a:r` for characters, the element itself for breaks and atoms.
    pub element: Range<usize>,
    /// `a:rPr` of the run, when present.
    pub rpr: Option<Range<usize>>,
}

/// Inherited text defaults for one level of a style list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LevelDefaults {
    pub size: Option<i64>,
    pub color: Option<ColorSpec>,
    pub latin: Option<String>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub align: Option<String>,
}

/// Default text layers. `title`, `body`, and `other` come from the master's `txStyles`. `presentation`
/// comes from `p:defaultTextStyle`. Each array holds levels 1 to 9.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StyleDefaults {
    pub title: [LevelDefaults; 9],
    pub body: [LevelDefaults; 9],
    pub other: [LevelDefaults; 9],
    pub presentation: [LevelDefaults; 9],
}

/// Shared inputs for reading any part.
pub(crate) struct Env<'a> {
    pub package: &'a Package,
    pub content_types: Option<&'a ContentTypes>,
    pub theme: &'a Theme,
    pub color_map: &'a ColorMap,
    pub defaults: &'a StyleDefaults,
    pub put: &'a dyn Fn(&[u8], &str) -> DltResult<AssetRef>,
}

/// Collects opaque regions and diagnostics for one part.
pub(crate) struct PartCtx<'a> {
    pub part: &'a str,
    pub xml: &'a [u8],
    pub offset: usize,
    pub regions: Vec<OpaqueRegion>,
    pub diagnostics: Vec<Diagnostic>,
}

impl PartCtx<'_> {
    pub fn push_region(&mut self, anchor: &str, node: XNode<'_, '_>) {
        self.regions.push(OpaqueRegion {
            part: self.part.to_string(),
            anchor: anchor.to_string(),
            sha256: pxml::range_sha256(self.xml, &span(node, self.offset)),
        });
    }

    pub fn diagnose(&mut self, code: &str, message: impl Into<String>, node_id: Option<String>) {
        self.diagnostics.push(Diagnostic { code: code.to_string(), message: message.into(), node_id, blocking: false, params: Default::default() });
    }
}

/// Occurrence counters for anchors. A segment is `key[occurrence]`, where the occurrence counts earlier
/// siblings with the same key under the same parent anchor.
#[derive(Default)]
pub(crate) struct Anchors {
    counts: BTreeMap<String, usize>,
}

impl Anchors {
    pub fn segment(&mut self, parent: &str, key: &str) -> String {
        let slot = self.counts.entry(format!("{parent}\u{1}{key}")).or_insert(0);
        let occurrence = *slot;
        *slot += 1;
        format!("{key}[{occurrence}]")
    }
}

/// Reads the presentation: presentation-level state, then every slide in order.
pub fn read_presentation(
    package: &Package,
    put: &dyn Fn(&[u8], &str) -> DltResult<AssetRef>,
) -> DltResult<(ReadPresentation, Vec<ReadPage>)> {
    let pres_bytes = package
        .part(ns::PRESENTATION_PART)
        .map_err(|_| malformed("package has no ppt/presentation.xml"))?;
    let pres_rels_bytes = package
        .part(ns::PRESENTATION_RELS_PART)
        .map_err(|_| malformed("presentation relationships are missing"))?;
    let pres_rels = opc::parse_rels(pres_rels_bytes)?;
    let content_types = package
        .part(opc::CONTENT_TYPES_PART)
        .ok()
        .and_then(|bytes| ContentTypes::parse(bytes).ok());

    let (doc, offset) = crate::xmlx::parse(pres_bytes)?;
    let root = doc.root_element();
    if !is_el(root, NS_P, "presentation") {
        return Err(malformed("ppt/presentation.xml root is not p:presentation"));
    }
    let mut pres = PartCtx { part: ns::PRESENTATION_PART, xml: pres_bytes, offset, regions: Vec::new(), diagnostics: Vec::new() };
    pres.regions.extend(rel_regions(ns::PRESENTATION_RELS_PART, &pres_rels));
    let mut anchors = Anchors::default();
    let root_anchor = "p:presentation[0]";
    for node in elements(root) {
        if !is_el(node, NS_P, "sldIdLst") {
            let anchor = format!("{root_anchor}/{}", anchors.segment(root_anchor, &pxml::default_key(node)));
            pres.push_region(&anchor, node);
        }
    }

    let slide_size = {
        let size = child(root, NS_P, "sldSz").ok_or_else(|| malformed("presentation has no sldSz"))?;
        let cx = attr_i64(size, "cx").ok_or_else(|| malformed("sldSz without cx"))?;
        let cy = attr_i64(size, "cy").ok_or_else(|| malformed("sldSz without cy"))?;
        if cx <= 0 || cy <= 0 {
            return Err(malformed("sldSz must be positive"));
        }
        Size { width: cx, height: cy }
    };

    let mut defaults = StyleDefaults::default();
    if let Some(default_style) = child(root, NS_P, "defaultTextStyle") {
        defaults.presentation = read_levels(default_style, NS_A, "lvl", "defRPr");
    }

    // The first slide master supplies the colour map, text styles, and theme.
    let mut theme = Theme::default();
    let mut color_map = ColorMap::default();
    let master_rel = child(root, NS_P, "sldMasterIdLst")
        .and_then(|list| child(list, NS_P, "sldMasterId"))
        .and_then(|entry| entry.attribute((NS_R, "id")))
        .and_then(|id| pres_rels.iter().find(|r| r.id == id));
    match master_rel {
        Some(master_rel) => {
            let master_part = opc::resolve_target(ns::PRESENTATION_PART, &master_rel.target)?;
            let master_bytes = package.part(&master_part).map_err(|_| malformed("slide master part is missing"))?;
            let (master_doc, _) = crate::xmlx::parse(master_bytes)?;
            let master_root = master_doc.root_element();
            if let Some(clr) = child(master_root, NS_P, "clrMap") {
                color_map = ColorMap::from_clr_map(clr);
            }
            if let Some(styles) = child(master_root, NS_P, "txStyles") {
                if let Some(title) = child(styles, NS_P, "titleStyle") {
                    defaults.title = read_levels(title, NS_A, "lvl", "defRPr");
                }
                if let Some(body) = child(styles, NS_P, "bodyStyle") {
                    defaults.body = read_levels(body, NS_A, "lvl", "defRPr");
                }
                if let Some(other) = child(styles, NS_P, "otherStyle") {
                    defaults.other = read_levels(other, NS_A, "lvl", "defRPr");
                }
            }
            let master_rels = match package.part(&opc::rels_part_for(&master_part)) {
                Ok(bytes) => opc::parse_rels(bytes)?,
                Err(_) => Vec::new(),
            };
            match master_rels.iter().find(|r| r.rel_type == ns::REL_THEME) {
                Some(theme_rel) => {
                    let theme_part = opc::resolve_target(&master_part, &theme_rel.target)?;
                    theme = Theme::parse(package.part(&theme_part).map_err(|_| malformed("theme part is missing"))?)?;
                }
                None => pres.diagnose("PPTX_THEME_MISSING", "the slide master names no theme; theme colours are unresolved", None),
            }
        }
        None => pres.diagnose("PPTX_MASTER_MISSING", "the presentation names no slide master", None),
    }

    let notes_master = child(root, NS_P, "notesMasterIdLst")
        .and_then(|list| child(list, NS_P, "notesMasterId"))
        .and_then(|entry| entry.attribute((NS_R, "id")))
        .and_then(|id| pres_rels.iter().find(|r| r.id == id))
        .map(|rel| opc::resolve_target(ns::PRESENTATION_PART, &rel.target))
        .transpose()?;

    let env = Env {
        package,
        content_types: content_types.as_ref(),
        theme: &theme,
        color_map: &color_map,
        defaults: &defaults,
        put,
    };

    // Slide list, in presentation order. Ids must be unique and at least 256; relationship ids must be unique.
    let mut seen_ids = BTreeSet::new();
    let mut seen_rids = BTreeSet::new();
    let mut seen_parts = BTreeSet::new();
    let mut entries: Vec<(u32, String, String)> = Vec::new();
    if let Some(list) = child(root, NS_P, "sldIdLst") {
        for entry in children(list, NS_P, "sldId") {
            let raw = attr_i64(entry, "id").ok_or_else(|| malformed("sldId without id"))?;
            let sld_id = u32::try_from(raw).map_err(|_| malformed("sldId out of range"))?;
            if sld_id < 256 {
                return Err(malformed(format!("sldId {sld_id} is below 256")));
            }
            if !seen_ids.insert(sld_id) {
                return Err(malformed(format!("duplicate sldId {sld_id}")));
            }
            let rid = entry.attribute((NS_R, "id")).ok_or_else(|| malformed("sldId without r:id"))?.to_string();
            if !seen_rids.insert(rid.clone()) {
                return Err(malformed(format!("duplicate slide relationship {rid}")));
            }
            let rel = pres_rels
                .iter()
                .find(|r| r.id == rid)
                .ok_or_else(|| malformed(format!("slide relationship {rid} is missing")))?;
            if rel.rel_type != ns::REL_SLIDE {
                return Err(malformed(format!("relationship {rid} is not a slide")));
            }
            let slide_part = opc::resolve_target(ns::PRESENTATION_PART, &rel.target)?;
            if !seen_parts.insert(slide_part.to_ascii_lowercase()) {
                return Err(malformed(format!("slide part {slide_part} is listed twice")));
            }
            entries.push((sld_id, rid, slide_part));
        }
    }

    let mut pages = Vec::with_capacity(entries.len());
    for (sld_id, rid, slide_part) in entries {
        let slide_bytes = package
            .part(&slide_part)
            .map_err(|_| malformed(format!("slide part {slide_part} is missing")))?;
        let rels = match package.part(&opc::rels_part_for(&slide_part)) {
            Ok(bytes) => opc::parse_rels(bytes)?,
            Err(_) => Vec::new(),
        };
        pages.push(read_slide(&env, sld_id, &rid, &slide_part, slide_bytes, &rels, slide_size)?);
    }

    let presentation = ReadPresentation {
        presentation_part: ns::PRESENTATION_PART.to_string(),
        slide_size,
        notes_master,
        theme,
        color_map,
        defaults,
        opaque: pres.regions,
        diagnostics: pres.diagnostics,
    };
    Ok((presentation, pages))
}

/// Per-slide reading state.
pub(crate) struct SlideCtx<'a, 'b> {
    pub env: &'b Env<'a>,
    pub part: &'b mut PartCtx<'a>,
    pub rels: &'b [Relationship],
    pub page_id: String,
}

/// Reads one slide and its notes.
fn read_slide(
    env: &Env<'_>,
    sld_id: u32,
    rid: &str,
    part: &str,
    xml: &[u8],
    rels: &[Relationship],
    slide_size: Size,
) -> DltResult<ReadPage> {
    let (doc, offset) = crate::xmlx::parse(xml)?;
    let root = doc.root_element();
    if !is_el(root, NS_P, "sld") {
        return Err(malformed(format!("{part} root is not p:sld")));
    }
    let page_id = format!("slide-{sld_id}");
    let mut pc = PartCtx { part, xml, offset, regions: Vec::new(), diagnostics: Vec::new() };
    let mut anchors = Anchors::default();
    let root_anchor = "p:sld[0]";
    for node in elements(root) {
        if !(is_el(node, NS_P, "cSld") || is_el(node, NS_P, "clrMapOvr")) {
            let anchor = format!("{root_anchor}/{}", anchors.segment(root_anchor, &pxml::default_key(node)));
            pc.push_region(&anchor, node);
        }
    }
    let c_sld = child(root, NS_P, "cSld").ok_or_else(|| malformed(format!("{part} has no cSld")))?;
    let c_sld_anchor = format!("{root_anchor}/p:cSld[0]");
    for node in elements(c_sld) {
        if !is_el(node, NS_P, "spTree") {
            let anchor = format!("{c_sld_anchor}/{}", anchors.segment(&c_sld_anchor, &pxml::default_key(node)));
            pc.push_region(&anchor, node);
        }
    }
    let sp_tree = child(c_sld, NS_P, "spTree").ok_or_else(|| malformed(format!("{part} has no spTree")))?;
    let tree_anchor = format!("{c_sld_anchor}/p:spTree[0]");
    for node in elements(sp_tree) {
        if is_el(node, NS_P, "nvGrpSpPr") || is_el(node, NS_P, "grpSpPr") {
            let anchor = format!("{tree_anchor}/{}", anchors.segment(&tree_anchor, &pxml::default_key(node)));
            pc.push_region(&anchor, node);
        }
    }

    // Every cNvPr in the slide must be unique, modelled or not, because new shapes take the next free id.
    let mut all_ids = BTreeSet::new();
    let mut max_shape_id = 0u32;
    for node in sp_tree.descendants().filter(|n| is_el(*n, NS_P, "cNvPr")) {
        let id: u32 = node
            .attribute("id")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| malformed(format!("{part}: cNvPr without a numeric id")))?;
        if !all_ids.insert(id) {
            return Err(malformed(format!("{part}: duplicate shape id {id}")));
        }
        max_shape_id = max_shape_id.max(id);
    }

    let mut sc = SlideCtx { env, part: &mut pc, rels, page_id: page_id.clone() };
    let mut nodes = Vec::new();
    let mut shapes = Vec::new();
    for node in elements(sp_tree) {
        if is_el(node, NS_P, "nvGrpSpPr") || is_el(node, NS_P, "grpSpPr") {
            continue;
        }
        let modelled = if is_el(node, NS_P, "sp") {
            read_sp(&mut sc, node, &tree_anchor, &mut anchors)?
        } else if is_el(node, NS_P, "pic") {
            read_pic(&mut sc, node, &tree_anchor, &mut anchors)?
        } else if is_el(node, NS_P, "cxnSp") {
            read_cxn(&mut sc, node, &tree_anchor, &mut anchors)?
        } else {
            None
        };
        match modelled {
            Some(shape) => {
                nodes.push(shape.node.clone());
                shapes.push(shape);
            }
            None => {
                let anchor = format!("{tree_anchor}/{}", anchors.segment(&tree_anchor, &shape_key(node)));
                sc.part.push_region(&anchor, node);
                let message = format!("{} in {part} is preserved and is not editable in this release", pxml::default_key(node));
                sc.part.diagnose("PPTX_UNMODELLED_OBJECT", message, None);
            }
        }
    }

    let notes = read_notes(env, rels, part, &mut pc.diagnostics)?;
    let notes_text = notes.as_ref().map(|n| n.text.clone()).unwrap_or_default();
    let notes_part = notes.as_ref().map(|n| n.part.clone());
    let mut opaque = pc.regions;
    opaque.extend(rel_regions(&opc::rels_part_for(part), rels));
    if let Some(n) = &notes {
        opaque.extend(n.opaque.iter().cloned());
    }
    Ok(ReadPage {
        page: Page { nodes, notes: notes_text, ..Page::new(page_id, slide_size) },
        map: PageMap {
            kind: KIND_PAGE.to_string(),
            origin: ORIGIN_SOURCE.to_string(),
            part: part.to_string(),
            sld_id,
            rel_id: rid.to_string(),
            notes_part,
            max_shape_id,
        },
        shapes,
        relationships: rels.to_vec(),
        notes: notes.map(|n| ReadNotes { part: n.part, text: n.text, rich: n.rich, body: n.body }),
        opaque,
        diagnostics: pc.diagnostics,
    })
}

/// A notes read result, with the opaque regions of the notes part.
struct NotesRead {
    part: String,
    text: String,
    rich: RichText,
    body: Option<ShapeSpans>,
    opaque: Vec<OpaqueRegion>,
}

/// Reads the notes slide that a slide relationship names, if any.
fn read_notes(env: &Env<'_>, rels: &[Relationship], slide_part: &str, diagnostics: &mut Vec<Diagnostic>) -> DltResult<Option<NotesRead>> {
    let Some(rel) = rels.iter().find(|r| r.rel_type == ns::REL_NOTES_SLIDE) else {
        return Ok(None);
    };
    let part = opc::resolve_target(slide_part, &rel.target)?;
    let bytes = match env.package.part(&part) {
        Ok(bytes) => bytes,
        Err(_) => {
            diagnostics.push(Diagnostic {
                code: "PPTX_NOTES_MISSING".into(),
                message: format!("notes part {part} is missing"),
                node_id: None,
                blocking: false,
                params: Default::default(),
            });
            return Ok(None);
        }
    };
    let (doc, offset) = crate::xmlx::parse(bytes)?;
    let root = doc.root_element();
    if !is_el(root, NS_P, "notes") {
        return Err(malformed(format!("{part} root is not p:notes")));
    }
    let mut pc = PartCtx { part: &part, xml: bytes, offset, regions: Vec::new(), diagnostics: Vec::new() };
    let mut anchors = Anchors::default();
    let root_anchor = "p:notes[0]";
    for node in elements(root) {
        if !(is_el(node, NS_P, "cSld") || is_el(node, NS_P, "clrMapOvr")) {
            let anchor = format!("{root_anchor}/{}", anchors.segment(root_anchor, &pxml::default_key(node)));
            pc.push_region(&anchor, node);
        }
    }
    let c_sld = child(root, NS_P, "cSld").ok_or_else(|| malformed(format!("{part} has no cSld")))?;
    let c_anchor = format!("{root_anchor}/p:cSld[0]");
    for node in elements(c_sld) {
        if !is_el(node, NS_P, "spTree") {
            let anchor = format!("{c_anchor}/{}", anchors.segment(&c_anchor, &pxml::default_key(node)));
            pc.push_region(&anchor, node);
        }
    }
    let tree = child(c_sld, NS_P, "spTree").ok_or_else(|| malformed(format!("{part} has no spTree")))?;
    let tree_anchor = format!("{c_anchor}/p:spTree[0]");
    let mut body: Option<ShapeSpans> = None;
    let mut text = String::new();
    let mut rich_notes = RichText::default();
    let plain_levels = TextLevels::default();
    for node in elements(tree) {
        if is_el(node, NS_P, "sp") && body.is_none() && is_body_placeholder(node) {
            let tx = child(node, NS_P, "txBody").ok_or_else(|| malformed("notes body placeholder has no txBody"))?;
            let mut notes = Vec::new();
            let (rich, paragraphs) = read_text_body(tx, &mut pc, &plain_levels, env, &mut notes)?;
            text = rich.plain();
            rich_notes = rich;
            body = Some(ShapeSpans {
                element: span(node, offset),
                sp_pr: child(node, NS_P, "spPr").map(|n| span(n, offset)),
                tx_body: Some(span(tx, offset)),
                paragraphs,
                ..ShapeSpans::default()
            });
        } else if is_el(node, NS_P, "nvGrpSpPr") || is_el(node, NS_P, "grpSpPr") {
            let anchor = format!("{tree_anchor}/{}", anchors.segment(&tree_anchor, &pxml::default_key(node)));
            pc.push_region(&anchor, node);
        } else {
            let anchor = format!("{tree_anchor}/{}", anchors.segment(&tree_anchor, &shape_key(node)));
            pc.push_region(&anchor, node);
        }
    }
    diagnostics.append(&mut pc.diagnostics);
    Ok(Some(NotesRead { part: part.clone(), text, rich: rich_notes, body, opaque: pc.regions }))
}

/// Preservation regions for relationships the writer does not manage. Each region hashes the canonical relationship.
fn rel_regions(rels_part: &str, rels: &[Relationship]) -> Vec<OpaqueRegion> {
    rels.iter()
        .filter(|r| !ns::is_managed_rel_type(&r.rel_type))
        .map(|r| OpaqueRegion {
            part: rels_part.to_string(),
            anchor: format!("Relationships/Relationship#{}", r.id),
            sha256: designllm_contracts::sha256(format!("{}|{}|{}|{}", r.id, r.rel_type, r.target, r.external).as_bytes()),
        })
        .collect()
}

/// Preservation regions for the non-modelled parts of a shape's non-visual properties. Children of `p:nvSpPr`
/// (or its picture and connector counterparts) are recorded whole, except `p:cNvPr`. For `p:cNvPr`, each child
/// (for example `a:extLst`) and each namespaced attribute is recorded separately, so a change is located precisely.
fn record_nv(pc: &mut PartCtx<'_>, shape_anchor: &str, nv: XNode<'_, '_>) {
    let nv_anchor = format!("{shape_anchor}/{}[0]", pxml::default_key(nv));
    let mut local = Anchors::default();
    for node in elements(nv) {
        if is_el(node, NS_P, "cNvPr") {
            let cnv_anchor = format!("{nv_anchor}/p:cNvPr[0]");
            record_attributes(pc, &cnv_anchor, node);
            for inner in elements(node) {
                let anchor = format!("{cnv_anchor}/{}", local.segment(&cnv_anchor, &pxml::default_key(inner)));
                pc.push_region(&anchor, inner);
            }
        } else {
            let anchor = format!("{nv_anchor}/{}", local.segment(&nv_anchor, &pxml::default_key(node)));
            pc.push_region(&anchor, node);
        }
    }
}

/// One preservation region per namespaced attribute of `node`. The hashed text is `prefix:name="value"`, with the
/// prefix the element uses for that namespace, so the bytes match the saved attribute.
fn record_attributes(pc: &mut PartCtx<'_>, anchor: &str, node: XNode<'_, '_>) {
    for attr in node.attributes() {
        let Some(uri) = attr.namespace() else { continue };
        let prefix = node
            .ancestors()
            .filter(|n| n.is_element())
            .flat_map(|n| n.namespaces())
            .find(|ns| ns.uri() == uri)
            .and_then(|ns| ns.name().map(str::to_string));
        let text = match prefix {
            Some(p) => format!("{p}:{}=\"{}\"", attr.name(), attr.value()),
            None => format!("{}=\"{}\"", attr.name(), attr.value()),
        };
        pc.regions.push(OpaqueRegion {
            part: pc.part.to_string(),
            anchor: format!("{anchor}/@{{{uri}}}{}", attr.name()),
            sha256: designllm_contracts::sha256(text.as_bytes()),
        });
    }
}

fn is_body_placeholder(sp: XNode<'_, '_>) -> bool {
    child(sp, NS_P, "nvSpPr")
        .and_then(|nv| child(nv, NS_P, "nvPr"))
        .and_then(|nvpr| child(nvpr, NS_P, "ph"))
        .map(|ph| ph.attribute("type") == Some("body"))
        .unwrap_or(false)
}

/// Key used in anchors. Shape-like elements include their `cNvPr/@id` so that anchors survive sibling changes.
fn shape_key(node: XNode<'_, '_>) -> String {
    let base = pxml::default_key(node);
    let nv = ["nvSpPr", "nvPicPr", "nvCxnSpPr", "nvGrpSpPr", "nvGraphicFramePr"]
        .iter()
        .find_map(|name| child(node, NS_P, name));
    match nv.and_then(|nv| child(nv, NS_P, "cNvPr")).and_then(|c| c.attribute("id")) {
        Some(id) => format!("{base}#{id}"),
        None => base,
    }
}

/// Reads `lvl1pPr..lvl9pPr` of a list-style container and returns the defaults for each level.
fn read_levels(list: XNode<'_, '_>, ns: &str, level_prefix: &str, run_tag: &str) -> [LevelDefaults; 9] {
    let mut out: [LevelDefaults; 9] = Default::default();
    for (index, slot) in out.iter_mut().enumerate() {
        let name = format!("{level_prefix}{}pPr", index + 1);
        if let Some(level) = child(list, ns, &name) {
            *slot = level_defaults(level, run_tag);
        }
    }
    out
}

fn level_defaults(level: XNode<'_, '_>, run_tag: &str) -> LevelDefaults {
    let mut out = LevelDefaults::default();
    if let Some(algn) = level.attribute("algn") {
        out.align = Some(align_name(algn).to_string());
    }
    if let Some(rpr) = child(level, NS_A, run_tag) {
        out.size = attr_i64(rpr, "sz");
        out.bold = bool_attr(rpr, "b");
        out.italic = bool_attr(rpr, "i");
        out.latin = child(rpr, NS_A, "latin").and_then(|l| l.attribute("typeface")).map(str::to_string);
        out.color = child(rpr, NS_A, "solidFill").and_then(|f| f.children().find(|c| c.is_element())).and_then(parse_color_element);
    }
    out
}

/// Maps `a:pPr/@algn` to the model's alignment names.
pub fn align_name(value: &str) -> &'static str {
    match value {
        "ctr" => "center",
        "r" => "right",
        "just" | "justLow" | "thaiDist" => "justify",
        "dist" => "distributed",
        _ => "left",
    }
}

/// Reverse of [`align_name`], for writing `a:pPr/@algn`.
pub fn align_attr(name: &str) -> &'static str {
    match name {
        "center" => "ctr",
        "right" => "r",
        "justify" => "just",
        "distributed" => "dist",
        _ => "l",
    }
}

/// Text inheritance context for one text body: its own list style, the master layer for its placeholder
/// type, and the font colour from its style reference.
#[derive(Clone, Debug, Default)]
pub(crate) struct TextLevels {
    pub shape: [LevelDefaults; 9],
    pub master: [LevelDefaults; 9],
    pub style_font: Option<ColorSpec>,
}

fn master_layer<'a>(defaults: &'a StyleDefaults, placeholder: Option<&str>) -> &'a [LevelDefaults; 9] {
    match placeholder {
        Some("title") | Some("ctrTitle") => &defaults.title,
        Some("body") | Some("subTitle") | Some("obj") => &defaults.body,
        _ => &defaults.other,
    }
}

fn read_sp(sc: &mut SlideCtx<'_, '_>, sp: XNode<'_, '_>, tree_anchor: &str, anchors: &mut Anchors) -> DltResult<Option<ReadShape>> {
    let env = sc.env;
    let Some(nv) = child(sp, NS_P, "nvSpPr") else { return Ok(None) };
    let Some(c_nv) = child(nv, NS_P, "cNvPr") else { return Ok(None) };
    let Some(id) = c_nv.attribute("id").and_then(|v| v.parse::<u32>().ok()) else { return Ok(None) };
    let Some(sp_pr) = child(sp, NS_P, "spPr") else { return Ok(None) };
    let Some(xfrm) = child(sp_pr, NS_A, "xfrm") else { return Ok(None) };
    let (Some(off), Some(ext)) = (child(xfrm, NS_A, "off"), child(xfrm, NS_A, "ext")) else { return Ok(None) };
    // A vector path is read only in its canonical form (see `editable`). Any other custom geometry stays opaque.
    let custom_path = match child(sp_pr, NS_A, "custGeom") {
        Some(geometry) => match editable::read_custom_geometry(geometry) {
            Some(path) => Some(path),
            None => return Ok(None),
        },
        None => None,
    };
    let prst = match custom_path {
        Some(_) => "custGeom",
        None => {
            let Some(prst) = child(sp_pr, NS_A, "prstGeom").and_then(|g| g.attribute("prst")) else { return Ok(None) };
            prst
        }
    };
    let (Some(x), Some(y), Some(cx), Some(cy)) = (attr_i64(off, "x"), attr_i64(off, "y"), attr_i64(ext, "cx"), attr_i64(ext, "cy")) else {
        return Ok(None);
    };
    let shape_anchor = format!("{tree_anchor}/{}", anchors.segment(tree_anchor, &format!("p:sp#{id}")));
    let placeholder = child(nv, NS_P, "nvPr")
        .and_then(|nvpr| child(nvpr, NS_P, "ph"))
        .map(|ph| ph.attribute("type").unwrap_or("obj").to_string());
    let text_box = child(nv, NS_P, "cNvSpPr").and_then(|c| bool_attr(c, "txBox")).unwrap_or(false);
    let mut notes: Vec<String> = Vec::new();
    let rotation = attr_i64(xfrm, "rot").unwrap_or(0);
    if rotation != 0 {
        notes.push("rotation_not_rendered".into());
    }
    let flip_h = bool_attr(xfrm, "flipH").unwrap_or(false);
    let flip_v = bool_attr(xfrm, "flipV").unwrap_or(false);

    let style_ref = child(sp, NS_P, "style");
    let style_fill = style_ref.and_then(|s| style_fill_of(env, s));
    let style_line = style_ref.and_then(|s| style_line_of(env, s));
    let style_font = style_ref
        .and_then(|s| child(s, NS_A, "fontRef"))
        .and_then(|f| f.children().find(|c| c.is_element()))
        .and_then(parse_color_element);

    let direct = direct_fill(sp_pr);
    let fill_span = direct.map(|(node, _)| span(node, offset_of(sc)));
    let fill = match direct {
        Some((node, kind)) => fill_string(env, node, kind, &mut notes),
        None => style_fill.unwrap_or_else(|| "none".to_string()),
    };
    let ln = child(sp_pr, NS_A, "ln");
    let (stroke, stroke_width) = line_of(env, ln, style_line.as_ref(), &mut notes);
    let (read_start, unknown_head_end, has_head_end, custom_head_end_size) = line_end_of(ln, "headEnd", "head", &mut notes);
    let (read_end, unknown_tail_end, has_tail_end, custom_tail_end_size) = line_end_of(ln, "tailEnd", "tail", &mut notes);

    let tx = child(sp, NS_P, "txBody");
    let body_pr = tx.and_then(|t| child(t, NS_A, "bodyPr"));
    let body = body_map(body_pr, &mut notes);
    let levels = TextLevels {
        shape: tx.and_then(|t| child(t, NS_A, "lstStyle")).map(|l| read_levels(l, NS_A, "lvl", "defRPr")).unwrap_or_default(),
        master: master_layer(env.defaults, placeholder.as_deref()).clone(),
        style_font,
    };
    let (rich, paragraph_spans) = match tx {
        Some(t) => read_text_body(t, sc.part, &levels, env, &mut notes)?,
        None => (RichText::default(), Vec::new()),
    };
    let line_geometry = matches!(prst, "line" | "straightConnector1");
    if line_geometry && (text_box || !rich.plain().is_empty()) {
        // A line carrying text cannot be represented by the Line node without dropping its text body; keep the whole
        // shape opaque so its geometry, endpoints, and text survive same-format saves unchanged.
        return Ok(None);
    }
    if custom_path.is_some() && (text_box || !rich.plain().is_empty()) {
        // A vector path carries no text in this release, so a path with text stays opaque.
        return Ok(None);
    }
    let kind = if text_box {
        NodeKind::Text
    } else if custom_path.is_some() {
        NodeKind::Path
    } else if prst == "ellipse" {
        NodeKind::Ellipse
    } else if line_geometry {
        NodeKind::Line
    } else {
        NodeKind::Rectangle
    };
    // An outer shadow is modelled on a preset rectangle or ellipse, where the renderer draws it. Any other effect list,
    // and a shadow in a form that `read_effect_list` does not accept, stays opaque.
    let effect_list = child(sp_pr, NS_A, "effectLst");
    let modelled_effect = match (kind, effect_list) {
        (NodeKind::Rectangle | NodeKind::Ellipse, Some(list)) if matches!(prst, "rect" | "ellipse") => {
            editable::read_effect_list(list).map(|shadows| (list, shadows))
        }
        _ => None,
    };
    let (effect_span, shadows) = match modelled_effect {
        Some((list, shadows)) => (Some(span(list, offset_of(sc))), shadows),
        None => (None, Vec::new()),
    };
    if effect_list.is_some() && effect_span.is_none() {
        // The effect list stays as written, so its shadow cannot be edited. The report says so.
        sc.part.diagnose(
            "PPTX_EFFECT_NOT_EDITABLE",
            format!("the effect list of {}:{id} is kept as written and is not editable in this release", sc.page_id),
            Some(format!("{}:{id}", sc.page_id)),
        );
    }
    let line_markers_mapped = kind == NodeKind::Line;
    if (has_head_end || has_tail_end) && !line_markers_mapped {
        notes.push("line_end_marker_on_non_line_shape_preserved_not_editable".into());
    }
    if custom_head_end_size || custom_tail_end_size {
        notes.push("line_end_marker_size_preview_approximate".into());
    }
    let default_run = run_style_for(None, 0, &levels, env, &mut Vec::new());
    let first_style = rich
        .paragraphs
        .iter()
        .flat_map(|p| p.units.iter())
        .find_map(|u| u.style().cloned())
        .unwrap_or(default_run);
    let mut node_style = Style {
        font_family: first_style.font_family,
        font_size: first_style.font_size,
        color: first_style.color,
        fill,
        stroke,
        stroke_width,
        stroke_start: line_markers_mapped.then_some(read_start).flatten(),
        stroke_end: line_markers_mapped.then_some(read_end).flatten(),
        bold: first_style.bold,
        italic: first_style.italic,
        align: rich.paragraphs.first().map(|p| p.props.align.clone()).unwrap_or_else(|| "left".into()),
        shadows,
        ..Style::default()
    };
    if custom_path.is_some() {
        // A vector path has no text, so its text fields take the `Style` defaults, not the deck's text defaults. The guard
        // refuses any other value on a path, and a path read from a deck must be saveable again.
        let base = Style::default();
        node_style.font_family = base.font_family;
        node_style.font_size = base.font_size;
        node_style.color = base.color;
        node_style.bold = base.bold;
        node_style.italic = base.italic;
        node_style.align = base.align;
    }

    // Unmodelled children and attributes of the shape: everything the writer never rewrites is hashed.
    let mut local = Anchors::default();
    record_attributes(sc.part, &shape_anchor, sp);
    for node in elements(sp) {
        if is_el(node, NS_P, "nvSpPr") {
            record_nv(sc.part, &shape_anchor, node);
        } else if is_el(node, NS_P, "spPr") {
            let base = format!("{shape_anchor}/p:spPr[0]");
            record_attributes(sc.part, &base, node);
            for inner in elements(node) {
                let modelled = is_el(inner, NS_A, "xfrm")
                    || is_el(inner, NS_A, "ln")
                    || direct_fill_kind(inner).is_some()
                    || (effect_span.is_some() && is_el(inner, NS_A, "effectLst"));
                if !modelled {
                    let anchor = format!("{base}/{}", local.segment(&base, &pxml::default_key(inner)));
                    sc.part.push_region(&anchor, inner);
                }
            }
        } else if is_el(node, NS_P, "txBody") {
            let base = format!("{shape_anchor}/p:txBody[0]");
            for inner in elements(node) {
                if is_el(inner, NS_A, "bodyPr") || is_el(inner, NS_A, "lstStyle") {
                    let anchor = format!("{base}/{}", local.segment(&base, &pxml::default_key(inner)));
                    sc.part.push_region(&anchor, inner);
                }
            }
        } else {
            let anchor = format!("{shape_anchor}/{}", local.segment(&shape_anchor, &shape_key(node)));
            sc.part.push_region(&anchor, node);
        }
    }

    let spans = ShapeSpans {
        element: span(sp, offset_of(sc)),
        xfrm: Some(span(xfrm, offset_of(sc))),
        off: Some(span(off, offset_of(sc))),
        ext: Some(span(ext, offset_of(sc))),
        sp_pr: Some(span(sp_pr, offset_of(sc))),
        fill: fill_span,
        ln: ln.map(|n| span(n, offset_of(sc))),
        tx_body: tx.map(|n| span(n, offset_of(sc))),
        paragraphs: paragraph_spans,
        effect_list: effect_span,
    };
    let map = ShapeMap {
        kind: KIND_SHAPE.to_string(),
        origin: ORIGIN_SOURCE.to_string(),
        cnvpr_id: id,
        geometry: prst.to_string(),
        text_box,
        has_text_body: tx.is_some(),
        paragraphs: rich.to_parts().1,
        body,
        media_part: None,
        rel_id: None,
        flip_h,
        flip_v,
        rotation,
        arrow: has_head_end || has_tail_end,
        unmapped_head_end: unknown_head_end || (!line_markers_mapped && has_head_end),
        unmapped_tail_end: unknown_tail_end || (!line_markers_mapped && has_tail_end),
        custom_head_end_size,
        custom_tail_end_size,
        placeholder,
        render_notes: notes,
        anchor: shape_anchor,
    };
    let node = Node {
        id: format!("{}:{id}", sc.page_id),
        kind,
        bounds: Rect { x, y, width: cx, height: cy },
        text: rich.plain(),
        style: node_style,
        asset: None,
        children: vec![],
        layout: None,
        interactions: vec![],
        variants: BTreeMap::new(),
        ext: NodeExt { path: custom_path, ..NodeExt::default() },
    };
    Ok(Some(ReadShape { node, map, spans }))
}

fn offset_of(sc: &SlideCtx<'_, '_>) -> usize {
    sc.part.offset
}

fn read_pic(sc: &mut SlideCtx<'_, '_>, pic: XNode<'_, '_>, tree_anchor: &str, anchors: &mut Anchors) -> DltResult<Option<ReadShape>> {
    let env = sc.env;
    let Some(nv) = child(pic, NS_P, "nvPicPr") else { return Ok(None) };
    let Some(c_nv) = child(nv, NS_P, "cNvPr") else { return Ok(None) };
    let Some(id) = c_nv.attribute("id").and_then(|v| v.parse::<u32>().ok()) else { return Ok(None) };
    let Some(blip_fill) = child(pic, NS_P, "blipFill") else { return Ok(None) };
    let Some(blip) = child(blip_fill, NS_A, "blip") else { return Ok(None) };
    let Some(embed) = blip.attribute((NS_R, "embed")) else { return Ok(None) };
    let Some(rel) = sc.rels.iter().find(|r| r.id == embed && r.rel_type == ns::REL_IMAGE) else { return Ok(None) };
    if rel.external {
        return Ok(None);
    }
    let media_part = opc::resolve_target(sc.part.part, &rel.target)?;
    let Ok(bytes) = env.package.part(&media_part) else { return Ok(None) };
    let extension = media_part.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let Some(extension_mime) = ns::mime_for_extension(&extension) else { return Ok(None) };
    let declared = env.content_types.and_then(|ct| ct.content_type_for(&media_part)).map(str::to_string);
    let mime = match declared.as_deref() {
        Some(d) if d.starts_with("image/") => d.to_string(),
        _ => extension_mime.to_string(),
    };
    if mime != extension_mime || !ns::magic_matches(&mime, bytes) {
        return Ok(None);
    }
    let Some(sp_pr) = child(pic, NS_P, "spPr") else { return Ok(None) };
    let Some(xfrm) = child(sp_pr, NS_A, "xfrm") else { return Ok(None) };
    let (Some(off), Some(ext)) = (child(xfrm, NS_A, "off"), child(xfrm, NS_A, "ext")) else { return Ok(None) };
    let (Some(x), Some(y), Some(cx), Some(cy)) = (attr_i64(off, "x"), attr_i64(off, "y"), attr_i64(ext, "cx"), attr_i64(ext, "cy")) else {
        return Ok(None);
    };
    let mut notes = Vec::new();
    if let Some(src) = child(blip_fill, NS_A, "srcRect") {
        if src.attributes().any(|a| a.value().trim() != "0") {
            notes.push("crop_not_rendered".into());
        }
    }
    let rotation = attr_i64(xfrm, "rot").unwrap_or(0);
    if rotation != 0 {
        notes.push("rotation_not_rendered".into());
    }
    let asset = (env.put)(bytes, &mime)?;
    let shape_anchor = format!("{tree_anchor}/{}", anchors.segment(tree_anchor, &format!("p:pic#{id}")));
    let mut local = Anchors::default();
    record_nv(sc.part, &shape_anchor, nv);
    sc.part.push_region(&format!("{shape_anchor}/{}", local.segment(&shape_anchor, "p:blipFill")), blip_fill);
    record_attributes(sc.part, &shape_anchor, pic);
    for node in elements(pic) {
        if is_el(node, NS_P, "nvPicPr") || is_el(node, NS_P, "blipFill") {
            continue;
        }
        if is_el(node, NS_P, "spPr") {
            let base = format!("{shape_anchor}/p:spPr[0]");
            record_attributes(sc.part, &base, node);
            for inner in elements(node) {
                if !is_el(inner, NS_A, "xfrm") {
                    let anchor = format!("{base}/{}", local.segment(&base, &pxml::default_key(inner)));
                    sc.part.push_region(&anchor, inner);
                }
            }
        } else {
            sc.part.push_region(&format!("{shape_anchor}/{}", local.segment(&shape_anchor, &pxml::default_key(node))), node);
        }
    }
    let spans = ShapeSpans {
        element: span(pic, offset_of(sc)),
        xfrm: Some(span(xfrm, offset_of(sc))),
        off: Some(span(off, offset_of(sc))),
        ext: Some(span(ext, offset_of(sc))),
        sp_pr: Some(span(sp_pr, offset_of(sc))),
        ..ShapeSpans::default()
    };
    let map = ShapeMap {
        kind: KIND_PICTURE.to_string(),
        origin: ORIGIN_SOURCE.to_string(),
        cnvpr_id: id,
        geometry: child(sp_pr, NS_A, "prstGeom").and_then(|g| g.attribute("prst")).unwrap_or("rect").to_string(),
        text_box: false,
        has_text_body: false,
        paragraphs: vec![],
        body: BodyMap::default(),
        media_part: Some(media_part),
        rel_id: Some(embed.to_string()),
        flip_h: bool_attr(xfrm, "flipH").unwrap_or(false),
        flip_v: bool_attr(xfrm, "flipV").unwrap_or(false),
        rotation,
        arrow: false,
        unmapped_head_end: false,
        unmapped_tail_end: false,
        custom_head_end_size: false,
        custom_tail_end_size: false,
        placeholder: None,
        render_notes: notes,
        anchor: shape_anchor,
    };
    let node = Node {
        id: format!("{}:{id}", sc.page_id),
        kind: NodeKind::Image,
        bounds: Rect { x, y, width: cx, height: cy },
        text: String::new(),
        style: Style::default(),
        asset: Some(asset),
        children: vec![],
        layout: None,
        interactions: vec![],
        variants: BTreeMap::new(),
        ext: Default::default(),
    };
    Ok(Some(ReadShape { node, map, spans }))
}

fn read_cxn(sc: &mut SlideCtx<'_, '_>, cxn: XNode<'_, '_>, tree_anchor: &str, anchors: &mut Anchors) -> DltResult<Option<ReadShape>> {
    let env = sc.env;
    let Some(nv) = child(cxn, NS_P, "nvCxnSpPr") else { return Ok(None) };
    let Some(c_nv) = child(nv, NS_P, "cNvPr") else { return Ok(None) };
    let Some(id) = c_nv.attribute("id").and_then(|v| v.parse::<u32>().ok()) else { return Ok(None) };
    let Some(sp_pr) = child(cxn, NS_P, "spPr") else { return Ok(None) };
    let Some(xfrm) = child(sp_pr, NS_A, "xfrm") else { return Ok(None) };
    let (Some(off), Some(ext)) = (child(xfrm, NS_A, "off"), child(xfrm, NS_A, "ext")) else { return Ok(None) };
    let Some(prst) = child(sp_pr, NS_A, "prstGeom").and_then(|g| g.attribute("prst")) else { return Ok(None) };
    if prst != "line" && prst != "straightConnector1" {
        return Ok(None);
    }
    let (Some(x), Some(y), Some(cx), Some(cy)) = (attr_i64(off, "x"), attr_i64(off, "y"), attr_i64(ext, "cx"), attr_i64(ext, "cy")) else {
        return Ok(None);
    };
    let style_ref = child(cxn, NS_P, "style");
    let style_line = style_ref.and_then(|s| style_line_of(env, s));
    let mut notes = Vec::new();
    let rotation = attr_i64(xfrm, "rot").unwrap_or(0);
    if rotation != 0 {
        notes.push("rotation_not_rendered".into());
    }
    let ln = child(sp_pr, NS_A, "ln");
    let (stroke, stroke_width) = line_of(env, ln, style_line.as_ref(), &mut notes);
    let (stroke_start, unmapped_head_end, has_head_end, custom_head_end_size) = line_end_of(ln, "headEnd", "head", &mut notes);
    let (stroke_end, unmapped_tail_end, has_tail_end, custom_tail_end_size) = line_end_of(ln, "tailEnd", "tail", &mut notes);
    let arrow = has_head_end || has_tail_end;
    if custom_head_end_size || custom_tail_end_size {
        notes.push("line_end_marker_size_preview_approximate".into());
    }
    let shape_anchor = format!("{tree_anchor}/{}", anchors.segment(tree_anchor, &format!("p:cxnSp#{id}")));
    let mut local = Anchors::default();
    record_nv(sc.part, &shape_anchor, nv);
    record_attributes(sc.part, &shape_anchor, cxn);
    for node in elements(cxn) {
        if is_el(node, NS_P, "nvCxnSpPr") {
            continue;
        }
        if is_el(node, NS_P, "spPr") {
            let base = format!("{shape_anchor}/p:spPr[0]");
            record_attributes(sc.part, &base, node);
            for inner in elements(node) {
                if !(is_el(inner, NS_A, "xfrm") || is_el(inner, NS_A, "ln")) {
                    let anchor = format!("{base}/{}", local.segment(&base, &pxml::default_key(inner)));
                    sc.part.push_region(&anchor, inner);
                }
            }
        } else {
            sc.part.push_region(&format!("{shape_anchor}/{}", local.segment(&shape_anchor, &shape_key(node))), node);
        }
    }
    let spans = ShapeSpans {
        element: span(cxn, offset_of(sc)),
        xfrm: Some(span(xfrm, offset_of(sc))),
        off: Some(span(off, offset_of(sc))),
        ext: Some(span(ext, offset_of(sc))),
        sp_pr: Some(span(sp_pr, offset_of(sc))),
        ln: ln.map(|n| span(n, offset_of(sc))),
        ..ShapeSpans::default()
    };
    let map = ShapeMap {
        kind: KIND_CONNECTOR.to_string(),
        origin: ORIGIN_SOURCE.to_string(),
        cnvpr_id: id,
        geometry: prst.to_string(),
        text_box: false,
        has_text_body: false,
        paragraphs: vec![],
        body: BodyMap::default(),
        media_part: None,
        rel_id: None,
        flip_h: bool_attr(xfrm, "flipH").unwrap_or(false),
        flip_v: bool_attr(xfrm, "flipV").unwrap_or(false),
        rotation,
        arrow,
        unmapped_head_end,
        unmapped_tail_end,
        custom_head_end_size,
        custom_tail_end_size,
        placeholder: None,
        render_notes: notes,
        anchor: shape_anchor,
    };
    let node = Node {
        id: format!("{}:{id}", sc.page_id),
        kind: NodeKind::Line,
        bounds: Rect { x, y, width: cx, height: cy },
        text: String::new(),
        style: Style {
            font_family: FALLBACK_FONT.into(),
            font_size: 1800 * 127,
            color: "#000000".into(),
            fill: "none".into(),
            stroke,
            stroke_width,
            stroke_start,
            stroke_end,
            bold: false,
            italic: false,
            align: "left".into(),
            ..Style::default()
        },
        asset: None,
        children: vec![],
        layout: None,
        interactions: vec![],
        variants: BTreeMap::new(),
        ext: Default::default(),
    };
    Ok(Some(ReadShape { node, map, spans }))
}

/// Reads a text body (`p:txBody` or a notes body): paragraphs, units, and their spans.
pub(crate) fn read_text_body(
    tx: XNode<'_, '_>,
    part: &mut PartCtx<'_>,
    levels: &TextLevels,
    env: &Env<'_>,
    notes: &mut Vec<String>,
) -> DltResult<(RichText, Vec<ParaSpans>)> {
    let mut paragraphs = Vec::new();
    let mut spans = Vec::new();
    for p in children(tx, NS_A, "p") {
        let (para, para_spans) = read_paragraph(p, part, levels, env, notes);
        paragraphs.push(para);
        spans.push(para_spans);
    }
    Ok((RichText { paragraphs }, spans))
}

fn read_paragraph(p: XNode<'_, '_>, part: &PartCtx<'_>, levels: &TextLevels, env: &Env<'_>, notes: &mut Vec<String>) -> (Para, ParaSpans) {
    let offset = part.offset;
    let ppr = child(p, NS_A, "pPr");
    let level = ppr.and_then(|n| attr_i64(n, "lvl")).unwrap_or(0).clamp(0, 8) as usize;
    let props = para_props(ppr, level, levels, notes);
    let mut units = Vec::new();
    let mut unit_spans = Vec::new();
    let mut end_rpr = None;
    let atom_default = run_style_for(None, level, levels, env, notes);
    for node in elements(p) {
        if is_el(node, NS_A, "pPr") {
            continue;
        }
        if is_el(node, NS_A, "endParaRPr") {
            end_rpr = Some(span(node, offset));
            continue;
        }
        if is_el(node, NS_A, "r") {
            let rpr = child(node, NS_A, "rPr");
            let style = run_style_for(rpr, level, levels, env, notes);
            let text = child(node, NS_A, "t").map(direct_text).unwrap_or_default();
            let rpr_span = rpr.map(|r| span(r, offset));
            for ch in text.chars() {
                if ch == '\n' || ch == '\r' {
                    // A raw newline in a:t is read as a line break. Only the first of "\r\n" is a break.
                    if ch == '\r' {
                        continue;
                    }
                    notes.push("raw_newline_read_as_break".into());
                    units.push(Unit::Break);
                    unit_spans.push(UnitSpans { kind: UnitKind::RawBreak, element: span(node, offset), rpr: rpr_span.clone() });
                } else {
                    units.push(Unit::Char { ch, style: style.clone() });
                    unit_spans.push(UnitSpans { kind: UnitKind::Char, element: span(node, offset), rpr: rpr_span.clone() });
                }
            }
            continue;
        }
        if is_el(node, NS_A, "br") {
            units.push(Unit::Break);
            unit_spans.push(UnitSpans { kind: UnitKind::Break, element: span(node, offset), rpr: None });
            continue;
        }
        if is_el(node, NS_A, "fld") {
            let rpr = child(node, NS_A, "rPr");
            let style = run_style_for(rpr, level, levels, env, notes);
            let display = child(node, NS_A, "t").map(direct_text).unwrap_or_default();
            units.push(Unit::Atom { element: "a:fld".into(), display, style });
            unit_spans.push(UnitSpans { kind: UnitKind::Atom, element: span(node, offset), rpr: rpr.map(|r| span(r, offset)) });
            continue;
        }
        // Any other child is kept as a protected atom and copied byte for byte.
        notes.push("paragraph_child_kept_as_atom".into());
        units.push(Unit::Atom { element: pxml::default_key(node), display: String::new(), style: atom_default.clone() });
        unit_spans.push(UnitSpans { kind: UnitKind::Atom, element: span(node, offset), rpr: None });
    }
    let para = Para { props, units };
    let para_spans = ParaSpans { element: span(p, offset), ppr: ppr.map(|n| span(n, offset)), end_rpr, units: unit_spans };
    (para, para_spans)
}

/// Paragraph properties from `a:pPr`. Alignment falls back to the list levels. Spacing is explicit only.
fn para_props(ppr: Option<XNode<'_, '_>>, level: usize, levels: &TextLevels, notes: &mut Vec<String>) -> ParaProps {
    let mut props = ParaProps::default();
    let inherited = levels.shape[level].align.clone().or_else(|| levels.master[level].align.clone());
    props.align = match ppr.and_then(|n| n.attribute("algn")) {
        Some(value) => align_name(value).to_string(),
        None => inherited.unwrap_or_else(|| "left".into()),
    };
    let Some(ppr) = ppr else { return props };
    if let Some(value) = child(ppr, NS_A, "spcBef").and_then(|s| spacing_emu(s, notes)) {
        props.space_before = value;
    }
    if let Some(value) = child(ppr, NS_A, "spcAft").and_then(|s| spacing_emu(s, notes)) {
        props.space_after = value;
    }
    if let Some(line) = child(ppr, NS_A, "lnSpc") {
        if let Some(pct) = child(line, NS_A, "spcPct").and_then(|p| attr_i64(p, "val")) {
            props.line_spacing_pct = (pct / 1000).clamp(1, 1000) as u32;
        } else if child(line, NS_A, "spcPts").is_some() {
            notes.push("fixed_line_spacing_not_rendered".into());
        }
    }
    if ["buChar", "buAutoNum", "buBlip"].iter().any(|name| child(ppr, NS_A, name).is_some()) {
        notes.push("bullets_not_rendered".into());
    }
    props
}

/// Spacing in EMU from `a:spcBef`/`a:spcAft`, where `spcPts` is in hundredths of a point.
/// Percentage spacing is not modelled and is reported.
fn spacing_emu(spacing: XNode<'_, '_>, notes: &mut Vec<String>) -> Option<i64> {
    if let Some(hundredths) = child(spacing, NS_A, "spcPts").and_then(|p| attr_i64(p, "val")) {
        return Some(hundredths * 127);
    }
    if child(spacing, NS_A, "spcPct").is_some() {
        notes.push("percent_spacing_not_rendered".into());
    }
    None
}

/// Run style for a run with `rpr`. Missing values come from the shape list level, the master layer, the
/// presentation defaults, and then the built-in defaults (18 pt, theme minor font, black).
fn run_style_for(rpr: Option<XNode<'_, '_>>, level: usize, levels: &TextLevels, env: &Env<'_>, notes: &mut Vec<String>) -> Style {
    let size = rpr
        .and_then(|r| attr_i64(r, "sz"))
        .or(levels.shape[level].size)
        .or(levels.master[level].size)
        .or(env.defaults.presentation[level].size)
        .unwrap_or(1800);
    let color_spec = rpr
        .and_then(|r| child(r, NS_A, "solidFill"))
        .and_then(|f| f.children().find(|c| c.is_element()))
        .and_then(parse_color_element)
        .or_else(|| levels.shape[level].color.clone())
        .or_else(|| levels.style_font.clone())
        .or_else(|| levels.master[level].color.clone())
        .or_else(|| env.defaults.presentation[level].color.clone());
    let color = match color_spec {
        Some(spec) => resolve_hex(env, &spec, None, notes).unwrap_or_else(|| "#000000".into()),
        None => "#000000".into(),
    };
    let typeface = rpr
        .and_then(|r| child(r, NS_A, "latin"))
        .and_then(|l| l.attribute("typeface"))
        .map(str::to_string)
        .or_else(|| levels.shape[level].latin.clone())
        .or_else(|| levels.master[level].latin.clone())
        .or_else(|| env.defaults.presentation[level].latin.clone())
        .unwrap_or_default();
    let bold = rpr
        .and_then(|r| bool_attr(r, "b"))
        .or(levels.shape[level].bold)
        .or(levels.master[level].bold)
        .or(env.defaults.presentation[level].bold)
        .unwrap_or(false);
    let italic = rpr
        .and_then(|r| bool_attr(r, "i"))
        .or(levels.shape[level].italic)
        .or(levels.master[level].italic)
        .or(env.defaults.presentation[level].italic)
        .unwrap_or(false);
    crate::theme::run_style(font_from_theme(&typeface, env.theme), size * 127, color, bold, italic, "left")
}

fn resolve_hex(env: &Env<'_>, spec: &ColorSpec, placeholder: Option<String>, notes: &mut Vec<String>) -> Option<String> {
    let context = ColorContext { theme: env.theme, map: env.color_map, placeholder };
    let resolved = env.theme.resolve(spec, &context);
    notes.extend(resolved.notes);
    resolved.hex
}

/// Direct fill element of `spPr`, with its kind name.
fn direct_fill<'a, 'i>(sp_pr: XNode<'a, 'i>) -> Option<(XNode<'a, 'i>, &'static str)> {
    elements(sp_pr).into_iter().find_map(|c| direct_fill_kind(c).map(|kind| (c, kind)))
}

fn direct_fill_kind(node: XNode<'_, '_>) -> Option<&'static str> {
    if node.tag_name().namespace() != Some(NS_A) {
        return None;
    }
    ["noFill", "solidFill", "gradFill", "blipFill", "pattFill", "grpFill"]
        .into_iter()
        .find(|k| *k == node.tag_name().name())
}

/// Converts a direct fill element to the model's fill string: `none`, `#RRGGBB`, or `unsupported:<kind>`.
fn fill_string(env: &Env<'_>, node: XNode<'_, '_>, kind: &str, notes: &mut Vec<String>) -> String {
    match kind {
        "noFill" => "none".into(),
        "solidFill" => {
            let spec = node.children().find(|c| c.is_element()).and_then(parse_color_element);
            match spec {
                Some(spec) => resolve_hex(env, &spec, None, notes).unwrap_or_else(|| "unsupported:solidFill".into()),
                None => "unsupported:solidFill".into(),
            }
        }
        other => {
            notes.push(format!("fill_{other}_not_rendered"));
            format!("unsupported:{other}")
        }
    }
}

/// Fill from `p:style/a:fillRef` through the theme fill list. Index 0 means no fill.
fn style_fill_of(env: &Env<'_>, style: XNode<'_, '_>) -> Option<String> {
    let fill_ref = child(style, NS_A, "fillRef")?;
    let index: usize = fill_ref.attribute("idx")?.parse().ok()?;
    if index == 0 {
        return Some("none".into());
    }
    let placeholder = fill_ref
        .children()
        .find(|c| c.is_element())
        .and_then(parse_color_element)
        .and_then(|spec| resolve_hex(env, &spec, None, &mut Vec::new()));
    match env.theme.fill_styles.get(index - 1)? {
        FillStyle::Solid(spec) => Some(
            resolve_hex(env, spec, placeholder, &mut Vec::new()).unwrap_or_else(|| "unsupported:style_fill".into()),
        ),
        FillStyle::Other(kind) => Some(format!("unsupported:{kind}")),
    }
}

/// Stroke colour and width from `p:style/a:lnRef`. Index 0 means no line.
fn style_line_of(env: &Env<'_>, style: XNode<'_, '_>) -> Option<(String, i64)> {
    let line_ref = child(style, NS_A, "lnRef")?;
    let index: usize = line_ref.attribute("idx")?.parse().ok()?;
    if index == 0 {
        return None;
    }
    let placeholder = line_ref
        .children()
        .find(|c| c.is_element())
        .and_then(parse_color_element)
        .and_then(|spec| resolve_hex(env, &spec, None, &mut Vec::new()));
    let line = env.theme.line_styles.get(index - 1)?;
    let width = line.width.unwrap_or(9525);
    let color = match &line.color {
        Some(spec) => resolve_hex(env, spec, placeholder, &mut Vec::new()).unwrap_or_else(|| "none".into()),
        None => "none".into(),
    };
    Some((color, width))
}

/// Stroke colour and width from `a:ln`. When the element is absent, the style reference supplies them.
fn line_of(env: &Env<'_>, ln: Option<XNode<'_, '_>>, style_line: Option<&(String, i64)>, notes: &mut Vec<String>) -> (String, i64) {
    match ln {
        Some(ln) => {
            let width = attr_i64(ln, "w").unwrap_or_else(|| style_line.map(|s| s.1).unwrap_or(9525));
            let fill = elements(ln).into_iter().find(|c| {
                c.tag_name().namespace() == Some(NS_A) && matches!(c.tag_name().name(), "noFill" | "solidFill" | "gradFill" | "pattFill")
            });
            let stroke = match fill {
                Some(f) if f.tag_name().name() == "noFill" => "none".to_string(),
                Some(f) if f.tag_name().name() == "solidFill" => {
                    match f.children().find(|c| c.is_element()).and_then(parse_color_element) {
                        Some(spec) => resolve_hex(env, &spec, None, notes).unwrap_or_else(|| "unsupported:line_color".into()),
                        None => "unsupported:line_color".into(),
                    }
                }
                Some(f) => {
                    notes.push(format!("line_{}_not_rendered", f.tag_name().name()));
                    format!("unsupported:{}", f.tag_name().name())
                }
                None => style_line.map(|s| s.0.clone()).unwrap_or_else(|| "none".into()),
            };
            (stroke, width)
        }
        None => match style_line {
            Some((color, width)) => (color.clone(), *width),
            None => ("none".into(), 0),
        },
    }
}

/// Maps the two endpoint types represented by the contract. Unknown OOXML types remain in the source XML and are
/// marked in ShapeMap so edits cannot silently replace them with a triangle.
fn line_end_of(ln: Option<XNode<'_, '_>>, element: &str, endpoint: &str, notes: &mut Vec<String>) -> (Option<LineEnd>, bool, bool, bool) {
    let Some(marker) = ln.and_then(|line| child(line, NS_A, element)) else { return (None, false, false, false) };
    let custom_size = ["w", "len"].into_iter().any(|name| marker.attribute(name).is_some_and(|value| value != "med"));
    match marker.attribute("type").unwrap_or("none") {
        "none" => (None, false, false, false),
        "open" => (Some(LineEnd::Open), false, true, custom_size),
        "triangle" => (Some(LineEnd::Filled), false, true, custom_size),
        _ => {
            notes.push(format!("line_end_marker_{endpoint}_type_not_mapped_preserved_preview_approximate"));
            (None, true, true, custom_size)
        }
    }
}

/// Body properties: insets, anchor, and wrap. Defaults follow PresentationML.
fn body_map(body_pr: Option<XNode<'_, '_>>, notes: &mut Vec<String>) -> BodyMap {
    let mut map = BodyMap::default();
    let Some(body) = body_pr else { return map };
    for (index, name) in ["lIns", "tIns", "rIns", "bIns"].iter().enumerate() {
        if let Some(v) = attr_i64(body, name) {
            map.insets[index] = v;
        }
    }
    map.anchor = match body.attribute("anchor") {
        Some("ctr") => "ctr".into(),
        Some("b") => "b".into(),
        _ => "t".into(),
    };
    map.wrap_square = body.attribute("wrap") != Some("none");
    if body.attribute("vert").is_some_and(|v| v != "horz") {
        notes.push("vertical_text_not_rendered".into());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alignment_names_round_trip() {
        for name in ["left", "center", "right", "justify", "distributed"] {
            assert_eq!(align_name(align_attr(name)), name);
        }
    }

    #[test]
    fn master_layer_selects_title_body_or_other() {
        let mut defaults = StyleDefaults::default();
        defaults.title[0].size = Some(4400);
        defaults.body[0].size = Some(2800);
        defaults.other[0].size = Some(1800);
        assert_eq!(master_layer(&defaults, Some("title"))[0].size, Some(4400));
        assert_eq!(master_layer(&defaults, Some("body"))[0].size, Some(2800));
        assert_eq!(master_layer(&defaults, None)[0].size, Some(1800));
    }
}
