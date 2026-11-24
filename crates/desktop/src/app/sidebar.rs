//! The left sidebar's two tabs (GUI_DESIGN §4, §10). The layers tab is one tree: pages at the top
//! level, nodes nested under them. The AI tab lists the request and its progress, and holds the
//! request box.

use designllm_contracts::{ArtifactBody, ArtifactKind, FigureRevisionRef, FlowBlock, FlowDocument, JobStatus, Node, NodeKind, Paragraph, RenderPage, RenderProfile, RenderRequest, RenderSnapshot, Selection, TextRange, MAX_JOB_FIGURE_REFS};
use designllm_flow::{LogicalParagraph, OBJECT_REPLACEMENT};
use eframe::egui::{self, Align2, CornerRadius, Pos2, Rect, Sense, Ui, Vec2};
use serde_json::{json, Value};

use super::{chrome::agent_letter, state::FlowFindMatch, DesktopApp, FlowFindCursor, FlowFindLookup, FlowFindPageRequest, FlowFindSearch, PendingAction};
use crate::{icons, theme, widgets};

/// The AI tab's label of a job whose request this window does not know (`job_prompts`).
pub(super) const JOB_LABEL: &str = "AI 작업";

/// Whether a name has no visible text: blank, or only whitespace and zero-width characters (U+200B..=U+200D, U+FEFF).
fn is_blank_name(name: &str) -> bool {
    name.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\u{200B}'..='\u{200D}' | '\u{FEFF}')).is_empty()
}

/// The undo row's actor name in two parts: the part that may be cut when the row is narrow, and the id suffix that stays
/// visible. A job shows its first 8 characters of id. A connection shows its client name (`mcp:<conn>:<client>`, not
/// blank, "agent" or "desktop") with the first 4 characters of its id, or with 8 characters of its id when it has no
/// client name. The command line shows its kind, anything else as it is. An empty id or name is never shown as a blank
/// gap. The row's hover text is the whole actor.
pub(super) fn agent_actor_parts(actor: &str) -> (String, String) {
    let mut parts = actor.splitn(3, ':');
    let short = |id: &str, n: usize| id.char_indices().nth(n).map_or(id, |(end, _)| &id[..end]).to_owned();
    match (parts.next(), parts.next(), parts.next()) {
        (Some("job"), Some(id), _) if !id.is_empty() => (format!("{JOB_LABEL} {}", short(id, 8)), String::new()),
        (Some("mcp"), Some(id), client) => {
            let name = client.map(str::trim).filter(|name| !is_blank_name(name) && *name != "agent" && *name != "desktop");
            match (name, id.is_empty()) {
                (Some(name), false) => (format!("에이전트 {name}"), format!(" · {}", short(id, 4))),
                (Some(name), true) => (format!("에이전트 {name}"), String::new()),
                (None, false) => (format!("에이전트 {}", short(id, 8)), String::new()),
                (None, true) => (actor.to_owned(), String::new()),
            }
        }
        (Some("cli"), _, _) => ("명령줄".into(), String::new()),
        _ => (actor.to_owned(), String::new()),
    }
}

/// The longest gap in seconds between the two clicks of a double click on a layer row: the system's double-click time
/// (Windows `GetDoubleClickTime`, 500 ms unless the user changed it).
pub(super) fn double_click_secs() -> f64 {
    // SAFETY: GetDoubleClickTime takes no arguments and only reads a system setting.
    f64::from(unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() }) / 1000.0
}

/// One row of the layers tree, computed before anything is drawn so the artifact is not borrowed
/// while commands run.
#[derive(Clone, Default)]
struct LayerRow {
    depth: usize,
    label: String,
    search_text: String,
    icon: &'static str,
    node_id: Option<String>,
    page_index: Option<u32>,
    section_id: Option<String>,
    section_indent: bool,
    /// The page a node belongs to; page_index remains set only on page rows for drag grouping.
    owner_page: Option<u32>,
    hidden: bool,
    locked: bool,
    has_children: bool,
    /// An outline row (text documents): the heading paragraph's text, selected as a text range on click.
    paragraph_text: Option<String>,
    search_result: Option<usize>,
}

/// The icon for a node kind (§4 layer rows): frames are columns, paths are vectors.
pub(super) fn node_icon(node: &Node) -> &'static str {
    match node.kind {
        NodeKind::Text => "tool-text",
        // A frame shows how it lays out its children: a column, a row, a grid, or free.
        NodeKind::Rectangle if !node.children.is_empty() || node.layout.is_some() => match node.layout.as_ref().map(|layout| layout.mode.as_str()) {
            Some("column") => "layer-col",
            Some("row") => "layer-row",
            Some("grid") => "layout-grid",
            _ => "tool-frame",
        },
        NodeKind::Rectangle => "tool-rect",
        NodeKind::Ellipse => "tool-oval",
        NodeKind::Line => "tool-line",
        NodeKind::Image => "ins-image",
        NodeKind::Video => "ins-video",
        NodeKind::Group => "group",
        NodeKind::Path => "layer-vector",
    }
}

/// Short text for a node in a row: its name, then its text, then its kind.
pub(super) fn node_label(node: &Node) -> String {
    if let Some(name) = node.ext.name.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
        return name.to_owned();
    }
    if !node.text.trim().is_empty() {
        return node.text.trim().to_owned();
    }
    match node.kind {
        NodeKind::Text => "텍스트",
        // A painted container is a frame in the layers and the panel (§9.1).
        NodeKind::Rectangle if !node.children.is_empty() || node.layout.is_some() => "프레임",
        NodeKind::Rectangle => "사각형",
        NodeKind::Ellipse => "타원",
        NodeKind::Line => "선",
        NodeKind::Image => "이미지",
        NodeKind::Video => "동영상",
        NodeKind::Group => "그룹",
        NodeKind::Path => "경로",
    }
    .to_owned()
}

fn push_nodes(rows: &mut Vec<LayerRow>, nodes: &[Node], depth: usize, owner_page: u32) {
    for node in nodes.iter().rev() {
        rows.push(LayerRow {
            depth,
            label: node_label(node),
            search_text: node.text.clone(),
            icon: node_icon(node),
            node_id: Some(node.id.clone()),
            page_index: None,
            section_id: None,
            section_indent: false,
            owner_page: Some(owner_page),
            hidden: node.ext.hidden == Some(true),
            locked: node.ext.locked == Some(true),
            has_children: !node.children.is_empty(),
            paragraph_text: None,
            search_result: None,
        });
        push_nodes(rows, &node.children, depth + 1, owner_page);
    }
}

/// Whether a row's name or text holds `query` (lowercase): the layers search filter and its next/previous result.
fn row_matches(row: &LayerRow, query: &str) -> bool {
    row.label.to_lowercase().contains(query) || row.search_text.to_lowercase().contains(query)
}

fn filter_rows(rows: Vec<LayerRow>, query: &str, page_scope: Option<u32>) -> Vec<LayerRow> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return rows;
    }
    let rows = if let Some(page) = page_scope {
        rows.into_iter().filter(|row| row.page_index == Some(page) || row.owner_page == Some(page)).collect()
    } else {
        rows
    };
    let mut visible = vec![false; rows.len()];
    for (index, row) in rows.iter().enumerate() {
        if !row_matches(row, &query) {
            continue;
        }
        visible[index] = true;
        let mut depth = row.depth;
        for (parent, candidate) in rows[..index].iter().enumerate().rev() {
            if candidate.page_index.is_some() && candidate.depth == 0 {
                visible[parent] = true;
                break;
            }
            if candidate.node_id.is_some() && candidate.depth < depth {
                visible[parent] = true;
                depth = candidate.depth;
            }
        }
    }
    rows.into_iter().zip(visible).filter_map(|(row, keep)| keep.then_some(row)).collect()
}

fn collect_flow_paragraphs<'a>(blocks: &'a [FlowBlock], out: &mut Vec<&'a Paragraph>) {
    for block in blocks {
        match block {
            FlowBlock::Paragraph(paragraph) => out.push(paragraph),
            FlowBlock::Table(table) => {
                for cell in table.rows.iter().flatten() {
                    collect_flow_paragraphs(&cell.blocks, out);
                }
            }
            FlowBlock::Image(_) | FlowBlock::Opaque { .. } => {}
        }
    }
}

fn folded_logical_text(logical: &LogicalParagraph) -> (String, Vec<usize>) {
    let mut folded = String::new();
    let mut boundaries = Vec::with_capacity(logical.graphemes.len() + 1);
    boundaries.push(0usize);
    for grapheme in &logical.graphemes {
        folded.push_str(&logical.text[grapheme.logical.clone()].to_lowercase());
        boundaries.push(folded.len());
    }
    (folded, boundaries)
}

fn logical_match_range_from(logical: &LogicalParagraph, query: &str, from: u32) -> Option<(u32, u32)> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let (folded, boundaries) = folded_logical_text(logical);
    let from = from as usize;
    let mut offset = *boundaries.get(from)?;
    while offset <= folded.len() {
        let at = offset + folded.get(offset..)?.find(&needle)?;
        let end = at + needle.len();
        if let (Ok(start), Ok(end)) = (boundaries.binary_search(&at), boundaries.binary_search(&end)) {
            if start >= from && start < end && logical.graphemes[start..end].iter().all(|grapheme| !grapheme.protected) {
                return Some((start as u32, end as u32));
            }
        }
        let Some(next) = folded.get(at..)?.chars().next().map(|character| at + character.len_utf8()) else {
            break;
        };
        offset = next;
    }
    None
}

fn logical_match_range_before(logical: &LogicalParagraph, query: &str, before: u32) -> Option<(u32, u32)> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let (folded, boundaries) = folded_logical_text(logical);
    let before = before as usize;
    let mut offset = 0;
    let mut found = None;
    while offset <= folded.len() {
        let Some(relative) = folded.get(offset..)?.find(&needle) else { break };
        let at = offset + relative;
        let end = at + needle.len();
        if let (Ok(start), Ok(end)) = (boundaries.binary_search(&at), boundaries.binary_search(&end)) {
            if start >= before {
                break;
            }
            if start < end && logical.graphemes[start..end].iter().all(|grapheme| !grapheme.protected) {
                found = Some((start as u32, end as u32));
            }
        }
        let Some(next) = folded.get(at..)?.chars().next().map(|character| at + character.len_utf8()) else {
            break;
        };
        offset = next;
    }
    found
}

fn logical_match_range(logical: &LogicalParagraph, query: &str) -> Option<(u32, u32)> {
    logical_match_range_from(logical, query, 0)
}

fn flow_find_matches(document: &FlowDocument, query: &str) -> Vec<FlowFindMatch> {
    let mut paragraphs = Vec::new();
    for section in &document.sections {
        collect_flow_paragraphs(&section.blocks, &mut paragraphs);
    }
    paragraphs
        .into_iter()
        .filter_map(|paragraph| {
            let logical = LogicalParagraph::new(paragraph);
            let (start, end) = logical_match_range(&logical, query)?;
            let text = logical.text.replace(OBJECT_REPLACEMENT, "…").replace('\r', " ").replace('\n', " ");
            Some(FlowFindMatch { paragraph_id: paragraph.id.clone(), text, range: TextRange { paragraph_id: paragraph.id.clone(), start, end }, page_index: None })
        })
        .collect()
}

