//! Operations on the slide model. Each operation is checked against the model and its mapping records. A failure
//! returns an error, and the caller discards the whole candidate. Nothing is applied partially.

use std::collections::{BTreeMap, BTreeSet};

use designllm_contracts::{
    sha256, AssetRef, AssetStore, Diagnostic, DltError, DltResult, EngineContext, ErrorCode, FigureLink, Id, Node,
    NodeKind, Operation, Page, Rect, Style, StyleField, StylePatch, TextRange, ThemePatch,
};
use designllm_contracts::media_header::{self, FIGURE_REPRESENTATIONS};
use designllm_package::opc;
use designllm_package::Package;
use designllm_pptx::model::to_value;
use designllm_pptx::ns::magic_matches;
use designllm_pptx::text::{ParaProps, RichText};
use designllm_pptx::v12::MAX_LINE_WIDTH_EMU;
use designllm_pptx::{BodyMap, PageMap, SegmentMap, ShapeMap};
use serde_json::Value;

use crate::{find_node, ids_in_use, invalid, page_map, shape_map, unsupported, SlideState};

/// EMU per hundredth of a point. Font sizes and spacing are stored on this grid.
const CENTIPOINT_EMU: i64 = 127;
const ALIGNMENTS: [&str; 5] = ["left", "center", "right", "justify", "distributed"];

/// Applies every operation in order. Returns the new state, the changed ids, and non-blocking diagnostics.
pub(crate) fn apply_all(
    mut state: SlideState,
    operations: &[Operation],
    package: &Package,
    ctx: &EngineContext<'_>,
) -> DltResult<(SlideState, Vec<Id>, Vec<Diagnostic>)> {
    let mut changed = BTreeSet::new();
    let mut diagnostics = Vec::new();
    for operation in operations {
        apply_one(&mut state, operation, package, ctx.assets, &mut changed, &mut diagnostics)?;
    }
    check_model(&state)?;
    Ok((state, changed.into_iter().collect(), diagnostics))
}

