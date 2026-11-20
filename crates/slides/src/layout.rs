//! Layout of slide pages into a render scene. Text is shaped only through `RenderProvider::shape_text`, and the
//! encoders draw the shaped glyphs. Wrapping and alignment are decided here, one visual line at a time.
//!
//! The source map has one region per visual line of a paragraph. A region's logical range is that line's share of
//! the paragraph's graphemes, with one rectangle per grapheme, so the cursor, hit-test, and selection queries work
//! line by line. A grapheme cluster is never split between lines or pieces.
//!
//! Known approximations: a paragraph's space-before and space-after are applied as given, but inherited spacing
//! from the master is not. Rotation, crop, and gradient fills are reported, not drawn.

use std::collections::{BTreeMap, BTreeSet};

use designllm_contracts::{
    canonical_json, sha256, Diagnostic, DltError, DltResult, EngineContext, ErrorCode, FontFaceRef, Node, NodeKind, Page,
    Point, Primitive, Rect, RenderPage, RenderProvider, RenderScene, ShapedText, SourceRegion, Style, TextRange,
};
use designllm_pptx::model::KIND_PAGE;
use designllm_pptx::text::{Para, RichText, Unit};
use designllm_pptx::{Mappings, ShapeMap};
use designllm_render::face_matches_family;
use serde_json::json;
use unicode_segmentation::UnicodeSegmentation;

use crate::shape_map;

/// Pages to lay out, as indices into `pages`. `None` means every page, in order.
pub fn select_pages(pages: &[Page], indices: Option<&[u32]>) -> DltResult<Vec<usize>> {
    match indices {
        None => Ok((0..pages.len()).collect()),
        Some(list) => {
            if list.is_empty() {
                return Err(DltError::invalid("page_indices cannot be empty; omit it to export every page"));
            }
            let mut out = Vec::with_capacity(list.len());
            for &index in list {
                let index = index as usize;
                if index >= pages.len() {
                    return Err(DltError::new(ErrorCode::NotFound, format!("page {index} does not exist")));
                }
                if !out.contains(&index) {
                    out.push(index);
                }
            }
            Ok(out)
        }
    }
}

/// Lays out the selected pages. `RenderPage.page_index` is the page's position in the whole document.
pub fn layout_slides(
    pages: &[Page],
    mappings: &Mappings,
    ctx: &EngineContext<'_>,
    selected: Option<&[usize]>,
) -> DltResult<RenderScene> {
    let indices: Vec<usize> = match selected {
        Some(list) => list.to_vec(),
        None => (0..pages.len()).collect(),
    };
    let mut render_pages = Vec::with_capacity(indices.len());
    let mut diagnostics = Vec::new();
    let mut fonts: BTreeMap<String, FontFaceRef> = BTreeMap::new();
    for index in &indices {
        let page = pages.get(*index).ok_or_else(|| DltError::new(ErrorCode::NotFound, format!("page {index} does not exist")))?;
        let mut out = PageOut::default();
        for node in &page.nodes {
            let map = shape_map(mappings, &node.id)?;
            layout_node(node, &map, ctx.renderer, &mut out)?;
        }
        for (key, face) in out.fonts.iter() {
            fonts.insert(key.clone(), face.clone());
        }
        diagnostics.extend(out.diagnostics);
        render_pages.push(RenderPage {
            id: page.id.clone(),
            page_index: *index as u32,
            size: page.size,
            primitives: out.primitives,
            source_map: out.regions,
        });
    }
    let faces: Vec<&FontFaceRef> = fonts.values().collect();
    let font_fingerprint = sha256(canonical_json(&serde_json::to_value(&faces).map_err(|e| DltError::new(ErrorCode::Internal, e.to_string()))?)?.as_bytes());
    let mut metadata = BTreeMap::new();
    metadata.insert("layout".to_string(), json!("fresh"));
    metadata.insert("total_pages".to_string(), json!(pages.len()));
    metadata.insert("pages_rendered".to_string(), json!(render_pages.len()));
    metadata.insert("page_kind".to_string(), json!(KIND_PAGE));
    Ok(RenderScene { pages: render_pages, font_fingerprint, diagnostics, metadata, links: vec![] })
}