fn flow_find_neighbor(
    search: &FlowFindSearch,
    document: &FlowDocument,
    cursor: Option<&FlowFindCursor>,
    forward: bool,
) -> Option<(usize, TextRange)> {
    let matches = &search.matches;
    if matches.is_empty() {
        return None;
    }
    let mut paragraphs = Vec::new();
    for section in &document.sections {
        collect_flow_paragraphs(&section.blocks, &mut paragraphs);
    }
    let paragraph = |id: &str| paragraphs.iter().copied().find(|paragraph| paragraph.id == id);
    let last_range = |logical: &LogicalParagraph| {
        logical_match_range_before(logical, &search.query, logical.graphemes.len().saturating_add(1) as u32)
    };

    let current = cursor.filter(|cursor| cursor.generation == search.generation)
        .filter(|cursor| matches.get(cursor.match_index).is_some_and(|hit| hit.paragraph_id == cursor.range.paragraph_id));
    let Some(current) = current else {
        let index = if forward { 0 } else { matches.len() - 1 };
        let hit = &matches[index];
        let logical = LogicalParagraph::new(paragraph(&hit.paragraph_id)?);
        let range = if forward {
            logical_match_range(&logical, &search.query)
        } else {
            last_range(&logical)
        }?;
        return Some((index, TextRange { paragraph_id: hit.paragraph_id.clone(), start: range.0, end: range.1 }));
    };

    let current_logical = LogicalParagraph::new(paragraph(&current.range.paragraph_id)?);
    let local = if forward {
        logical_match_range_from(&current_logical, &search.query, current.range.start.saturating_add(1))
    } else {
        logical_match_range_before(&current_logical, &search.query, current.range.start)
    };
    if let Some((start, end)) = local {
        return Some((current.match_index, TextRange { paragraph_id: current.range.paragraph_id.clone(), start, end }));
    }

    for step in 1..=matches.len() {
        let index = if forward {
            (current.match_index + step) % matches.len()
        } else {
            (current.match_index + matches.len() - (step % matches.len())) % matches.len()
        };
        let hit = &matches[index];
        let logical = LogicalParagraph::new(paragraph(&hit.paragraph_id)?);
        let range = if forward { logical_match_range(&logical, &search.query) } else { last_range(&logical) }?;
        return Some((index, TextRange { paragraph_id: hit.paragraph_id.clone(), start: range.0, end: range.1 }));
    }
    None
}

fn page_contains_flow_range(page: &RenderPage, target: &TextRange) -> bool {
    page.source_map.iter().any(|region| {
        region.logical_range.as_ref().is_some_and(|range| {
            range.paragraph_id == target.paragraph_id && range.start <= target.start && target.start < range.end
        })
    })
}

fn page_contains_flow_match(page: &RenderPage, hit: &FlowFindMatch) -> bool {
    page_contains_flow_range(page, &hit.range)
}

fn same_text_range(left: &TextRange, right: &TextRange) -> bool {
    left.paragraph_id == right.paragraph_id && left.start == right.start && left.end == right.end
}

impl DesktopApp {
    pub(super) fn reset_flow_find_search(&mut self) {
        self.flow_find_generation = self.flow_find_generation.wrapping_add(1);
        self.flow_find = None;
        self.flow_find_lookup = None;
        self.flow_find_cursor = None;
    }

    fn cached_flow_page_for_range(&self, artifact_id: &str, revision: u64, range: &TextRange) -> Option<(u32, designllm_contracts::Size)> {
        let rendered = self.page_textures
            .values()
            .filter(|texture| texture.revision == revision && page_contains_flow_range(&texture.page, range))
            .map(|texture| (texture.page.page_index, texture.page.size))
            .min_by_key(|(page_index, _)| *page_index);
        rendered.or_else(|| {
            self.flow_find_cached_page.as_ref().and_then(|(cached_artifact, cached_revision, page)| {
                (cached_artifact == artifact_id && *cached_revision == revision && page_contains_flow_range(page, range))
                    .then_some((page.page_index, page.size))
            })
        })
    }

    pub(super) fn open_flow_find_result(&mut self, match_index: usize) {
        let Some(search) = self.flow_find.as_ref() else { return };
        let Some(hit) = search.matches.get(match_index).cloned() else { return };
        self.open_flow_find_range(match_index, hit.range);
    }

    pub(super) fn move_flow_find_match(&mut self, forward: bool) {
        self.sync_flow_find_search();
        let target = {
            let Some(search) = self.flow_find.as_ref() else { return };
            let Some(artifact) = self.artifact.as_ref().filter(|artifact| artifact.id == search.artifact_id && artifact.revision == search.revision) else {
                return;
            };
            let ArtifactBody::Flow(document) = &artifact.body else { return };
            flow_find_neighbor(search, document, self.flow_find_cursor.as_ref(), forward)
        };
        if let Some((match_index, range)) = target {
            self.open_flow_find_range(match_index, range);
        }
    }

    fn open_flow_find_range(&mut self, match_index: usize, range: TextRange) {
        let Some(search) = self.flow_find.as_ref() else { return };
        let Some(hit) = search.matches.get(match_index) else { return };
        if hit.paragraph_id != range.paragraph_id {
            return;
        }
        let generation = search.generation;
        let revision = search.revision;
        let artifact_id = search.artifact_id.clone();
        let cached_hit_page = same_text_range(&hit.range, &range).then_some(hit.page_index).flatten();
        if let Some((page_index, size)) = cached_hit_page.map(|index| {
            let size = self.known_sizes.get(&index).copied().unwrap_or_else(|| {
                self.active.as_ref().map_or(designllm_contracts::Size::a4(), |active| super::board::default_page_size(active.kind))
            });
            (index, size)
        }).or_else(|| self.cached_flow_page_for_range(&artifact_id, revision, &range)) {
            self.known_sizes.insert(page_index, size);
            self.flow_find_lookup = None;
            self.select_flow_find_result(match_index, range, page_index);
            return;
        }
        self.flow_find_lookup = Some(FlowFindLookup {
            generation,
            match_index,
            range,
            next_page: 0,
            total_pages: self.total_pages.max(1),
        });
        self.request_flow_find_page();
    }

    fn select_flow_find_result(&mut self, match_index: usize, range: TextRange, page_index: u32) {
        let Some(generation) = self.flow_find.as_ref().map(|search| search.generation) else { return };
        self.flow_find_cursor = Some(FlowFindCursor { generation, match_index, range: range.clone() });
        self.invoke_ui("view.zoom_page", json!({ "page_index": page_index }));
        self.set_selection(Selection { node_ids: Vec::new(), text_range: Some(range) });
    }

    fn request_flow_find_page(&mut self) {
        if self.flow_find_request.is_some() {
            return;
        }
        let (generation, match_index, range, page_index, total_pages, artifact_id, revision, hit) = {
            let Some(search) = self.flow_find.as_ref() else { return };
            let Some(lookup) = self.flow_find_lookup.as_ref() else { return };
            if lookup.generation != search.generation || search.query.is_empty() {
                self.flow_find_lookup = None;
                return;
            }
            let Some(hit) = search.matches.get(lookup.match_index).cloned() else {
                self.flow_find_lookup = None;
                return;
            };
            (
                lookup.generation,
                lookup.match_index,
                lookup.range.clone(),
                lookup.next_page,
                lookup.total_pages,
                search.artifact_id.clone(),
                search.revision,
                hit,
            )
        };
        if hit.paragraph_id != range.paragraph_id {
            self.flow_find_lookup = None;
            return;
        }
        let cached_hit_page = same_text_range(&hit.range, &range).then_some(hit.page_index).flatten();
        if let Some((page_index, size)) = cached_hit_page.map(|index| {
            let size = self.known_sizes.get(&index).copied().unwrap_or_else(|| {
                self.active.as_ref().map_or(designllm_contracts::Size::a4(), |active| super::board::default_page_size(active.kind))
            });
            (index, size)
        }).or_else(|| self.cached_flow_page_for_range(&artifact_id, revision, &range)) {
            self.known_sizes.insert(page_index, size);
            self.flow_find_lookup = None;
            self.select_flow_find_result(match_index, range, page_index);
            return;
        }
        if page_index >= total_pages {
            self.flow_find_lookup = None;
            self.set_error("검색 결과의 페이지를 찾지 못했습니다".into());
            return;
        }
        if !self.shows_revision(&artifact_id, revision) {
            self.flow_find_lookup = None;
            return;
        }
        let request = FlowFindPageRequest { generation, match_index, range, page_index, artifact_id: artifact_id.clone(), revision };
        self.flow_find_request = Some(request.clone());
        let render = RenderRequest {
            artifact_id,
            expected_revision: revision,
            page_index,
            profile: RenderProfile::default(),
            region: None,
            include_node_frames: false,
        };
        match serde_json::to_value(render) {
            Ok(params) => {
                if self.call("render.get", params, PendingAction::FlowFindPage { request: request.clone() }).is_none() {
                    self.flow_find_request = None;
                    self.flow_find_lookup = None;
                }
            }
            Err(_) => {
                self.flow_find_request = None;
                self.flow_find_lookup = None;
                self.set_error("검색 결과 페이지를 요청하지 못했습니다".into());
            }
        }
    }

    pub(super) fn flow_find_page_received(&mut self, request: &FlowFindPageRequest, value: Value) {
        if self.flow_find_request.as_ref() != Some(request) {
            return;
        }
        self.flow_find_request = None;
        let current = self.flow_find.as_ref().is_some_and(|search| {
            search.generation == request.generation && search.artifact_id == request.artifact_id && search.revision == request.revision
        }) && self.active.as_ref().is_some_and(|active| active.id == request.artifact_id && active.revision == request.revision)
            && self.artifact.as_ref().is_some_and(|artifact| artifact.id == request.artifact_id && artifact.revision == request.revision);
        if !current {
            self.request_flow_find_page();
            return;
        }
        let lookup_matches = self.flow_find_lookup.as_ref().is_some_and(|lookup| {
            lookup.generation == request.generation
                && lookup.match_index == request.match_index
                && same_text_range(&lookup.range, &request.range)
                && lookup.next_page == request.page_index
        });
        let Some(snapshot) = serde_json::from_value::<RenderSnapshot>(value).ok() else {
            if lookup_matches {
                self.flow_find_lookup = None;
                self.set_error("검색 결과 페이지를 읽지 못했습니다".into());
            }
            self.request_flow_find_page();
            return;
        };
        let Some(page) = snapshot.scene.pages.iter().find(|page| page.page_index == request.page_index) else {
            if lookup_matches {
                self.flow_find_lookup = None;
                self.set_error("검색 결과 페이지를 불러오지 못했습니다".into());
            }
            self.request_flow_find_page();
            return;
        };

        let page_index = page.page_index;
        let page_size = page.size;
        let total_pages = snapshot
            .scene
            .metadata
            .get("total_pages")
            .and_then(serde_json::Value::as_u64)
            .and_then(|total| u32::try_from(total).ok())
            .unwrap_or_else(|| self.total_pages.max(page_index.saturating_add(1)));
        self.known_sizes.insert(page_index, page_size);
        self.total_pages = total_pages;
        self.flow_find_cached_page = Some((request.artifact_id.clone(), request.revision, page.clone()));
        if let Some(search) = self.flow_find.as_mut() {
            for hit in &mut search.matches {
                if hit.page_index.is_none() && page_contains_flow_match(page, hit) {
                    hit.page_index = Some(page_index);
                }
            }
        }

        if lookup_matches {
            if page_contains_flow_range(page, &request.range) {
                self.flow_find_lookup = None;
                self.select_flow_find_result(request.match_index, request.range.clone(), page_index);
            } else if page_index.saturating_add(1) < total_pages {
                if let Some(lookup) = self.flow_find_lookup.as_mut() {
                    lookup.total_pages = total_pages;
                    lookup.next_page = page_index.saturating_add(1);
                }
            } else {
                self.flow_find_lookup = None;
                self.set_error("검색 결과의 페이지를 찾지 못했습니다".into());
            }
        }
        self.request_flow_find_page();
    }