fn apply_one(
    state: &mut SlideState,
    operation: &Operation,
    package: &Package,
    assets: &dyn AssetStore,
    changed: &mut BTreeSet<Id>,
    diagnostics: &mut Vec<Diagnostic>,
) -> DltResult<()> {
    match operation {
        Operation::SetText { node_id, text } => {
            let (page, index) = locate(state, node_id)?;
            require_text(&state.pages[page].nodes[index])?;
            let mut map = shape_map(&state.mappings, node_id)?;
            let node = &mut state.pages[page].nodes[index];
            let mut rich = RichText::from_parts(&node.text, &map.paragraphs)?;
            let end = rich.flat_len();
            rich.replace(0, end, text, None)?;
            store(node, &mut map, &rich);
            state.mappings.insert(node_id.clone(), to_value(&map)?);
            changed.insert(node_id.clone());
        }
        Operation::ReplaceText { range, text } => {
            let (node_id, paragraph) = parse_paragraph_id(&range.paragraph_id)?;
            let (page, index) = locate(state, &node_id)?;
            require_text(&state.pages[page].nodes[index])?;
            let mut map = shape_map(&state.mappings, &node_id)?;
            let node = &mut state.pages[page].nodes[index];
            let mut rich = RichText::from_parts(&node.text, &map.paragraphs)?;
            let (start, end) = flat_of(&rich, paragraph, range)?;
            rich.replace(start, end, text, None)?;
            store(node, &mut map, &rich);
            state.mappings.insert(node_id.clone(), to_value(&map)?);
            changed.insert(node_id);
        }
        Operation::SetStyle { node_ids, patch } => {
            if node_ids.is_empty() {
                return Err(invalid("set_style needs at least one node"));
            }
            for node_id in node_ids {
                let (page, index) = locate(state, node_id)?;
                let kind = state.pages[page].nodes[index].kind;
                let font = font_part(patch, kind, diagnostics, node_id)?;
                let mut map = shape_map(&state.mappings, node_id)?;
                // The preset is in the mapping record, not in the node: a shadow is only read on `rect` and `ellipse`.
                if patch.shadows.as_ref().is_some_and(|shadows| !shadows.is_empty()) {
                    designllm_pptx::guard::check_shadow_preset(&map.geometry, node_id, "in slides")?;
                }
                let start_marker_touched = patch.stroke_start.is_some() || patch.clear.contains(&StyleField::StrokeStart);
                let end_marker_touched = patch.stroke_end.is_some() || patch.clear.contains(&StyleField::StrokeEnd);
                let current_node = &state.pages[page].nodes[index];
                let legacy_marker_mapping = kind == NodeKind::Line
                    && map.arrow
                    && current_node.style.stroke_start.is_none()
                    && current_node.style.stroke_end.is_none()
                    && !map.unmapped_head_end
                    && !map.unmapped_tail_end
                    && !map.custom_head_end_size
                    && !map.custom_tail_end_size;
                if (start_marker_touched && map.unmapped_head_end) || (end_marker_touched && map.unmapped_tail_end) {
                    return Err(unsupported("the source PPTX uses an unmapped line-end marker type; its original endpoint is preserved and cannot be edited"));
                }
                if legacy_marker_mapping && (start_marker_touched || end_marker_touched) {
                    return Err(unsupported("line-end details are not available in this saved PPTX mapping; the original endpoints are preserved but cannot be edited"));
                }
                let node = &mut state.pages[page].nodes[index];
                apply_shape_patch(node, patch, kind, node_id, diagnostics)?;
                if kind == NodeKind::Line && (start_marker_touched || end_marker_touched) {
                    if patch.clear.contains(&StyleField::StrokeStart) && patch.stroke_start.is_none() {
                        map.custom_head_end_size = false;
                    }
                    if patch.clear.contains(&StyleField::StrokeEnd) && patch.stroke_end.is_none() {
                        map.custom_tail_end_size = false;
                    }
                    refresh_line_marker_mapping(&mut map, node);
                }
                if matches!(kind, NodeKind::Text | NodeKind::Rectangle | NodeKind::Ellipse) {
                    let mut rich = RichText::from_parts(&node.text, &map.paragraphs)?;
                    let end = rich.flat_len();
                    if font_is_set(&font) {
                        rich.restyle(0, end, &font)?;
                    }
                    if let Some(align) = &patch.align {
                        for index in 0..rich.paragraphs.len() {
                            let mut props = rich.paragraphs[index].props.clone();
                            props.align = align.clone();
                            rich.set_props(index, props)?;
                        }
                    }
                    store(node, &mut map, &rich);
                }
                state.mappings.insert(node_id.clone(), to_value(&map)?);
                changed.insert(node_id.clone());
            }
        }
        Operation::SetTextStyleRange { range, patch } => {
            let (node_id, paragraph) = parse_paragraph_id(&range.paragraph_id)?;
            let (page, index) = locate(state, &node_id)?;
            require_text(&state.pages[page].nodes[index])?;
            if patch.fill.is_some() || patch.stroke.is_some() || patch.stroke_width.is_some() {
                return Err(unsupported("fill and outline apply to the shape, not to a text range"));
            }
            let font = font_part(patch, NodeKind::Text, diagnostics, &node_id)?;
            let mut map = shape_map(&state.mappings, &node_id)?;
            let node = &mut state.pages[page].nodes[index];
            let mut rich = RichText::from_parts(&node.text, &map.paragraphs)?;
            let (start, end) = flat_of(&rich, paragraph, range)?;
            rich.restyle(start, end, &font)?;
            store(node, &mut map, &rich);
            state.mappings.insert(node_id.clone(), to_value(&map)?);
            changed.insert(node_id);
        }
        Operation::SetNamedStyle { .. } => {
            return Err(unsupported("named styles are not supported for slides; set the style on each shape or range"));
        }
        Operation::SetGeometry { node_id, bounds } => {
            let (page, index) = locate(state, node_id)?;
            let kind = state.pages[page].nodes[index].kind;
            check_bounds(bounds, kind)?;
            state.pages[page].nodes[index].bounds = *bounds;
            changed.insert(node_id.clone());
        }
        Operation::AddNode { page_id, parent_id, node } => {
            if parent_id.is_some() {
                return Err(unsupported("nodes can only be added to a slide; groups are not supported"));
            }
            add_node(state, page_id, node, package, assets, changed, diagnostics)?;
        }
        Operation::RemoveNode { node_id } => {
            let (page, index) = locate(state, node_id)?;
            let removed = state.pages[page].nodes.remove(index);
            let map = shape_map(&state.mappings, node_id)?;
            state.mappings.remove(node_id);
            let part = page_map_of(state, page)?.part;
            state.opaque.retain(|r| {
                !(r.part == part && (r.anchor == map.anchor || r.anchor.starts_with(&format!("{}/", map.anchor))))
            });
            state.figure_links.retain(|link| link.target_node_id != removed.id);
            changed.insert(removed.id);
        }
        Operation::UpdateFigureLink { link } => {
            update_figure_link(state, link, package, assets, changed)?;
        }
        Operation::SetParagraphFormat { paragraph_id, format } => {
            let (node_id, paragraph) = parse_paragraph_id(paragraph_id)?;
            let (page, index) = locate(state, &node_id)?;
            require_text(&state.pages[page].nodes[index])?;
            if format.line_spacing_percent.is_some() {
                return Err(unsupported("line_spacing_percent is not supported for slides; leave it out"));
            }
            if format.line_spacing != 0 {
                return Err(unsupported("line spacing is not supported for slides yet; leave line_spacing at 0"));
            }
            if format.indent != 0 {
                return Err(unsupported("paragraph indent is not supported for slides yet; leave indent at 0"));
            }
            if !ALIGNMENTS.contains(&format.align.as_str()) {
                return Err(invalid(format!("unknown alignment {:?}", format.align)));
            }
            let space_before = grid_emu(format.space_before, "space_before", diagnostics, &node_id)?;
            let space_after = grid_emu(format.space_after, "space_after", diagnostics, &node_id)?;
            let mut map = shape_map(&state.mappings, &node_id)?;
            let node = &mut state.pages[page].nodes[index];
            let mut rich = RichText::from_parts(&node.text, &map.paragraphs)?;
            let mut props = rich
                .paragraphs
                .get(paragraph)
                .ok_or_else(|| invalid(format!("paragraph {paragraph} does not exist")))?
                .props
                .clone();
            props.align = format.align.clone();
            props.space_before = space_before;
            props.space_after = space_after;
            rich.set_props(paragraph, props)?;
            store(node, &mut map, &rich);
            state.mappings.insert(node_id.clone(), to_value(&map)?);
            changed.insert(node_id);
        }
        Operation::InsertPage { index, page } => {
            insert_page(state, *index, page, package, assets, changed, diagnostics)?;
        }
        Operation::RemovePage { page_id } => {
            remove_page(state, page_id, package, changed)?;
        }
        Operation::ReorderPages { page_ids } => {
            reorder_pages(state, page_ids, changed)?;
        }
        Operation::SetPageNotes { page_id, notes } => {
            let index = page_index(state, page_id)?;
            let text = normalize_notes(notes)?;
            let mut map = page_map(&state.mappings, page_id)?;
            if map.notes_part.is_none() && !text.is_empty() {
                let allocation = designllm_pptx::allocate_page(package, &state.mappings, true)?;
                map.notes_part = allocation.notes_part;
                state.mappings.insert(page_id.clone(), to_value(&map)?);
            }
            state.pages[index].notes = text;
            changed.insert(page_id.clone());
        }
        Operation::SetTheme { patch } => {
            set_theme(state, patch, package, changed, diagnostics)?;
        }
        Operation::ReplaceImageAsset { node_id, .. } => {
            let mut error = unsupported(format!("replace_image_asset on {node_id} is a Canvas-only operation and is not supported for slides"));
            error.affected_ids.push(node_id.clone());
            return Err(error);
        }
        other => {
            let name = other.name();
            return Err(unsupported(format!("operation {name} is not supported for slides")));
        }
    }
    Ok(())
}

