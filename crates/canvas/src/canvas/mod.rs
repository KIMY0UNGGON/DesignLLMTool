//! CanvasEngine: structured design artifacts (pages of absolute, row, and column layouts with text, shapes, lines, and
//! images), click interactions, native JSON projects, and HTML/PNG/SVG/PDF export.

pub(crate) mod figure;
pub(crate) mod components;
pub(crate) mod guard;
pub(crate) mod html;
pub(crate) mod layout;
pub(crate) mod model;
pub(crate) mod ops;
pub(crate) mod repeat;
pub(crate) mod templates;
pub(crate) mod tokens;

use crate::common::{blocking, fail, invalid, normalize_title_text, unsupported};
use crate::digest;
use crate::native;
use crate::CANVAS_ENGINE_ID;
use designllm_contracts::{
    new_id, sha256, Artifact, ArtifactBody, ArtifactKind, AssetRef, AssetStore, CanvasDocument, CapabilityExtensions, CapabilityReport,
    CAPABILITY_EXTENSION_DESIGN_TOKENS, CAPABILITY_EXTENSION_LISTS,
    CAPABILITY_EXTENSION_COMPONENTS,
    CAPABILITY_EXTENSION_BOARD_TITLES, CAPABILITY_EXTENSION_PAGE_SECTIONS,
    ContentDigest, CreateRequest, Diagnostic, DltError, DltResult, EncodedOutput, Editability, EngineAdapter,
    EngineContext, ErrorCode, ExportRequest, Feature, FeatureSupport, FigureLink, OpenInput, Operation, Page, PreparedChange,
    PreservationVerification, Node, Primitive, Rect, RenderScene, Size, Support,
};
use designllm_contracts::media_header::{mp4_has_video_track, webm_has_video_track, PNG_SIGNATURE};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Native project format name inside the envelope.
pub(crate) const CANVAS_FORMAT: &str = "designllm.canvas";
/// MIME type of the native project file.
pub const CANVAS_NATIVE_MIME: &str = "application/vnd.designllm.canvas+json";

/// Structured design engine (`designllm_canvas::CanvasEngine::default()`).
#[derive(Debug, Clone, Copy, Default)]
pub struct CanvasEngine;

fn canvas_doc(artifact: &Artifact) -> DltResult<&CanvasDocument> {
    match &artifact.body {
        ArtifactBody::Canvas(doc) => Ok(doc),
        _ => Err(invalid("KIND_MISMATCH", "artifact body is not a canvas document")),
    }
}

fn feature(read: Support, render: Support, edit: Support, preserve: Support, write: Support, reason: &str) -> FeatureSupport {
    FeatureSupport { read, render, edit, preserve, write, reason: reason.to_string() }
}

/// Capability keys for canvas artifacts. Every feature key is present, so no key is inferred as supported.
pub(crate) fn capability_report() -> CapabilityReport {
    use Support::{Approximate, Supported, Unsupported};
    let na = "not applicable to canvas artifacts";
    let mut report = BTreeMap::new();
    report.insert(Feature::Text, feature(Supported, Supported, Supported, Supported, Supported, "node text edited by grapheme range; font-backed shaping"));
    report.insert(Feature::TextStyle, feature(Supported, Supported, Approximate, Supported, Supported, "style applies to a whole node; per-range styles are not supported"));
    report.insert(Feature::NamedStyles, feature(Supported, Supported, Supported, Supported, Supported, "named style entries; nodes keep inline styles"));
    report.insert(Feature::Geometry, feature(Supported, Supported, Supported, Supported, Supported, "bounds in EMU; child bounds relative to the parent"));
    report.insert(Feature::PageSetup, feature(Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "page size is set at creation; no page setup operation"));
    report.insert(Feature::ParagraphFormat, feature(Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, na));
    report.insert(Feature::Tables, feature(Supported, Approximate, Supported, Supported, Supported, "Canvas tables support equal or weighted columns and semantic HTML; merge/unmerge preserves covered plain text when its style matches the anchor, while differing styles and references remain refused"));
    report.insert(Feature::Images, feature(Supported, Approximate, Supported, Supported, Supported, "Canvas images support PNG/JPEG/BMP, the first GIF frame, static WebP, fit (fill/contain/cover), and non-destructive source-pixel crop for static PNG/JPEG(default EXIF)/BMP/WebP. APNG/GIF/SVG/animated WebP crop and FigureLink/component crop are refused. Desktop video import extracts a PNG first-frame poster and binds it to the source hash; generic direct AddNode validates MP4/WebM video tracks and the PNG poster, but does not attest that the poster matches the source. Static PNG/SVG/PDF exports use the poster and HTML uses controls. Video source MIME/container/video-track and poster PNG are checked, and source+poster+all referenced assets must fit the native 128 MiB limit. HTML playback depends on available WebView2 codecs. Image replacement accepts PNG/JPEG/BMP and rejects saved crops outside replacement bounds"));
    report.insert(Feature::Pages, feature(Supported, Supported, Supported, Supported, Supported, "insert, remove, and reorder pages"));
    report.insert(Feature::Notes, feature(Supported, Unsupported, Supported, Supported, Supported, "Page.notes remains document notes; board memos and titles are separate page-local overlays excluded from exports. Page sections organize the sidebar without changing page or export order"));
    report.insert(Feature::Diagram, feature(Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, na));
    report.insert(Feature::Plot, feature(Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, na));
    report.insert(Feature::Dataset, feature(Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, na));
    report.insert(Feature::Evidence, feature(Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, na));
    report.insert(Feature::FigurePanels, feature(Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, na));
    report.insert(Feature::FigureLinks, feature(Supported, Supported, Supported, Supported, Supported, "PNG renders only, on image nodes: width kept, height from the render aspect; refused inside a row or column that stretches children"));
    report.insert(Feature::CanvasLayout, feature(Supported, Supported, Supported, Supported, Supported, "absolute, row, column, and grid; fixed, fill, and hug sizing with min and max; groups and rectangles hold children (rectangles are painted); gap, padding per side, justify, align, align_self; breakpoints on the page width in CSS px"));
    report.insert(Feature::Interactions, feature(Supported, Supported, Supported, Supported, Supported, "HTML click actions: navigate page, toggle visibility, set variant, OpenUrl (https/mailto/tel), and Back within exported page history; Back with no prior page is a no-op; at most one OpenUrl per node"));
    report.insert(Feature::WebSource, feature(Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "canvas artifacts have no source files; use a WebCode artifact"));
    report.insert(Feature::NativeRoundtrip, feature(Supported, Supported, Supported, Supported, Supported, "JSON project embeds referenced image/video/poster assets and checks each SHA-256; canvas.video.v1 blocks readers that cannot preserve video nodes"));
    report.insert(Feature::RenderSvg, feature(Unsupported, Supported, Unsupported, Unsupported, Supported, "encoded by the injected renderer; editability is reported per output"));
    report.insert(Feature::RenderPdf, feature(Unsupported, Supported, Unsupported, Unsupported, Supported, "encoded by the injected renderer; editability is reported per output"));
    report.insert(Feature::RenderPng, feature(Unsupported, Supported, Unsupported, Unsupported, Supported, "encoded by the injected renderer at the requested dpi"));
    report.insert(Feature::HitTest, feature(Unsupported, Supported, Unsupported, Unsupported, Unsupported, "hit tests use the scene source map through the renderer"));
    report.insert(Feature::TextSelection, feature(Unsupported, Supported, Unsupported, Unsupported, Unsupported, "grapheme rects in the source map give caret and selection positions"));
    report.insert(Feature::Effects, feature(Approximate, Approximate, Approximate, Approximate, Approximate, "drawn: corner radius, opacity per paint (an image's opacity as a whole), one outer shadow (rectangles, ellipses, images), two-stop gradient (rectangles, ellipses, paths), stroke dash, cap, and join (not on images), font weight, underline, strikethrough, letter spacing, line height, vertical align; refused: gradients on text, lines, and images, shadows on text, lines, and paths, more than one shadow, stroke dash, cap, or join on images; HTML draws shape-border dashes with the browser's own dash lengths"));
    report.insert(Feature::VectorPaths, feature(Supported, Supported, Supported, Supported, Supported, "path nodes with SVG path data and content icons (IconRef); image fit fill, contain, and cover"));
    report.insert(Feature::GridLayout, feature(Supported, Supported, Supported, Supported, Supported, "grid mode with grid_columns, gap and row_gap, and grid_span per child; align_self in flow and grid; justify applies to row and column"));
    report
}