    pub(super) fn flow_find_page_failed(&mut self, request: &FlowFindPageRequest, error: &designllm_contracts::DltError) {
        if self.flow_find_request.as_ref() != Some(request) {
            return;
        }
        self.flow_find_request = None;
        let current = self.flow_find.as_ref().is_some_and(|search| {
            search.generation == request.generation && search.artifact_id == request.artifact_id && search.revision == request.revision
        });
        if current && self.flow_find_lookup.as_ref().is_some_and(|lookup| {
            lookup.generation == request.generation
                && lookup.match_index == request.match_index
                && same_text_range(&lookup.range, &request.range)
                && lookup.next_page == request.page_index
        }) {
            self.flow_find_lookup = None;
            if !matches!(&error.code, &designllm_contracts::ErrorCode::StaleRender | &designllm_contracts::ErrorCode::RevisionConflict | &designllm_contracts::ErrorCode::RenderPending) {
                self.set_error("검색 결과 페이지를 불러오지 못했습니다".into());
            }
        }
        self.request_flow_find_page();
    }

    pub(super) fn flow_find_page_dropped(&mut self, request: &FlowFindPageRequest) {
        if self.flow_find_request.as_ref() == Some(request) {
            self.flow_find_request = None;
        }
        self.request_flow_find_page();
    }

    pub(super) fn sync_flow_find_search(&mut self) {
        let query = self.layer_find_query.trim().to_owned();
        let Some((artifact_id, revision, kind)) = self.active.as_ref().map(|active| (active.id.clone(), active.revision, active.kind)) else {
            if self.flow_find.is_some() || self.flow_find_lookup.is_some() {
                self.reset_flow_find_search();
            }
            return;
        };
        if query.is_empty() || !matches!(kind, ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp) {
            if self.flow_find.is_some() || self.flow_find_lookup.is_some() {
                self.reset_flow_find_search();
            }
            return;
        }
        let Some(artifact) = self.artifact.as_ref().filter(|artifact| artifact.id == artifact_id && artifact.revision == revision) else {
            if self.flow_find.as_ref().is_some_and(|search| search.artifact_id != artifact_id || search.revision != revision || search.query != query) {
                self.reset_flow_find_search();
            }
            return;
        };
        let ArtifactBody::Flow(document) = &artifact.body else {
            self.reset_flow_find_search();
            return;
        };
        if self.flow_find.as_ref().is_some_and(|search| {
            search.artifact_id == artifact_id && search.revision == revision && search.query == query
        }) {
            return;
        }
        self.flow_find_cursor = None;
        self.flow_find_generation = self.flow_find_generation.wrapping_add(1);
        self.flow_find_lookup = None;
        self.flow_find = Some(FlowFindSearch {
            artifact_id,
            revision,
            query: query.clone(),
            generation: self.flow_find_generation,
            matches: flow_find_matches(document, &query),
        });
    }

    fn flow_find_rows(&self) -> Vec<LayerRow> {
        let Some(search) = self.flow_find.as_ref() else { return Vec::new() };
        search
            .matches
            .iter()
            .enumerate()
            .map(|(index, hit)| LayerRow {
                depth: 0,
                label: hit.text.trim().to_owned(),
                search_text: hit.text.clone(),
                icon: "ins-paragraph",
                node_id: Some(hit.paragraph_id.clone()),
                paragraph_text: Some(hit.text.clone()),
                search_result: Some(index),
                ..Default::default()
            })
            .collect()
    }