/// Adds theme values to the deck's theme edits. A source shape whose colour or font comes from the theme takes the
/// value it reads with under the new theme, as the saved file shows it; a value set on the shape itself is kept:
/// - the shape style (fill, stroke and the rest) is merged with the source style;
/// - text runs that line up with the source (same paragraphs, same runs per paragraph) are merged run by run;
/// - text runs that no longer line up keep their values, which save as set values, and a SLIDES_THEME_KEPT_AS_SET
///   diagnostic names the shape: a run value alone cannot tell a theme value from a set one.
///
/// The shape's text fields equal its first run, as the reader sets them.
///
/// `package` is the original package. Changed ids: `theme` and each shape that took new values.
fn set_theme(
    state: &mut SlideState,
    patch: &ThemePatch,
    package: &Package,
    changed: &mut BTreeSet<Id>,
    diagnostics: &mut Vec<Diagnostic>,
) -> DltResult<()> {
    let patch = designllm_pptx::theme::checked_patch(patch)?;
    let next = match &state.theme {
        Some(current) => current.merged(&patch),
        None => patch,
    };
    // One copy of the package takes the current and then the new theme, each written from the original theme bytes.
    let mut work = package.clone();
    let old = match &state.theme {
        Some(current) => {
            designllm_pptx::theme::write_theme_from(&mut work, package, current)?;
            source_shapes(&work)?
        }
        None => source_shapes(package)?,
    };
    designllm_pptx::theme::write_theme_from(&mut work, package, &next)?;
    let new = source_shapes(&work)?;
    for page in &mut state.pages {
        for node in &mut page.nodes {
            let (Some((old_style, old_paragraphs)), Some((new_style, new_paragraphs))) = (old.get(&node.id), new.get(&node.id)) else {
                continue;
            };
            if old_style == new_style && old_paragraphs == new_paragraphs {
                continue;
            }
            let mut map = shape_map(&state.mappings, &node.id)?;
            if map.origin != designllm_pptx::ORIGIN_SOURCE {
                continue;
            }
            let style = to_value(&node.style)?;
            node.style = designllm_pptx::model::from_value(&node.id, &rebase(&style, old_style, new_style))?;
            let mut text_changed = false;
            if old_paragraphs != new_paragraphs {
                let current = to_value(&map.paragraphs)?;
                if run_layout(&current) == run_layout(old_paragraphs) {
                    let paragraphs = rebase(&current, old_paragraphs, new_paragraphs);
                    text_changed = paragraphs != current;
                    map.paragraphs = designllm_pptx::model::from_value(&node.id, &paragraphs)?;
                    state.mappings.insert(node.id.clone(), to_value(&map)?);
                } else {
                    diagnostics.push(Diagnostic {
                        code: crate::SLIDES_THEME_KEPT_AS_SET.into(),
                        message: format!(
                            "shape {}: its text runs no longer line up with the file, so a run's colour or font cannot be told to come from the theme; the text keeps its colours and fonts, saved as set values",
                            node.id
                        ),
                        node_id: Some(node.id.clone()),
                        blocking: false,
                        params: Default::default(),
                    });
                }
            }
            follow_first_run(node, &map);
            if text_changed || to_value(&node.style)? != style {
                changed.insert(node.id.clone());
            }
        }
    }
    state.theme = Some(next);
    changed.insert("theme".into());
    Ok(())
}

/// Style and paragraph records of every source shape of `package`, as the reader produces them, by node id. Pictures
/// are not hashed: only the styles are read.
fn source_shapes(package: &Package) -> DltResult<BTreeMap<Id, (Value, Value)>> {
    let unhashed = |bytes: &[u8], mime: &str| -> DltResult<AssetRef> {
        Ok(AssetRef { sha256: String::new(), mime: mime.to_string(), bytes: bytes.len() as u64 })
    };
    let (_, pages) = designllm_pptx::reader::read_presentation(package, &unhashed)?;
    let mut out = BTreeMap::new();
    for shape in pages.iter().flat_map(|page| page.shapes.iter()) {
        out.insert(shape.node.id.clone(), (to_value(&shape.node.style)?, to_value(&shape.map.paragraphs)?));
    }
    Ok(out)
}

/// The number of segments of each paragraph record.
fn run_layout(paragraphs: &Value) -> Vec<usize> {
    paragraphs
        .as_array()
        .map(|list| list.iter().map(|paragraph| paragraph["segments"].as_array().map_or(0, Vec::len)).collect())
        .unwrap_or_default()
}

/// The shape's text fields from its first run, as the reader sets them.
fn follow_first_run(node: &mut Node, map: &ShapeMap) {
    let first = map.paragraphs.iter().flat_map(|paragraph| &paragraph.segments).find_map(|segment| match segment {
        SegmentMap::Run { chars, style } if *chars > 0 => Some(style),
        SegmentMap::Atom { style, .. } => Some(style),
        _ => None,
    });
    if let Some(style) = first {
        node.style.font_family.clone_from(&style.font_family);
        node.style.font_size = style.font_size;
        node.style.color.clone_from(&style.color);
        node.style.bold = style.bold;
        node.style.italic = style.italic;
    }
}