/// Every asset referenced by a node, keyed by hash.
pub(crate) fn asset_refs(doc: &CanvasDocument) -> BTreeMap<String, AssetRef> {
    let mut out = BTreeMap::new();
    for page in &doc.pages {
        for node in model::flatten(&page.nodes) {
            if let Some(asset) = &node.asset {
                out.entry(asset.sha256.clone()).or_insert_with(|| asset.clone());
            }
            if let Some(poster) = &node.ext.video_poster_asset {
                out.entry(poster.sha256.clone()).or_insert_with(|| poster.clone());
            }
        }
    }
    out
}

/// Blocking when a referenced asset cannot be found in the store. Missing assets are never silently dropped.
fn asset_diagnostics(doc: &CanvasDocument, assets: &dyn AssetStore) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for (hash, reference) in asset_refs(doc) {
        if assets.lookup(&hash).is_err() {
            out.push(blocking(
                "ASSET_MISSING",
                format!("asset {hash} ({}) is not in the workspace store", reference.mime),
                None,
            ));
        }
    }
    out
}

fn image_crop_diagnostics(doc: &CanvasDocument, assets: &dyn AssetStore) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for node in doc.pages.iter().flat_map(|page| model::flatten(&page.nodes)) {
        let Some(crop) = node.ext.image_crop_pixel_rect else { continue };
        let Some(asset) = node.asset.as_ref() else { continue };
        match ops::image_crop_source_dimensions(asset, assets, &node.id) {
            Ok((width, height)) if ops::pixel_rect_within(crop, width, height) => {}
            Ok((width, height)) => out.push(blocking(
                "IMAGE_CROP_BOUNDS",
                format!("crop rectangle is outside the {width}×{height} source image"),
                Some(node.id.clone()),
            )),
            Err(error) => out.extend(error.diagnostics),
        }
    }
    out
}

fn image_crop_figure_link_diagnostics(doc: &CanvasDocument, links: &[FigureLink]) -> Vec<Diagnostic> {
    links.iter().filter_map(|link| {
        model::find_node_in_doc(doc, &link.target_node_id)
            .filter(|node| node.ext.image_crop_pixel_rect.is_some())
            .map(|_| blocking(
                "IMAGE_CROP_FIGURE_LINKED",
                "a Figure-linked image cannot carry crop metadata yet",
                Some(link.target_node_id.clone()),
            ))
    }).collect()
}

fn video_asset_diagnostics(doc: &CanvasDocument, assets: &dyn AssetStore) -> Vec<Diagnostic> {
    let videos: Vec<&Node> = doc.pages.iter().flat_map(|page| model::flatten(&page.nodes))
        .filter(|node| node.kind == designllm_contracts::NodeKind::Video).collect();
    if videos.is_empty() { return Vec::new(); }

    let mut diagnostics = Vec::new();
    let mut total = Some(0u64);
    let mut metadata_mismatch = false;
    for reference in asset_refs(doc).values() {
        let canonical = match assets.lookup(&reference.sha256) {
            Ok(canonical) if canonical.sha256 == reference.sha256 && canonical.mime == reference.mime && canonical.bytes == reference.bytes => canonical,
            _ => {
                metadata_mismatch = true;
                break;
            }
        };
        total = total.and_then(|bytes| bytes.checked_add(canonical.bytes));
    }
    if metadata_mismatch || total.is_none_or(|bytes| bytes > native::MAX_EMBEDDED_BYTES) {
        let (code, message) = if metadata_mismatch {
            ("VIDEO_ASSET_METADATA", "video project asset metadata cannot be verified against the canonical workspace assets".to_owned())
        } else {
            ("VIDEO_ASSETS_TOO_LARGE", format!("video source, poster, and referenced assets exceed the {} MiB native project asset limit", native::MAX_EMBEDDED_BYTES / (1024 * 1024)))
        };
        for node in &videos {
            diagnostics.push(blocking(code, message.clone(), Some(node.id.clone())));
        }
        return diagnostics;
    }

    for node in videos {
        let Some(source) = node.asset.as_ref() else { continue };
        let Some(poster) = node.ext.video_poster_asset.as_ref() else { continue };
        let read_verified = |asset: &AssetRef, role: &str| -> Result<Vec<u8>, String> {
            let canonical = assets.lookup(&asset.sha256).map_err(|_| format!("{role} asset is missing"))?;
            if canonical.sha256 != asset.sha256 || canonical.mime != asset.mime || canonical.bytes != asset.bytes {
                return Err(format!("{role} metadata does not match the canonical workspace asset"));
            }
            let bytes = assets.read(&canonical).map_err(|_| format!("{role} asset could not be read"))?;
            if bytes.len() as u64 != canonical.bytes || sha256(&bytes) != canonical.sha256 {
                return Err(format!("{role} bytes do not match their stored length and SHA-256"));
            }
            Ok(bytes)
        };

        let source_bytes = match read_verified(source, "video source") {
            Ok(bytes) => bytes,
            Err(message) => {
                diagnostics.push(blocking("VIDEO_SOURCE_ASSET", message, Some(node.id.clone())));
                continue;
            }
        };
        let signature_ok = match source.mime.as_str() {
            "video/mp4" => mp4_has_video_track(&source_bytes),
            "video/webm" => webm_has_video_track(&source_bytes),
            _ => false,
        };
        if !signature_ok {
            diagnostics.push(blocking("VIDEO_SOURCE_FORMAT", "video MIME and MP4/WebM container signature do not match", Some(node.id.clone())));
            continue;
        }
        let poster_bytes = match read_verified(poster, "video poster") {
            Ok(bytes) => bytes,
            Err(message) => {
                diagnostics.push(blocking("VIDEO_POSTER_ASSET", message, Some(node.id.clone())));
                continue;
            }
        };
        if !poster_bytes.starts_with(&PNG_SIGNATURE) {
            diagnostics.push(blocking("VIDEO_POSTER_FORMAT", "automatic video poster must be PNG", Some(node.id.clone())));
            continue;
        }
        let Some(header_size) = designllm_render::image_pixel_size(&poster_bytes) else {
            diagnostics.push(blocking("VIDEO_POSTER_DIMENSIONS", "video poster PNG has no valid dimensions", Some(node.id.clone())));
            continue;
        };
        if header_size.0 == 0 || header_size.1 == 0 || u64::from(header_size.0) * u64::from(header_size.1) > 64 * 1024 * 1024 {
            diagnostics.push(blocking("VIDEO_POSTER_DIMENSIONS", "video poster exceeds the 64 megapixel limit", Some(node.id.clone())));
            continue;
        }
        match designllm_render::decode_image(&poster_bytes) {
            Ok((width, height, _pixels)) if (width, height) == header_size => {}
            Ok(_) => diagnostics.push(blocking("VIDEO_POSTER_DIMENSIONS", "decoded poster dimensions do not match the PNG header", Some(node.id.clone()))),
            Err(error) => diagnostics.push(blocking("VIDEO_POSTER_DECODE", format!("video poster could not be decoded: {}", error.message), Some(node.id.clone()))),
        }
    }
    diagnostics
}

fn video_static_export_diagnostics(doc: &CanvasDocument, scene: &RenderScene) -> Vec<Diagnostic> {
    let mut reported = BTreeSet::new();
    let mut diagnostics = Vec::new();
    for node_id in scene.pages.iter().flat_map(|page| page.primitives.iter()).filter_map(|primitive| match primitive {
        Primitive::Image { node_id, .. } => Some(node_id.as_str()),
        _ => None,
    }) {
        if !reported.insert(node_id.to_owned()) { continue; }
        if model::find_node_in_doc(doc, node_id).is_some_and(|node| node.kind == designllm_contracts::NodeKind::Video) {
            diagnostics.push(blocking(
                "VIDEO_MOTION_NOT_RENDERED",
                "static export contains the extracted poster only; video motion and audio are omitted",
                Some(node_id.to_owned()),
            ));
        }
    }
    diagnostics
}

