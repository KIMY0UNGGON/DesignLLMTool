//! Structural validation and lookups for canvas documents.
//! Every check returns a blocking diagnostic on failure, so nothing invalid can be committed.

use super::guard;
use crate::common::{blocking, valid_color, valid_id, valid_key, MAX_EXTENT_EMU, MAX_NODES, MAX_PAGES, MAX_TEXT_BYTES};
use designllm_contracts::{
    check_interaction_url, layout_guides::{calculate_page_guides, GuideError, MAX_GUIDE_COUNT}, CanvasBoardMemo, CanvasBoardMemoKind, CanvasDocument, CanvasPageGuides, CanvasTable, Diagnostic, DltError, DltResult, ErrorCode, Interaction, LayoutSpec, Node, NodeExt,
    NodeKind, PathCommand, PathSpec, Rect, Size, Style, StylePatch, EMU_PER_CSS_PX, EMU_PER_PT, MAX_BREAKPOINTS, PATH_MAX_COMMANDS, PATH_MAX_COORDINATE,
};

use std::collections::{BTreeMap, BTreeSet};

pub(crate) const MAX_DEPTH: usize = 16;
// Largest grid column count (defined in contracts) and largest grid span.
pub(crate) use designllm_contracts::MAX_GRID_COLUMNS;
pub(crate) const MAX_GRID_SPAN: u32 = 24;
/// Longest layer name and page name, in bytes.
pub(crate) const MAX_NODE_NAME_BYTES: usize = 256;
/// Longest alternative text, in bytes.
pub(crate) const MAX_ALT_BYTES: usize = 4096;
// Keep aligned with desktop app::kind_commands::canvas_table_cell.
const DEFAULT_CANVAS_TABLE_CELL_HEIGHT_EMU: i64 = 36 * EMU_PER_CSS_PX;
const MAX_FONT_SIZE_EMU: i64 = 1_000 * EMU_PER_PT;
const MAX_STROKE_EMU: i64 = 100 * EMU_PER_PT;
const MAX_FONT_FAMILY_CHARS: usize = 128;

pub(crate) fn fail_with(code: ErrorCode, diagnostic: &str, message: impl Into<String>, node_id: Option<&str>) -> DltError {
    let message = message.into();
    let mut error = DltError::new(code, message.clone());
    error.diagnostics.push(blocking(diagnostic, message, node_id.map(str::to_string)));
    if let Some(id) = node_id {
        error.affected_ids.push(id.to_string());
    }
    error
}

pub(crate) fn any_blocking(diagnostics: &[Diagnostic]) -> bool {
    diagnostics.iter().any(|d| d.blocking)
}

/// Converts a list of diagnostics into one error when any of them is blocking.
pub(crate) fn into_result(diagnostics: Vec<Diagnostic>) -> DltResult<Vec<Diagnostic>> {
    if let Some(first) = diagnostics.iter().find(|d| d.blocking).cloned() {
        let mut error = DltError::new(ErrorCode::InvalidArgument, first.message.clone());
        error.affected_ids = diagnostics.iter().filter(|d| d.blocking).filter_map(|d| d.node_id.clone()).collect();
        error.diagnostics = diagnostics;
        return Err(error);
    }
    Ok(diagnostics)
}

/// Bounds rule: positions in +-10000 mm, sizes positive up to 10000 mm. Lines may have zero width or height
/// (not both zero).
pub(crate) fn validate_bounds(bounds: &Rect, kind: NodeKind) -> DltResult<()> {
    let within = |v: i64| v.abs() <= MAX_EXTENT_EMU;
    if !within(bounds.x) || !within(bounds.y) {
        return Err(invalid_bounds("position is outside the allowed range"));
    }
    let (min_w, min_h) = if kind == NodeKind::Line { (0, 0) } else { (1, 1) };
    if bounds.width < min_w || bounds.height < min_h || bounds.width > MAX_EXTENT_EMU || bounds.height > MAX_EXTENT_EMU {
        return Err(invalid_bounds("size is outside the allowed range"));
    }
    if kind == NodeKind::Line && bounds.width == 0 && bounds.height == 0 {
        return Err(invalid_bounds("a line needs a non-zero extent"));
    }
    Ok(())
}

fn invalid_bounds(message: &str) -> DltError {
    fail_with(ErrorCode::InvalidArgument, "GEOMETRY_RANGE", message, None)
}

pub(crate) fn validate_style(style: &Style) -> DltResult<()> {
    let family = style.font_family.trim();
    if family.is_empty() || family.chars().count() > MAX_FONT_FAMILY_CHARS || style.font_family.chars().any(|c| c.is_control()) {
        return Err(fail_with(ErrorCode::InvalidArgument, "STYLE_FONT_FAMILY", "font family is empty or invalid", None));
    }
    if style.font_size <= 0 || style.font_size > MAX_FONT_SIZE_EMU {
        return Err(fail_with(ErrorCode::InvalidArgument, "STYLE_FONT_SIZE", "font size must be between 0 and 1000 pt", None));
    }
    if style.font_weight.is_some_and(|weight| !(100..=900).contains(&weight)) {
        return Err(fail_with(ErrorCode::InvalidArgument, "STYLE_FONT_WEIGHT", "font weight must be between 100 and 900", None));
    }
    if style.stroke_width < 0 || style.stroke_width > MAX_STROKE_EMU {
        return Err(fail_with(ErrorCode::InvalidArgument, "STYLE_STROKE_WIDTH", "stroke width must be between 0 and 100 pt", None));
    }
    for color in [&style.color, &style.fill, &style.stroke] {
        if !valid_color(color) {
            return Err(fail_with(ErrorCode::InvalidArgument, "STYLE_COLOR", format!("invalid color {color}"), None));
        }
    }
    if !matches!(style.align.as_str(), "left" | "center" | "right" | "justify") {
        return Err(fail_with(ErrorCode::InvalidArgument, "STYLE_ALIGN", "align must be left, center, right, or justify", None));
    }
    if style.opacity_milli.is_some_and(|m| m > 1000) {
        return Err(fail_with(ErrorCode::InvalidArgument, "STYLE_OPACITY", "opacity must be between 0 and 1000", None));
    }
    if style.corner_radius.is_some_and(|r| !(0..=MAX_EXTENT_EMU).contains(&r)) {
        return Err(fail_with(ErrorCode::InvalidArgument, "STYLE_CORNER_RADIUS", "corner radius must be between 0 and 10000 mm", None));
    }
    if style.line_height_milli.is_some_and(|m| !(100..=5000).contains(&m)) {
        return Err(fail_with(ErrorCode::InvalidArgument, "STYLE_LINE_HEIGHT", "line height must be between 0.1 and 5 times the font size", None));
    }
    if style.letter_spacing.is_some_and(|s| s.abs() > MAX_FONT_SIZE_EMU) {
        return Err(fail_with(ErrorCode::InvalidArgument, "STYLE_LETTER_SPACING", "letter spacing must be within 1000 pt", None));
    }
    for shadow in &style.shadows {
        if shadow.blur < 0 || shadow.spread < 0 || !valid_color(&shadow.color) {
            return Err(fail_with(ErrorCode::InvalidArgument, "STYLE_SHADOW", "shadow blur and spread must not be negative and its colour must be valid", None));
        }
    }
    Ok(())
}

pub(crate) fn validate_patch(patch: &StylePatch) -> DltResult<()> {
    guard::check_patch(patch)?;
    let mut probe = Style::default();
    patch.apply(&mut probe);
    validate_style(&probe)
}

/// A leaf (not a group or rectangle) takes only sizing, limits, `align_self`, and `grid_span`.
fn leaf_only(spec: &LayoutSpec) -> bool {
    spec.mode == "absolute"
        && spec.gap == 0
        && spec.padding == 0
        && spec.align == "start"
        && spec.justify.is_none()
        && spec.padding_sides.is_none()
        && spec.grid_columns.is_none()
        && spec.row_gap.is_none()
        && spec.breakpoints.is_empty()
}