/// `current` with each value that still equals its `before` value replaced by its `after` value. Objects merge key by
/// key and arrays of one length item by item, so a value that differs from `before` (set on the shape) is kept.
fn rebase(current: &Value, before: &Value, after: &Value) -> Value {
    if current == before {
        return after.clone();
    }
    match (current, before, after) {
        (Value::Object(current), Value::Object(before), Value::Object(after)) => Value::Object(
            current
                .iter()
                .map(|(key, value)| {
                    let merged = match (before.get(key), after.get(key)) {
                        (Some(before), Some(after)) => rebase(value, before, after),
                        _ => value.clone(),
                    };
                    (key.clone(), merged)
                })
                .collect(),
        ),
        (Value::Array(current), Value::Array(before), Value::Array(after)) if current.len() == before.len() && before.len() == after.len() => {
            Value::Array(current.iter().zip(before).zip(after).map(|((current, before), after)| rebase(current, before, after)).collect())
        }
        _ => current.clone(),
    }
}

/// Writes a rich text back to the node and its mapping record.
fn store(node: &mut Node, map: &mut ShapeMap, rich: &RichText) {
    designllm_pptx::store_rich_text(node, map, rich);
    map.has_text_body = !rich.paragraphs.is_empty();
}

fn require_text(node: &Node) -> DltResult<()> {
    match node.kind {
        NodeKind::Text | NodeKind::Rectangle | NodeKind::Ellipse => Ok(()),
        _ => Err(unsupported(format!("node {} has no text body", node.id))),
    }
}

/// Finds a node by id: page index and node index.
fn locate(state: &SlideState, node_id: &str) -> DltResult<(usize, usize)> {
    for (page, slide) in state.pages.iter().enumerate() {
        if let Some(index) = slide.nodes.iter().position(|n| n.id == node_id) {
            return Ok((page, index));
        }
    }
    Err(DltError::new(ErrorCode::NotFound, format!("node {node_id} does not exist")))
}

fn page_index(state: &SlideState, page_id: &str) -> DltResult<usize> {
    state
        .pages
        .iter()
        .position(|p| p.id == page_id)
        .ok_or_else(|| DltError::new(ErrorCode::NotFound, format!("page {page_id} does not exist")))
}

fn page_map_of(state: &SlideState, page: usize) -> DltResult<PageMap> {
    page_map(&state.mappings, &state.pages[page].id)
}

/// Paragraph ids are `{node_id}#p{index}`.
fn parse_paragraph_id(paragraph_id: &str) -> DltResult<(String, usize)> {
    let (node_id, index) = paragraph_id
        .rsplit_once("#p")
        .ok_or_else(|| invalid(format!("paragraph id {paragraph_id:?} must be {{node_id}}#p{{index}}")))?;
    let index: usize = index.parse().map_err(|_| invalid(format!("paragraph id {paragraph_id:?} has a bad index")))?;
    Ok((node_id.to_string(), index))
}

/// Converts a grapheme range inside one paragraph to flat positions of the rich text.
fn flat_of(rich: &RichText, paragraph: usize, range: &TextRange) -> DltResult<(usize, usize)> {
    rich.flat_range(paragraph, range.start, range.end)
}

/// Font-only part of a patch, with sizes on the 1/100 pt grid. Reports rounding.
fn font_part(patch: &StylePatch, kind: NodeKind, diagnostics: &mut Vec<Diagnostic>, node_id: &str) -> DltResult<StylePatch> {
    if patch.font_family.is_some() || patch.font_size.is_some() || patch.bold.is_some() || patch.italic.is_some() || patch.color.is_some() {
        if matches!(kind, NodeKind::Image | NodeKind::Line) {
            return Err(unsupported(format!("node {node_id} has no text; text formatting does not apply")));
        }
    }
    let mut font = StylePatch {
        font_family: patch.font_family.clone(),
        font_size: None,
        color: None,
        bold: patch.bold,
        italic: patch.italic,
        ..StylePatch::default()
    };
    if let Some(family) = &font.font_family {
        if family.trim().is_empty() {
            return Err(invalid("font family cannot be empty"));
        }
    }
    if let Some(size) = patch.font_size {
        font.font_size = Some(grid_emu(size, "font_size", diagnostics, node_id)?);
        if size <= 0 {
            return Err(invalid("font size must be positive"));
        }
    }
    if let Some(color) = &patch.color {
        font.color = Some(color_value(color)?);
    }
    Ok(font)
}

fn font_is_set(patch: &StylePatch) -> bool {
    patch.font_family.is_some() || patch.font_size.is_some() || patch.color.is_some() || patch.bold.is_some() || patch.italic.is_some()
}

fn refresh_line_marker_mapping(map: &mut ShapeMap, node: &Node) {
    map.arrow = node.style.stroke_start.is_some()
        || node.style.stroke_end.is_some()
        || map.unmapped_head_end
        || map.unmapped_tail_end;
    map.render_notes.retain(|note| !note.starts_with("line_end_marker_"));
    if map.unmapped_head_end || map.unmapped_tail_end {
        map.render_notes.push("line_end_marker_unsupported_type_preserved_preview_approximate".into());
    }
    if map.custom_head_end_size || map.custom_tail_end_size {
        map.render_notes.push("line_end_marker_size_preview_approximate".into());
    }
}