/// Structural, figure-link, asset, and layout diagnostics for a document. Blocking entries mean the document is invalid.
fn check(doc: &CanvasDocument, links: &[FigureLink], context: &EngineContext<'_>) -> Vec<Diagnostic> {
    let mut diagnostics = model::validate_document(doc);
    if model::any_blocking(&diagnostics) {
        return diagnostics;
    }
    let mut covered_cells = BTreeSet::new();
    for page in &doc.pages {
        for node in model::flatten(&page.nodes) {
            if let Some(table) = &node.ext.table {
                if let Ok(cells) = model::covered_table_cell_ids(table) {
                    covered_cells.extend(cells);
                }
            }
        }
    }
    for link in links {
        if covered_cells.contains(link.target_node_id.as_str()) {
            diagnostics.push(blocking(
                "TABLE_SPAN_FIGURE_LINK",
                "a covered table cell cannot be the target of a linked figure",
                Some(link.target_node_id.clone()),
            ));
        }
    }
    diagnostics.extend(image_crop_figure_link_diagnostics(doc, links));
    if model::any_blocking(&diagnostics) {
        return diagnostics;
    }
    diagnostics.extend(components::validate_document(doc));
    if model::any_blocking(&diagnostics) {
        return diagnostics;
    }
    diagnostics.extend(figure::diagnostics(doc, links));
    if model::any_blocking(&diagnostics) {
        return diagnostics;
    }
    diagnostics.extend(asset_diagnostics(doc, context.assets));
    if model::any_blocking(&diagnostics) {
        return diagnostics;
    }
    diagnostics.extend(image_crop_diagnostics(doc, context.assets));
    if model::any_blocking(&diagnostics) {
        return diagnostics;
    }
    diagnostics.extend(video_asset_diagnostics(doc, context.assets));
    if model::any_blocking(&diagnostics) {
        return diagnostics;
    }
    match layout::layout_pages(doc, context.renderer, None) {
        Ok(out) => {
            let frames = out.frames.clone();
            match scene_of(out) {
                Ok(scene) => {
                    // The template rules run on the laid-out document as warnings: an edit is never refused by them.
                    // v1.1 documents keep their validate and change outputs byte for byte: the warnings come only with v1.2 values.
                    let warnings = if native::uses_v12(doc) { templates::design_warnings(doc, &scene, &frames) } else { Vec::new() };
                    diagnostics.extend(scene.diagnostics);
                    diagnostics.extend(warnings);
                }
                Err(err) => diagnostics.push(blocking("LAYOUT", err.message, None)),
            }
        }
        Err(err) => diagnostics.push(blocking("LAYOUT", err.message, None)),
    }
    diagnostics
}

/// The page-tree node with this id, and the index of its page.
fn find_node_page<'a>(doc: &'a CanvasDocument, id: &str) -> Option<(usize, &'a Node)> {
    doc.pages.iter().enumerate().find_map(|(index, page)| find_in(&page.nodes, id).map(|node| (index, node)))
}

fn find_in<'a>(nodes: &'a [Node], id: &str) -> Option<&'a Node> {
    nodes.iter().find_map(|node| if node.id == id { Some(node) } else { find_in(&node.children, id) })
}

/// The ids of a node and of everything it contains.
fn subtree_ids(node: &Node, out: &mut BTreeSet<String>) {
    out.insert(node.id.clone());
    for child in &node.children {
        subtree_ids(child, out);
    }
}

/// A merged table span is one selection unit for cropped export. Keep all source cells in the
/// scene and use the anchor's frame even when the selection names only a covered cell.
fn expand_selected_table_spans(
    nodes: &[Node],
    selected: &mut BTreeSet<String>,
    drawn: &mut BTreeSet<String>,
) -> DltResult<()> {
    let requested: Vec<String> = selected.iter().cloned().collect();
    for node in nodes {
        if let Some(table) = &node.ext.table {
            let grid = model::table_span_grid(table).map_err(|problem| invalid("TABLE_CELL_SPANS", problem))?;
            for (row_index, row) in table.rows.iter().enumerate() {
                for (column_index, cell_id) in row.cell_ids.iter().enumerate() {
                    if !requested.iter().any(|selected_id| selected_id == cell_id) {
                        continue;
                    }
                    let cell_index = row_index * grid.columns + column_index;
                    let (anchor_index, row_span, column_span) = match grid.cells[cell_index] {
                        model::TableCellPosition::Anchor { row_span, column_span } => (cell_index, row_span, column_span),
                        model::TableCellPosition::Covered { anchor_index } => {
                            let model::TableCellPosition::Anchor { row_span, column_span } = grid.cells[anchor_index] else {
                                return Err(invalid("TABLE_CELL_SPANS", "a covered cell has no valid span anchor"));
                            };
                            (anchor_index, row_span, column_span)
                        }
                    };
                    if row_span == 1 && column_span == 1 {
                        continue;
                    }

                    let anchor_row = anchor_index / grid.columns;
                    let anchor_column = anchor_index % grid.columns;
                    let mut members = BTreeSet::new();
                    for span_row in anchor_row..anchor_row + row_span {
                        for span_column in anchor_column..anchor_column + column_span {
                            members.insert(table.rows[span_row].cell_ids[span_column].clone());
                        }
                    }
                    let anchor_id = table.rows[anchor_row].cell_ids[anchor_column].clone();
                    selected.retain(|id| !members.contains(id));
                    selected.insert(anchor_id);
                    drawn.extend(members);
                }
            }
        }
        expand_selected_table_spans(&node.children, selected, drawn)?;
    }
    Ok(())
}

fn primitive_node_id(primitive: &Primitive) -> &str {
    match primitive {
        Primitive::Text { node_id, .. }
        | Primitive::Shape { node_id, .. }
        | Primitive::Line { node_id, .. }
        | Primitive::Image { node_id, .. }
        | Primitive::Path { node_id, .. } => node_id,
    }
}

/// The smallest rectangle that holds both.
fn union_rect(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (a.x + a.width).max(b.x + b.width);
    let bottom = (a.y + a.height).max(b.y + b.height);
    Rect { x, y, width: right - x, height: bottom - y }
}

/// The part both share; None unless it has a positive area.
fn intersect_rect(a: Rect, b: Rect) -> Option<Rect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    (right > x && bottom > y).then_some(Rect { x, y, width: right - x, height: bottom - y })
}

/// The scene of a node selection (`ExportRequest.node_ids`): the selected nodes and what they contain, on the one page they are
/// on, cropped to the union of the selected nodes' bounds. The nodes must be on one page and drawn (not hidden), and
/// `page_indices`, when given, must name that page.
fn selection_scene(
    doc: &CanvasDocument,
    renderer: &dyn designllm_contracts::RenderProvider,
    ids: &[String],
    page_indices: Option<&[u32]>,
    format: &str,
) -> DltResult<RenderScene> {
    let mut page: Option<usize> = None;
    let mut selected: BTreeSet<String> = BTreeSet::new();
    let mut drawn: BTreeSet<String> = BTreeSet::new();
    for id in ids {
        let (index, node) =
            find_node_page(doc, id).ok_or_else(|| invalid("NODE_SELECTION", format!("node {id} is not in the document")))?;
        if page.is_some_and(|first| first != index) {
            return Err(invalid("NODE_SELECTION_PAGES", "the selected nodes are on more than one page; export one page at a time"));
        }
        page = Some(index);
        selected.insert(id.clone());
        subtree_ids(node, &mut drawn);
    }
    let index = page.ok_or_else(|| invalid("NODE_SELECTION", "no nodes were selected"))?;
    if doc.pages[index].role == Some(designllm_contracts::PageRole::Component) && page_indices.is_none() {
        return Err(invalid("COMPONENT_PAGE_EXPLICIT_SELECTION", "exporting a component page requires an explicit page_indices selection"));
    }
    if page_indices.is_some_and(|list| !list.contains(&(index as u32))) {
        return Err(invalid("PAGE_INDICES", "page_indices does not name the page of the selected nodes"));
    }
    expand_selected_table_spans(&doc.pages[index].nodes, &mut selected, &mut drawn)?;
    let layout::LayoutOutput { pages, diagnostics, fonts, viewports, frames, links } = layout::layout_pages(doc, renderer, Some(&[index as u32]))?;
    let page = pages.into_iter().next().ok_or_else(|| invalid("NODE_SELECTION", "the page of the selected nodes is not laid out"))?;
    let selected_links: Vec<designllm_contracts::SceneLink> = links.into_iter().filter(|link| drawn.contains(&link.node_id)).collect();
    if format == "pdf" && !selected_links.is_empty() {
        return Err(unsupported("PDF_LINK_SELECTION", "Canvas PDF node selection with OpenUrl links is not supported for a cropped selection"));
    }
    let mut bounds: Option<Rect> = None;
    for id in &selected {
        let frame = *frames.get(id.as_str()).ok_or_else(|| invalid("NODE_HIDDEN", format!("node {id} is hidden and draws nothing")))?;
        bounds = Some(bounds.map_or(frame, |union| union_rect(union, frame)));
    }
    let page_rect = Rect { x: 0, y: 0, width: page.size.width, height: page.size.height };
    let area = bounds
        .and_then(|bounds| intersect_rect(bounds, page_rect))
        .ok_or_else(|| invalid("NODE_OUTSIDE_PAGE", "the selected nodes have no area inside their page"))?;
    let mut only = page;
    only.primitives.retain(|primitive| drawn.contains(primitive_node_id(primitive)));
    only.source_map.retain(|source| drawn.contains(&source.node_id));
    let cropped = designllm_render::page_region(&only, area)?;
    let mut metadata = BTreeMap::new();
    metadata.insert("canvas".to_string(), json!({ "viewport_css_px": viewports, "layout": "fresh" }));
    let scene = RenderScene { pages: vec![cropped], font_fingerprint: layout::font_fingerprint(&fonts)?, diagnostics, metadata, links: vec![] };
    check_canvas_scene_links(&scene)?;
    Ok(scene)
}