#[derive(Default)]
struct PageOut {
    primitives: Vec<Primitive>,
    regions: Vec<SourceRegion>,
    diagnostics: Vec<Diagnostic>,
    fonts: BTreeMap<String, FontFaceRef>,
    /// Substitutions already reported on this page: node id, requested family, and the families drawn.
    substitutions: BTreeSet<(String, String, String)>,
}

impl PageOut {
    fn note(&mut self, node_id: &str, message: &str, code: &str) {
        self.diagnostics.push(Diagnostic { code: code.to_string(), message: message.to_string(), node_id: Some(node_id.to_string()), blocking: false, params: Default::default() });
    }

    fn use_font(&mut self, shaped: &ShapedText) {
        for face in &shaped.fonts_used {
            self.fonts.entry(face.key.clone()).or_insert_with(|| face.clone());
        }
    }

    /// Reports text drawn with no face that carries the requested family name. Any family name of a drawn face
    /// counts, so a font installed under its Korean name (맑은 고딕) is not reported. Blank text is not reported.
    fn note_substitution(&mut self, node_id: &str, requested: &str, text: &str, shaped: &ShapedText) {
        if requested.trim().is_empty() || text.trim().is_empty() || shaped.fonts_used.is_empty() {
            return;
        }
        if shaped.fonts_used.iter().any(|face| face_matches_family(face, requested)) {
            return;
        }
        let drawn: BTreeSet<&str> = shaped
            .fonts_used
            .iter()
            .map(|face| if face.family.is_empty() { face.postscript_name.as_str() } else { face.family.as_str() })
            .collect();
        let drawn = drawn.into_iter().collect::<Vec<_>>().join(", ");
        if self.substitutions.insert((node_id.to_string(), requested.to_string(), drawn.clone())) {
            self.diagnostics.push(Diagnostic {
                code: designllm_contracts::diag_codes::FONT_SUBSTITUTED.into(),
                message: format!("font {requested} is not available; the text is drawn with {drawn}"),
                node_id: Some(node_id.to_string()),
                blocking: false,
                params: Default::default(),
            });
        }
    }
}

fn layout_node(node: &Node, map: &ShapeMap, renderer: &dyn RenderProvider, out: &mut PageOut) -> DltResult<()> {
    for note in &map.render_notes {
        let code = format!("PPTX_{}", note.to_ascii_uppercase());
        out.note(&node.id, &format!("{note}: the object is kept in the file but drawn approximately or not at all"), &code);
    }
    match node.kind {
        NodeKind::Path => {
            let path = node
                .ext
                .path
                .clone()
                .ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "vector path without geometry"))?;
            let fill_drawn = node.style.fill.starts_with('#');
            let stroke_drawn = node.style.stroke.starts_with('#') && node.style.stroke_width > 0;
            if fill_drawn || stroke_drawn {
                out.primitives.push(Primitive::Path { node_id: node.id.clone(), bounds: node.bounds, path, style: node.style.clone() });
            }
            out.regions.push(SourceRegion { node_id: node.id.clone(), bounds: node.bounds, logical_range: None, grapheme_rects: vec![] });
        }
        NodeKind::Video => return Err(DltError::unsupported("NodeKind::Video is not supported by the slides layout yet")),
        NodeKind::Image => {
            let asset = node.asset.clone().ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "picture without asset"))?;
            out.primitives.push(Primitive::Image { node_id: node.id.clone(), bounds: node.bounds, asset, fit: None, image_crop_pixel_rect: None, style: None });
            out.regions.push(SourceRegion { node_id: node.id.clone(), bounds: node.bounds, logical_range: None, grapheme_rects: vec![] });
        }
        NodeKind::Line => {
            let b = node.bounds;
            let (mut x1, mut x2) = (b.x, b.x + b.width);
            let (mut y1, mut y2) = (b.y, b.y + b.height);
            if map.flip_h {
                std::mem::swap(&mut x1, &mut x2);
            }
            if map.flip_v {
                std::mem::swap(&mut y1, &mut y2);
            }
            out.primitives.push(Primitive::Line {
                node_id: node.id.clone(),
                from: Point { x: x1, y: y1 },
                to: Point { x: x2, y: y2 },
                style: node.style.clone(),
                // RenderScene::Line.arrow is the legacy end-filled fallback. Typed endpoints are drawn from Style.
                arrow: map.arrow && node.style.stroke_start.is_none() && node.style.stroke_end.is_none(),
            });
            out.regions.push(SourceRegion { node_id: node.id.clone(), bounds: b, logical_range: None, grapheme_rects: vec![] });
        }
        NodeKind::Text | NodeKind::Rectangle | NodeKind::Ellipse => {
            let fill_drawn = node.style.fill.starts_with('#');
            let stroke_drawn = node.style.stroke.starts_with('#') && node.style.stroke_width > 0;
            if node.style.fill.starts_with("unsupported:") {
                out.note(&node.id, "this fill kind is kept in the file and not drawn", "PPTX_FILL_NOT_RENDERED");
            }
            if fill_drawn || stroke_drawn {
                out.primitives.push(Primitive::Shape {
                    node_id: node.id.clone(),
                    bounds: node.bounds,
                    shape: map.geometry.clone(),
                    style: node.style.clone(),
                });
            }
            let rich = RichText::from_parts(&node.text, &map.paragraphs)?;
            layout_text(node, map, &rich, renderer, out)?;
            if rich.paragraphs.is_empty() {
                out.regions.push(SourceRegion { node_id: node.id.clone(), bounds: node.bounds, logical_range: None, grapheme_rects: vec![] });
            }
        }
        NodeKind::Group => {
            out.note(&node.id, "groups are not drawn in this release", "PPTX_GROUP_NOT_RENDERED");
        }
    }
    Ok(())
}