/// Applies the shape-level part of a patch to the node and its style. Text formatting of runs is applied by the
/// caller. A fill or outline is refused on kinds that cannot carry one.
fn apply_shape_patch(
    node: &mut Node,
    patch: &StylePatch,
    kind: NodeKind,
    node_id: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> DltResult<()> {
    let marker_start = patch.stroke_start.is_some() || patch.clear.contains(&StyleField::StrokeStart);
    let marker_end = patch.stroke_end.is_some() || patch.clear.contains(&StyleField::StrokeEnd);
    if (marker_start || marker_end) && kind != NodeKind::Line {
        return Err(unsupported(format!("node {node_id} has no line endpoints")));
    }
    if patch.fill.is_some() && matches!(kind, NodeKind::Line | NodeKind::Image) {
        return Err(unsupported(format!("node {node_id} has no fill")));
    }
    if (patch.stroke.is_some() || patch.stroke_width.is_some()) && kind == NodeKind::Image {
        return Err(unsupported(format!("node {node_id} has no outline")));
    }
    if let Some(fill) = &patch.fill {
        node.style.fill = fill_value(fill)?;
    }
    if let Some(stroke) = &patch.stroke {
        node.style.stroke = fill_value(stroke)?;
    }
    if let Some(width) = patch.stroke_width {
        // A width equal to the stored one is not a change, so an original width outside the range is kept as it is.
        if width != node.style.stroke_width {
            node.style.stroke_width = checked_outline_width(width, node_id)?;
        }
    }
    if patch.clear.contains(&StyleField::StrokeStart) {
        node.style.stroke_start = None;
    }
    if patch.clear.contains(&StyleField::StrokeEnd) {
        node.style.stroke_end = None;
    }
    if let Some(end) = patch.stroke_start {
        node.style.stroke_start = Some(end);
    }
    if let Some(end) = patch.stroke_end {
        node.style.stroke_end = Some(end);
    }
    if let Some(align) = &patch.align {
        if !ALIGNMENTS.contains(&align.as_str()) {
            return Err(invalid(format!("unknown alignment {align:?}")));
        }
        node.style.align = align.clone();
    }
    if patch.clear.contains(&StyleField::Shadows) {
        node.style.shadows.clear();
    }
    if let Some(shadows) = &patch.shadows {
        node.style.shadows = shadows.iter().map(designllm_pptx::editable::canonical_shadow).collect();
    }
    if font_is_set(patch) {
        if matches!(kind, NodeKind::Image | NodeKind::Line | NodeKind::Path) {
            return Err(unsupported(format!("node {node_id} has no text; text formatting does not apply")));
        }
        if let Some(family) = &patch.font_family {
            node.style.font_family = family.clone();
        }
        if let Some(size) = patch.font_size {
            node.style.font_size = grid_emu(size, "font_size", diagnostics, node_id)?;
        }
        if let Some(color) = &patch.color {
            node.style.color = color_value(color)?;
        }
        if let Some(bold) = patch.bold {
            node.style.bold = bold;
        }
        if let Some(italic) = patch.italic {
            node.style.italic = italic;
        }
    }
    Ok(())
}

/// Fill or outline value: `none` or `#RRGGBB`.
fn fill_value(value: &str) -> DltResult<String> {
    if value == "none" {
        return Ok("none".into());
    }
    color_value(value)
}

/// Colour value `#RRGGBB`, normalized to uppercase.
fn color_value(value: &str) -> DltResult<String> {
    designllm_pptx::theme::normalize_hex(value).ok_or_else(|| invalid(format!("colour {value:?} must be #RRGGBB")))
}

/// Outline width in EMU that the PPTX writer can store: `0..=MAX_LINE_WIDTH_EMU`. Applied to a width that a change sets or
/// adds, so a width that would not save is refused before the change is applied.
fn checked_outline_width(width: i64, node_id: &str) -> DltResult<i64> {
    if (0..=MAX_LINE_WIDTH_EMU).contains(&width) {
        return Ok(width);
    }
    Err(invalid(format!("node {node_id}: outline width {width} EMU is outside 0..={MAX_LINE_WIDTH_EMU} (0 to 1584 pt)")))
}

/// EMU value on the 1/100 pt grid. Off-grid values round to the nearest grid value, half away from zero, and the
/// rounding is reported.
fn grid_emu(value: i64, field: &str, diagnostics: &mut Vec<Diagnostic>, node_id: &str) -> DltResult<i64> {
    if value < 0 {
        return Err(invalid(format!("{field} cannot be negative")));
    }
    let rounded = ((value + CENTIPOINT_EMU / 2) / CENTIPOINT_EMU) * CENTIPOINT_EMU;
    if rounded != value {
        diagnostics.push(Diagnostic {
            code: designllm_contracts::diag_codes::NATIVE_UNIT_ROUNDING.into(),
            message: format!("{field} {value} EMU rounded to {rounded} EMU on the 1/100 pt grid"),
            node_id: Some(node_id.to_string()),
            blocking: false,
            params: Default::default(),
        });
    }
    Ok(rounded)
}

fn check_bounds(bounds: &Rect, kind: NodeKind) -> DltResult<()> {
    if bounds.width < 0 || bounds.height < 0 {
        return Err(invalid("bounds cannot have negative size"));
    }
    let minimum_zero = matches!(kind, NodeKind::Line);
    if !minimum_zero && (bounds.width == 0 || bounds.height == 0) {
        return Err(invalid("a shape needs a positive width and height"));
    }
    Ok(())
}

fn normalize_notes(text: &str) -> DltResult<String> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    for ch in normalized.chars() {
        if ch == '\u{FFFC}' {
            return Err(unsupported("inline fields cannot be written in notes"));
        }
        if (ch as u32) < 0x20 && ch != '\n' && ch != '\t' && ch != '\u{000B}' {
            return Err(invalid(format!("control character U+{:04X} cannot be stored in notes", ch as u32)));
        }
    }
    Ok(normalized)
}