fn scene_from(
    doc: &CanvasDocument,
    renderer: &dyn designllm_contracts::RenderProvider,
    only: Option<&[u32]>,
) -> DltResult<RenderScene> {
    scene_of(layout::layout_pages(doc, renderer, only)?)
}

/// The render scene of a layout.
fn scene_of(out: layout::LayoutOutput) -> DltResult<RenderScene> {
    let mut metadata = BTreeMap::new();
    metadata.insert("canvas".to_string(), json!({ "viewport_css_px": out.viewports, "layout": "fresh" }));
    let scene = RenderScene {
        pages: out.pages,
        font_fingerprint: layout::font_fingerprint(&out.fonts)?,
        diagnostics: out.diagnostics,
        metadata,
        links: out.links,
    };
    check_canvas_scene_links(&scene)?;
    Ok(scene)
}

fn html_layout_diagnostic_should_forward(diagnostic: &Diagnostic) -> bool {
    matches!(diagnostic.code.as_str(), "TABLE_SPAN_PARENT_OVERFLOW" | "TABLE_SPAN_PAGE_OVERFLOW")
        || (diagnostic.code == "TEXT_OVERFLOW" && diagnostic.params.get("axis").and_then(Value::as_str) == Some("width"))
}

fn check_canvas_scene_links(scene: &RenderScene) -> DltResult<()> {
    scene.check_links().map_err(|error| {
        let mut mapped = DltError::new(ErrorCode::Internal, format!("engine {CANVAS_ENGINE_ID} produced invalid RenderScene.links: {}", error.message));
        mapped.affected_ids = error.affected_ids;
        mapped.diagnostics = error.diagnostics;
        mapped
    })
}

fn check_page_selection(doc: &CanvasDocument, format: &str, indices: Option<&[u32]>) -> DltResult<Option<Vec<u32>>> {
    let single_only = matches!(format, "png" | "svg");
    match indices {
        None => {
            if doc.pages.iter().any(|page| page.role == Some(designllm_contracts::PageRole::Component)) {
                let regular: Vec<u32> = doc.pages.iter().enumerate().filter_map(|(index, page)| {
                    (page.role != Some(designllm_contracts::PageRole::Component)).then_some(index as u32)
                }).collect();
                if regular.is_empty() {
                    return Err(invalid("COMPONENT_ONLY_DOCUMENT", "default export needs a regular page; select a component page explicitly with page_indices"));
                }
                return if single_only { Ok(Some(vec![regular[0]])) } else { Ok(Some(regular)) };
            }
            if single_only && doc.pages.len() != 1 {
                return Err(invalid(
                    "SINGLE_PAGE_REQUIRED",
                    format!("{format} export writes one file and needs exactly one page; choose page_indices"),
                ));
            }
            Ok(None)
        }
        Some(list) => {
            if list.is_empty() {
                return Err(invalid("PAGE_SELECTION", "page_indices is empty"));
            }
            let mut sorted: Vec<u32> = list.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            if let Some(bad) = sorted.iter().find(|&&i| i as usize >= doc.pages.len()) {
                return Err(invalid("PAGE_INDEX", format!("page index {bad} is out of range")));
            }
            if single_only && sorted.len() != 1 {
                return Err(invalid(
                    "SINGLE_PAGE_REQUIRED",
                    format!("{format} export writes one file and needs exactly one page"),
                ));
            }
            Ok(Some(sorted))
        }
    }
}

impl CanvasEngine {
    /// The render scene of `layout`, with the laid-out frame of every node drawn (absolute, in page coordinates, EMU) in
    /// `metadata["node_frames"]`, by node ID, for the GUI's selection outlines. Only this entry point carries them: `layout`
    /// keeps the v1.1 metadata, so the render snapshot of a v1.1 document does not change.
    pub fn layout_with_node_frames(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<RenderScene> {
        let doc = canvas_doc(artifact)?;
        guard::check_document(doc)?;
        model::into_result(image_crop_diagnostics(doc, context.assets))?;
        model::into_result(video_asset_diagnostics(doc, context.assets))?;
        model::into_result(image_crop_figure_link_diagnostics(doc, &artifact.figure_links))?;
        model::into_result(tokens::validate_artifact_state(artifact))?;
        let out = layout::layout_pages(doc, context.renderer, None)?;
        let frames = serde_json::to_value(&out.frames).map_err(|e| DltError::invalid(e.to_string()))?;
        let mut scene = scene_of(out)?;
        scene.metadata.insert("node_frames".to_string(), frames);
        Ok(scene)
    }
}

impl EngineAdapter for CanvasEngine {
    fn id(&self) -> &'static str {
        CANVAS_ENGINE_ID
    }

    fn kinds(&self) -> Vec<ArtifactKind> {
        vec![ArtifactKind::Canvas]
    }

    fn templates(&self) -> Vec<designllm_contracts::TemplateInfo> {
        templates::catalog()
    }

    fn exports_nodes(&self, format: &str) -> bool {
        matches!(format, "png" | "svg" | "pdf")
    }

    fn create(&self, request: &CreateRequest, context: &EngineContext<'_>) -> DltResult<Artifact> {
        if request.kind != ArtifactKind::Canvas {
            return Err(invalid("KIND", "CanvasEngine creates canvas artifacts only"));
        }
        let doc = match request.template.as_deref() {
            None | Some("blank") => {
                let size = request.size.unwrap_or_else(Size::slide);
                model::validate_page_size(size)?;
                CanvasDocument {
                    pages: vec![Page::new(new_id(), size)],
                    styles: BTreeMap::new(),
                    ..Default::default()
                }
            }
            Some(name) => {
                let unknown = || unsupported("TEMPLATE", format!("template {name} is not available; choose one of {}", templates::TEMPLATES.join(", ")));
                let default = templates::default_size(name).ok_or_else(unknown)?;
                let size = request.size.unwrap_or(default);
                model::validate_page_size(size)?;
                if !templates::is_responsive(name) && size != default {
                    return Err(invalid("TEMPLATE_SIZE", format!("template {name} has a fixed page size")));
                }
                let doc = templates::build(name, size).ok_or_else(unknown)?;
                // The template must pass its own rules at this size (contrast, touch targets, no clipped text, nothing drawn outside
                // its page or its frame). A size where a rule fails is refused, and the failing rules are named.
                let out = layout::layout_pages(&doc, context.renderer, None)?;
                let frames = out.frames.clone();
                let scene = scene_of(out)?;
                let findings = templates::lint(&doc, &scene, &frames);
                if !findings.is_empty() {
                    let (width, height) = (size.width / designllm_contracts::EMU_PER_CSS_PX, size.height / designllm_contracts::EMU_PER_CSS_PX);
                    let rules: BTreeSet<String> = findings.iter().map(|d| d.code.clone()).collect();
                    let names = rules.iter().cloned().collect::<Vec<_>>().join(", ");
                    // One TEMPLATE_FIT diagnostic per failed rule carries the size and the rule; the measured findings follow.
                    let per_rule = rules.iter().map(|rule| {
                        blocking("TEMPLATE_FIT", format!("template {name} does not fit {width} x {height} px: {rule}"), None)
                            .with_param("size_w", width)
                            .with_param("size_h", height)
                            .with_param("rule", rule.clone())
                    });
                    let mut error = fail(ErrorCode::InvalidArgument, "TEMPLATE_FIT", format!("template {name} does not fit {width} x {height} px: {names}"));
                    error.diagnostics = per_rule.chain(findings).collect();
                    return Err(error);
                }
                doc
            }
        };
        let title = normalize_title_text(&request.title, "Untitled canvas");
        let mut artifact = Artifact::new(ArtifactKind::Canvas, title, CANVAS_ENGINE_ID, ArtifactBody::Canvas(doc));
        artifact.capabilities = capability_report();
        Ok(artifact)
    }