    /// The layers tree of the open document.
    pub(super) fn draw_layers(&mut self, ui: &mut Ui, palette: &theme::Palette) {
        if self.layer_find_open {
            let mut query = self.layer_find_query.clone();
            let focus = self.layer_find_focus;
            let mut close = false;
            let mut changed = false;
            let mut previous = false;
            let mut next = false;
            let show_find_navigation = self.active.as_ref().is_some_and(|active| {
                matches!(active.kind, ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp | ArtifactKind::Canvas)
            });
            ui.horizontal(|ui| {
                let reserved = if show_find_navigation { 108.0 } else { 36.0 };
                let response = widgets::text_field(ui, palette, &mut query, "레이어 찾기", (ui.available_width() - reserved).max(0.0));
                if focus {
                    response.request_focus();
                }
                changed = response.changed();
                if show_find_navigation {
                    previous = widgets::icon_button(ui, palette, "chev-up", "이전 찾기 · Shift+F3", false).clicked();
                    next = widgets::icon_button(ui, palette, "chev-down", "다음 찾기 · F3", false).clicked();
                }
                close = widgets::icon_button(ui, palette, "close", "찾기 닫기", false).clicked();
            });
            self.layer_find_focus = false;
            if changed {
                self.layer_find_query = query;
            }
            if close {
                self.layer_find_open = false;
                self.layer_find_query.clear();
            }
            if !close && previous {
                self.invoke_ui("layer.find.previous", json!({}));
            }
            if !close && next {
                self.invoke_ui("layer.find.next", json!({}));
            }
        }
        self.sync_flow_find_search();
        let searching = !self.layer_find_query.trim().is_empty();
        let flow_search = searching && self.active.as_ref().is_some_and(|item| matches!(item.kind, ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp));
        let flow_search_waiting = flow_search && self.flow_find.is_none();
        let figure = self.active.as_ref().is_some_and(|item| item.kind == ArtifactKind::Figure);
        let selected = self.selected_nodes.clone();
        let page_index = self.view_ref().page_index;
        let layer_state_supported = self.active.as_ref().is_some_and(|item| item.kind == ArtifactKind::Canvas);
        // A WebCode project's rows are its files: a file has no name to change (the web engine refuses SetNodeInfo), so a
        // double click opens no rename field.
        let web_files = self.artifact.as_ref().is_some_and(|artifact| matches!(artifact.body, ArtifactBody::WebCode(_)));
        let page_scope = self.artifact.as_ref().and_then(|artifact| {
            matches!(&artifact.body, ArtifactBody::Canvas(_) | ArtifactBody::Slides(_)).then_some(page_index)
        });
        let rows = if flow_search { self.flow_find_rows() } else { filter_rows(self.layer_rows(), &self.layer_find_query, page_scope) };
        let reserved: std::collections::BTreeSet<String> = self.reservations.iter().flat_map(|reservation| reservation.node_ids.iter().cloned()).collect();
        let warned: std::collections::BTreeSet<String> = self.lint_warnings.iter().filter_map(|warning| warning.node_id.clone()).collect();
        let paragraph = self.context.as_ref().and_then(|context| context.selection.text_range.as_ref().map(|range| range.paragraph_id.clone()));
        let marks = row_marks(&rows, |row| match (&row.node_id, row.page_index) {
            (Some(id), _) if row.paragraph_text.is_some() => paragraph.as_deref() == Some(id.as_str()) || selected.contains(id),
            (Some(id), _) => selected.contains(id),
            (None, Some(index)) => selected.is_empty() && index == page_index,
            _ => false,
        });
        let open_id = egui::Id::new("layers-open");
        let mut overrides: std::collections::BTreeMap<String, bool> = ui.data(|data| data.get_temp(open_id)).unwrap_or_default();
        let drag_id = egui::Id::new("layers-drag");
        let page_drag_id = egui::Id::new("pages-drag");
        let page_reorder_pending = self.pending_page_reorder_selection.is_some();
        let page_drag_allowed = page_scope.is_some() && !searching && !flow_search && !page_reorder_pending;
        let mut placed: Vec<Rect> = Vec::with_capacity(rows.len());
        let mut row_page = 0u32;
        // Rows under a closed row are skipped until the depth comes back up.
        let mut hidden_below: Option<usize> = None;
        // The last click on a row's own area (its key and time), the first half of a double click on that row; this frame's
        // click that keeps it as the first half.
        let last_click = ui.id().with("layer-row-last-click");
        let mut first_click_kept = false;
        for (position, row) in rows.iter().enumerate() {
            if let Some(index) = row.page_index {
                row_page = index;
            }
            if hidden_below.is_some_and(|depth| row.depth > depth) {
                placed.push(Rect::NOTHING);
                continue;
            }
            hidden_below = None;
            let key = row_key(row);
            let mark = marks[position];
            // Open by default: the current page and every row on the way to the selection (and the selection itself).
            // A figure's panels are all open (figure-board): it has few elements.
            let open_default = mark.ancestor || mark.selected || row.page_index == Some(page_index) || (figure && row.depth == 0);
            let open = row.has_children && (searching || overrides.get(&key).copied().unwrap_or(open_default));
            if row.has_children && !open {
                hidden_below = Some(row.depth);
            }
            let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), theme::LAYER_ROW), Sense::click_and_drag());
            placed.push(rect);
            if let Some(test) = self.ui_test.as_mut() { test.note_layer_row(row.node_id.as_deref(), row.page_index.or(row.owner_page), &row.label, rect, ui.clip_rect()); }
            if response.drag_started_by(egui::PointerButton::Primary) {
                if row.depth == 1 {
                    if let Some(id) = &row.node_id {
                        let grabbed = LayerDrag { node_id: id.clone(), page: row_page };
                        ui.data_mut(|data| data.insert_temp(drag_id, grabbed));
                    }
                } else if page_drag_allowed && row.depth == 0 {
                    if let Some(index) = row.page_index {
                        if let Some(grabbed) = self.capture_page_row_drag(index) {
                            ui.data_mut(|data| data.insert_temp(page_drag_id, grabbed));
                        }
                    }
                }
            }
            let is_selected = mark.selected;
            // tokens `component.layer-row`: radius 6; selected accent-soft, its descendants at 45 %.
            let corner = CornerRadius::same(theme::RADIUS_CONTROL);
            if is_selected {
                ui.painter().rect_filled(rect, corner, palette.accent_soft);
            } else if mark.descendant {
                ui.painter().rect_filled(rect, corner, palette.derived(theme::Derived::RowDescendant));
            } else if response.hovered() {
                ui.painter().rect_filled(rect, corner, palette.surface_2);
            }
            let mut x = rect.left() + 2.0 + (row.depth + usize::from(row.section_indent)) as f32 * 16.0;
            let twisty = Rect::from_min_size(Pos2::new(x, rect.center().y - 8.0), Vec2::splat(16.0));
            if row.has_children {
                icons::paint(ui, twisty, if open { "chev-down" } else { "chev-right" }, if is_selected { palette.accent } else { palette.ink_3 });
                let toggle = ui.interact(twisty.expand(4.0), ui.id().with(("twisty", key.as_str())), Sense::click());
                if toggle.clicked() {
                    overrides.insert(key.clone(), !open);
                }
            }
            x += 16.0 + 4.0;
            let icon_rect = Rect::from_min_size(Pos2::new(x, rect.center().y - 8.0), Vec2::splat(16.0));
            icons::paint(ui, icon_rect, row.icon, if is_selected { palette.accent } else { palette.ink_2 });
            x += 16.0 + 4.0;
            let renaming = self.layer_rename.as_ref().is_some_and(|rename| rename.key == key);
            let section_header = row.section_id.is_some();
            let color = if is_selected { palette.accent_ink } else if section_header { palette.ink_2 } else { palette.ink };
            let font = if section_header { theme::display_font(true, 12.0) } else { theme::font(theme::Type::Body) };
            let agent_here = row.node_id.as_ref().is_some_and(|id| reserved.contains(id)) || (row.page_index.is_some() && !open && mark.holds_reserved(&rows, position, &reserved));
            // tokens `lint-warning.layer-row`: `state-warn` 16 warn at the row end (4 left of the agent dot); a closed
            // row shows it when a node inside it has a warning.
            let warn_here = row.node_id.as_ref().is_some_and(|id| warned.contains(id)) || (row.has_children && !open && mark.holds_reserved(&rows, position, &warned));
            let state_slots = if layer_state_supported { row.hidden as usize + row.locked as usize } else { 0 };
            let state_space = state_slots as f32 * 20.0;
            let room = rect.right() - 8.0 - x - if agent_here { 12.0 } else { 0.0 } - if warn_here { 20.0 } else { 0.0 } - state_space;
            let label = widgets::truncate_to_width(ui, &row.label, room, color);
            if !renaming {
                ui.painter().text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, label, font, color);
            }
            if agent_here {
                ui.painter().circle_filled(Pos2::new(rect.right() - 8.0 - 3.0, rect.center().y), 3.0, palette.agent(0));
            }
            if warn_here {
                let right = rect.right() - 8.0 - if agent_here { 6.0 + 4.0 } else { 0.0 };
                icons::paint(ui, Rect::from_min_size(Pos2::new(right - 16.0, rect.center().y - 8.0), Vec2::splat(16.0)), "state-warn", palette.warn);
            }
            let mut state_action_clicked = false;
            let mut state_right = rect.right() - 8.0 - if agent_here { 12.0 } else { 0.0 } - if warn_here { 20.0 } else { 0.0 };
            if layer_state_supported && row.hidden {
                state_right -= 16.0;
                let state_rect = Rect::from_min_size(Pos2::new(state_right, rect.center().y - 8.0), Vec2::splat(16.0));
                let sense = if row.locked { Sense::hover() } else { Sense::click() };
                let state = ui
                    .interact(state_rect, ui.id().with(("layer-hidden", key.as_str())), sense)
                    .on_hover_text(if row.locked { "숨김" } else { "표시" });
                icons::paint(ui, state_rect, "eye-off", palette.ink_2);
                if state.clicked() {
                    if let Some(id) = &row.node_id {
                        state_action_clicked = true;
                        self.select_node(id.clone());
                        self.invoke_ui("edit.show", json!({}));
                    }
                }
                state_right -= 4.0;
            }
            if layer_state_supported && row.locked {
                state_right -= 16.0;
                let state_rect = Rect::from_min_size(Pos2::new(state_right, rect.center().y - 8.0), Vec2::splat(16.0));
                let state = ui
                    .interact(state_rect, ui.id().with(("layer-locked", key.as_str())), Sense::click())
                    .on_hover_text("잠금 해제");
                icons::paint(ui, state_rect, "lock", palette.ink_2);
                if state.clicked() {
                    if let Some(id) = &row.node_id {
                        state_action_clicked = true;
                        self.select_node(id.clone());
                        self.invoke_ui("edit.unlock", json!({}));
                    }
                }
            }
            // egui's `double_clicked` counts clicks anywhere within 0.3 s (a click on another row just before makes it a triple
            // click, and the first click of a pair a double one), so it misses a double click on a row that follows another
            // click. A double click here is two clicks on this row's own area within the system's double-click time, with no
            // other click between them (an icon, the twisty, another row, the board); the click that makes one is no first half
            // of another, so a third click is a single one.
            // The right click menus (68 item 16): the row of a Canvas or Slides node (any depth) its node's menu, a Canvas page
            // row the page menu. Each menu is the row's own (its key), so it stays with its node or page when the rows move.
            let menu_id = ui.id().with(("layer-row-menu", key.as_str()));
            if let (Some(node_id), Some(owner)) = (row.node_id.as_deref(), row.owner_page) {
                if row.paragraph_text.is_none() && row.search_result.is_none() {
                    self.node_row_menu(ui.ctx(), &response, menu_id, node_id, owner);
                }
            } else if let Some(index) = row.page_index.filter(|_| row.depth == 0 && row.section_id.is_none() && row.search_result.is_none()) {
                if self.active.as_ref().is_some_and(|active| active.kind == ArtifactKind::Canvas) {
                    self.page_row_menu(ui.ctx(), &response, menu_id, index);
                }
            }
            let now = ui.input(|input| input.time);
            let row_clicked = response.clicked() && !state_action_clicked;
            let double_clicked = row_clicked
                && ui.data(|data| data.get_temp::<(String, f64)>(last_click)).is_some_and(|(id, at)| id == key && now - at <= double_click_secs());
            if row_clicked {
                if double_clicked {
                    ui.data_mut(|data| data.remove::<(String, f64)>(last_click));
                } else {
                    ui.data_mut(|data| data.insert_temp(last_click, (key.clone(), now)));
                    first_click_kept = true;
                }
            }
            if double_clicked {
                if let Some(section_id) = row.section_id.as_deref() {
                    self.open_page_section_editor(section_id);
                } else if !renaming && !web_files {
                    self.layer_rename = Some(LayerRename { key: key.clone(), text: row.label.clone() });
                }
            }
            if renaming {
                self.draw_rename_field(ui, rect, x, palette);
            }
            if row_clicked {
                if let Some(result) = row.search_result {
                    self.open_flow_find_result(result);
                } else if row.section_id.is_some() {
                    if let Some(index) = row.owner_page {
                        self.invoke_ui("nav.page", json!({ "page_index": index }));
                    }
                } else {
                    match (&row.node_id, row.page_index) {
                        (Some(id), _) if row.paragraph_text.is_some() => {
                            match outline_many(&self.selected_nodes, paragraph.as_deref(), id, super::multi::additive_modifier(ui.ctx())) {
                                Some(ids) => self.invoke_ui("select.many", json!({ "node_ids": ids })),
                                None => self.select_paragraph(id.clone(), row.paragraph_text.as_deref().unwrap_or_default()),
                            }
                        }
                        (Some(id), _) => {
                            if row_page != page_index {
                                self.invoke_ui("nav.page", json!({ "page_index": row_page }));
                            }
                            if super::multi::additive_modifier(ui.ctx()) {
                                let ids = super::multi::toggle_id(&self.selected_nodes, id);
                                self.invoke_ui("select.many", json!({ "node_ids": ids }));
                            } else {
                                let (command, args) = self.layer_row_command(id);
                                self.invoke_ui(command, args);
                            }
                        }
                        (None, Some(index)) => {
                            self.invoke_ui("nav.page", json!({ "page_index": index }));
                        }
                        _ => {}
                    }
                }
            }
        }
        // Any other click this frame (an icon, the twisty, the board) ends the first half of a row's double click.
        if !first_click_kept && ui.input(|input| input.pointer.any_click()) {
            ui.data_mut(|data| data.remove::<(String, f64)>(last_click));
        }
        let drag: Option<LayerDrag> = ui.data(|data| data.get_temp(drag_id));
        if let Some(drag) = drag {
            let pointer = ui.ctx().pointer_latest_pos();
            let slots = child_slots(&rows, &placed);
            let target = pointer.and_then(|pos| slots.iter().find(|slot| slot.page == drag.page && slot.rect.contains(pos)));
            if ui.input(|input| input.pointer.primary_down()) {
                if let (Some(slot), Some(pos)) = (target, pointer) {
                    let y = if pos.y < slot.rect.center().y { slot.rect.top() } else { slot.rect.bottom() };
                    ui.painter().hline(slot.rect.x_range(), y, egui::Stroke::new(2.0, palette.accent));
                }
            } else {
                ui.data_mut(|data| data.remove::<LayerDrag>(drag_id));
                if let Some(slot) = target.filter(|slot| slot.node_id != drag.node_id) {
                    self.invoke_ui("layer.move", json!({ "node_id": drag.node_id.clone(), "index": slot.index }));
                }
            }
        }
        let page_drag: Option<PageRowDrag> = ui.data(|data| data.get_temp(page_drag_id));
        if let Some(drag) = page_drag {
            let pointer = ui.ctx().pointer_latest_pos();
            let target = page_drag_allowed.then(|| {
                pointer.and_then(|position| rows.iter().zip(&placed).find_map(|(row, rect)| {
                    (row.depth == 0 && rect.contains(position)).then(|| row.page_index.map(|index| (index, *rect))).flatten()
                }))
            }).flatten();
            if ui.input(|input| input.pointer.primary_down()) {
                if let (Some((_, rect)), Some(position)) = (target, pointer) {
                    let y = if position.y < rect.center().y { rect.top() } else { rect.bottom() };
                    ui.painter().hline(rect.x_range(), y, egui::Stroke::new(2.0, palette.accent));
                }
            } else {
                ui.data_mut(|data| data.remove::<PageRowDrag>(page_drag_id));
                if let Some((target_index, rect)) = target {
                    let before = pointer.is_some_and(|position| position.y < rect.center().y);
                    if let Some(to_index) = page_reorder_index(drag.page_index as usize, target_index as usize, before, drag.page_ids.len()) {
                        if to_index != drag.page_index as usize {
                            if self.page_row_drag_is_current(&drag) {
                                if reordered_page_ids(
                                    &drag.page_ids,
                                    drag.page_index as usize,
                                    to_index,
                                ).is_some() {
                                    if let Err(message) = self.invoke("page.reorder", json!({ "page_index": drag.page_index, "to_index": to_index })) {
                                        self.set_error(message);
                                    }
                                }
                            } else {
                                self.set_error("페이지 순서가 바뀌었습니다. 최신 목록에서 다시 시도해 주세요".into());
                            }
                        }
                    }
                }
            }
        }
        ui.data_mut(|data| data.insert_temp(open_id, overrides));
        if searching && rows.is_empty() && !flow_search_waiting {
            ui.label("일치하는 레이어가 없습니다");
        }
        self.draw_web_mappings(ui, palette);
        // The add row: 4 below the tree, in the page icon's column, `ink-2` (tokens `layer-row.add-row`).
        let add = match self.active.as_ref().map(|item| item.kind) {
            Some(ArtifactKind::Canvas) => Some(("page-add", "페이지 추가", "add.page")),
            Some(ArtifactKind::Slides) => Some(("add-slide", "슬라이드 추가", "add.slide")),
            _ => None,
        };
        if let Some((icon, label, command)) = add {
            ui.add_space(4.0);
            let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), theme::LAYER_ROW), Sense::click());
            if response.hovered() {
                ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_2);
            }
            let icon_rect = Rect::from_min_size(Pos2::new(rect.left() + 2.0 + 16.0 + 4.0, rect.center().y - 8.0), Vec2::splat(16.0));
            icons::paint(ui, icon_rect, icon, palette.ink_2);
            ui.painter().text(Pos2::new(icon_rect.right() + 4.0, rect.center().y), Align2::LEFT_CENTER, label, theme::font(theme::Type::Body), palette.ink_2);
            if response.clicked() {
                self.invoke_ui(command, json!({}));
            }
        }
        if self.active.as_ref().is_some_and(|active| active.kind == ArtifactKind::Canvas)
            && self.page_sections_write_supported()
        {
            ui.add_space(4.0);
            if widgets::text_button(ui, palette, "+ 섹션 추가", palette.ink_2).clicked() {
                self.open_page_section_for_current_page();
            }
        }
    }

    pub(super) fn apply_pending_page_reorder_selection(&mut self) {
        let Some(pending) = self.pending_page_reorder_selection.clone() else { return };
        let same_artifact = self.active.as_ref().is_some_and(|active| active.id == pending.artifact_id)
            && self.artifact.as_ref().is_some_and(|artifact| artifact.id == pending.artifact_id);
        if !same_artifact {
            self.pending_page_reorder_selection = None;
            return;
        }
        let Some(artifact) = self.artifact.as_ref() else { return };
        let Some(page_ids) = page_ids_for_body(&artifact.body) else {
            self.pending_page_reorder_selection = None;
            return;
        };
        if page_ids == pending.base_page_ids {
            let page_index = self.view_ref().page_index;
            if page_index != pending.last_page_index {
                self.remember_pending_page_reorder_selection(page_index);
            }
            return;
        }

        if page_ids != pending.expected_page_ids {
            self.pending_page_reorder_selection = None;
            if let Some(index) = pending.selected_page_id.as_deref().and_then(|page_id| page_index_for_id(&page_ids, page_id)) {
                self.view().page_index = index;
                self.page_index = index;
                self.invoke_ui("nav.page", json!({ "page_index": index }));
            }
            return;
        }

        let Some(committed_revision) = pending.committed_revision else { return };
        if artifact.revision >= committed_revision {
            if let Some(index) = pending.selected_page_id.as_deref().and_then(|page_id| {
                page_reorder_result_selection(&page_ids, &pending.expected_page_ids, page_id)
            }) {
                self.view().page_index = index;
                self.page_index = index;
                self.pending_page_reorder_selection = None;
                self.invoke_ui("nav.page", json!({ "page_index": index }));
            } else {
                self.pending_page_reorder_selection = None;
            }
        }
    }

    pub(super) fn commit_pending_page_reorder_selection(&mut self, token: &str, revision: u64) {
        let Some(pending) = self.pending_page_reorder_selection.as_mut().filter(|pending| pending.token == token) else { return };
        pending.committed_revision = Some(revision);
    }

    pub(super) fn cancel_pending_page_reorder_selection(&mut self, token: &str) {
        if self.pending_page_reorder_selection.as_ref().is_some_and(|pending| pending.token == token) {
            self.pending_page_reorder_selection = None;
        }
    }

    pub(super) fn remember_pending_page_reorder_selection(&mut self, page_index: u32) {
        let Some(mut pending) = self.pending_page_reorder_selection.clone() else { return };
        if !self.active.as_ref().is_some_and(|active| active.id == pending.artifact_id) { return; }
        let Some(artifact) = self.artifact.as_ref().filter(|artifact| artifact.id == pending.artifact_id) else { return };
        if artifact.revision < pending.base_revision { return; }
        let Some(page_ids) = page_ids_for_body(&artifact.body) else { return };
        let selection_order = if page_ids == pending.base_page_ids {
            &pending.base_page_ids
        } else if page_ids == pending.expected_page_ids {
            &pending.expected_page_ids
        } else {
            return;
        };
        let Some(page_id) = selection_order.get(page_index as usize) else { return };
        pending.selected_page_id = Some(page_id.clone());
        pending.last_page_index = page_index;
        self.pending_page_reorder_selection = Some(pending);
    }

    fn capture_page_row_drag(&self, page_index: u32) -> Option<PageRowDrag> {
        let (active, artifact, context) = (self.active.as_ref()?, self.artifact.as_ref()?, self.context.as_ref()?);
        if !matches!(active.kind, ArtifactKind::Canvas | ArtifactKind::Slides)
            || active.id != artifact.id || context.artifact_id != artifact.id
            || active.revision != artifact.revision || context.revision != artifact.revision
            || context.visible_page != self.view_ref().page_index
        {
            return None;
        }
        let page_ids = page_ids_for_body(&artifact.body)?;
        let page_id = page_ids.get(page_index as usize)?.clone();
        Some(PageRowDrag {
            artifact_id: artifact.id.clone(),
            revision: artifact.revision,
            page_id,
            page_index,
            page_ids: page_ids.clone(),
        })
    }

    fn page_row_drag_is_current(&self, drag: &PageRowDrag) -> bool {
        let (Some(active), Some(artifact), Some(context)) = (&self.active, &self.artifact, &self.context) else { return false };
        active.id == drag.artifact_id
            && artifact.id == drag.artifact_id
            && context.artifact_id == drag.artifact_id
            && active.revision == drag.revision
            && artifact.revision == drag.revision
            && context.revision == drag.revision
            && page_ids_for_body(&artifact.body).as_deref() == Some(drag.page_ids.as_slice())
            && drag.page_ids.get(drag.page_index as usize).is_some_and(|page_id| page_id == &drag.page_id)
    }

    fn layer_rows(&self) -> Vec<LayerRow> {
        let mut rows = Vec::new();
        let Some(artifact) = self.artifact.as_ref() else {
            return rows;
        };
        match &artifact.body {
            // Pages in document order (canvas-board, slides-board); nodes in reverse paint order, so the node drawn on top comes first.
            ArtifactBody::Canvas(document) => {
                let mut section_indent = false;
                for (index, page) in document.pages.iter().enumerate() {
                    if let Some(section) = super::page_sections::section_started_at(&document.page_sections, &page.id) {
                        rows.push(LayerRow {
                            depth: 0,
                            label: section.name.clone(),
                            icon: "group",
                            section_id: Some(section.id.clone()),
                            owner_page: Some(index as u32),
                            ..Default::default()
                        });
                        section_indent = true;
                    }
                    let page_row = rows.len();
                    rows.push(LayerRow {
                        depth: 0,
                        label: self.page_title(index as u32),
                        icon: "layer-page",
                        page_index: Some(index as u32),
                        section_indent,
                        has_children: !page.nodes.is_empty(),
                        ..Default::default()
                    });
                    push_nodes(&mut rows, &page.nodes, 1, index as u32);
                    if section_indent {
                        for row in &mut rows[page_row..] { row.section_indent = true; }
                    }
                }
            }
            ArtifactBody::Slides(document) => {
                for (index, page) in document.pages.iter().enumerate() {
                    rows.push(LayerRow {
                        depth: 0,
                        label: self.page_title(index as u32),
                        icon: "layer-slide",
                        page_index: Some(index as u32),
                        has_children: !page.nodes.is_empty(),
                        ..Default::default()
                    });
                    push_nodes(&mut rows, &page.nodes, 1, index as u32);
                }
            }
            // figure-board: the panels are the top level, a diagram's boxes and connectors under it.
            ArtifactBody::Figure(figure) => {
                for panel in &figure.panels {
                    let (icon, name, children) = match &panel.body {
                        designllm_contracts::PanelBody::Plot(plot) => ("ins-chart", plot.y_axis.label.clone(), 0),
                        designllm_contracts::PanelBody::Diagram(diagram) => ("ins-diagram", String::from("구조도"), diagram.nodes.len() + diagram.edges.len()),
                        designllm_contracts::PanelBody::Image(_) => ("ins-image", String::from("이미지"), 0),
                    };
                    let label = if name.trim().is_empty() { panel.label.clone() } else { format!("{} {}", panel.label, name.trim()) };
                    rows.push(LayerRow { depth: 0, label, icon, node_id: Some(panel.id.clone()), has_children: children > 0, ..Default::default() });
                    if let designllm_contracts::PanelBody::Diagram(diagram) = &panel.body {
                        for node in &diagram.nodes {
                            rows.push(LayerRow { depth: 1, label: node_label(node), search_text: node.text.clone(), icon: node_icon(node), node_id: Some(node.id.clone()), ..Default::default() });
                        }
                        let text_of = |id: &str| diagram.nodes.iter().find(|node| node.id == id).map(node_label).unwrap_or_default();
                        for edge in &diagram.edges {
                            let label = format!("{} → {}", text_of(&edge.from), text_of(&edge.to));
                            rows.push(LayerRow { depth: 1, label, icon: "tool-connector", node_id: Some(edge.id.clone()), ..Default::default() });
                        }
                    }
                }
            }
            // The outline of a text document: heading paragraphs only, indented by level (§4).
            ArtifactBody::Flow(document) => {
                for section in &document.sections {
                    for block in &section.blocks {
                        let designllm_contracts::FlowBlock::Paragraph(paragraph) = block else {
                            continue;
                        };
                        let Some(level) = paragraph.style_ref.as_deref().and_then(heading_level) else {
                            continue;
                        };
                        let text: String = paragraph.runs.iter().map(|run| run.text.as_str()).collect();
                        if text.trim().is_empty() {
                            continue;
                        }
                        rows.push(LayerRow {
                            depth: level.saturating_sub(1).min(4),
                            label: text.trim().to_owned(),
                            icon: "ins-paragraph",
                            node_id: Some(paragraph.id.clone()),
                            paragraph_text: Some(text),
                            ..Default::default()
                        });
                    }
                }
            }
            // The files of a WebCode project, in path order. A file row opens the file (web_mapping.rs).
            ArtifactBody::WebCode(document) => rows.extend(document.files.keys().map(|path| LayerRow {
                label: path.clone(),
                icon: "code",
                node_id: Some(path.clone()),
                ..Default::default()
            })),
        }
        for row in &mut rows {
            if let Some(id) = row.node_id.clone() {
                if let Some(title) = self.linked_picture_title(&id) {
                    row.label = title;
                }
            }
        }
        rows
    }

    fn selectable_layer_rows(&self) -> Vec<LayerRow> {
        let current_page = self.active.as_ref().and_then(|active| {
            matches!(active.kind, ArtifactKind::Canvas | ArtifactKind::Slides).then_some(self.view_ref().page_index)
        });
        self.layer_rows()
            .into_iter()
            .filter(|row| row.node_id.is_some() && current_page.is_none_or(|page| row.owner_page == Some(page)))
            .collect()
    }

    /// Every node row of the layers tree in its order across pages, as (node id, page, whether its name or text holds the
    /// lowercase `query`).
    pub(super) fn layer_search_rows(&self, query: &str) -> Vec<(String, u32, bool)> {
        self.layer_rows()
            .into_iter()
            .filter_map(|row| {
                let matched = row_matches(&row, query);
                Some((row.node_id?, row.owner_page?, matched))
            })
            .collect()
    }

    pub(super) fn has_selectable_layers(&self) -> bool {
        !self.selectable_layer_rows().is_empty()
    }

    /// Selects the adjacent item in the same order as the current layer tree. Canvas and Slides
    /// stay on the visible page; flowing documents cycle their outline rows.
    pub(super) fn select_adjacent_layer(&mut self, forward: bool) -> Result<(), String> {
        if !self.screen_is_editor() {
            return Err("레이어 선택은 문서 편집 화면에서만 사용할 수 있습니다".into());
        }
        let (Some(active), Some(artifact), Some(context)) = (&self.active, &self.artifact, &self.context) else {
            return Err("편집 상태를 읽은 뒤 다시 시도해 주세요".into());
        };
        if active.id != artifact.id
            || active.revision != artifact.revision
            || context.artifact_id != artifact.id
            || context.revision != artifact.revision
            || context.view_id != self.view_id
        {
            return Err("작업물 상태가 바뀌었습니다. 최신 내용을 다시 불러온 뒤 다시 시도해 주세요".into());
        }
        if self.pending.values().any(|pending| matches!(pending.method.as_str(), "selection.set" | "context.read")) {
            return Err("선택 상태를 갱신하는 중입니다. 잠시 뒤 다시 시도해 주세요".into());
        }

        let rows = self.selectable_layer_rows();
        if rows.is_empty() {
            return Err("현재 페이지에 선택할 레이어가 없습니다".into());
        }
        let selection = self.local_selection.as_ref();
        let current_node = selection
            .and_then(|selection| selection.node_ids.first())
            .or_else(|| selection.is_none().then_some(self.selected_node.as_ref()).flatten());
        if let Some(node_id) = current_node {
            let exists_in_tree = self.layer_rows().iter().any(|row| row.node_id.as_deref() == Some(node_id));
            if !exists_in_tree {
                return Err("선택한 레이어가 문서에 없습니다. 최신 내용을 다시 불러와 주세요".into());
            }
            if !rows.iter().any(|row| row.node_id.as_deref() == Some(node_id)) {
                return Err("선택한 레이어가 현재 페이지에 없습니다. 현재 페이지에서 다시 선택해 주세요".into());
            }
        }
        let current = current_node.cloned().or_else(|| selection.and_then(|selection| selection.text_range.as_ref().map(|range| range.paragraph_id.clone())));
        let current_index = current.as_deref().and_then(|id| rows.iter().position(|row| row.node_id.as_deref() == Some(id)));
        let index = match current_index {
            Some(index) if forward => (index + 1) % rows.len(),
            Some(0) if !forward => rows.len() - 1,
            Some(index) => index - 1,
            None if forward => 0,
            None => rows.len() - 1,
        };
        let row = &rows[index];
        let Some(node_id) = row.node_id.clone() else { return Ok(()) };
        if let Some(text) = &row.paragraph_text {
            self.select_paragraph(node_id, text);
        } else {
            let (command, args) = self.layer_row_command(&node_id);
            return self.invoke(command, args);
        }
        Ok(())
    }

    /// The agent's recent change batches on this document, newest first. Each one is taken back on its own
    /// with its undo icon (`ai.undo_change`).
    fn draw_agent_batches(&mut self, ui: &mut Ui, palette: &theme::Palette) {
        let batches: Vec<(String, String)> = self
            .agent_batches
            .iter()
            .rev()
            .take(5)
            .map(|batch| (batch.request_id.clone(), batch.actor.clone()))
            .collect();
        for (request_id, actor) in batches {
            let mut take_back = false;
            ui.horizontal(|ui| {
                // The undo icon keeps its place in the column. The name is cut to the width left before it, and the id
                // suffix after it stays whole (a label's truncation cuts its end, so the suffix is not part of it).
                let (name, suffix) = agent_actor_parts(&actor);
                let font = theme::display_font(false, 12.0);
                let room = ui.available_width() - theme::ICON_BUTTON - ui.spacing().item_spacing.x;
                let suffix_width = if suffix.is_empty() { 0.0 } else { ui.painter().layout_no_wrap(suffix.clone(), font.clone(), palette.ink_3).size().x };
                ui.scope(|ui| {
                    ui.set_max_width((room - suffix_width).max(0.0));
                    let text = egui::RichText::new(name).font(font.clone()).color(palette.ink_3);
                    ui.add(egui::Label::new(text).truncate().show_tooltip_when_elided(false)).on_hover_text(&actor);
                });
                if !suffix.is_empty() {
                    ui.add(egui::Label::new(egui::RichText::new(suffix).font(font).color(palette.ink_3)).extend());
                }
                if widgets::icon_button(ui, palette, "undo", "되돌리기", false).clicked() {
                    take_back = true;
                }
            });
            if take_back {
                self.invoke_ui("ai.undo_change", json!({ "change_id": request_id }));
            }
        }
    }

    /// The AI tab: the current request with its progress, then the request box. Its margin is the layers tab's, so the
    /// first element starts at the same x in both tabs.
    pub(super) fn draw_ai_tab(&mut self, ui: &mut Ui, palette: &theme::Palette) {
        egui::Frame::new().inner_margin(egui::Margin::same(theme::TREE_PADDING as i8)).show(ui, |ui| {
            // The sidebar lays out its tab strip with zero item spacing and the tab inherits it; the tab's rows use the
            // window's spacing, so a title and its runner's name, or the request box and its scope, do not touch.
            ui.spacing_mut().item_spacing = ui.ctx().style().spacing.item_spacing;
            self.draw_ai_tab_contents(ui, palette);
        });
    }

    fn draw_ai_tab_contents(&mut self, ui: &mut Ui, palette: &theme::Palette) {
        if let Some(job) = self.job.clone() {
            let state = super::chrome::job_agent_state(job.status);
            ui.horizontal(|ui| {
                widgets::avatar(ui, palette, agent_letter(&self.active_runtime()), palette.agents[0], state, 18.0);
                // The request the job was started with; a job this window did not start (another client's, or one started
                // before the window opened) shows the general label, not what the request box holds now.
                let label = self.job_prompts.get(&job.id).map_or(JOB_LABEL, String::as_str);
                // The runner's name keeps its width (up to half the row) and the title is cut to what is left, so a long
                // request neither hides the name nor runs past the column.
                let runner_font = theme::display_font(false, 12.0);
                let runner = job.client_name.as_deref().map(|client| {
                    let width = ui.painter().layout_no_wrap(client.to_owned(), runner_font.clone(), palette.ink_3).size().x.ceil();
                    (client, width.min(ui.available_width() / 2.0))
                });
                let title_width = ui.available_width() - runner.map_or(0.0, |(_, width)| width + ui.spacing().item_spacing.x);
                ui.scope(|ui| {
                    ui.set_max_width(title_width.max(0.0));
                    ui.add(egui::Label::new(egui::RichText::new(label).font(theme::display_font(false, 13.0)).color(palette.ink)).truncate());
                });
                if let Some((client, width)) = runner {
                    ui.scope(|ui| {
                        ui.set_max_width(width);
                        ui.add(egui::Label::new(egui::RichText::new(client).font(runner_font).color(palette.ink_3)).truncate());
                    });
                }
            });
            if super::chrome::job_status_running(job.status) {
                let stages = match job.status {
                    JobStatus::Queued => [widgets::Stage::Current(0.0), widgets::Stage::Pending, widgets::Stage::Pending],
                    JobStatus::Running => [widgets::Stage::Done, widgets::Stage::Current(0.5), widgets::Stage::Pending],
                    _ => [widgets::Stage::Done, widgets::Stage::Done, widgets::Stage::Current(0.5)],
                };
                widgets::progress_rail(ui, palette, stages, palette.agents[0]);
                if widgets::text_button(ui, palette, "■ 취소", palette.danger).clicked() {
                    self.invoke_ui("ai.cancel", json!({}));
                }
            } else if matches!(job.status, JobStatus::Failed | JobStatus::Conflict) && widgets::text_button(ui, palette, "⟳ 다시 요청", palette.accent_ink).clicked() {
                self.invoke_ui("ai.retry", json!({}));
            }
            // Why a failed job failed: its error, in the words the window uses for errors (a change outside the job's targets
            // is CONTENT_LOCKED).
            if let Some(error) = job.error.as_ref().filter(|_| matches!(job.status, JobStatus::Failed | JobStatus::Conflict)) {
                ui.label(egui::RichText::new(super::user_error(error)).font(theme::display_font(false, 12.0)).color(palette.danger));
            }
            widgets::divider(ui, palette);
        }
        self.draw_other_running_jobs(ui, palette);
        self.draw_agent_batches(ui, palette);
        ui.add_space(8.0);
        let mut prompt = self.ai_prompt.clone();
        let response = widgets::text_field(ui, palette, &mut prompt, "요청", ui.available_width());
        if std::mem::take(&mut self.ai_focus_request) {
            response.request_focus();
        }
        if response.changed() {
            self.ai_prompt = prompt;
        }
        // The scope the request carries: the whole artifact, the selected targets, or the words of the refusal that holds it.
        let scope = self.ai_scope_label();
        ui.label(egui::RichText::new(scope).font(theme::display_font(false, 12.0)).color(palette.ink_3));
        self.draw_ai_figure_refs(ui);
        self.draw_figure_ai_attachments(ui);
        ui.horizontal(|ui| {
            // The segments are the whole set the request sends. With the data intent on (an agent's choice, not a segment),
            // no segment is raised.
            let selected = match (self.allow_content, self.allow_design, self.allow_data) {
                (false, true, false) => 1,
                (true, false, false) => 2,
                (true, true, false) => 0,
                _ => usize::MAX,
            };
            if let Some(index) = widgets::segmented(ui, palette, &["내용+디자인", "디자인만", "내용만"], selected) {
                let args = match index {
                    1 => json!({ "mode": "design_only" }),
                    2 => json!({ "mode": "content_only" }),
                    _ => json!({ "mode": "both" }),
                };
                self.invoke_ui("ai.toggle_intent", args);
            }
        });
        self.draw_proposals(ui, palette);
        // The runner stays as it is while a job runs or its evidence is imported; the request control shows why it waits.
        let wait = self.ai_job_wait();
        let block = self.ai_request_block(&self.ai_prompt);
        ui.horizontal(|ui| {
            // The same words and menu as the home row's runner (`runner_labels`, `runner_popover`), on one line.
            let (runtime, model) = self.runner_labels();
            let runner_label = format!("{runtime} · {model}");
            let dropdown_width = (ui.available_width() - 76.0).clamp(120.0, 220.0);
            let runner_response = ui
                .add_enabled_ui(wait.is_none(), |ui| widgets::dropdown(ui, palette, &runner_label, None, dropdown_width))
                .inner;
            let model_draft_id = egui::Id::new(super::home::AI_RUNNER_DRAFT);
            for change in self.runner_popover(ui, palette, &runner_response, model_draft_id) {
                self.invoke_ui("home.runner", change);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let send = ui.add_enabled_ui(block.is_none(), |ui| widgets::labeled_button(ui, palette, Some("send"), "요청", true).clicked());
                if send.inner {
                    let prompt = self.ai_prompt.clone();
                    self.invoke_ui("ai.request", json!({ "prompt": prompt }));
                }
            });
        });
        // The reason the control is off. An empty prompt needs no reason: the field itself asks for it.
        if let Some(reason) = block.filter(|_| !self.ai_prompt.trim().is_empty()) {
            ui.label(egui::RichText::new(reason).font(theme::display_font(false, 12.0)).color(palette.ink_3));
        }
    }

    /// Explicitly pins the exact Figure revisions available to the AI request.
    pub(super) fn draw_ai_figure_refs(&mut self, ui: &mut Ui) {
        let palette = &theme::palette(ui.ctx());
        let artifacts = &self.artifacts;
        let refs = &mut self.ai_figure_refs;
        if !artifacts.iter().any(|summary| summary.kind == ArtifactKind::Figure) && refs.is_empty() { return; }

        let mut remove = None;
        let mut additions = Vec::new();
        let selected_count = refs.len();
        ui.collapsing(format!("그림 참조 ({selected_count})"), |ui| {
            ui.label("선택한 Figure ID와 버전만 이 AI 작업에 승인됩니다. 파일 첨부는 별도 source입니다.");
            for (index, reference) in refs.iter().enumerate() {
                let title = artifacts.iter().find(|summary| summary.id == reference.figure_id)
                    .map_or(reference.figure_id.as_str(), |summary| summary.title.as_str());
                ui.horizontal(|ui| {
                    ui.label(format!("{title} · 버전 {}", reference.figure_revision));
                    if widgets::compact_button(ui, palette, "빼기").clicked() { remove = Some(index); }
                });
            }
            if selected_count < MAX_JOB_FIGURE_REFS {
                for summary in artifacts.iter().filter(|summary| summary.kind == ArtifactKind::Figure) {
                    if refs.iter().any(|reference| reference.figure_id == summary.id && reference.figure_revision == summary.revision) { continue; }
                    let mut checked = false;
                    if widgets::checkbox(ui, palette, &mut checked, &format!("{} · 버전 {} 승인", summary.title, summary.revision)).changed() && checked {
                        additions.push(FigureRevisionRef { figure_id: summary.id.clone(), figure_revision: summary.revision });
                    }
                }
            } else {
                ui.label("그림 참조는 한 작업에 128개까지 선택할 수 있습니다.");
            }
        });
        if let Some(index) = remove { refs.remove(index); }
        for reference in additions {
            if refs.len() == MAX_JOB_FIGURE_REFS { break; }
            if !refs.iter().any(|selected| selected == &reference) { refs.push(reference); }
        }
    }
}