/// Adds a node to a page at the top of the z-order and creates its mapping record.
#[allow(clippy::too_many_arguments)]
fn add_node(
    state: &mut SlideState,
    page_id: &str,
    node: &Node,
    package: &Package,
    assets: &dyn AssetStore,
    changed: &mut BTreeSet<Id>,
    diagnostics: &mut Vec<Diagnostic>,
) -> DltResult<()> {
    let page_index = page_index(state, page_id)?;
    if ids_in_use(&state.pages).contains(&node.id) {
        return Err(DltError::new(ErrorCode::AlreadyExists, format!("id {} is already in use", node.id)));
    }
    if node.children.iter().next().is_some() {
        return Err(unsupported("nested nodes are not supported for slides"));
    }
    if node.layout.is_some() || !node.interactions.is_empty() || !node.variants.is_empty() {
        return Err(unsupported("layout, interactions, and variants are not supported for slides"));
    }
    let mut page_record = page_map(&state.mappings, page_id)?;
    let cnvpr = page_record.max_shape_id.max(1) + 1;
    page_record.max_shape_id = cnvpr;
    state.mappings.insert(page_id.to_string(), to_value(&page_record)?);

    let mut stored = node.clone();
    let mut map = match node.kind {
        NodeKind::Path => {
            if node.ext.path.is_none() {
                return Err(invalid("a vector path needs its geometry"));
            }
            if !node.text.is_empty() {
                return Err(unsupported("text in a vector path is not supported for slides"));
            }
            check_bounds(&node.bounds, node.kind)?;
            let mut style = node.style.clone();
            style.fill = fill_value(&node.style.fill)?;
            style.stroke = fill_value(&node.style.stroke)?;
            style.stroke_width = checked_outline_width(node.style.stroke_width, &node.id)?;
            stored.style = style;
            stored.asset = None;
            // A vector path has no text body: its mapping says so, and the writer writes none.
            let mut map = designllm_pptx::new_shape_map("shape", cnvpr, "custGeom", false, &RichText::default(), BodyMap::default());
            map.has_text_body = false;
            map
        }
        NodeKind::Video => return Err(DltError::unsupported("NodeKind::Video cannot be added to a slide yet")),
        NodeKind::Text | NodeKind::Rectangle | NodeKind::Ellipse => {
            check_bounds(&node.bounds, node.kind)?;
            let geometry = if node.kind == NodeKind::Ellipse { "ellipse" } else { "rect" };
            let mut style = node.style.clone();
            style.fill = fill_value(&node.style.fill)?;
            style.stroke = fill_value(&node.style.stroke)?;
            style.stroke_width = checked_outline_width(node.style.stroke_width, &node.id)?;
            style.font_size = grid_emu(node.style.font_size, "font_size", diagnostics, &node.id)?;
            style.color = color_value(&node.style.color)?;
            // A shadow belongs to the shape, not to its text: the text runs take the style without it.
            let shadows = std::mem::take(&mut style.shadows);
            let props = ParaProps { align: node.style.align.clone(), ..ParaProps::default() };
            let rich = RichText::from_plain(&node.text, &designllm_pptx::text::canonical_run_style(style.clone()), &props)?;
            style.shadows = shadows.iter().map(designllm_pptx::editable::canonical_shadow).collect();
            let mut map = designllm_pptx::new_shape_map(
                "shape",
                cnvpr,
                geometry,
                node.kind == NodeKind::Text,
                &rich,
                BodyMap::default(),
            );
            map.has_text_body = true;
            stored.style = style;
            stored.asset = None;
            store(&mut stored, &mut map, &rich);
            map
        }
        NodeKind::Line => {
            check_bounds(&node.bounds, node.kind)?;
            let mut style = node.style.clone();
            style.stroke = fill_value(&node.style.stroke)?;
            style.fill = "none".into();
            style.stroke_width = checked_outline_width(node.style.stroke_width, &node.id)?;
            stored.style = style;
            stored.asset = None;
            let mut map = designllm_pptx::new_shape_map("connector", cnvpr, "line", false, &RichText::default(), BodyMap::default());
            refresh_line_marker_mapping(&mut map, &stored);
            map
        }
        NodeKind::Image => {
            let asset = node.asset.clone().ok_or_else(|| invalid("a picture needs an asset"))?;
            let bytes = assets.read(&asset)?;
            if designllm_contracts::sha256(&bytes) != asset.sha256 {
                return Err(invalid("picture bytes do not match their hash"));
            }
            if !designllm_pptx::ns::magic_matches(&asset.mime, &bytes) {
                return Err(unsupported(format!("picture type {} is not supported; use PNG, JPEG, GIF, or BMP", asset.mime)));
            }
            check_bounds(&node.bounds, node.kind)?;
            if node.style != Style::default() {
                diagnostics.push(Diagnostic {
                    code: "SLIDES_STYLE_IGNORED".into(),
                    message: format!("picture {} keeps its own appearance; the given style was not applied", node.id),
                    node_id: Some(node.id.clone()),
                    blocking: false,
                    params: Default::default(),
                });
            }
            stored.style = Style::default();
            stored.text.clear();
            stored.asset = Some(asset.clone());
            let mut map = designllm_pptx::new_shape_map("picture", cnvpr, "rect", false, &RichText::default(), BodyMap::default());
            map.media_part = Some(designllm_pptx::allocate_media(package, &asset)?);
            map
        }
        NodeKind::Group => return Err(unsupported("group nodes are not supported for slides")),
    };
    map.anchor = String::new();
    let mut map_value = to_value(&map)?;
    if let Value::Object(object) = &mut map_value {
        object.insert("origin".into(), Value::String("added".into()));
    }
    state.mappings.insert(stored.id.clone(), map_value);
    changed.insert(stored.id.clone());
    state.pages[page_index].nodes.push(stored);
    Ok(())
}