    fn open(&self, input: &OpenInput, context: &EngineContext<'_>) -> DltResult<Artifact> {
        if input.kind != ArtifactKind::Canvas {
            return Err(invalid("KIND", "CanvasEngine opens canvas project files only"));
        }
        let bytes = context.assets.read(&input.asset)?;
        if bytes.len() as u64 != input.asset.bytes || sha256(&bytes) != input.asset.sha256 {
            return Err(fail(ErrorCode::IntegrityError, "OPEN_ASSET_HASH", "input bytes do not match their reference"));
        }
        let decoded = native::decode(CANVAS_FORMAT, &bytes)?;
        let mut artifact = decoded.artifact;
        if artifact.kind != ArtifactKind::Canvas || artifact.engine_id != CANVAS_ENGINE_ID {
            return Err(invalid("PROJECT_KIND", "the file is not a canvas project for this engine"));
        }
        let doc = canvas_doc(&artifact)?.clone();
        // A saved file that carries a v1.2 value is refused as a whole, never opened with the value stripped.
        guard::check_document(&doc)?;
        model::into_result(model::validate_document(&doc))?;
        model::into_result(tokens::validate_artifact_state(&artifact))?;
        model::into_result(tokens::validate_artifact_state(&artifact))?;
        native::restore_assets(&decoded.assets, context.assets)?;
        let diagnostics = check(&doc, &artifact.figure_links, context);
        model::into_result(diagnostics.clone())?;
        artifact.capabilities = capability_report();
        artifact.diagnostics = diagnostics.into_iter().filter(|d| !d.blocking).collect();
        artifact.native = None;
        Ok(artifact)
    }

    fn capabilities(&self, _artifact: &Artifact) -> CapabilityReport {
        capability_report()
    }

    fn capability_extensions(&self, _artifact: &Artifact) -> CapabilityExtensions {
        BTreeMap::from([(
            CAPABILITY_EXTENSION_LISTS.to_string(),
            FeatureSupport {
                read: Support::Supported,
                render: Support::Supported,
                edit: Support::Supported,
                preserve: Support::Supported,
                write: Support::Supported,
                reason: "Canvas Text nodes support bullet and numbered lists; each hard paragraph is one item and wrapped lines continue that item".into(),
            },
        ), (
            CAPABILITY_EXTENSION_DESIGN_TOKENS.to_string(),
            FeatureSupport {
                read: Support::Supported,
                render: Support::Supported,
                edit: Support::Supported,
                preserve: Support::Supported,
                write: Support::Supported,
                reason: "Canvas supports local light/dark tokens and shared publisher references; shared propagation is synchronized atomically by Core, while named styles and variants are unsupported".into(),
            },
        ), (
            CAPABILITY_EXTENSION_COMPONENTS.to_string(),
            FeatureSupport {
                read: Support::Supported,
                render: Support::Supported,
                edit: Support::Supported,
                preserve: Support::Supported,
                write: Support::Supported,
                reason: "Local Canvas component pages and same-document materialized Group instances support text/style overrides, source refresh, detach, native/Store3 persistence, and default export filtering; nested and cross-workspace components are refused".into(),
            },
        ), (
            CAPABILITY_EXTENSION_BOARD_TITLES.to_string(),
            FeatureSupport {
                read: Support::Supported,
                render: Support::Unsupported,
                edit: Support::Supported,
                preserve: Support::Supported,
                write: Support::Supported,
                reason: "Page-local board titles are Desktop overlays; their text color and bounds are preserved in native files and omitted from document exports".into(),
            },
        ), (
            CAPABILITY_EXTENSION_PAGE_SECTIONS.to_string(),
            FeatureSupport {
                read: Support::Supported,
                render: Support::Unsupported,
                edit: Support::Supported,
                preserve: Support::Supported,
                write: Support::Supported,
                reason: "Page sections group Canvas pages in the sidebar by start-page markers without changing page or export order".into(),
            },
        )])
    }

    fn supported_persisted_features(&self) -> BTreeSet<String> {
        native::PERSISTED_FEATURE_KEYS.iter().map(|key| (*key).to_owned()).collect()
    }

    fn persisted_features(&self, artifact: &Artifact) -> DltResult<BTreeSet<String>> {
        // As in the other engines, a body carrying a value canvas refuses (such as `ext.math`) is not classified for
        // storage. Model validation stays with save and open (judgement_47_table_span_text).
        guard::check_refused_values(canvas_doc(artifact)?)?;
        Ok(native::feature_keys_of(artifact).into_iter().collect())
    }

    fn design_tokens(&self, artifact: &Artifact) -> DltResult<BTreeMap<String, designllm_contracts::TokenDef>> {
        let doc = canvas_doc(artifact)?;
        model::into_result(model::validate_document(doc))?;
        model::into_result(tokens::validate_artifact_state(artifact))?;
        if artifact.design_system.is_some() {
            return Err(invalid("DESIGN_SYSTEM_CHAIN", "a design system cannot consume another design system"));
        }
        Ok(doc.tokens.clone())
    }

    fn validate_shared_design_system_snapshot(
        &self,
        artifact: &Artifact,
        shared_tokens: &BTreeMap<String, designllm_contracts::TokenDef>,
    ) -> DltResult<()> {
        let document = canvas_doc(artifact)?;
        model::into_result(model::validate_document(document))?;
        model::into_result(tokens::validate_artifact_state(artifact))?;
        tokens::validate_shared_snapshot(document, shared_tokens)
    }

    fn sync_design_system(
        &self,
        artifact: &Artifact,
        shared_tokens: &BTreeMap<String, designllm_contracts::TokenDef>,
        design_system: Option<&designllm_contracts::DesignSystemRef>,
    ) -> DltResult<PreparedChange> {
        let mut next = artifact.clone();
        next.design_system = design_system.cloned();
        let mut changed = BTreeSet::new();
        if next.design_system != artifact.design_system {
            changed.insert("artifact:design_system".into());
        }
        let ArtifactBody::Canvas(document) = &mut next.body else { return Err(invalid("DESIGN_SYSTEM_BODY", "shared tokens require a Canvas artifact")) };
        let updated_nodes = if design_system.is_some() {
            tokens::sync_shared_bindings(document, shared_tokens)?
        } else {
            tokens::unbind_shared_bindings(document)?
        };
        components::ensure_unlocked_materialized_updates(document, &updated_nodes)?;
        changed.extend(updated_nodes);
        let mut findings = model::validate_document(canvas_doc(&next)?);
        findings.extend(tokens::validate_artifact_state(&next));
        let diagnostics = model::into_result(findings)?;
        Ok(PreparedChange { artifact: next, changed_ids: changed.into_iter().collect(), diagnostics })
    }

    fn inspect(&self, artifact: &Artifact, node_ids: &[String]) -> DltResult<Value> {
        let doc = canvas_doc(artifact)?;
        if node_ids.is_empty() {
            let pages: Vec<Value> = doc
                .pages
                .iter()
                .map(|p| json!({ "id": p.id, "size": p.size, "node_count": model::flatten(&p.nodes).len() }))
                .collect();
            return Ok(json!({ "artifact_id": artifact.id, "revision": artifact.revision, "pages": pages }));
        }
        let mut items = Vec::with_capacity(node_ids.len());
        for id in node_ids {
            if let Some(page) = doc.pages.iter().find(|p| &p.id == id) {
                items.push(json!({ "kind": "page", "page": page }));
            } else if let Some(node) = model::find_node_in_doc(doc, id) {
                items.push(json!({ "kind": "node", "node": node }));
            } else {
                let mut error = DltError::new(ErrorCode::NotFound, format!("{id} is not a page or node of this canvas"));
                error.affected_ids.push(id.clone());
                return Err(error);
            }
        }
        Ok(json!({ "artifact_id": artifact.id, "revision": artifact.revision, "items": items }))
    }