pub(crate) fn validate_layout(spec: &LayoutSpec, kind: NodeKind) -> DltResult<()> {
    if kind == NodeKind::Line {
        return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_TARGET", "lines take no layout", None));
    }
    if !matches!(kind, NodeKind::Group | NodeKind::Rectangle) && !leaf_only(spec) {
        return Err(fail_with(
            ErrorCode::InvalidArgument,
            "LAYOUT_LEAF",
            "this node takes only width and height modes, min and max, align_self, and grid_span",
            None,
        ));
    }
    if !matches!(spec.mode.as_str(), "absolute" | "row" | "column" | "grid") {
        return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_MODE", "mode must be absolute, row, column, or grid", None));
    }
    for mode in [&spec.width_mode, &spec.height_mode] {
        if !matches!(mode.as_str(), "fixed" | "fill" | "hug") {
            return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_SIZE_MODE", "size mode must be fixed, fill, or hug", None));
        }
    }
    let grid = std::iter::once(spec.mode.as_str())
        .chain(spec.breakpoints.iter().filter_map(|b| b.mode.as_deref()))
        .any(|mode| mode == "grid");
    let grid_columns_given = spec.grid_columns.is_some() || spec.breakpoints.iter().any(|b| b.grid_columns.is_some());
    let justify_given = spec.justify.is_some() || spec.breakpoints.iter().any(|b| b.justify.is_some());
    if grid {
        if spec.width_mode == "hug" {
            return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_GRID_WIDTH", "a grid takes a fixed or fill width", None));
        }
        if !grid_columns_given {
            return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_GRID_COLUMNS", "a grid needs grid_columns", None));
        }
        if justify_given {
            return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_JUSTIFY_MODE", "justify applies to row and column layouts only", None));
        }
    }
    let bad_columns = |c: u32| c == 0 || c > MAX_GRID_COLUMNS;
    if spec.grid_columns.is_some_and(bad_columns) || spec.breakpoints.iter().any(|b| b.grid_columns.is_some_and(bad_columns)) {
        return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_GRID_COLUMNS", "grid_columns must be between 1 and 24", None));
    }
    if spec.grid_span.is_some_and(|s| s == 0 || s > MAX_GRID_SPAN) {
        return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_GRID_SPAN", "grid_span must be between 1 and 24", None));
    }
    let in_range = |v: &i64| (0..=MAX_EXTENT_EMU).contains(v);
    if spec.row_gap.as_ref().is_some_and(|g| !in_range(g)) || spec.padding_sides.is_some_and(|sides| !sides.iter().all(in_range)) {
        return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_SPACING", "row gap and padding sides must be between 0 and 10000 mm", None));
    }
    if !matches!(spec.align.as_str(), "start" | "center" | "end" | "stretch") {
        return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_ALIGN", "align must be start, center, end, or stretch", None));
    }
    if spec.gap < 0 || spec.padding < 0 || spec.gap > MAX_EXTENT_EMU || spec.padding > MAX_EXTENT_EMU {
        return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_SPACING", "gap and padding must be between 0 and 10000 mm", None));
    }
    for (name, min, max) in [
        ("width", spec.min_width, spec.max_width),
        ("height", spec.min_height, spec.max_height),
    ] {
        if min.is_some_and(|v| v < 0) || max.is_some_and(|v| v < 0) {
            return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_MIN_MAX", format!("{name} limits must not be negative"), None));
        }
        if let (Some(lo), Some(hi)) = (min, max) {
            if lo > hi {
                return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_MIN_MAX", format!("min {name} exceeds max {name}"), None));
            }
        }
    }
    if spec.breakpoints.len() > MAX_BREAKPOINTS {
        return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_BREAKPOINTS", "too many breakpoints", None));
    }
    let mut previous: Option<u32> = None;
    for bp in &spec.breakpoints {
        if previous.is_some_and(|p| bp.min_width_css_px <= p) {
            return Err(fail_with(
                ErrorCode::InvalidArgument,
                "LAYOUT_BREAKPOINTS",
                "breakpoints must have strictly increasing min_width_css_px",
                None,
            ));
        }
        previous = Some(bp.min_width_css_px);
        if let Some(mode) = &bp.mode {
            if !matches!(mode.as_str(), "absolute" | "row" | "column" | "grid") {
                return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_MODE", "breakpoint mode is invalid", None));
            }
        }
        if let Some(align) = &bp.align {
            if !matches!(align.as_str(), "start" | "center" | "end" | "stretch") {
                return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_ALIGN", "breakpoint align is invalid", None));
            }
        }
        if bp.gap.is_some_and(|g| g < 0) || bp.padding.is_some_and(|p| p < 0) {
            return Err(fail_with(ErrorCode::InvalidArgument, "LAYOUT_SPACING", "breakpoint spacing must not be negative", None));
        }
    }
    Ok(())
}

/// A vector path: a positive view box within the coordinate limit, between 1 and `PATH_MAX_COMMANDS` commands, a leading
/// move, and coordinates in range. The limits are the parser's (`PathSpec::from_svg`), so a parsed path always passes.
pub(crate) fn validate_path(path: &PathSpec) -> Result<(), &'static str> {
    let view_box = path.view_box;
    if view_box.width <= 0 || view_box.height <= 0 {
        return Err("path view box must have a positive size");
    }
    // A range test, not `abs()`: `i64::MIN.abs()` overflows.
    let within = |v: i64| (-PATH_MAX_COORDINATE..=PATH_MAX_COORDINATE).contains(&v);
    if ![view_box.x, view_box.y, view_box.width, view_box.height].into_iter().all(within) {
        return Err("path view box is out of range");
    }
    if path.commands.is_empty() || path.commands.len() > PATH_MAX_COMMANDS {
        return Err("path must have between 1 and 4096 commands");
    }
    if !matches!(path.commands[0], PathCommand::MoveTo { .. }) {
        return Err("path must start with a move command");
    }
    for command in &path.commands {
        let ok = match command {
            PathCommand::MoveTo { x, y } | PathCommand::LineTo { x, y } => within(*x) && within(*y),
            PathCommand::CubicTo { x1, y1, x2, y2, x, y } => [x1, y1, x2, y2, x, y].iter().all(|v| within(**v)),
            PathCommand::Close => true,
        };
        if !ok {
            return Err("path coordinates are out of range");
        }
    }
    Ok(())
}

/// Problem with a short user text (layer name, page name, alternative text), or None when it is fine.
pub(crate) fn info_text_problem(text: &str, max_bytes: usize) -> Option<&'static str> {
    if text.len() > max_bytes {
        return Some("is too long");
    }
    if text.chars().any(|c| c.is_control()) {
        return Some("contains control characters");
    }
    None
}

pub(crate) fn validate_page_size(size: Size) -> DltResult<()> {
    if !size.valid() {
        return Err(fail_with(ErrorCode::InvalidArgument, "PAGE_SIZE", "page size must be positive and at most 10000 mm", None));
    }
    Ok(())
}

fn board_memo_text_problem(text: &str) -> Option<&'static str> {
    if text.len() > MAX_TEXT_BYTES {
        return Some("is too long");
    }
    if text.chars().any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t')) {
        return Some("contains unsupported control characters");
    }
    None
}

fn validate_board_memo(memo: &CanvasBoardMemo, page_ids: &BTreeSet<&str>, out: &mut Vec<Diagnostic>) {
    if !page_ids.contains(memo.page_id.as_str()) {
        out.push(blocking("BOARD_MEMO_PAGE", "board memo refers to a missing Canvas page", Some(memo.id.clone())));
    }
    let bounds = &memo.bounds;
    if bounds.x < -MAX_EXTENT_EMU || bounds.x > MAX_EXTENT_EMU
        || bounds.y < -MAX_EXTENT_EMU || bounds.y > MAX_EXTENT_EMU
        || bounds.width < 1 || bounds.width > MAX_EXTENT_EMU
        || bounds.height < 1 || bounds.height > MAX_EXTENT_EMU
    {
        out.push(blocking("BOARD_MEMO_BOUNDS", "board memo bounds are outside the signed page-local EMU range", Some(memo.id.clone())));
    }
    if memo.color.len() != 7 || !valid_color(&memo.color) {
        out.push(blocking("BOARD_MEMO_COLOR", "board memo color must be an opaque #RRGGBB color", Some(memo.id.clone())));
    }
    if let Some(problem) = board_memo_text_problem(&memo.text) {
        out.push(blocking("BOARD_MEMO_TEXT", format!("board memo text {problem}"), Some(memo.id.clone())));
    }
}