#[cfg(test)]
mod layer_find_tests {
    use super::*;

    #[test]
    fn layer_find_keeps_the_matching_node_and_its_ancestors() {
        let rows = vec![
            LayerRow { depth: 0, label: "페이지 1".into(), page_index: Some(0), has_children: true, ..Default::default() },
            LayerRow { depth: 1, label: "프레임".into(), node_id: Some("frame".into()), has_children: true, ..Default::default() },
            LayerRow { depth: 2, label: "Hero".into(), search_text: "Primary Button".into(), node_id: Some("button".into()), ..Default::default() },
            LayerRow { depth: 0, label: "페이지 2".into(), page_index: Some(1), has_children: true, ..Default::default() },
            LayerRow { depth: 1, label: "Secondary".into(), node_id: Some("secondary".into()), ..Default::default() },
        ];
        let filtered = filter_rows(rows, "button", None);
        assert_eq!(filtered.iter().map(|row| row.label.as_str()).collect::<Vec<_>>(), ["페이지 1", "프레임", "Hero"]);
    }

    #[test]
    fn layer_find_searches_current_page_text_and_layer_names_in_korean() {
        let node = |id: &str, name: &str| {
            serde_json::from_value::<Node>(serde_json::json!({
                "id": id,
                "kind": "text",
                "bounds": { "x": 0, "y": 0, "width": 100, "height": 100 },
                "text": "주문을 완료하는 버튼",
                "name": name,
            }))
            .expect("a text node with a layer name")
        };
        let mut rows = Vec::new();
        for (page, title, layer_name) in [(0, "페이지 1", "CTA 레이어"), (1, "페이지 2", "본문 레이어")] {
            rows.push(LayerRow { depth: 0, label: title.into(), page_index: Some(page), has_children: true, ..Default::default() });
            push_nodes(&mut rows, &[node(if page == 0 { "cta" } else { "body" }, layer_name)], 1, page);
        }

        for query in ["주문을 완료", "CTA 레이어"] {
            let found = filter_rows(rows.clone(), query, Some(0));
            assert_eq!(found.iter().map(|row| row.label.as_str()).collect::<Vec<_>>(), ["페이지 1", "CTA 레이어"], "{query}");
        }
        let all_pages = filter_rows(rows, "주문을 완료", None);
        assert_eq!(
            all_pages.iter().map(|row| row.label.as_str()).collect::<Vec<_>>(),
            ["페이지 1", "CTA 레이어", "페이지 2", "본문 레이어"]
        );
    }