    fn prepare_change(&self, artifact: &Artifact, operations: &[Operation], context: &EngineContext<'_>) -> DltResult<PreparedChange> {
        let original = canvas_doc(artifact)?;
        let operations = repeat::prepare_pastes(original, operations)?;
        guard::check_operations(&operations)?;
        let mut working = original.clone();
        let mut links = artifact.figure_links.clone();
        let (changed_ids, component_diagnostics) = ops::apply(&mut working, &mut links, context.assets, &operations)?;
        guard::check_document(&working)?;
        let mut next = artifact.clone();
        next.body = ArtifactBody::Canvas(working);
        next.figure_links = links;
        next.capabilities = capability_report();
        let mut diagnostics = check(canvas_doc(&next)?, &next.figure_links, context);
        diagnostics.extend(tokens::validate_artifact_state(&next));
        diagnostics.extend(component_diagnostics);
        let diagnostics = model::into_result(diagnostics)?;
        next.diagnostics = diagnostics.iter().filter(|d| !d.blocking).cloned().collect();
        Ok(PreparedChange { artifact: next, changed_ids, diagnostics })
    }

    fn validate(&self, artifact: &Artifact, context: &EngineContext<'_>) -> Vec<Diagnostic> {
        match canvas_doc(artifact) {
            Ok(doc) => {
                let mut diagnostics = guard::document_diagnostics(doc);
                diagnostics.extend(check(doc, &artifact.figure_links, context));
                diagnostics.extend(tokens::validate_artifact_state(artifact));
                diagnostics
            }
            Err(err) => err.diagnostics,
        }
    }

    fn content_digest(&self, artifact: &Artifact) -> DltResult<ContentDigest> {
        let doc = canvas_doc(artifact)?;
        guard::check_document(doc)?;
        model::into_result(image_crop_figure_link_diagnostics(doc, &artifact.figure_links))?;
        model::into_result(tokens::validate_artifact_state(artifact))?;
        digest::canvas_digest(doc, &artifact.figure_links)
    }