fn validate_page_sections(doc: &CanvasDocument, out: &mut Vec<Diagnostic>) {
    if doc.page_sections.len() > MAX_PAGES {
        out.push(blocking("PAGE_SECTIONS_TOO_MANY", "canvas has too many page sections", None));
    }
    let page_indices: BTreeMap<&str, usize> = doc.pages.iter().enumerate()
        .map(|(index, page)| (page.id.as_str(), index)).collect();
    let mut start_pages = BTreeSet::new();
    let mut previous_section_id: Option<&str> = None;
    for section in &doc.page_sections {
        if !valid_id(&section.id) {
            out.push(blocking("PAGE_SECTION_ID", "page section identifier is invalid", Some(section.id.clone())));
        }
        if section.name.is_empty() {
            out.push(blocking("PAGE_SECTION_NAME", "page section name cannot be empty", Some(section.id.clone())));
        } else if let Some(problem) = info_text_problem(&section.name, MAX_NODE_NAME_BYTES) {
            out.push(blocking("PAGE_SECTION_NAME", format!("page section name {problem}"), Some(section.id.clone())));
        }
        let Some(page_index) = page_indices.get(section.start_page_id.as_str()).copied() else {
            out.push(blocking("PAGE_SECTION_START", "page section start refers to a missing page", Some(section.id.clone())));
            continue;
        };
        if doc.pages[page_index].role == Some(designllm_contracts::PageRole::Component) {
            out.push(blocking("PAGE_SECTION_COMPONENT_START", "a component page cannot start a section", Some(section.id.clone())));
        }
        if !start_pages.insert(section.start_page_id.as_str()) {
            out.push(blocking("PAGE_SECTION_DUPLICATE", "only one page section may start at a page", Some(section.id.clone())));
        }
        if previous_section_id.is_some_and(|previous| section.id.as_str() <= previous) {
            out.push(blocking("PAGE_SECTION_ORDER", "page sections must be stored in section ID order", Some(section.id.clone())));
        }
        previous_section_id = Some(section.id.as_str());
    }
}

fn validate_page_guides(doc: &CanvasDocument, out: &mut Vec<Diagnostic>) {
    if doc.layout_guides.len() > MAX_PAGES {
        out.push(blocking("CANVAS_GUIDES_TOO_MANY", "canvas has too many page guide settings", None));
    }
    for (page_id, guides) in &doc.layout_guides {
        if !valid_id(page_id) {
            out.push(blocking("CANVAS_GUIDES_PAGE_ID", "page guide key is not a valid page id", Some(page_id.clone())));
            continue;
        }
        let Some(page) = doc.pages.iter().find(|page| page.id == *page_id) else {
            out.push(blocking("CANVAS_GUIDES_PAGE", "page guides refer to a missing Canvas page", Some(page_id.clone())));
            continue;
        };
        if let Some((code, message)) = page_guides_problem(page.size, guides) {
            out.push(blocking(code, message, Some(page_id.clone())));
        }
    }
}

pub(crate) fn page_guides_problem(size: Size, guides: &CanvasPageGuides) -> Option<(&'static str, &'static str)> {
    let axes = [guides.columns, guides.rows];
    if axes.iter().all(Option::is_none) {
        return Some(("CANVAS_GUIDES_EMPTY", "page guide settings must define columns, rows, or both"));
    }
    for axis in axes.into_iter().flatten() {
        if axis.count == 0 || axis.count > MAX_GUIDE_COUNT {
            return Some(("CANVAS_GUIDES_COUNT", "guide count must be between 1 and 24"));
        }
        if axis.spacing_emu < 0 {
            return Some(("CANVAS_GUIDES_SPACING", "guide spacing must be nonnegative EMU"));
        }
        if axis.leading_margin_emu < 0 || axis.trailing_margin_emu < 0 {
            return Some(("CANVAS_GUIDES_MARGIN", "guide margins must be nonnegative EMU"));
        }
    }
    calculate_page_guides(size.width, size.height, guides.columns, guides.rows)
        .err()
        .map(page_guide_error_detail)
}

pub(crate) fn page_guide_error_detail(error: GuideError) -> (&'static str, &'static str) {
    match error {
        GuideError::InvalidPageSize => ("CANVAS_GUIDES_PAGE_SIZE", "guide page size must be positive"),
        GuideError::NoAxes => ("CANVAS_GUIDES_EMPTY", "page guide settings must define columns, rows, or both"),
        GuideError::CountOutOfRange => ("CANVAS_GUIDES_COUNT", "guide count must be between 1 and 24"),
        GuideError::NegativeSpacingOrMargin => ("CANVAS_GUIDES_VALUE", "guide spacing and margins must be nonnegative EMU"),
        GuideError::NoTrackSpace => ("CANVAS_GUIDES_AREA", "guide spacing and margins must leave at least 1 EMU for each track"),
        GuideError::Overflow => ("CANVAS_GUIDES_OVERFLOW", "guide geometry exceeds the supported EMU range"),
        GuideError::InvalidRect | GuideError::InvalidViewScale => ("CANVAS_GUIDES_GEOMETRY", "guide geometry is invalid"),
    }
}

/// Every page, node, table-row, board-memo, and section ID in document order. Used for uniqueness checks.
pub(crate) fn all_ids(doc: &CanvasDocument) -> Vec<String> {
    let mut out = Vec::new();
    for page in &doc.pages {
        out.push(page.id.clone());
        walk_ids(&page.nodes, &mut out);
    }
    out.extend(doc.board_memos.iter().map(|memo| memo.id.clone()));
    out.extend(doc.page_sections.iter().map(|section| section.id.clone()));
    out
}

fn walk_ids(nodes: &[Node], out: &mut Vec<String>) {
    for node in nodes {
        out.push(node.id.clone());
        if let Some(table) = &node.ext.table {
            out.extend(table.rows.iter().map(|row| row.id.clone()));
        }
        walk_ids(&node.children, out);
    }
}

pub(crate) fn find_node<'a>(nodes: &'a [Node], id: &str) -> Option<&'a Node> {
    for node in nodes {
        if node.id == id {
            return Some(node);
        }
        if let Some(found) = find_node(&node.children, id) {
            return Some(found);
        }
    }
    None
}

pub(crate) fn find_node_mut<'a>(nodes: &'a mut [Node], id: &str) -> Option<&'a mut Node> {
    for node in nodes.iter_mut() {
        if node.id == id {
            return Some(node);
        }
        if let Some(found) = find_node_mut(&mut node.children, id) {
            return Some(found);
        }
    }
    None
}

pub(crate) fn find_node_in_doc<'a>(doc: &'a CanvasDocument, id: &str) -> Option<&'a Node> {
    doc.pages.iter().find_map(|page| find_node(&page.nodes, id))
}

pub(crate) fn find_node_in_doc_mut<'a>(doc: &'a mut CanvasDocument, id: &str) -> Option<&'a mut Node> {
    doc.pages.iter_mut().find_map(|page| find_node_mut(&mut page.nodes, id))
}

pub(crate) fn subtree_ids(node: &Node, out: &mut BTreeSet<String>) {
    out.insert(node.id.clone());
    if let Some(table) = &node.ext.table {
        out.extend(table.rows.iter().map(|row| row.id.clone()));
    }
    for child in &node.children {
        subtree_ids(child, out);
    }
}