/// One piece of a visual line: text in one style, or one protected inline element.
struct Piece {
    /// Text that is shaped and drawn. An inline element without display text has none.
    text: String,
    style: Style,
    shaped: ShapedText,
    /// Advance in EMU. An inline element without display text has none.
    width: i64,
    /// A protected inline element is one logical grapheme, however its display text shapes.
    atom: bool,
}

/// One visual line of a paragraph.
struct Line {
    pieces: Vec<Piece>,
    /// The width that alignment uses. Trailing spaces do not count.
    width: i64,
    /// The advance of every piece, trailing spaces included. The line is drawn this wide.
    advance: i64,
    height: i64,
    baseline: i64,
    /// The logical graphemes of the line in order, as (x offset from the line start, width). A line that ends with
    /// a break ends with that break's zero-width slot.
    slots: Vec<(i64, i64)>,
}

/// A breakable token: consecutive grapheme clusters that wrap as one unit of text. A break is a token of its own.
struct Token<'a> {
    clusters: Vec<&'a [Unit]>,
    is_break: bool,
}

/// A run of pieces: text in one style, or one protected inline element.
enum Segment {
    Text(String, Style),
    Inline(String, Style),
}

const DEFAULT_SIZE_EMU: i64 = 18 * 12_700;

fn default_style() -> Style {
    designllm_pptx::theme::run_style("Pretendard".into(), DEFAULT_SIZE_EMU, "#000000".into(), false, false, "left")
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x11FF | 0x2E80..=0x2FDF | 0x3000..=0x303F | 0x3040..=0x30FF | 0x3100..=0x31FF | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF | 0xA960..=0xA97F | 0xAC00..=0xD7AF | 0xD7B0..=0xD7FF | 0xF900..=0xFAFF | 0xFE30..=0xFE4F
        | 0xFF00..=0xFFEF | 0x20000..=0x2FA1F)
}

/// The grapheme clusters of a paragraph, as slices of its units. Every unit is one character of the plain text, so
/// a cluster's units are the characters of one grapheme, under Unicode 16.0 segmentation.
fn clusters_of(units: &[Unit]) -> Vec<&[Unit]> {
    let plain: String = units.iter().map(Unit::plain).collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    for grapheme in plain.graphemes(true) {
        let len = grapheme.chars().count();
        out.push(&units[start..start + len]);
        start += len;
    }
    out
}