    fn layout(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<RenderScene> {
        let doc = canvas_doc(artifact)?;
        guard::check_document(doc)?;
        model::into_result(image_crop_diagnostics(doc, context.assets))?;
        model::into_result(video_asset_diagnostics(doc, context.assets))?;
        model::into_result(image_crop_figure_link_diagnostics(doc, &artifact.figure_links))?;
        model::into_result(tokens::validate_artifact_state(artifact))?;
        scene_from(doc, context.renderer, None)
    }

    fn layout_with_node_frames(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<RenderScene> {
        // The inherent method (its path call resolves to it, not to this trait method).
        CanvasEngine::layout_with_node_frames(self, artifact, context)
    }

    fn encode_native(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<EncodedOutput> {
        let doc = canvas_doc(artifact)?;
        guard::check_document(doc)?;
        let mut findings = model::validate_document(doc);
        findings.extend(image_crop_diagnostics(doc, context.assets));
        findings.extend(video_asset_diagnostics(doc, context.assets));
        findings.extend(image_crop_figure_link_diagnostics(doc, &artifact.figure_links));
        findings.extend(tokens::validate_artifact_state(artifact));
        findings.extend(figure::diagnostics(doc, &artifact.figure_links));
        let diagnostics = model::into_result(findings)?;
        let bytes = native::encode(CANVAS_FORMAT, artifact, &asset_refs(doc), context.assets)?;
        Ok(EncodedOutput {
            bytes,
            mime: CANVAS_NATIVE_MIME.to_string(),
            editability: Editability::NativeData,
            diagnostics,
        })
    }

    fn verify_saved(&self, artifact: &Artifact, bytes: &[u8], context: &EngineContext<'_>) -> DltResult<PreservationVerification> {
        let doc = canvas_doc(artifact)?;
        guard::check_document(doc)?;
        model::into_result(image_crop_diagnostics(doc, context.assets))?;
        model::into_result(video_asset_diagnostics(doc, context.assets))?;
        model::into_result(image_crop_figure_link_diagnostics(doc, &artifact.figure_links))?;
        model::into_result(tokens::validate_artifact_state(artifact))?;
        let decoded = match native::decode(CANVAS_FORMAT, bytes) {
            Ok(decoded) => decoded,
            Err(err) => {
                return Ok(PreservationVerification {
                    checked_parts: 0,
                    checked_regions: 0,
                    diagnostics: vec![blocking("SAVE_UNREADABLE", err.message, None)],
                })
            }
        };
        let mut diagnostics = Vec::new();
        if !native::same_artifact(artifact, &decoded.artifact) {
            diagnostics.push(blocking("SAVE_MISMATCH", "the re-read project differs from the artifact being saved", None));
        }
        let expected = asset_refs(doc);
        if expected.len() != decoded.assets.len() {
            diagnostics.push(blocking("SAVE_ASSETS", "embedded asset count differs from the referenced assets", None));
        }
        let mut checked_parts = 0u32;
        for (reference, data) in &decoded.assets {
            checked_parts += 1;
            let referenced = expected.get(&reference.sha256);
            if sha256(data) != reference.sha256 || referenced.is_none() {
                diagnostics.push(blocking("SAVE_ASSET", "an embedded asset is not a referenced asset", None));
            }
        }
        Ok(PreservationVerification { checked_parts, checked_regions: 0, diagnostics })
    }

    fn export(&self, artifact: &Artifact, request: &ExportRequest, context: &EngineContext<'_>) -> DltResult<EncodedOutput> {
        if request.artifact_id != artifact.id {
            return Err(invalid("ARTIFACT_ID", "export request names another artifact"));
        }
        if request.expected_revision != artifact.revision {
            let mut error = DltError::new(ErrorCode::RevisionConflict, "export request was made against another revision");
            error.current_revision = Some(artifact.revision);
            return Err(error);
        }
        let doc = canvas_doc(artifact)?;
        guard::check_document(doc)?;
        model::into_result(model::validate_document(doc))?;
        model::into_result(image_crop_diagnostics(doc, context.assets))?;
        model::into_result(video_asset_diagnostics(doc, context.assets))?;
        model::into_result(image_crop_figure_link_diagnostics(doc, &artifact.figure_links))?;
        model::into_result(tokens::validate_artifact_state(artifact))?;
        if let Some(ids) = request.node_ids.as_deref() {
            if !self.exports_nodes(&request.format) {
                return Err(unsupported(
                    "EXPORT_NODES",
                    format!("canvas export of a node selection is not supported for {}", request.format),
                ));
            }
            let scene = selection_scene(doc, context.renderer, ids, request.page_indices.as_deref(), &request.format)?;
            let mut output = context.renderer.encode(&scene, &request.format, &request.profile, context.assets)?;
            output.diagnostics.extend(video_static_export_diagnostics(doc, &scene));
            output.diagnostics.extend(repeat::export_diagnostics(doc));
            return Ok(output);
        }
        let selection = check_page_selection(doc, &request.format, request.page_indices.as_deref())?;
        match request.format.as_str() {
            "html" => {
                let out = html::export_html(doc, selection.as_deref(), &artifact.title, context.assets, Some(context.renderer))?;
                let mut diagnostics = out.diagnostics;
                for warning in layout::layout_pages(doc, context.renderer, selection.as_deref())?
                    .diagnostics
                    .into_iter()
                    .filter(html_layout_diagnostic_should_forward)
                {
                    if !diagnostics.iter().any(|existing| {
                        existing.code == warning.code && existing.node_id == warning.node_id && existing.params == warning.params
                    }) {
                        diagnostics.push(warning);
                    }
                }
                diagnostics.extend(repeat::export_diagnostics(doc));
                Ok(EncodedOutput {
                    bytes: out.bytes,
                    mime: "text/html; charset=utf-8".into(),
                    editability: Editability::Objects,
                    diagnostics,
                })
            }
            "png" | "svg" | "pdf" => {
                let scene = scene_from(doc, context.renderer, selection.as_deref())?;
                let mut output = context.renderer.encode(&scene, &request.format, &request.profile, context.assets)?;
                output.diagnostics.extend(video_static_export_diagnostics(doc, &scene));
                output.diagnostics.extend(repeat::export_diagnostics(doc));
                Ok(output)
            }
            other => Err(unsupported("EXPORT_FORMAT", format!("canvas export format {other} is not supported"))),
        }
    }
}

#[cfg(test)]
mod table_span_render_guard_tests {
    use super::*;
    use designllm_contracts::{AssetRef, CanvasTable, CanvasTableRow, CanvasTableSpan, LayoutSpec, NodeExt, NodeKind, Page, Style};
    use designllm_contracts::media_header::is_webm_container;
    use std::path::Path;

    struct EmptyAssets;
    impl AssetStore for EmptyAssets {
        fn put(&self, _: &[u8], _: &str) -> DltResult<AssetRef> { Err(DltError::unsupported("no asset writes")) }
        fn read(&self, _: &AssetRef) -> DltResult<Vec<u8>> { Err(DltError::unsupported("no asset reads")) }
        fn lookup(&self, hash: &str) -> DltResult<AssetRef> { Err(DltError::new(ErrorCode::NotFound, hash)) }
    }

    #[derive(Default)]
    struct VideoTestAssets(BTreeMap<String, (AssetRef, Vec<u8>)>);
    impl AssetStore for VideoTestAssets {
        fn put(&self, _: &[u8], _: &str) -> DltResult<AssetRef> { Err(DltError::unsupported("no asset writes")) }
        fn read(&self, asset: &AssetRef) -> DltResult<Vec<u8>> {
            self.0.get(&asset.sha256).filter(|(reference, _)| reference == asset).map(|(_, bytes)| bytes.clone())
                .ok_or_else(|| DltError::new(ErrorCode::NotFound, asset.sha256.clone()))
        }
        fn lookup(&self, hash: &str) -> DltResult<AssetRef> {
            self.0.get(hash).map(|(reference, _)| reference.clone()).ok_or_else(|| DltError::new(ErrorCode::NotFound, hash.to_owned()))
        }
    }

    fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&u32::try_from(8 + payload.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(payload);
        bytes
    }

    fn fake_mp4(brand: &[u8; 4], track_type: &[u8; 4]) -> Vec<u8> {
        let mut file_type = brand.to_vec();
        file_type.extend_from_slice(&[0; 4]); // minor version
        file_type.extend_from_slice(brand); // compatible brand
        let file_type = mp4_box(b"ftyp", &file_type);
        let mut handler = vec![0; 8]; // FullBox header and pre-defined
        handler.extend_from_slice(track_type);
        handler.extend_from_slice(&[0; 12]); // reserved
        handler.push(0); // name
        let handler = mp4_box(b"hdlr", &handler);
        let media = mp4_box(b"mdia", &handler);
        let track = mp4_box(b"trak", &media);
        let movie = mp4_box(b"moov", &track);
        [file_type, movie].concat()
    }

    fn fake_webm() -> Vec<u8> {
        // EBML header (size 7) containing DocType (ID 0x4282, four-byte "webm").
        b"\x1A\x45\xDF\xA3\x87\x42\x82\x84webm".to_vec()
    }

    fn ebml_element(id: &[u8], data: &[u8]) -> Vec<u8> {
        assert!(data.len() < 127, "the tiny fixture uses a one-byte EBML size");
        let mut bytes = id.to_vec();
        bytes.push(0x80 | data.len() as u8);
        bytes.extend_from_slice(data);
        bytes
    }

    fn fake_webm_with_track_type(track_type: &[u8]) -> Vec<u8> {
        let doc_type = ebml_element(&[0x42, 0x82], b"webm");
        let header = ebml_element(&[0x1A, 0x45, 0xDF, 0xA3], &doc_type);
        let track_type = ebml_element(&[0x83], track_type);
        let track_entry = ebml_element(&[0xAE], &track_type);
        let tracks = ebml_element(&[0x16, 0x54, 0xAE, 0x6B], &track_entry);
        let segment = ebml_element(&[0x18, 0x53, 0x80, 0x67], &tracks);
        [header, segment].concat()
    }

    fn fake_webm_with_track(track_type: u8) -> Vec<u8> {
        fake_webm_with_track_type(&[track_type])
    }

    fn stored_asset(bytes: Vec<u8>, mime: &str) -> (AssetRef, Vec<u8>) {
        (AssetRef { sha256: sha256(&bytes), mime: mime.into(), bytes: bytes.len() as u64 }, bytes)
    }

    fn video_document(source: AssetRef, poster: AssetRef) -> CanvasDocument {
        let video = Node {
            id: "video".into(), kind: designllm_contracts::NodeKind::Video,
            bounds: Rect { x: 0, y: 0, width: 640_000, height: 360_000 },
            text: String::new(), style: Style::default(), asset: Some(source), children: Vec::new(), layout: None,
            interactions: Vec::new(), variants: Default::default(),
            ext: NodeExt { video_poster_asset: Some(poster), ..Default::default() },
        };
        CanvasDocument {
            pages: vec![Page { nodes: vec![video], ..Page::new("page", Size::slide()) }],
            ..Default::default()
        }
    }

    #[test]
    fn video_source_and_decoded_poster_are_checked_before_persisting() {
        let source = stored_asset(fake_mp4(b"isom", b"vide"), "video/mp4");
        let poster_bytes = designllm_render::png_from_rgba(1, 1, &[10, 20, 30, 255]).expect("poster PNG");
        let poster = stored_asset(poster_bytes, "image/png");
        let assets = VideoTestAssets([
            (source.0.sha256.clone(), source.clone()),
            (poster.0.sha256.clone(), poster.clone()),
        ].into_iter().collect());
        let document = video_document(source.0.clone(), poster.0.clone());
        assert!(video_asset_diagnostics(&document, &assets).is_empty(), "a canonical MP4 signature and decoded PNG poster are accepted");

        let audio_only = stored_asset(fake_mp4(b"M4A ", b"soun"), "video/mp4");
        let audio_assets = VideoTestAssets([
            (audio_only.0.sha256.clone(), audio_only.clone()),
            (poster.0.sha256.clone(), poster.clone()),
        ].into_iter().collect());
        let audio_document = video_document(audio_only.0, poster.0.clone());
        assert!(video_asset_diagnostics(&audio_document, &audio_assets).iter().any(|diagnostic| diagnostic.code == "VIDEO_SOURCE_FORMAT"),
            "an audio-only ISO-BMFF brand is not accepted as MP4 video");

        let no_video_track = stored_asset(fake_mp4(b"isom", b"soun"), "video/mp4");
        let no_video_assets = VideoTestAssets([
            (no_video_track.0.sha256.clone(), no_video_track.clone()),
            (poster.0.sha256.clone(), poster.clone()),
        ].into_iter().collect());
        let no_video_document = video_document(no_video_track.0, poster.0.clone());
        assert!(video_asset_diagnostics(&no_video_document, &no_video_assets).iter().any(|diagnostic| diagnostic.code == "VIDEO_SOURCE_FORMAT"),
            "an ftyp/isom file with no video handler is rejected even if it has a poster");

        let webm = fake_webm_with_track(1);
        assert!(is_webm_container(&webm), "EBML DocType must explicitly identify WebM");
        assert!(webm_has_video_track(&webm), "a video TrackType is required");
        assert!(!webm_has_video_track(&fake_webm_with_track(2)), "an audio-only WebM track is not a video source");
        assert!(webm_has_video_track(&fake_webm_with_track_type(&[0, 0, 0, 0, 0, 0, 0, 1])), "an eight-byte unsigned TrackType value of one is valid");
        assert!(!webm_has_video_track(&fake_webm_with_track_type(&[0xFF; 9])), "TrackType values wider than u64 are malformed");
        assert!(!webm_has_video_track(&fake_webm_with_track_type(&[])), "an empty TrackType integer is malformed");
        assert!(!webm_has_video_track(&fake_webm()), "an EBML/WebM header without tracks is not playable video");
    }

    #[test]
    fn webm_input_set_acceptance_through_video_asset_diagnostics() {
        let normal = fake_webm_with_track(1);
        let truncated = normal[..normal.len() - 1].to_vec();
        let header_cut = fake_webm()[..6].to_vec();
        let poster = stored_asset(designllm_render::png_from_rgba(1, 1, &[1, 2, 3, 255]).expect("poster PNG"), "image/png");
        let mut table = Vec::new();
        for (name, bytes) in [("W1 normal", normal), ("W2 truncated", truncated), ("W3 header only", fake_webm()), ("W4 header cut", header_cut)] {
            let source = stored_asset(bytes, "video/webm");
            let document = video_document(source.0.clone(), poster.0.clone());
            let assets = VideoTestAssets([(source.0.sha256.clone(), source), (poster.0.sha256.clone(), poster.clone())].into_iter().collect());
            table.push((name, video_asset_diagnostics(&document, &assets).is_empty()));
        }
        println!("49e canvas video source accepts: {table:?}");
        assert_eq!(table, [("W1 normal", true), ("W2 truncated", false), ("W3 header only", false), ("W4 header cut", false)]);
    }

    #[test]
    fn webm_unknown_segment_size_and_truncated_vints_are_handled_without_panics() {
        let doc_type = ebml_element(&[0x42, 0x82], b"webm");
        let header = ebml_element(&[0x1A, 0x45, 0xDF, 0xA3], &doc_type);
        let track_type = ebml_element(&[0x83], &[1]);
        let track_entry = ebml_element(&[0xAE], &track_type);
        let tracks = ebml_element(&[0x16, 0x54, 0xAE, 0x6B], &track_entry);
        let mut unknown_size_segment = b"\x18\x53\x80\x67\x01\xFF\xFF\xFF\xFF\xFF\xFF\xFF".to_vec();
        unknown_size_segment.extend_from_slice(&tracks);
        let webm = [header.clone(), unknown_size_segment].concat();
        assert!(webm_has_video_track(&webm), "eight-byte unknown segment sizes are valid EBML");

        let truncated_segment_size = [header, b"\x18\x53\x80\x67\x01".to_vec()].concat();
        assert!(!webm_has_video_track(&truncated_segment_size), "truncated eight-byte sizes are rejected without panic");
        // The VINT edge cases (zero prefix, truncated eight-byte VINT) are tested next to the reader in contracts::media_header.
    }

    #[test]
    fn video_asset_budget_rejects_a_document_that_native_cannot_embed() {
        let source = AssetRef {
            sha256: "a".repeat(64), mime: "video/mp4".into(),
            bytes: native::MAX_EMBEDDED_BYTES + 1,
        };
        let poster_bytes = designllm_render::png_from_rgba(1, 1, &[1, 2, 3, 255]).expect("poster PNG");
        let poster = stored_asset(poster_bytes, "image/png");
        let assets = VideoTestAssets([
            (source.sha256.clone(), (source.clone(), Vec::new())),
            (poster.0.sha256.clone(), poster.clone()),
        ].into_iter().collect());
        let document = video_document(source, poster.0);
        assert!(video_asset_diagnostics(&document, &assets).iter().any(|diagnostic| diagnostic.code == "VIDEO_ASSETS_TOO_LARGE"),
            "source, poster, and all referenced assets must fit the existing native embedded-asset cap before commit");
    }

    #[test]
    fn static_video_scene_requires_motion_audio_loss_consent() {
        let source = AssetRef { sha256: "a".repeat(64), mime: "video/mp4".into(), bytes: 16 };
        let poster = AssetRef { sha256: "b".repeat(64), mime: "image/png".into(), bytes: 12 };
        let document = video_document(source, poster.clone());
        let scene = RenderScene {
            pages: vec![designllm_contracts::RenderPage {
                id: "page".into(), page_index: 0, size: Size::slide(),
                primitives: vec![Primitive::Image {
                    node_id: "video".into(), bounds: Rect { x: 0, y: 0, width: 640_000, height: 360_000 },
                    asset: poster, fit: Some(designllm_contracts::ImageFit::Contain), image_crop_pixel_rect: None, style: None,
                }],
                source_map: Vec::new(),
            }],
            font_fingerprint: String::new(), diagnostics: Vec::new(), metadata: Default::default(), links: Vec::new(),
        };
        let losses = video_static_export_diagnostics(&document, &scene);
        assert_eq!(losses.len(), 1);
        assert_eq!(losses[0].code, "VIDEO_MOTION_NOT_RENDERED");
        assert!(losses[0].blocking, "static poster output requires the standard explicit export-loss consent");
    }

    #[test]
    fn html_export_forwards_blocking_table_span_overflow_diagnostics() {
        for code in ["TABLE_SPAN_PARENT_OVERFLOW", "TABLE_SPAN_PAGE_OVERFLOW"] {
            let diagnostic = Diagnostic {
                code: code.into(),
                message: "table span extends beyond its available output region".into(),
                node_id: Some("anchor".into()),
                blocking: true,
                params: Default::default(),
            };
            assert!(html_layout_diagnostic_should_forward(&diagnostic), "HTML export forwards {code} into the Core loss gate");
        }
        let width_text_overflow = Diagnostic {
            code: "TEXT_OVERFLOW".into(),
            message: "text exceeds width".into(),
            node_id: Some("text".into()),
            blocking: false,
            params: [("axis".into(), Value::String("width".into()))].into_iter().collect(),
        };
        assert!(html_layout_diagnostic_should_forward(&width_text_overflow), "existing HTML width warning remains forwarded");
    }

    #[test]
    fn public_layout_paths_render_plain_covered_text_with_its_source_id() {
        let px = designllm_contracts::EMU_PER_CSS_PX;
        let cell = |id: &str, text: &str| Node {
            id: id.into(), kind: NodeKind::Rectangle, bounds: Rect { x: 0, y: 0, width: 200 * px, height: 36 * px },
            text: text.into(), style: Style::default(), asset: None, children: Vec::new(),
            layout: Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() }),
            interactions: Vec::new(), variants: Default::default(), ext: NodeExt::default(),
        };
        let table = Node {
            id: "table".into(), kind: NodeKind::Group, bounds: Rect { x: 20 * px, y: 20 * px, width: 400 * px, height: 120 * px },
            text: String::new(), style: Style::default(), asset: None,
            children: vec![cell("anchor", ""), cell("covered", "must not disappear")],
            layout: Some(LayoutSpec { mode: "grid".into(), height_mode: "hug".into(), grid_columns: Some(2), gap: 0, row_gap: Some(0), ..Default::default() }),
            interactions: Vec::new(), variants: Default::default(),
            ext: NodeExt { table: Some(CanvasTable {
                rows: vec![CanvasTableRow { id: "row".into(), cell_ids: vec!["anchor".into(), "covered".into()] }],
                header_row_id: None,
                column_widths: None,
                cell_spans: [("anchor".into(), CanvasTableSpan { row_span: 1, column_span: 2 })].into_iter().collect(),
            }), ..Default::default() },
        };
        let artifact = Artifact::new(ArtifactKind::Canvas, "span guard", CANVAS_ENGINE_ID, ArtifactBody::Canvas(CanvasDocument {
            pages: vec![Page { nodes: vec![table], ..Page::new("page", Size::slide()) }],
            ..Default::default()
        }));
        let ArtifactBody::Canvas(document) = &artifact.body else { panic!("Canvas body") };
        let fonts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts");
        let renderer = designllm_render::Renderer::new(&fonts).expect("bundled fonts load");
        let context = EngineContext { assets: &EmptyAssets, renderer: &renderer };

        for result in [CanvasEngine.layout(&artifact, &context), CanvasEngine.layout_with_node_frames(&artifact, &context)] {
            let scene = result.expect("the public render path preserves plain covered text");
            assert!(scene.pages[0].primitives.iter().any(|primitive| matches!(primitive,
                Primitive::Text { node_id, text, .. } if node_id == "covered" && text == "must not disappear"
            )));
            assert!(scene.pages[0].source_map.iter().any(|region| region.node_id == "covered"));
        }

        for selected_id in ["anchor", "covered"] {
            let selected = selection_scene(document, &renderer, &[selected_id.into()], None, "png")
                .expect("cropped selection keeps the whole merged span");
            let page = &selected.pages[0];
            assert!(page.primitives.iter().any(|primitive| matches!(primitive,
                Primitive::Shape { node_id, .. } if node_id == "anchor"
            )), "selecting {selected_id} includes the merged anchor shape");
            assert!(page.primitives.iter().any(|primitive| matches!(primitive,
                Primitive::Text { node_id, text, .. } if node_id == "covered" && text == "must not disappear"
            )), "selecting {selected_id} includes covered-cell text");
            assert!(page.source_map.iter().any(|region| region.node_id == "covered"), "selection keeps the covered source ID");
        }

        let request = ExportRequest {
            profile: designllm_contracts::RenderProfile { dpi: 96, transparent: false },
            ..ExportRequest::new(artifact.id.clone(), artifact.revision, "html", Editability::None)
        };
        let output = CanvasEngine.export(&artifact, &request, &context).expect("public HTML export of a populated span");
        let html = String::from_utf8(output.bytes).expect("HTML is UTF-8");
        assert!(html.contains("data-dlm-id=\"covered\""));
        assert!(html.contains("must not disappear"));
        assert_eq!(html.matches("<td ").count(), 1, "covered text stays inside the single spanned cell");
    }
}