    #[test]
    fn flow_find_uses_grapheme_ranges_runs_protected_atoms_and_nested_tables() {
        use designllm_contracts::{FlowDocument, Paragraph, ParagraphFormat, Section, Style, Table, TableCell, TextRun};

        let run = |id: &str, text: &str, protected: bool| TextRun { protected, ..TextRun::new(id, text) };
        let paragraph = |id: &str, runs: Vec<TextRun>| Paragraph { id: id.into(), runs, style_ref: None, format: ParagraphFormat::default() };
        let document = FlowDocument {
            sections: vec![Section {
                id: "section".into(),
                setup: designllm_contracts::PageSetup::default(),
                blocks: vec![
                    FlowBlock::Paragraph(paragraph("unicode", vec![run("r1", "A👩‍👩‍👧‍👦e", false), run("r2", "\u{301}한글", false)])),
                    FlowBlock::Paragraph(paragraph("protected", vec![run("r3", "보이는", false), run("r4", "비밀값", true), run("r5", "본문", false)])),
                    FlowBlock::Table(Table {
                        id: "table".into(),
                        column_widths: vec![9525],
                        rows: vec![vec![TableCell {
                            id: "cell".into(),
                            blocks: vec![FlowBlock::Paragraph(paragraph("cell-paragraph", vec![run("r6", "표 안의 문장", false)]))],
                            column_span: 1,
                            row_span: 1,
                        }]],
                        style: Style::default(),
                    }),
                ],
            }],
            styles: Default::default(),
        };

        let unicode = flow_find_matches(&document, "e\u{301}");
        assert_eq!(unicode.len(), 1);
        assert_eq!(unicode[0].paragraph_id, "unicode");
        assert_eq!((unicode[0].range.start, unicode[0].range.end), (2, 3), "the family emoji is one grapheme and the match crosses runs");
        assert!(flow_find_matches(&document, "\u{301}").is_empty(), "a query cannot select only part of a grapheme");
        assert!(flow_find_matches(&document, "비밀값").is_empty(), "protected inline text is not searchable");
        let table = flow_find_matches(&document, "표 안의 문장");
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].paragraph_id, "cell-paragraph", "nested table paragraphs are included");
        assert!(flow_find_matches(&document, "  ").is_empty(), "an empty query does not match every paragraph");
    }

    #[test]
    fn flow_find_navigation_keeps_occurrence_ranges_compact_and_skips_protected_runs() {
        use designllm_contracts::{Paragraph, ParagraphFormat, TextRun};
        let run = |id: &str, text: &str, protected: bool| TextRun { protected, ..TextRun::new(id, text) };
        let repeated = Paragraph {
            id: "repeated".into(),
            runs: vec![run("r1", "aa", false), run("r2", "aa", true), run("r3", "aa", false)],
            style_ref: None,
            format: ParagraphFormat::default(),
        };
        let logical = LogicalParagraph::new(&repeated);
        assert_eq!(logical.text, "aa\u{FFFC}aa", "protected source text remains an inline atom");
        assert_eq!(logical_match_range_from(&logical, "aa", 0), Some((0, 2)));
        assert_eq!(logical_match_range_from(&logical, "aa", 1), Some((3, 5)), "next skips the protected run");
        assert_eq!(logical_match_range_before(&logical, "aa", 3), Some((0, 2)), "previous returns the prior grapheme range");
        assert_eq!(logical_match_range_before(&logical, "aa", 0), None);

        let overlapping = LogicalParagraph::new(&Paragraph {
            id: "overlapping".into(),
            runs: vec![run("r4", "aaa", false)],
            style_ref: None,
            format: ParagraphFormat::default(),
        });
        assert_eq!(logical_match_range_from(&overlapping, "aa", 0), Some((0, 2)));
        assert_eq!(logical_match_range_from(&overlapping, "aa", 1), Some((1, 3)), "overlapping matches keep distinct ranges");
    }

    #[test]
    fn flow_find_page_mapping_uses_logical_source_ranges() {
        let hit = FlowFindMatch {
            paragraph_id: "p1".into(),
            text: "찾을 본문".into(),
            range: TextRange { paragraph_id: "p1".into(), start: 4, end: 8 },
            page_index: None,
        };
        let page = |index, range: Option<TextRange>| RenderPage {
            id: format!("page-{index}"),
            page_index: index,
            size: designllm_contracts::Size::a4(),
            primitives: Vec::new(),
            source_map: range.into_iter().map(|logical_range| designllm_contracts::SourceRegion {
                node_id: "run-1".into(),
                bounds: designllm_contracts::Rect { x: 0, y: 0, width: 10, height: 10 },
                logical_range: Some(logical_range),
                grapheme_rects: Vec::new(),
            }).collect(),
        };
        assert!(!page(0, Some(TextRange { paragraph_id: "p1".into(), start: 0, end: 4 })).source_map.iter().any(|region| {
            region.logical_range.as_ref().is_some_and(|range| range.paragraph_id == hit.paragraph_id && range.start <= hit.range.start && hit.range.start < range.end)
        }));
        assert!(page(2, Some(TextRange { paragraph_id: "p1".into(), start: 4, end: 9 })).source_map.iter().any(|region| {
            region.logical_range.as_ref().is_some_and(|range| range.paragraph_id == hit.paragraph_id && range.start <= hit.range.start && hit.range.start < range.end)
        }));
        assert!(!page(1, Some(TextRange { paragraph_id: "other".into(), start: 4, end: 9 })).source_map.iter().any(|region| {
            region.logical_range.as_ref().is_some_and(|range| range.paragraph_id == hit.paragraph_id && range.start <= hit.range.start && hit.range.start < range.end)
        }));
    }

    #[test]
    fn layer_state_hidden_and_locked_nodes_remain_in_the_layer_tree() {
        let node: Node = serde_json::from_value(serde_json::json!({
            "id": "hidden-node",
            "kind": "text",
            "bounds": { "x": 0, "y": 0, "width": 100, "height": 100 },
            "text": "hidden",
            "hidden": true,
            "locked": true,
        }))
        .expect("a hidden and locked node");
        let mut rows = Vec::new();
        push_nodes(&mut rows, &[node], 1, 0);

        assert_eq!(rows.len(), 1, "hidden nodes stay in the layer tree");
        assert_eq!(rows[0].node_id.as_deref(), Some("hidden-node"));
        assert!(rows[0].hidden);
        assert!(rows[0].locked);
    }
}