/// Checks one node and its subtree. Duplicate IDs across the whole document are checked by the caller.
pub(crate) fn validate_node(node: &Node, depth: usize, out: &mut Vec<Diagnostic>) {
    let id = Some(node.id.as_str());
    if !valid_id(&node.id) {
        out.push(blocking("NODE_ID", "node id is empty or contains invalid characters", None));
        return;
    }
    if depth > MAX_DEPTH {
        out.push(blocking("NODE_DEPTH", "node tree is too deep", id.map(str::to_string)));
        return;
    }
    if node.interactions.iter().filter(|interaction| matches!(interaction, Interaction::OpenUrl { .. })).count() > 1 {
        out.push(blocking("INTERACTION_OPEN_URL_MULTIPLE", "a node may have at most one OpenUrl interaction", id.map(str::to_string)));
    }
    if let Err(err) = validate_bounds(&node.bounds, node.kind) {
        out.push(blocking("GEOMETRY_RANGE", err.message.clone(), id.map(str::to_string)));
    }
    if let Err(err) = validate_style(&node.style) {
        out.push(blocking("STYLE", err.message.clone(), id.map(str::to_string)));
    }
    if node.text.len() > MAX_TEXT_BYTES {
        out.push(blocking("TEXT_TOO_LONG", "node text exceeds the size limit", id.map(str::to_string)));
    }
    if !node.text.is_empty() && !matches!(node.kind, NodeKind::Text | NodeKind::Rectangle | NodeKind::Ellipse) {
        out.push(blocking("NODE_TEXT_UNSUPPORTED", "only text and shape nodes can hold text", id.map(str::to_string)));
    }
    match (&node.kind, &node.asset) {
        (NodeKind::Image, None) => out.push(blocking("IMAGE_ASSET", "image node has no asset", id.map(str::to_string))),
        (NodeKind::Image, Some(asset)) if !asset.mime.starts_with("image/") => {
            out.push(blocking("IMAGE_MIME", "asset is not an image", id.map(str::to_string)))
        }
        (NodeKind::Video, None) => out.push(blocking("VIDEO_ASSET", "video node has no source asset", id.map(str::to_string))),
        (NodeKind::Video, Some(asset)) if asset.bytes == 0 || !matches!(asset.mime.as_str(), "video/mp4" | "video/webm") => {
            out.push(blocking("VIDEO_MIME", "video source must be a non-empty MP4 or WebM asset", id.map(str::to_string)))
        }
        (kind, Some(_)) if !matches!(kind, NodeKind::Image | NodeKind::Video) => {
            out.push(blocking("NODE_ASSET_UNSUPPORTED", "only image and video nodes reference assets", id.map(str::to_string)))
        }
        _ => {}
    }
    if !node.children.is_empty() && !matches!(node.kind, NodeKind::Group | NodeKind::Rectangle) {
        out.push(blocking("NODE_CHILDREN", "only group and rectangle nodes can have children", id.map(str::to_string)));
    }
    if let Some(layout) = &node.layout {
        if let Err(err) = validate_layout(layout, node.kind) {
            out.push(blocking("LAYOUT", err.message.clone(), id.map(str::to_string)));
        }
    }
    match (&node.kind, &node.ext.path) {
        (NodeKind::Path, None) => out.push(blocking("NODE_PATH", "path node has no path", id.map(str::to_string))),
        (kind, Some(_)) if *kind != NodeKind::Path => {
            out.push(blocking("NODE_PATH", "only path nodes take a path", id.map(str::to_string)))
        }
        (NodeKind::Path, Some(path)) => {
            if let Err(message) = validate_path(path) {
                out.push(blocking("NODE_PATH", message, id.map(str::to_string)));
            }
        }
        _ => {}
    }
    if node.ext.icon.is_some() && node.kind != NodeKind::Path {
        out.push(blocking("NODE_ICON", "only path nodes take an icon", id.map(str::to_string)));
    }
    if node.ext.image_fit.is_some() && node.kind != NodeKind::Image {
        out.push(blocking("NODE_IMAGE_FIT", "only image nodes take an image fit", id.map(str::to_string)));
    }
    match (node.kind, &node.ext.video_poster_asset) {
        (NodeKind::Video, None) => out.push(blocking("VIDEO_POSTER_MISSING", "video node requires its automatically extracted PNG first-frame poster", id.map(str::to_string))),
        (NodeKind::Video, Some(poster)) if poster.mime != "image/png" || poster.bytes == 0 => {
            out.push(blocking("VIDEO_POSTER_ASSET", "video poster must be a non-empty PNG asset", id.map(str::to_string)))
        }
        (kind, Some(_)) if kind != NodeKind::Video => {
            out.push(blocking("VIDEO_POSTER_NODE_KIND", "only video nodes carry a first-frame poster asset", id.map(str::to_string)))
        }
        _ => {}
    }
    if node.kind == NodeKind::Video && !node.interactions.is_empty() {
        out.push(blocking("VIDEO_INTERACTIONS_UNSUPPORTED", "video player controls cannot share a Canvas node interaction", id.map(str::to_string)));
    }
    if node.kind == NodeKind::Video && !node.variants.is_empty() {
        out.push(blocking("VIDEO_VARIANTS_UNSUPPORTED", "video nodes do not support interaction variants", id.map(str::to_string)));
    }
    if let Some(crop) = node.ext.image_crop_pixel_rect {
        if node.kind != NodeKind::Image {
            out.push(blocking("NODE_IMAGE_CROP", "only image nodes take an image crop", id.map(str::to_string)));
        }
        if crop.width == 0 || crop.height == 0
            || crop.x.checked_add(crop.width).is_none()
            || crop.y.checked_add(crop.height).is_none()
        {
            out.push(blocking("IMAGE_CROP_RECT", "image crop must have positive dimensions and in-range source-pixel edges", id.map(str::to_string)));
        }
    }
    if node.ext.list.is_some() && node.kind != NodeKind::Text {
        out.push(blocking("NODE_LIST_KIND", "only text nodes take a list kind", id.map(str::to_string)));
    }
    if let Some(table) = &node.ext.table {
        validate_table(node, table, out);
    }
    if let Some(name) = &node.ext.name {
        if let Some(problem) = info_text_problem(name, MAX_NODE_NAME_BYTES) {
            out.push(blocking("NODE_NAME", format!("layer name {problem}"), id.map(str::to_string)));
        }
    }
    if let Some(alt) = &node.ext.alt {
        if let Some(problem) = info_text_problem(alt, MAX_ALT_BYTES) {
            out.push(blocking("NODE_ALT", format!("alternative text {problem}"), id.map(str::to_string)));
        }
    }
    for (key, patch) in &node.variants {
        if !valid_key(key) {
            out.push(blocking("VARIANT_KEY", "variant name is invalid", id.map(str::to_string)));
        }
        if validate_patch(patch).is_err() {
            out.push(blocking("VARIANT_STYLE", "variant style is invalid", id.map(str::to_string)));
        }
    }
    for child in &node.children {
        validate_node(child, depth + 1, out);
    }
}

fn validate_table(node: &Node, table: &CanvasTable, out: &mut Vec<Diagnostic>) {
    let id = Some(node.id.clone());
    if node.kind != NodeKind::Group {
        out.push(blocking("TABLE_NODE_KIND", "Canvas table metadata requires a Group node", id.clone()));
    }
    let Some(first_row) = table.rows.first() else {
        out.push(blocking("TABLE_ROWS", "a Canvas table requires at least one row", id.clone()));
        return;
    };
    if table.rows.len() > MAX_NODES {
        out.push(blocking("TABLE_ROWS", "Canvas table has too many rows", id.clone()));
    }
    let columns = first_row.cell_ids.len();
    if columns == 0 || columns > MAX_GRID_COLUMNS as usize {
        out.push(blocking("TABLE_COLUMNS", "Canvas table columns must be between 1 and 24", id.clone()));
    }
    if table.column_widths.as_ref().is_some_and(|widths| widths.len() != columns || widths.iter().any(|weight| *weight == 0)) {
        out.push(blocking("TABLE_COLUMN_WIDTHS", "custom Canvas column widths must have one positive weight per column", id.clone()));
    }
    if table.header_row_id.as_deref().is_some_and(|header| header != first_row.id) {
        out.push(blocking("TABLE_HEADER_ROW", "only the first row can be the header row", id.clone()));
    }
    let mut referenced = BTreeSet::new();
    let mut expected_cell_ids = Vec::new();
    for row in &table.rows {
        if !valid_id(&row.id) {
            out.push(blocking("TABLE_ROW_ID", "table row id is invalid", id.clone()));
        }
        if row.cell_ids.len() != columns {
            out.push(blocking("TABLE_ROW_COLUMNS", "all Canvas table rows must have the same number of cells", id.clone()));
        }
        for cell_id in &row.cell_ids {
            if !valid_id(cell_id) {
                out.push(blocking("TABLE_CELL_ID", "table cell id is invalid", id.clone()));
            }
            if !referenced.insert(cell_id.as_str()) {
                out.push(blocking("TABLE_CELL_DUPLICATE", format!("cell {cell_id} is used more than once in the table"), Some(cell_id.clone())));
            }
            expected_cell_ids.push(cell_id.as_str());
        }
    }
    let layout_matches = node.layout.as_ref().is_some_and(|layout| {
        layout.mode == "grid"
            && layout.grid_columns == Some(columns as u32)
            && layout.gap == 0
            && layout.row_gap.unwrap_or(layout.gap) == 0
            && layout.breakpoints.iter().all(|breakpoint| {
                breakpoint.mode.as_deref().is_none_or(|mode| mode == "grid")
                    && breakpoint.grid_columns.is_none_or(|count| count == columns as u32)
                    && breakpoint.gap.is_none_or(|gap| gap == 0)
            })
    });
    if !layout_matches {
        out.push(blocking("TABLE_LAYOUT", "Canvas table Group must use a fixed-column, zero-gap grid", id.clone()));
    }
    let child_ids_match = expected_cell_ids.len() == node.children.len()
        && expected_cell_ids.iter().zip(&node.children).all(|(expected, child)| *expected == child.id);
    if !child_ids_match {
        out.push(blocking("TABLE_CELLS", "table rows must list every direct cell child in row-major order", id.clone()));
    }
    let span_grid = match table_span_grid(table) {
        Ok(grid) => Some(grid),
        Err(problem) => {
            out.push(blocking("TABLE_CELL_SPANS", problem, id.clone()));
            None
        }
    };
    for child in &node.children {
        if !referenced.contains(child.id.as_str()) {
            continue;
        }
        if child.kind != NodeKind::Rectangle || !child.children.is_empty() {
            out.push(blocking("TABLE_CELL_NODE", "Canvas table cells must be leaf Rectangle nodes", Some(child.id.clone())));
        }
        if child.ext.hidden == Some(true) {
            out.push(blocking("TABLE_CELL_HIDDEN", "Canvas table cells cannot be hidden", Some(child.id.clone())));
        }
        let cell_layout_matches = child.layout.as_ref().is_some_and(|layout| {
            layout.width_mode == "fill"
                && layout.height_mode == "hug"
                && layout.min_width.is_none()
                && layout.max_width.is_none()
                && layout.min_height.is_none()
                && layout.max_height.is_none()
                && layout.align_self.is_none_or(|align| align == designllm_contracts::AlignSelf::Auto)
                && layout.grid_span.is_none()
        });
        if !cell_layout_matches {
            out.push(blocking("TABLE_CELL_LAYOUT", "Canvas table cells must fill their column and hug their row content", Some(child.id.clone())));
        }
    }
    if child_ids_match {
        if let Some(grid) = span_grid {
            for (index, position) in grid.cells.iter().enumerate() {
                let TableCellPosition::Covered { anchor_index } = position else { continue };
                let covered = &node.children[index];
                let anchor = &node.children[*anchor_index];
                if covered.style != anchor.style || covered.asset.is_some() || !covered.children.is_empty()
                    || !covered.interactions.is_empty() || !covered.variants.is_empty() || covered.ext != NodeExt::default()
                {
                    out.push(blocking(
                        "TABLE_SPAN_HIDDEN_DATA",
                        "a merged table span can cover plain text only when cell styling and non-text data are identical",
                        Some(covered.id.clone()),
                    ));
                }
                if covered.layout.as_ref().is_none_or(|layout| !default_table_cell_layout(layout)) {
                    out.push(blocking(
                        "TABLE_SPAN_HIDDEN_LAYOUT",
                        "a covered table cell must keep the layout defaults used by Canvas table creation",
                        Some(covered.id.clone()),
                    ));
                }
                // Table placement owns each cell's x/y/width. For empty Rectangle cells, hug height is still measured
                // from bounds.height, so retain the intrinsic height used by the table creator.
                if covered.bounds.height != DEFAULT_CANVAS_TABLE_CELL_HEIGHT_EMU {
                    out.push(blocking(
                        "TABLE_SPAN_HIDDEN_GEOMETRY",
                        "a covered table cell cannot carry a custom intrinsic height that would affect its row",
                        Some(covered.id.clone()),
                    ));
                }
            }
        }
    }
}