/// Splits clusters into tokens. A token ends after a space, and each CJK cluster, protected inline element, and
/// break is a token of its own.
fn tokenize(units: &[Unit]) -> Vec<Token<'_>> {
    let mut tokens: Vec<Token<'_>> = Vec::new();
    let mut current: Vec<&[Unit]> = Vec::new();
    for cluster in clusters_of(units) {
        if matches!(cluster.first(), Some(Unit::Break)) {
            flush(&mut current, &mut tokens);
            tokens.push(Token { clusters: vec![cluster], is_break: true });
        } else if cluster.iter().any(|unit| matches!(unit, Unit::Atom { .. })) {
            flush(&mut current, &mut tokens);
            tokens.push(Token { clusters: vec![cluster], is_break: false });
        } else {
            let first = match cluster.first() {
                Some(Unit::Char { ch, .. }) => *ch,
                _ => continue,
            };
            if is_cjk(first) {
                flush(&mut current, &mut tokens);
                tokens.push(Token { clusters: vec![cluster], is_break: false });
            } else {
                current.push(cluster);
                if first == ' ' {
                    flush(&mut current, &mut tokens);
                }
            }
        }
    }
    flush(&mut current, &mut tokens);
    tokens
}

fn flush<'a>(current: &mut Vec<&'a [Unit]>, tokens: &mut Vec<Token<'a>>) {
    if !current.is_empty() {
        tokens.push(Token { clusters: std::mem::take(current), is_break: false });
    }
}

/// Text segments and inline elements of a run of clusters. Consecutive text clusters with equal styles share a
/// segment, and each inline element has one of its own.
fn segments_of(clusters: &[&[Unit]]) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for cluster in clusters {
        if let Some(Unit::Atom { display, style, .. }) = cluster.iter().find(|unit| matches!(unit, Unit::Atom { .. })) {
            out.push(Segment::Inline(display.clone(), style.clone()));
            continue;
        }
        let Some(Unit::Char { style, .. }) = cluster.first() else { continue };
        let text: String = cluster
            .iter()
            .filter_map(|unit| match unit {
                Unit::Char { ch, .. } => Some(*ch),
                _ => None,
            })
            .collect();
        match out.last_mut() {
            Some(Segment::Text(existing, last)) if *last == *style => existing.push_str(&text),
            _ => out.push(Segment::Text(text, style.clone())),
        }
    }
    out
}

/// Pieces for a run of clusters, shaped in order.
fn pieces_for(clusters: &[&[Unit]], renderer: &dyn RenderProvider, out: &mut PageOut, node_id: &str) -> DltResult<Vec<Piece>> {
    let mut pieces = Vec::new();
    for segment in segments_of(clusters) {
        pieces.push(match segment {
            Segment::Text(text, style) => shape_piece(text, style, false, renderer, out, node_id)?,
            Segment::Inline(display, style) => inline_piece(display, style, renderer, out, node_id)?,
        });
    }
    Ok(pieces)
}

fn shape_piece(
    text: String,
    style: Style,
    atom: bool,
    renderer: &dyn RenderProvider,
    out: &mut PageOut,
    node_id: &str,
) -> DltResult<Piece> {
    let shaped = renderer.shape_text(&text, &style)?;
    out.use_font(&shaped);
    out.note_substitution(node_id, &style.font_family, &text, &shaped);
    if !shaped.missing_glyphs.is_empty() {
        out.note(node_id, &format!("missing glyphs: {}", shaped.missing_glyphs.join(" ")), "PPTX_MISSING_GLYPHS");
    }
    let width = shaped.size.width;
    Ok(Piece { text, style, shaped, width, atom })
}

/// A protected inline element. Its display text is shaped like text. Without display text it has no width, but it is
/// still one grapheme, and its style gives the line its height.
fn inline_piece(display: String, style: Style, renderer: &dyn RenderProvider, out: &mut PageOut, node_id: &str) -> DltResult<Piece> {
    if display.is_empty() {
        let shaped = renderer.shape_text(" ", &style)?;
        return Ok(Piece { text: String::new(), style, shaped, width: 0, atom: true });
    }
    shape_piece(display, style, true, renderer, out, node_id)
}