/// Replaces a picture's bytes with a figure render and records the link. The picture keeps its position and width,
/// and its height follows the render's pixel aspect ratio. Every check runs before the model changes, so a refused
/// change leaves nothing behind.
fn update_figure_link(
    state: &mut SlideState,
    link: &FigureLink,
    package: &Package,
    assets: &dyn AssetStore,
    changed: &mut BTreeSet<Id>,
) -> DltResult<()> {
    let (page, index) = locate(state, &link.target_node_id)?;
    if state.pages[page].nodes[index].kind != NodeKind::Image {
        return Err(invalid(format!("node {} is not a picture; a figure link can only target a picture", link.target_node_id)));
    }
    if !FIGURE_REPRESENTATIONS.contains(&link.representation.as_str()) {
        return Err(unsupported(format!(
            "figure representation {:?} is not supported for slides; supported: {}",
            link.representation,
            FIGURE_REPRESENTATIONS.join(", ")
        )));
    }
    if link.figure_id.trim().is_empty() {
        return Err(invalid("a figure link needs a figure id"));
    }
    let (asset, bytes) = read_render(&link.render_hash, assets)?;
    let (pixel_width, pixel_height) = png_pixel_size(&bytes)
        .ok_or_else(|| invalid(format!("render {} is not a readable PNG", link.render_hash)))?;
    let height = scaled_height(state.pages[page].nodes[index].bounds.width, pixel_width, pixel_height)?;
    let media = designllm_pptx::allocate_media(package, &asset)?;

    let mut map = shape_map(&state.mappings, &link.target_node_id)?;
    map.media_part = Some(media);
    state.mappings.insert(link.target_node_id.clone(), to_value(&map)?);

    let node = &mut state.pages[page].nodes[index];
    node.asset = Some(asset);
    node.bounds.height = height;

    state.figure_links.retain(|existing| existing.target_node_id != link.target_node_id);
    state.figure_links.push(link.clone());
    state.figure_links.sort_by(|a, b| a.target_node_id.cmp(&b.target_node_id));
    changed.insert(link.target_node_id.clone());
    Ok(())
}

/// Reads the PNG that a figure link names. The store must hold it as `image/png`, its bytes must hash to
/// `render_hash`, and the bytes must start with the PNG signature.
fn read_render(render_hash: &str, assets: &dyn AssetStore) -> DltResult<(AssetRef, Vec<u8>)> {
    if !is_lowercase_sha256(render_hash) {
        return Err(invalid(format!("render hash {render_hash:?} is not a lowercase SHA-256 hex digest")));
    }
    let asset = assets.lookup(render_hash)?;
    if asset.mime != "image/png" {
        return Err(invalid(format!("render {render_hash} is stored as {}; a png figure link needs image/png", asset.mime)));
    }
    let bytes = assets.read(&asset)?;
    if sha256(&bytes) != render_hash || bytes.len() as u64 != asset.bytes {
        return Err(DltError::new(ErrorCode::IntegrityError, format!("render {render_hash} does not match its stored bytes")));
    }
    if !magic_matches("image/png", &bytes) {
        return Err(invalid(format!("render {render_hash} does not start with the PNG signature")));
    }
    Ok((asset, bytes))
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The shared PNG rule (`media_header::png_size`), and both sizes at most 2^31 - 1 as the PNG format allows.
fn png_pixel_size(bytes: &[u8]) -> Option<(u32, u32)> {
    media_header::png_size(bytes).filter(|(width, height)| *width <= 0x7FFF_FFFF && *height <= 0x7FFF_FFFF)
}

/// `width` scaled by `pixel_height / pixel_width`, rounded half up to whole EMU (`media_header::aspect_height`).
/// A height that is not positive is refused.
fn scaled_height(width: i64, pixel_width: u32, pixel_height: u32) -> DltResult<i64> {
    media_header::aspect_height(width, pixel_width, pixel_height)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid("the picture height for this figure is not a positive length"))
}

fn insert_page(
    state: &mut SlideState,
    index: u32,
    page: &Page,
    package: &Package,
    assets: &dyn AssetStore,
    changed: &mut BTreeSet<Id>,
    diagnostics: &mut Vec<Diagnostic>,
) -> DltResult<()> {
    let at = index as usize;
    if at > state.pages.len() {
        return Err(invalid(format!("insert index {index} is beyond the last slide")));
    }
    if ids_in_use(&state.pages).contains(&page.id) {
        return Err(DltError::new(ErrorCode::AlreadyExists, format!("id {} is already in use", page.id)));
    }
    if page.size != state.slide_size {
        return Err(unsupported("a new slide must have the presentation's slide size"));
    }
    let notes = normalize_notes(&page.notes)?;
    let allocation = designllm_pptx::allocate_page(package, &state.mappings, !notes.is_empty())?;
    let record = PageMap {
        kind: designllm_pptx::model::KIND_PAGE.to_string(),
        origin: designllm_pptx::ORIGIN_ADDED.to_string(),
        part: allocation.part,
        sld_id: allocation.sld_id,
        rel_id: String::new(),
        notes_part: allocation.notes_part,
        max_shape_id: 1,
    };
    state.mappings.insert(page.id.clone(), to_value(&record)?);
    // A page name is refused before this point (`guard::check_operations`), so `name` is always empty here. Nothing is
    // dropped: the name is not rebuilt.
    state.pages.insert(
        at,
        Page { notes: notes.clone(), ..Page::new(page.id.clone(), page.size) },
    );
    changed.insert(page.id.clone());
    for node in &page.nodes {
        add_node(state, &page.id, node, package, assets, changed, diagnostics)?;
    }
    Ok(())
}