fn default_table_cell_layout(layout: &LayoutSpec) -> bool {
    layout.mode == "absolute"
        && layout.width_mode == "fill"
        && layout.height_mode == "hug"
        && layout.gap == 0
        && layout.padding == 0
        && layout.align == "start"
        && layout.min_width.is_none()
        && layout.max_width.is_none()
        && layout.min_height.is_none()
        && layout.max_height.is_none()
        && layout.breakpoints.is_empty()
        && layout.justify.is_none()
        && layout.padding_sides.is_none()
        && layout.grid_columns.is_none()
        && layout.row_gap.is_none()
        && layout.align_self.is_none_or(|align| align == designllm_contracts::AlignSelf::Auto)
        && layout.grid_span.is_none()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TableCellPosition {
    Anchor { row_span: usize, column_span: usize },
    Covered { anchor_index: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableSpanGrid {
    pub columns: usize,
    /// Row-major cell positions. An unmerged cell is an Anchor with 1 × 1 span.
    pub cells: Vec<TableCellPosition>,
}

/// Resolves the sparse anchor map into a dense table grid. Invalid spans never reach layout or export.
pub(crate) fn table_span_grid(table: &CanvasTable) -> Result<TableSpanGrid, &'static str> {
    let rows = table.rows.len();
    let columns = table.rows.first().map_or(0, |row| row.cell_ids.len());
    let count = rows.checked_mul(columns).ok_or("table cell count overflows")?;
    if rows == 0 || columns == 0 || table.rows.iter().any(|row| row.cell_ids.len() != columns) {
        return Err("table spans require a non-empty rectangular cell grid");
    }
    let mut indices = BTreeMap::new();
    for (row_index, row) in table.rows.iter().enumerate() {
        for (column_index, cell_id) in row.cell_ids.iter().enumerate() {
            if indices.insert(cell_id.as_str(), row_index * columns + column_index).is_some() {
                return Err("table span cell IDs must be unique");
            }
        }
    }
    let mut cells = vec![None; count];
    for (cell_id, span) in &table.cell_spans {
        let Some(&anchor_index) = indices.get(cell_id.as_str()) else {
            return Err("table span anchor does not refer to a cell in this table");
        };
        let row = anchor_index / columns;
        let column = anchor_index % columns;
        let row_span = usize::try_from(span.row_span).map_err(|_| "table row span is out of range")?;
        let column_span = usize::try_from(span.column_span).map_err(|_| "table column span is out of range")?;
        if row_span == 0 || column_span == 0 || (row_span == 1 && column_span == 1) {
            return Err("merged table spans must cover more than one cell");
        }
        let row_end = row.checked_add(row_span).ok_or("table row span overflows")?;
        let column_end = column.checked_add(column_span).ok_or("table column span overflows")?;
        if row_end > rows || column_end > columns {
            return Err("table span extends beyond the cell grid");
        }
        if row == 0 && table.header_row_id.as_ref() == table.rows.first().map(|first| &first.id) && row_span > 1 {
            return Err("a header cell cannot span into the table body");
        }
        for covered_row in row..row_end {
            for covered_column in column..column_end {
                let index = covered_row * columns + covered_column;
                if cells[index].is_some() {
                    return Err("table spans overlap");
                }
                cells[index] = Some(if index == anchor_index {
                    TableCellPosition::Anchor { row_span, column_span }
                } else {
                    TableCellPosition::Covered { anchor_index }
                });
            }
        }
    }
    Ok(TableSpanGrid { columns, cells: cells.into_iter().map(|position| position.unwrap_or(TableCellPosition::Anchor { row_span: 1, column_span: 1 })).collect() })
}

pub(crate) fn covered_table_cell_ids(table: &CanvasTable) -> Result<BTreeSet<&str>, &'static str> {
    let grid = table_span_grid(table)?;
    Ok(grid.cells.iter().enumerate().filter_map(|(index, position)| {
        matches!(position, TableCellPosition::Covered { .. })
            .then(|| table.rows[index / grid.columns].cell_ids[index % grid.columns].as_str())
    }).collect())
}

/// Interaction targets must exist. SetVariant also needs the variant on the target node.
pub(crate) fn validate_interaction(doc: &CanvasDocument, interaction: &Interaction) -> DltResult<()> {
    match interaction {
        Interaction::OpenUrl { url } => check_interaction_url(url)?,
        Interaction::Back => {}
        Interaction::NavigatePage { page_id } => {
            if !doc.pages.iter().any(|p| &p.id == page_id) {
                return Err(fail_with(ErrorCode::NotFound, "INTERACTION_TARGET", format!("page {page_id} does not exist"), None));
            }
        }
        Interaction::ToggleVisibility { node_id } => {
            if find_node_in_doc(doc, node_id).is_none() {
                return Err(fail_with(ErrorCode::NotFound, "INTERACTION_TARGET", format!("node {node_id} does not exist"), Some(node_id)));
            }
        }
        Interaction::SetVariant { node_id, variant } => {
            let Some(target) = find_node_in_doc(doc, node_id) else {
                return Err(fail_with(ErrorCode::NotFound, "INTERACTION_TARGET", format!("node {node_id} does not exist"), Some(node_id)));
            };
            if !target.variants.contains_key(variant) {
                return Err(fail_with(
                    ErrorCode::InvalidArgument,
                    "INTERACTION_VARIANT",
                    format!("node {node_id} has no variant {variant}"),
                    Some(node_id),
                ));
            }
        }
    }
    Ok(())
}

/// Full structural check of a document. Returns all findings. Blocking findings mean the document is invalid.
pub(crate) fn validate_document(doc: &CanvasDocument) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    validate_page_sections(doc, &mut out);
    validate_page_guides(doc, &mut out);
    if doc.pages.is_empty() {
        out.push(blocking("CANVAS_NO_PAGES", "canvas has no pages", None));
        out.extend(super::tokens::validate_document(doc));
        return out;
    }
    if doc.pages.len() > MAX_PAGES {
        out.push(blocking("CANVAS_TOO_MANY_PAGES", "canvas has too many pages", None));
    }
    let ids = all_ids(doc);
    let mut seen = BTreeSet::new();
    for id in &ids {
        if !valid_id(id) {
            out.push(blocking("NODE_ID", "identifier is empty or contains invalid characters", None));
        }
        if !seen.insert(id.clone()) {
            out.push(blocking("DUPLICATE_ID", format!("identifier {id} is used more than once"), Some(id.clone())));
        }
    }
    if ids.len() > MAX_NODES + MAX_PAGES {
        out.push(blocking("CANVAS_TOO_MANY_NODES", "canvas has too many nodes", None));
    }
    let page_ids: BTreeSet<&str> = doc.pages.iter().map(|page| page.id.as_str()).collect();
    for page in &doc.pages {
        if let Err(err) = validate_page_size(page.size) {
            out.push(blocking("PAGE_SIZE", err.message.clone(), Some(page.id.clone())));
        }
        if page.notes.len() > MAX_TEXT_BYTES {
            out.push(blocking("PAGE_NOTES_TOO_LONG", "page notes exceed the size limit", Some(page.id.clone())));
        }
        if let Some(name) = &page.name {
            if let Some(problem) = info_text_problem(name, MAX_NODE_NAME_BYTES) {
                out.push(blocking("PAGE_NAME", format!("page name {problem}"), Some(page.id.clone())));
            }
        }
        for node in &page.nodes {
            validate_node(node, 0, &mut out);
        }
    }
    for memo in &doc.board_memos {
        validate_board_memo(memo, &page_ids, &mut out);
    }
    let mut title_pages = BTreeSet::new();
    for memo in doc.board_memos.iter().filter(|memo| memo.kind == CanvasBoardMemoKind::Title) {
        if !title_pages.insert(memo.page_id.as_str()) {
            out.push(blocking("BOARD_TITLE_MULTIPLE", "a page may have only one board title", Some(memo.id.clone())));
        }
    }
    for (name, style) in &doc.styles {
        if !valid_key(name) || validate_style(style).is_err() {
            out.push(blocking("NAMED_STYLE", "named style is invalid", None));
        }
    }
    // Interactions are checked after all IDs exist. NavigatePage targets are pages, so they are covered here too.
    let all_nodes: Vec<&Node> = doc.pages.iter().flat_map(|p| flatten(&p.nodes)).collect();
    for node in &all_nodes {
        for interaction in &node.interactions {
            if let Err(err) = validate_interaction(doc, interaction) {
                out.push(blocking("INTERACTION_TARGET", err.message.clone(), Some(node.id.clone())));
            }
        }
    }
    let mut covered_cells = BTreeSet::new();
    for node in &all_nodes {
        let Some(table) = &node.ext.table else { continue };
        if let Ok(grid) = table_span_grid(table) {
            for (index, position) in grid.cells.iter().enumerate() {
                if matches!(position, TableCellPosition::Covered { .. }) {
                    covered_cells.insert(table.rows[index / grid.columns].cell_ids[index % grid.columns].as_str());
                }
            }
        }
    }
    for binding in &doc.bindings {
        if covered_cells.contains(binding.node_id.as_str()) {
            out.push(blocking("TABLE_SPAN_TOKEN_BINDING", "a covered table cell cannot retain a design token binding", Some(binding.node_id.clone())));
        }
    }
    for node in &all_nodes {
        for interaction in &node.interactions {
            let target = match interaction {
                Interaction::ToggleVisibility { node_id } | Interaction::SetVariant { node_id, .. } => Some(node_id.as_str()),
                _ => None,
            };
            if target.is_some_and(|target| covered_cells.contains(target)) {
                out.push(blocking(
                    "TABLE_SPAN_INTERACTION_TARGET",
                    "an interaction cannot target a covered table cell",
                    Some(node.id.clone()),
                ));
            }
        }
    }
    out.extend(super::tokens::validate_document(doc));
    out
}