/// The outline level of a paragraph style (`Title` and `Heading1`…, `heading 2`, `제목 3`, `개요 1`), or none for body text.
fn heading_level(style: &str) -> Option<usize> {
    let lower = style.trim().to_lowercase();
    if lower == "title" || lower == "제목" {
        return Some(1);
    }
    let rest = ["heading", "제목", "개요"].iter().find_map(|prefix| lower.strip_prefix(prefix))?;
    let digits: String = rest.trim().chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<usize>().ok().filter(|level| *level >= 1)
}

/// The key a row is known by (renaming, open state): its node id, or `page:<index>`.
fn row_key(row: &LayerRow) -> String {
    if let Some(section_id) = &row.section_id { return format!("section:{section_id}"); }
    match (&row.node_id, row.page_index) {
        (Some(id), _) => id.clone(),
        (None, Some(index)) => format!("page:{index}"),
        _ => String::new(),
    }
}

/// How a row relates to the selection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RowMark {
    selected: bool,
    /// A row on the way down to a selected row.
    ancestor: bool,
    /// A row under a selected row.
    descendant: bool,
}

impl RowMark {
    /// Whether a reserved node sits under the row at `position`.
    fn holds_reserved(&self, rows: &[LayerRow], position: usize, reserved: &std::collections::BTreeSet<String>) -> bool {
        let depth = rows[position].depth;
        rows[position + 1..]
            .iter()
            .take_while(|row| row.depth > depth)
            .any(|row| row.node_id.as_ref().is_some_and(|id| reserved.contains(id)))
    }
}

/// The selection marks of every row, in row order (rows are a depth-first list).
fn row_marks(rows: &[LayerRow], is_selected: impl Fn(&LayerRow) -> bool) -> Vec<RowMark> {
    let mut marks = vec![RowMark::default(); rows.len()];
    let mut path: Vec<usize> = Vec::new();
    let mut under: Option<usize> = None;
    for (index, row) in rows.iter().enumerate() {
        while path.last().is_some_and(|&parent| rows[parent].depth >= row.depth) {
            path.pop();
        }
        if under.is_some_and(|depth| row.depth <= depth) {
            under = None;
        }
        if is_selected(row) {
            marks[index].selected = true;
            for &parent in &path {
                marks[parent].ancestor = true;
            }
            under.get_or_insert(row.depth);
        } else if under.is_some() {
            marks[index].descendant = true;
        }
        path.push(index);
    }
    marks
}

/// A layer dragged to a new place among its page's children.
#[derive(Clone)]
struct LayerDrag {
    node_id: String,
    page: u32,
}

#[derive(Clone)]
struct PageRowDrag {
    artifact_id: String,
    revision: u64,
    page_id: String,
    page_index: u32,
    page_ids: Vec<String>,
}

pub(super) fn page_ids_for_body(body: &ArtifactBody) -> Option<Vec<String>> {
    match body {
        ArtifactBody::Canvas(document) => Some(document.pages.iter().map(|page| page.id.clone()).collect()),
        ArtifactBody::Slides(document) => Some(document.pages.iter().map(|page| page.id.clone()).collect()),
        _ => None,
    }
}

/// The paragraphs an outline row's click selects together (`select.many`), or `None` when the click selects the row's paragraph
/// alone as a text range. With Shift or Ctrl (`additive`) the paragraph `id` joins the ones chosen so far, or leaves them: the
/// selected paragraphs, else the one selected as a text range (`text_paragraph`).
fn outline_many(selected: &[String], text_paragraph: Option<&str>, id: &str, additive: bool) -> Option<Vec<String>> {
    if !additive {
        return None;
    }
    let chosen: Vec<String> = if selected.is_empty() { text_paragraph.map(str::to_owned).into_iter().collect() } else { selected.to_vec() };
    Some(super::multi::toggle_id(&chosen, id))
}

fn page_index_for_id(page_ids: &[String], page_id: &str) -> Option<u32> {
    page_ids.iter().position(|candidate| candidate == page_id).and_then(|index| u32::try_from(index).ok())
}

#[cfg(test)]
fn page_id_at(page_ids: &[String], page_index: u32) -> Option<String> {
    page_ids.get(page_index as usize).cloned()
}

fn page_reorder_result_selection(current: &[String], expected: &[String], selected_page_id: &str) -> Option<u32> {
    (current == expected).then(|| page_index_for_id(current, selected_page_id)).flatten()
}

fn reordered_page_ids(page_ids: &[String], from: usize, to: usize) -> Option<Vec<String>> {
    if from >= page_ids.len() || to >= page_ids.len() || from == to {
        return None;
    }
    let mut reordered = page_ids.to_vec();
    let page_id = reordered.remove(from);
    reordered.insert(to, page_id);
    Some(reordered)
}