fn remove_page(state: &mut SlideState, page_id: &str, package: &Package, changed: &mut BTreeSet<Id>) -> DltResult<()> {
    let index = page_index(state, page_id)?;
    if state.pages.len() == 1 {
        return Err(invalid("a presentation keeps at least one slide"));
    }
    let record = page_map(&state.mappings, page_id)?;
    if record.origin == designllm_pptx::ORIGIN_SOURCE {
        for name in package.names() {
            let Some(owner) = name.strip_prefix("ppt/slides/_rels/").and_then(|n| n.strip_suffix(".rels")) else {
                continue;
            };
            let owner_part = format!("ppt/slides/{owner}");
            if owner_part == record.part {
                continue;
            }
            let rels = opc::parse_rels(package.part(&name)?)?;
            for rel in rels.iter().filter(|r| !r.external) {
                if opc::resolve_target(&owner_part, &rel.target)? == record.part {
                    return Err(unsupported(format!(
                        "slide {page_id} is linked from {owner_part}; remove that link before removing the slide"
                    )));
                }
            }
        }
    }
    let removed = state.pages.remove(index);
    for node in &removed.nodes {
        state.mappings.remove(&node.id);
        changed.insert(node.id.clone());
    }
    state.figure_links.retain(|link| !removed.nodes.iter().any(|node| node.id == link.target_node_id));
    state.mappings.remove(page_id);
    state.opaque.retain(|r| r.part != record.part && Some(&r.part) != record.notes_part.as_ref());
    changed.insert(page_id.to_string());
    Ok(())
}

fn reorder_pages(state: &mut SlideState, page_ids: &[Id], changed: &mut BTreeSet<Id>) -> DltResult<()> {
    let current: BTreeSet<&str> = state.pages.iter().map(|p| p.id.as_str()).collect();
    let requested: BTreeSet<&str> = page_ids.iter().map(String::as_str).collect();
    if page_ids.len() != state.pages.len() || requested.len() != page_ids.len() || requested != current {
        return Err(invalid("reorder_pages must list every slide exactly once"));
    }
    let mut by_id: BTreeMap<String, Page> = state.pages.drain(..).map(|p| (p.id.clone(), p)).collect();
    state.pages = page_ids.iter().map(|id| by_id.remove(id).expect("checked above")).collect();
    changed.extend(page_ids.iter().cloned());
    Ok(())
}

/// Checks the invariants that every saved state must satisfy. A violation is a model bug, not a user error.
pub(crate) fn check_model(state: &SlideState) -> DltResult<()> {
    let mut page_ids = BTreeSet::new();
    let mut node_ids = BTreeSet::new();
    let mut sld_ids = BTreeSet::new();
    for page in &state.pages {
        if !page_ids.insert(page.id.clone()) {
            return Err(DltError::new(ErrorCode::IntegrityError, format!("page id {} is duplicated", page.id)));
        }
        let record = page_map(&state.mappings, &page.id)?;
        if !sld_ids.insert(record.sld_id) {
            return Err(DltError::new(ErrorCode::IntegrityError, format!("slide id {} is duplicated", record.sld_id)));
        }
        let mut cnvprs = BTreeSet::new();
        for node in &page.nodes {
            if !node_ids.insert(node.id.clone()) {
                return Err(DltError::new(ErrorCode::IntegrityError, format!("node id {} is duplicated", node.id)));
            }
            let map = shape_map(&state.mappings, &node.id)?;
            if !cnvprs.insert(map.cnvpr_id) {
                return Err(DltError::new(ErrorCode::IntegrityError, format!("shape id {} is duplicated on a slide", map.cnvpr_id)));
            }
            if map.cnvpr_id > record.max_shape_id {
                return Err(DltError::new(ErrorCode::IntegrityError, "a shape id is above the slide's recorded maximum"));
            }
            if node.kind == NodeKind::Image && node.asset.is_none() {
                return Err(DltError::new(ErrorCode::IntegrityError, format!("picture {} has no asset", node.id)));
            }
            RichText::from_parts(&node.text, &map.paragraphs)?;
        }
    }
    for link in &state.figure_links {
        let linked = find_node(&state.pages, &link.target_node_id).is_some_and(|(_, node)| {
            node.kind == NodeKind::Image && node.asset.as_ref().is_some_and(|asset| asset.sha256 == link.render_hash)
        });
        if !linked {
            return Err(DltError::new(
                ErrorCode::IntegrityError,
                format!("figure link for {} does not match a picture of that render", link.target_node_id),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod media_header_tests {
    use super::*;

    /// Task 49e input set: signature, IHDR chunk (25 bytes) and IEND chunk; the IHDR CRC is not read.
    fn png_file(width: u32, ihdr_length: u32) -> Vec<u8> {
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        out.extend_from_slice(&ihdr_length.to_be_bytes());
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]);
        out
    }

    /// The PNG gate of `update_figure_link` on the input set, plus the slide-only size limit.
    #[test]
    fn png_input_set_acceptance_of_the_figure_link_gate() {
        let normal = png_file(4, 13);
        let mut no_signature = normal.clone();
        no_signature[..8].fill(0);
        let no_iend = normal[..normal.len() - 12].to_vec();
        let inputs = [("P1 normal", normal), ("P2 no signature", no_signature), ("P3 IHDR length 14", png_file(4, 14)), ("P4 zero width", png_file(0, 13)), ("P5 no IEND", no_iend)];
        let table: Vec<_> = inputs.iter().map(|(name, bytes)| (*name, png_pixel_size(bytes).is_some())).collect();
        println!("49e slides UpdateFigureLink PNG gate accepts: {table:?}");
        assert_eq!(table, [("P1 normal", true), ("P2 no signature", false), ("P3 IHDR length 14", false), ("P4 zero width", false), ("P5 no IEND", true)]);
        assert_eq!(png_pixel_size(&png_file(0x7FFF_FFFF, 13)), Some((0x7FFF_FFFF, 2)));
        assert_eq!(png_pixel_size(&png_file(0x8000_0000, 13)), None, "slides keep the PNG limit of 2^31 - 1 pixels");
    }
}