pub(crate) fn flatten(nodes: &[Node]) -> Vec<&Node> {
    let mut out = Vec::new();
    for node in nodes {
        out.push(node);
        out.extend(flatten(&node.children));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::CanvasAxisGuides;
    use designllm_contracts::{AssetRef, Breakpoint, CanvasPageSection, CanvasTableRow, CanvasTableSpan, NodeExt, Page};

    /// Every recipe of the shape library that MCP clients are told to use is a path Canvas accepts.
    #[test]
    fn the_shape_library_recipes_are_valid_canvas_paths() {
        for recipe in designllm_contracts::shape_library::SHAPES {
            assert_eq!(validate_path(&recipe.path()), Ok(()), "{}", recipe.id);
        }
    }

    fn video_node() -> Node {
        Node {
            id: "video".into(), kind: NodeKind::Video, bounds: Rect { x: 0, y: 0, width: 300, height: 200 },
            text: String::new(), style: Style::default(), asset: Some(AssetRef { sha256: "a".repeat(64), mime: "video/mp4".into(), bytes: 64 }),
            children: Vec::new(), layout: None, interactions: Vec::new(), variants: Default::default(),
            ext: NodeExt { video_poster_asset: Some(AssetRef { sha256: "b".repeat(64), mime: "image/png".into(), bytes: 32 }), ..Default::default() },
        }
    }

    #[test]
    fn video_node_requires_video_source_png_poster_and_has_no_player_interactions() {
        let node = video_node();
        let document = CanvasDocument {
            pages: vec![Page { nodes: vec![node.clone()], ..Page::new("page", Size::slide()) }],
            ..Default::default()
        };
        assert!(!any_blocking(&validate_document(&document)), "source/poster shape is structurally valid");

        let mut missing_poster = document.clone();
        missing_poster.pages[0].nodes[0].ext.video_poster_asset = None;
        assert!(validate_document(&missing_poster).iter().any(|diagnostic| diagnostic.code == "VIDEO_POSTER_MISSING"));

        let mut wrong_source = document.clone();
        wrong_source.pages[0].nodes[0].asset.as_mut().unwrap().mime = "application/octet-stream".into();
        assert!(validate_document(&wrong_source).iter().any(|diagnostic| diagnostic.code == "VIDEO_MIME"));

        let mut linked = document;
        linked.pages[0].nodes[0].interactions.push(Interaction::OpenUrl { url: "https://example.test".into() });
        assert!(validate_document(&linked).iter().any(|diagnostic| diagnostic.code == "VIDEO_INTERACTIONS_UNSUPPORTED"));
    }

    #[test]
    fn breakpoints_must_increase() {
        let mut spec = LayoutSpec::default();
        spec.breakpoints = vec![
            Breakpoint { min_width_css_px: 800, mode: None, gap: None, padding: None, align: None, ..Default::default() },
            Breakpoint { min_width_css_px: 600, mode: None, gap: None, padding: None, align: None, ..Default::default() },
        ];
        assert!(validate_layout(&spec, NodeKind::Group).is_err());
    }

    #[test]
    fn bounds_reject_negative_size_and_allow_negative_position() {
        let ok = Rect { x: -1_000, y: -1_000, width: 1_000, height: 1_000 };
        assert!(validate_bounds(&ok, NodeKind::Rectangle).is_ok());
        let bad = Rect { x: 0, y: 0, width: -1, height: 1 };
        assert!(validate_bounds(&bad, NodeKind::Rectangle).is_err());
        let zero_line = Rect { x: 0, y: 0, width: 0, height: 0 };
        assert!(validate_bounds(&zero_line, NodeKind::Line).is_err());
    }

    #[test]
    fn canvas_board_memos_validate_page_signed_bounds_text_and_color() {
        let mut document = CanvasDocument {
            pages: vec![Page { notes: "kept separately".into(), ..Page::new("p", Size::slide()) }],
            ..Default::default()
        };
        let memo = CanvasBoardMemo {
            kind: CanvasBoardMemoKind::Memo,
            id: "memo-1".into(),
            page_id: "p".into(),
            bounds: Rect { x: -20_000, y: 30_000, width: 240_000, height: 120_000 },
            text: "first line\nsecond line".into(),
            color: "#FFF2CC".into(),
        };
        document.board_memos.push(memo.clone());
        assert!(validate_document(&document).iter().all(|diagnostic| !diagnostic.blocking));
        assert!(all_ids(&document).contains(&"memo-1".to_owned()));

        let mut title = memo.clone();
        title.kind = CanvasBoardMemoKind::Title;
        title.id = "title-1".into();
        title.color = "#223344".into();
        document.board_memos.push(title.clone());
        assert!(validate_document(&document).iter().all(|diagnostic| !diagnostic.blocking));
        let mut duplicate_title = document.clone();
        duplicate_title.board_memos.push(CanvasBoardMemo { id: "title-2".into(), ..title });
        assert!(validate_document(&duplicate_title).iter().any(|diagnostic| diagnostic.code == "BOARD_TITLE_MULTIPLE"));

        let mut duplicate_id = document.clone();
        duplicate_id.board_memos[0].id = "p".into();
        assert!(validate_document(&duplicate_id).iter().any(|diagnostic| diagnostic.code == "DUPLICATE_ID"));

        let mut missing_page = document.clone();
        missing_page.board_memos[0].page_id = "missing".into();
        assert!(validate_document(&missing_page).iter().any(|diagnostic| diagnostic.code == "BOARD_MEMO_PAGE"));

        let mut invalid_bounds = document.clone();
        invalid_bounds.board_memos[0].bounds.x = i64::MIN;
        assert!(validate_document(&invalid_bounds).iter().any(|diagnostic| diagnostic.code == "BOARD_MEMO_BOUNDS"));

        for color in ["transparent", "#FFF2CC00", "#FFF2CC80"] {
            let mut invalid_color = document.clone();
            invalid_color.board_memos[0].color = color.into();
            assert!(validate_document(&invalid_color).iter().any(|diagnostic| diagnostic.code == "BOARD_MEMO_COLOR"), "{color} is not an opaque memo color");
        }

        let mut invalid_text = document;
        invalid_text.board_memos[0].text.push('\u{0000}');
        assert!(validate_document(&invalid_text).iter().any(|diagnostic| diagnostic.code == "BOARD_MEMO_TEXT"));
    }

    #[test]
    fn canvas_page_sections_validate_starts_names_uniqueness_and_canonical_order() {
        let pages = vec![
            Page::new("p1", Size::slide()),
            Page::new("p2", Size::slide()),
            Page::new("p3", Size::slide()),
        ];
        let section_a = CanvasPageSection { id: "s1".into(), name: "Part 1".into(), start_page_id: "p1".into() };
        let section_b = CanvasPageSection { id: "s2".into(), name: "Part 2".into(), start_page_id: "p3".into() };
        let mut document = CanvasDocument { pages, page_sections: vec![section_a.clone(), section_b.clone()], ..Default::default() };
        assert!(validate_document(&document).iter().all(|diagnostic| !diagnostic.blocking));
        assert!(all_ids(&document).contains(&"s1".to_owned()));

        document.page_sections[1].start_page_id = "missing".into();
        assert!(validate_document(&document).iter().any(|diagnostic| diagnostic.code == "PAGE_SECTION_START"));

        document.page_sections = vec![section_a.clone(), CanvasPageSection { start_page_id: "p1".into(), ..section_b.clone() }];
        assert!(validate_document(&document).iter().any(|diagnostic| diagnostic.code == "PAGE_SECTION_DUPLICATE"));

        document.page_sections = vec![section_b.clone(), section_a.clone()];
        assert!(validate_document(&document).iter().any(|diagnostic| diagnostic.code == "PAGE_SECTION_ORDER"));

        document.page_sections = vec![CanvasPageSection { name: String::new(), ..section_a.clone() }];
        assert!(validate_document(&document).iter().any(|diagnostic| diagnostic.code == "PAGE_SECTION_NAME"));

        document.pages[0].role = Some(designllm_contracts::PageRole::Component);
        document.page_sections = vec![section_a];
        assert!(validate_document(&document).iter().any(|diagnostic| diagnostic.code == "PAGE_SECTION_COMPONENT_START"));
    }

    fn table_document() -> CanvasDocument {
        let cell = |id: &str| Node {
            id: id.into(), kind: NodeKind::Rectangle, bounds: Rect { x: 0, y: 0, width: 120 * EMU_PER_CSS_PX, height: 36 * EMU_PER_CSS_PX },
            text: String::new(), style: Style::default(), asset: None, children: vec![],
            layout: Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..LayoutSpec::default() }),
            interactions: vec![], variants: Default::default(), ext: NodeExt::default(),
        };
        let table = Node {
            id: "table".into(), kind: NodeKind::Group, bounds: Rect { x: 0, y: 0, width: 400, height: 200 },
            text: String::new(), style: Style::default(), asset: None,
            children: vec![cell("c11"), cell("c12"), cell("c21"), cell("c22")],
            layout: Some(LayoutSpec { mode: "grid".into(), grid_columns: Some(2), ..LayoutSpec::default() }),
            interactions: vec![], variants: Default::default(),
            ext: NodeExt {
                table: Some(CanvasTable {
                    rows: vec![
                        CanvasTableRow { id: "r1".into(), cell_ids: vec!["c11".into(), "c12".into()] },
                        CanvasTableRow { id: "r2".into(), cell_ids: vec!["c21".into(), "c22".into()] },
                    ],
                    header_row_id: Some("r1".into()),
                    column_widths: None,
                    cell_spans: Default::default(),
                }),
                ..NodeExt::default()
            },
        };
        CanvasDocument {
            pages: vec![Page {
                nodes: vec![table],
                ..Page::new("p", designllm_contracts::Size::slide())
            }],
            ..CanvasDocument::default()
        }
    }

    #[test]
    fn canvas_table_requires_a_group_grid_and_matching_row_major_cell_nodes() {
        assert!(validate_document(&table_document()).iter().all(|diagnostic| !diagnostic.blocking));

        let mut wrong_columns = table_document();
        if let Some(table) = &mut wrong_columns.pages[0].nodes[0].ext.table {
            table.rows[1].cell_ids.pop();
        }
        assert!(validate_document(&wrong_columns).iter().any(|diagnostic| diagnostic.code == "TABLE_ROW_COLUMNS"));

        let mut wrong_order = table_document();
        wrong_order.pages[0].nodes[0].children.swap(0, 1);
        assert!(validate_document(&wrong_order).iter().any(|diagnostic| diagnostic.code == "TABLE_CELLS"));

        let mut wrong_grid = table_document();
        wrong_grid.pages[0].nodes[0].layout.as_mut().unwrap().grid_columns = Some(3);
        assert!(validate_document(&wrong_grid).iter().any(|diagnostic| diagnostic.code == "TABLE_LAYOUT"));

        let mut fixed_cell = table_document();
        fixed_cell.pages[0].nodes[0].children[0].layout.as_mut().unwrap().width_mode = "fixed".into();
        assert!(validate_document(&fixed_cell).iter().any(|diagnostic| diagnostic.code == "TABLE_CELL_LAYOUT"));
    }

    #[test]
    fn canvas_table_row_ids_are_unique_and_header_is_first() {
        let mut duplicate = table_document();
        duplicate.pages[0].nodes[0].ext.table.as_mut().unwrap().rows[0].id = "c11".into();
        assert!(validate_document(&duplicate).iter().any(|diagnostic| diagnostic.code == "DUPLICATE_ID"));

        let mut header = table_document();
        header.pages[0].nodes[0].ext.table.as_mut().unwrap().header_row_id = Some("r2".into());
        assert!(validate_document(&header).iter().any(|diagnostic| diagnostic.code == "TABLE_HEADER_ROW"));
    }

    #[test]
    fn merged_table_spans_are_rectangular_non_overlapping_and_keep_cell_nodes() {
        let mut merged = table_document();
        let table = merged.pages[0].nodes[0].ext.table.as_mut().unwrap();
        table.header_row_id = None;
        table.cell_spans.insert("c11".into(), CanvasTableSpan { row_span: 2, column_span: 2 });
        let grid = table_span_grid(table).expect("a rectangular span resolves to a dense cell grid");
        assert_eq!(grid.cells[0], TableCellPosition::Anchor { row_span: 2, column_span: 2 });
        assert_eq!(grid.cells[1], TableCellPosition::Covered { anchor_index: 0 });
        assert_eq!(grid.cells[2], TableCellPosition::Covered { anchor_index: 0 });
        assert_eq!(grid.cells[3], TableCellPosition::Covered { anchor_index: 0 });
        assert!(!validate_document(&merged).iter().any(|diagnostic| diagnostic.blocking), "a valid span map passes model validation");

        let mut out_of_bounds = table_document();
        out_of_bounds.pages[0].nodes[0].ext.table.as_mut().unwrap().cell_spans
            .insert("c12".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        assert!(validate_document(&out_of_bounds).iter().any(|diagnostic| diagnostic.code == "TABLE_CELL_SPANS"));

        let mut overlap = table_document();
        let table = overlap.pages[0].nodes[0].ext.table.as_mut().unwrap();
        table.header_row_id = None;
        table.cell_spans.insert("c11".into(), CanvasTableSpan { row_span: 2, column_span: 2 });
        table.cell_spans.insert("c22".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        assert!(validate_document(&overlap).iter().any(|diagnostic| diagnostic.code == "TABLE_CELL_SPANS"));
    }

    #[test]
    fn merged_table_spans_allow_plain_covered_text_but_reject_different_styles_and_header_crossing() {
        let mut text = table_document();
        text.pages[0].nodes[0].ext.table.as_mut().unwrap().cell_spans
            .insert("c11".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        text.pages[0].nodes[0].children[1].text = "keep me".into();
        assert!(validate_document(&text).iter().all(|diagnostic| !diagnostic.blocking), "covered plain text with the anchor style remains valid");

        let mut style = table_document();
        style.pages[0].nodes[0].ext.table.as_mut().unwrap().cell_spans
            .insert("c11".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        style.pages[0].nodes[0].children[1].style.fill = "#AABBCC".into();
        assert!(validate_document(&style).iter().any(|diagnostic| diagnostic.code == "TABLE_SPAN_HIDDEN_DATA"));

        let mut header_crossing = table_document();
        header_crossing.pages[0].nodes[0].ext.table.as_mut().unwrap().cell_spans
            .insert("c11".into(), CanvasTableSpan { row_span: 2, column_span: 1 });
        assert!(validate_document(&header_crossing).iter().any(|diagnostic| diagnostic.code == "TABLE_CELL_SPANS"));
    }

    #[test]
    fn merged_table_spans_reject_covered_layout_and_intrinsic_geometry_but_allow_track_bounds() {
        let mut padded = table_document();
        padded.pages[0].nodes[0].ext.table.as_mut().unwrap().cell_spans
            .insert("c11".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        padded.pages[0].nodes[0].children[1].layout.as_mut().unwrap().padding = 12 * EMU_PER_CSS_PX;
        assert!(validate_document(&padded).iter().any(|diagnostic| diagnostic.code == "TABLE_SPAN_HIDDEN_LAYOUT"));

        let mut gapped = table_document();
        gapped.pages[0].nodes[0].ext.table.as_mut().unwrap().cell_spans
            .insert("c11".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        gapped.pages[0].nodes[0].children[1].layout.as_mut().unwrap().gap = EMU_PER_CSS_PX;
        assert!(validate_document(&gapped).iter().any(|diagnostic| diagnostic.code == "TABLE_SPAN_HIDDEN_LAYOUT"));

        let mut aligned = table_document();
        aligned.pages[0].nodes[0].ext.table.as_mut().unwrap().cell_spans
            .insert("c11".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        aligned.pages[0].nodes[0].children[1].layout.as_mut().unwrap().align = "center".into();
        assert!(validate_document(&aligned).iter().any(|diagnostic| diagnostic.code == "TABLE_SPAN_HIDDEN_LAYOUT"));

        let mut breakpoint = table_document();
        breakpoint.pages[0].nodes[0].ext.table.as_mut().unwrap().cell_spans
            .insert("c11".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        breakpoint.pages[0].nodes[0].children[1].layout.as_mut().unwrap().breakpoints.push(Breakpoint {
            min_width_css_px: 600, padding: Some(8 * EMU_PER_CSS_PX), ..Breakpoint::default()
        });
        assert!(validate_document(&breakpoint).iter().any(|diagnostic| diagnostic.code == "TABLE_SPAN_HIDDEN_LAYOUT"));

        let mut custom_height = table_document();
        custom_height.pages[0].nodes[0].ext.table.as_mut().unwrap().cell_spans
            .insert("c11".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        custom_height.pages[0].nodes[0].children[1].bounds.height += EMU_PER_CSS_PX;
        assert!(validate_document(&custom_height).iter().any(|diagnostic| diagnostic.code == "TABLE_SPAN_HIDDEN_GEOMETRY"));

        let mut track_position = table_document();
        track_position.pages[0].nodes[0].ext.table.as_mut().unwrap().cell_spans
            .insert("c11".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        track_position.pages[0].nodes[0].children[1].bounds = Rect {
            x: 120 * EMU_PER_CSS_PX, y: 36 * EMU_PER_CSS_PX, width: 90 * EMU_PER_CSS_PX,
            height: DEFAULT_CANVAS_TABLE_CELL_HEIGHT_EMU,
        };
        let diagnostics = validate_document(&track_position);
        assert!(!diagnostics.iter().any(|diagnostic| matches!(diagnostic.code.as_str(), "TABLE_SPAN_HIDDEN_LAYOUT" | "TABLE_SPAN_HIDDEN_GEOMETRY")));
    }

    #[test]
    fn covered_cells_cannot_keep_document_token_bindings() {
        use designllm_contracts::{BindTarget, TokenBinding, TokenRef, TokenSource};

        let mut document = table_document();
        document.pages[0].nodes[0].ext.table.as_mut().unwrap().cell_spans
            .insert("c11".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        document.bindings.push(TokenBinding {
            node_id: "c12".into(),
            target: BindTarget::StyleFill,
            token: TokenRef { source: TokenSource::Local, token_id: "brand".into() },
        });
        assert!(validate_document(&document).iter().any(|diagnostic| diagnostic.code == "TABLE_SPAN_TOKEN_BINDING"));
    }

    #[test]
    fn interactions_cannot_target_a_covered_table_cell() {
        let mut document = table_document();
        document.pages[0].nodes[0].ext.table.as_mut().unwrap().cell_spans
            .insert("c11".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        document.pages[0].nodes.push(Node {
            id: "trigger".into(), kind: NodeKind::Rectangle, bounds: Rect { x: 0, y: 0, width: 10, height: 10 },
            text: String::new(), style: Style::default(), asset: None, children: Vec::new(), layout: None,
            interactions: vec![Interaction::ToggleVisibility { node_id: "c12".into() }], variants: Default::default(), ext: NodeExt::default(),
        });
        assert!(validate_document(&document).iter().any(|diagnostic| diagnostic.code == "TABLE_SPAN_INTERACTION_TARGET"));
    }

    #[test]
    fn canvas_page_guides_validate_axes_page_keys_and_track_area() {
        let page = Page::new("p1", Size { width: 100, height: 80 });
        let columns = CanvasAxisGuides { count: 3, spacing_emu: 1, leading_margin_emu: 1, trailing_margin_emu: 2 };
        let valid = CanvasPageGuides { columns: Some(columns), rows: None };
        let mut document = CanvasDocument {
            pages: vec![page],
            layout_guides: BTreeMap::from([("p1".into(), valid.clone())]),
            ..Default::default()
        };
        assert!(validate_document(&document).iter().all(|diagnostic| !diagnostic.blocking));
        let layout = calculate_page_guides(100, 80, valid.columns, valid.rows).expect("positive tracks fit inside the page");
        assert_eq!(layout.columns.unwrap().lines_emu, vec![1, 33, 34, 65, 66, 98]);

        document.layout_guides.insert("missing".into(), valid.clone());
        assert!(validate_document(&document).iter().any(|diagnostic| diagnostic.code == "CANVAS_GUIDES_PAGE"));
        document.layout_guides.remove("missing");

        document.layout_guides.get_mut("p1").unwrap().columns.as_mut().unwrap().count = 0;
        assert!(validate_document(&document).iter().any(|diagnostic| diagnostic.code == "CANVAS_GUIDES_COUNT"));
        document.layout_guides.get_mut("p1").unwrap().columns = None;
        assert!(validate_document(&document).iter().any(|diagnostic| diagnostic.code == "CANVAS_GUIDES_EMPTY"));
        document.layout_guides.get_mut("p1").unwrap().columns = Some(CanvasAxisGuides { count: 2, spacing_emu: 99, leading_margin_emu: 1, trailing_margin_emu: 1 });
        assert!(validate_document(&document).iter().any(|diagnostic| diagnostic.code == "CANVAS_GUIDES_AREA"));
    }
}