/// Builds the visual lines of one paragraph. `wrap` is the available width, or `None` for no wrapping.
fn build_lines(para: &Para, wrap: Option<i64>, renderer: &dyn RenderProvider, out: &mut PageOut, node_id: &str) -> DltResult<Vec<Line>> {
    let line_factor = para.props.line_spacing_pct.max(1) as i64;
    let tokens = tokenize(&para.units);
    // Each line with its pieces, and whether a break ends it.
    let mut built: Vec<(Vec<Piece>, bool)> = Vec::new();
    let mut current: Vec<Piece> = Vec::new();
    let mut current_width = 0i64;

    for token in &tokens {
        if token.is_break {
            // The break ends the line, and the next token starts a new one.
            built.push((std::mem::take(&mut current), true));
            current_width = 0;
            continue;
        }
        let pieces = pieces_for(&token.clusters, renderer, out, node_id)?;
        let token_width: i64 = pieces.iter().map(|p| p.width).sum();
        let too_wide = wrap.is_some_and(|w| token_width > w && w > 0);
        if let Some(limit) = wrap {
            if !current.is_empty() && current_width + token_width > limit {
                built.push((std::mem::take(&mut current), false));
                current_width = 0;
            }
        }
        if too_wide && token.clusters.len() > 1 {
            // A token wider than the line breaks at cluster boundaries.
            for cluster in &token.clusters {
                let cluster_pieces = pieces_for(std::slice::from_ref(cluster), renderer, out, node_id)?;
                let cluster_width: i64 = cluster_pieces.iter().map(|p| p.width).sum();
                if let Some(limit) = wrap {
                    if !current.is_empty() && current_width + cluster_width > limit {
                        built.push((std::mem::take(&mut current), false));
                        current_width = 0;
                    }
                }
                current_width += cluster_width;
                current.extend(cluster_pieces);
            }
        } else {
            current_width += token_width;
            current.extend(pieces);
        }
    }
    built.push((current, false));

    // Metrics: a line's height and baseline come from its tallest piece. Empty lines use the default style.
    let empty_style = para.units.iter().find_map(|u| u.style().cloned()).unwrap_or_else(default_style);
    let mut result = Vec::with_capacity(built.len());
    for (pieces, ends_with_break) in built {
        let metrics_source = if pieces.is_empty() {
            Some(shape_piece(" ".to_string(), empty_style.clone(), false, renderer, out, node_id)?)
        } else {
            None
        };
        let (height, baseline) = {
            let sources: Vec<&ShapedText> = match &metrics_source {
                Some(p) => vec![&p.shaped],
                None => pieces.iter().map(|p| &p.shaped).collect(),
            };
            let height = sources.iter().map(|s| s.ascent + s.descent + s.line_gap).max().unwrap_or(0);
            let baseline = sources.iter().map(|s| s.baseline).max().unwrap_or(0);
            (height, baseline)
        };
        let height = height * line_factor / 100;
        let advance: i64 = pieces.iter().map(|p| p.width).sum();
        // Trailing spaces do not count toward alignment.
        let mut width = advance;
        if let Some(last) = pieces.last() {
            let trailing = last.text.chars().rev().take_while(|c| *c == ' ').count();
            if trailing > 0 && last.text.chars().all(|c| c == ' ') {
                width -= last.width;
            }
        }
        let slots = slots_of(&pieces, ends_with_break);
        result.push(Line { pieces, width: width.max(0), advance, height, baseline, slots });
    }
    Ok(result)
}

/// The logical graphemes of a line, as (x offset from the line start, width). A text piece gives one slot per
/// grapheme it shapes to. A protected inline element gives one slot. A break at the end of the line gives a
/// zero-width slot after the pieces.
fn slots_of(pieces: &[Piece], ends_with_break: bool) -> Vec<(i64, i64)> {
    let mut slots = Vec::new();
    let mut x = 0i64;
    for piece in pieces {
        if piece.atom {
            slots.push((x, piece.width));
        } else {
            slots.extend(piece.shaped.grapheme_rects.iter().map(|rect| (x + rect.x, rect.width)));
        }
        x += piece.width;
    }
    if ends_with_break {
        slots.push((x, 0));
    }
    slots
}