fn page_reorder_index(from: usize, target: usize, before: bool, count: usize) -> Option<usize> {
    if from >= count || target >= count || from == target { return None; }
    Some(match (from < target, before) {
        (true, true) => target - 1,
        (true, false) => target,
        (false, true) => target,
        (false, false) => target + 1,
    })
}

/// One child of a page as the layers list shows it: its node, its page, its z-order index (0 is the back)
/// and its row on the screen.
struct LayerSlot {
    node_id: String,
    page: u32,
    index: u32,
    rect: Rect,
}

/// The children of each page in list order. The list shows the front child first, so the first row of a
/// page with `count` children has z-order index `count - 1`.
fn child_slots(rows: &[LayerRow], rects: &[Rect]) -> Vec<LayerSlot> {
    let mut slots = Vec::new();
    let mut page = 0;
    let mut block: Vec<(String, Rect)> = Vec::new();
    let flush = |block: &mut Vec<(String, Rect)>, page: u32, slots: &mut Vec<LayerSlot>| {
        let count = block.len();
        for (position, (node_id, rect)) in block.drain(..).enumerate() {
            slots.push(LayerSlot { node_id, page, index: (count - 1 - position) as u32, rect });
        }
    };
    for (row, rect) in rows.iter().zip(rects) {
        if let Some(index) = row.page_index {
            flush(&mut block, page, &mut slots);
            page = index;
        } else if row.depth == 1 {
            if let Some(id) = &row.node_id {
                block.push((id.clone(), *rect));
            }
        }
    }
    flush(&mut block, page, &mut slots);
    slots
}

/// A layer being renamed in place: the row's key (a node id, or `page:<index>`) and the text.
pub(super) struct LayerRename {
    pub key: String,
    pub text: String,
}

impl DesktopApp {
    /// The name field of a renamed row. Enter or leaving the field commits: a node is renamed with
    /// `edit.rename`, a page with `props.rename_page` (after going to that page).
    fn draw_rename_field(&mut self, ui: &mut Ui, rect: Rect, x: f32, palette: &theme::Palette) {
        let Some(mut rename) = self.layer_rename.take() else {
            return;
        };
        let field = Rect::from_min_max(Pos2::new(x, rect.top() + 2.0), Pos2::new(rect.right() - 6.0, rect.bottom() - 2.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(field));
        let response = widgets::edit_line(&mut child, palette, &mut rename.text, widgets::Field {
            width: field.width(),
            height: Some(field.height()),
            margin: Some(egui::Margin::symmetric(4, 2)),
            ..Default::default()
        });
        if !response.lost_focus() {
            response.request_focus();
            self.layer_rename = Some(rename);
            return;
        }
        let name = rename.text.trim().to_owned();
        if name.is_empty() {
            return;
        }
        if let Some(index) = rename.key.strip_prefix("page:").and_then(|value| value.parse::<u32>().ok()) {
            self.invoke_ui("nav.page", json!({ "page_index": index }));
            self.invoke_ui("props.rename_page", json!({ "name": name }));
        } else {
            self.invoke_ui("edit.rename", json!({ "node_id": rename.key, "name": name }));
        }
    }
}

#[cfg(test)]
mod layer_drag_tests {
    use super::*;

    fn row(depth: usize, node_id: Option<&str>, page_index: Option<u32>) -> LayerRow {
        LayerRow { depth, icon: "layer-page", node_id: node_id.map(str::to_owned), page_index, ..Default::default() }
    }

    fn rects(count: usize) -> Vec<Rect> {
        vec![Rect::from_min_size(Pos2::ZERO, Vec2::splat(10.0)); count]
    }

    #[test]
    fn the_front_child_of_a_page_has_the_highest_z_index() {
        let rows = vec![row(0, None, Some(0)), row(1, Some("c"), None), row(1, Some("b"), None), row(1, Some("a"), None)];
        let slots: Vec<(String, u32)> = child_slots(&rows, &rects(rows.len())).into_iter().map(|slot| (slot.node_id, slot.index)).collect();
        assert_eq!(slots, [("c".to_owned(), 2), ("b".to_owned(), 1), ("a".to_owned(), 0)]);
    }

    #[test]
    fn each_page_numbers_only_its_own_children() {
        let rows = vec![row(0, None, Some(1)), row(1, Some("y"), None), row(0, None, Some(0)), row(1, Some("x"), None)];
        let slots: Vec<(u32, u32)> = child_slots(&rows, &rects(rows.len())).into_iter().map(|slot| (slot.page, slot.index)).collect();
        assert_eq!(slots, [(1, 0), (0, 0)]);
    }

    #[test]
    fn page_row_drop_uses_final_index_after_removing_the_source_page() {
        assert_eq!(page_reorder_index(0, 2, true, 3), Some(1), "drop before a later page uses its post-removal index");
        assert_eq!(page_reorder_index(0, 2, false, 3), Some(2), "drop after a later page uses its post-removal index");
        assert_eq!(page_reorder_index(2, 0, true, 3), Some(0), "drop before an earlier page moves to the front");
        assert_eq!(page_reorder_index(2, 0, false, 3), Some(1), "drop after an earlier page uses its post-removal index");
        assert_eq!(page_reorder_index(1, 1, false, 3), None, "dropping on the source page is a no-op");
        let before = ["a", "b", "c"].map(str::to_owned);
        let after = reordered_page_ids(&before, 0, 2).expect("the requested order changes");
        assert_eq!(after, ["b", "c", "a"]);
        let initially_selected = page_id_at(&before, 1).expect("initially selected B");
        let selected_page_id = page_id_at(&before, 2).expect("selection changes to C before the response");
        assert_eq!(initially_selected, "b");
        assert_eq!(selected_page_id, "c");
        assert_eq!(page_reorder_result_selection(&after, &after, &selected_page_id), Some(1), "the latest selected page follows its ID in the committed order");
        assert_eq!(page_reorder_result_selection(&before, &after, &selected_page_id), None, "an unrelated revision cannot be mistaken for the requested reorder");
    }

    /// An outline click with Shift or Ctrl adds the heading to the chosen paragraphs (the one selected as a text range counts) or
    /// takes it out; a plain click selects the heading alone as a text range.
    #[test]
    fn an_additive_outline_click_adds_or_removes_the_paragraph() {
        let ids = |list: &[&str]| list.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        assert_eq!(outline_many(&[], Some("p1"), "p2", false), None, "a plain click selects the text range");
        assert_eq!(outline_many(&[], Some("p1"), "p2", true), Some(ids(&["p1", "p2"])), "the text-range paragraph counts");
        assert_eq!(outline_many(&[], None, "p2", true), Some(ids(&["p2"])));
        assert_eq!(outline_many(&ids(&["p1", "p2"]), None, "p3", true), Some(ids(&["p1", "p2", "p3"])));
        assert_eq!(outline_many(&ids(&["p1", "p2"]), None, "p1", true), Some(ids(&["p2"])), "a chosen paragraph leaves");
    }

    #[test]
    fn heading_styles_give_outline_levels_and_body_text_none() {
        assert_eq!(heading_level("Heading1"), Some(1));
        assert_eq!(heading_level("heading 2"), Some(2));
        assert_eq!(heading_level("Title"), Some(1));
        assert_eq!(heading_level("제목 3"), Some(3));
        assert_eq!(heading_level("개요 1"), Some(1));
        assert_eq!(heading_level("Normal"), None);
        assert_eq!(heading_level("HeadingX"), None);
    }

    #[test]
    fn the_rows_on_the_way_to_the_selection_open_and_the_rows_under_it_are_tinted() {
        // page 0 › frame f › (a, b); page 1 › c. `f` is selected.
        let rows = vec![row(0, None, Some(0)), row(1, Some("f"), None), row(2, Some("a"), None), row(2, Some("b"), None), row(0, None, Some(1)), row(1, Some("c"), None)];
        let marks = row_marks(&rows, |row| row.node_id.as_deref() == Some("f"));
        assert!(marks[0].ancestor && !marks[0].selected);
        assert!(marks[1].selected);
        assert!(marks[2].descendant && marks[3].descendant);
        assert_eq!(marks[4], RowMark::default());
        assert_eq!(marks[5], RowMark::default());
    }
}

#[cfg(test)]
#[path = "ai_tab_tests.rs"]
mod ai_tab_tests;

/// Task 68 item 13 (U06): a rename names the node it was given (a layer row's `node_id`), or the selection without one.
#[cfg(test)]
mod layer_rename_tests {
    use super::*;
    use crate::app::{
        link_flow_tests::canvas_with_three_boxes,
        test_support::{Host, Window},
    };
    use eframe::egui::{Event, Key, Modifiers};

    fn rectangle_id(host: &mut Host, canvas: &str) -> String {
        let artifact = host.artifact(canvas);
        let ArtifactBody::Canvas(document) = &artifact.body else {
            panic!("a canvas");
        };
        let node = document.pages[0].nodes.iter().find(|node| matches!(node.kind, NodeKind::Rectangle)).expect("the rectangle is on the page");
        node.id.clone()
    }

    fn name_of(host: &mut Host, canvas: &str, id: &str) -> Option<String> {
        let artifact = host.artifact(canvas);
        super::super::doc_tree::find_node(&artifact, id).and_then(|node| node.ext.name.clone())
    }

    /// A canvas with text boxes `a`, `b`, `c` and one rectangle, opened in a window.
    fn opened_with_rectangle(host: &mut Host) -> (Window, String, String) {
        let canvas = canvas_with_three_boxes(host, [0, 1_000_000, 2_000_000]);
        let mut window = Window::open(host, &canvas);
        window.step(host, "add.rect", json!({})).expect("a rectangle is added");
        let rect = rectangle_id(host, &canvas);
        (window, canvas, rect)
    }

    /// The layers tab's rename of row `key`: the field takes `name`, and Enter commits it.
    fn rename_layer_row(window: &mut Window, host: &mut Host, key: &str, name: &str) {
        window.app.layer_rename = Some(LayerRename { key: key.to_owned(), text: name.to_owned() });
        window.draw(host, 1.0);
        let enter = Event::Key { key: Key::Enter, physical_key: Some(Key::Enter), pressed: true, repeat: false, modifiers: Modifiers::default() };
        window.frame(host, 1.0, vec![enter]);
        window.settle(host);
    }

    #[test]
    fn a_layer_rename_names_its_own_row_and_leaves_the_selected_text() {
        let mut host = Host::start();
        let (mut window, canvas, rect) = opened_with_rectangle(&mut host);
        window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the text is selected");

        rename_layer_row(&mut window, &mut host, &rect, "테스트");

        assert_eq!(name_of(&mut host, &canvas, &rect).as_deref(), Some("테스트"), "the rectangle row is renamed");
        assert_eq!(name_of(&mut host, &canvas, "a"), None, "the selected text keeps its name");
    }

    #[test]
    fn a_rename_without_a_node_id_names_the_selection() {
        let mut host = Host::start();
        let (mut window, canvas, rect) = opened_with_rectangle(&mut host);
        window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the text is selected");

        window.step(&mut host, "edit.rename", json!({ "name": "선택 대상" })).expect("the selection is renamed");

        assert_eq!(name_of(&mut host, &canvas, "a").as_deref(), Some("선택 대상"));
        assert_eq!(name_of(&mut host, &canvas, &rect), None);
    }

    #[test]
    fn a_rename_of_a_missing_node_is_refused_and_changes_nothing() {
        let mut host = Host::start();
        let (mut window, canvas, _rect) = opened_with_rectangle(&mut host);
        window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the text is selected");
        let revision = host.artifact(&canvas).revision;

        let result = window.step(&mut host, "edit.rename", json!({ "node_id": "missing", "name": "테스트" }));

        assert!(result.is_err(), "a node that does not exist is refused");
        assert_eq!(host.artifact(&canvas).revision, revision, "no change is applied");
        assert_eq!(name_of(&mut host, &canvas, "a"), None, "the selection keeps its name");
    }
}