/// Lays out the text of one node: wraps paragraphs into lines, aligns them, anchors the block, and records the
/// source map with one region per visual line.
fn layout_text(node: &Node, map: &ShapeMap, rich: &RichText, renderer: &dyn RenderProvider, out: &mut PageOut) -> DltResult<()> {
    if rich.paragraphs.is_empty() {
        return Ok(());
    }
    let body = &map.body;
    let [left, top, right, bottom] = body.insets;
    let bounds = node.bounds;
    let inner_x = bounds.x + left;
    let inner_y = bounds.y + top;
    let inner_w = (bounds.width - left - right).max(0);
    let inner_h = (bounds.height - top - bottom).max(0);
    let wrap = if body.wrap_square { Some(inner_w) } else { None };

    // Build lines per paragraph.
    let mut paragraph_lines: Vec<Vec<Line>> = Vec::with_capacity(rich.paragraphs.len());
    for para in &rich.paragraphs {
        paragraph_lines.push(build_lines(para, wrap, renderer, out, &node.id)?);
    }

    // Vertical block: lines and paragraph spacing.
    let mut total = 0i64;
    for (para, lines) in rich.paragraphs.iter().zip(&paragraph_lines) {
        total += para.props.space_before;
        for line in lines {
            total += line.height;
        }
        total += para.props.space_after;
    }
    if total > inner_h && !body.wrap_square {
        out.note(&node.id, "text is taller than its shape", "PPTX_TEXT_OVERFLOW");
    } else if total > inner_h {
        out.note(&node.id, "text is taller than its shape; the overflow is drawn", "PPTX_TEXT_OVERFLOW");
    }
    let block_top = match body.anchor.as_str() {
        "ctr" => inner_y + (inner_h - total) / 2,
        "b" => inner_y + inner_h - total,
        _ => inner_y,
    };

    // Placement and source map. Each line's region numbers its graphemes within the paragraph.
    let mut y = block_top;
    for (index, (para, lines)) in rich.paragraphs.iter().zip(&paragraph_lines).enumerate() {
        y += para.props.space_before;
        let paragraph_id = format!("{}#p{index}", node.id);
        let plain = para.plain();
        let graphemes = plain.graphemes(true).count() as u32;
        let mut next = 0u32;
        for line in lines {
            let line_x = match para.props.align.as_str() {
                "center" => inner_x + (inner_w - line.width) / 2,
                "right" => inner_x + inner_w - line.width,
                _ => inner_x,
            };
            let line_top = y;
            let baseline_y = line_top + line.baseline;
            let mut x = line_x;
            for piece in &line.pieces {
                if !piece.text.is_empty() {
                    out.primitives.push(Primitive::Text {
                        node_id: node.id.clone(),
                        bounds: Rect { x, y: line_top, width: piece.width, height: line.height },
                        origin: Point { x, y: baseline_y },
                        text: piece.text.clone(),
                        style: piece.style.clone(),
                        shaped: piece.shaped.clone(),
                    });
                }
                x += piece.width;
            }
            let rects: Vec<Rect> = line
                .slots
                .iter()
                .map(|&(dx, width)| Rect { x: line_x + dx, y: line_top, width, height: line.height })
                .collect();
            let count = rects.len() as u32;
            out.regions.push(SourceRegion {
                node_id: node.id.clone(),
                bounds: Rect { x: line_x, y: line_top, width: line.advance, height: line.height },
                logical_range: Some(TextRange { paragraph_id: paragraph_id.clone(), start: next, end: next + count }),
                grapheme_rects: rects,
            });
            next += count;
            y += line.height;
        }
        if next != graphemes {
            out.note(
                &node.id,
                &format!("the source map covers {next} of the {graphemes} graphemes of paragraph {index}"),
                "PPTX_SOURCE_MAP_MISMATCH",
            );
        }
        y += para.props.space_after;
    }
    Ok(())
}
