//! Multi-select on the board (state-multiselect mockup): shift or ctrl click adds to the selection, a drag
//! that starts on empty board draws a marquee, and the selection's shared box carries align, distribute and
//! group. The host's selection (`selection.set`) holds the ids; the first id is the primary node.

use std::collections::BTreeMap;

use designllm_contracts::{ArtifactBody, Intent, Node, Operation, Primitive, Rect as ModelRect, RenderPage};
use eframe::egui::{self, Context, CornerRadius, Pos2, Rect, Response, Stroke, StrokeKind, Ui};

use super::DesktopApp;
use crate::{board, theme};

/// The edges an align can share: the shared box's left, centre, right, top, middle or bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Edge {
    Left,
    HCenter,
    Right,
    Top,
    VCenter,
    Bottom,
}

impl Edge {
    pub(super) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "left" => Self::Left,
            "h-center" => Self::HCenter,
            "right" => Self::Right,
            "top" => Self::Top,
            "v-center" => Self::VCenter,
            "bottom" => Self::Bottom,
            _ => return None,
        })
    }
}

/// The smallest rectangle that holds every rectangle; None when there are none.
pub(super) fn union_of(rects: &[ModelRect]) -> Option<ModelRect> {
    let left = rects.iter().map(|rect| rect.x).min()?;
    let top = rects.iter().map(|rect| rect.y).min()?;
    let right = rects.iter().map(|rect| rect.x + rect.width).max()?;
    let bottom = rects.iter().map(|rect| rect.y + rect.height).max()?;
    Some(ModelRect { x: left, y: top, width: right - left, height: bottom - top })
}

/// The rectangles moved so that they share one edge of their union.
pub(super) fn align_rects(rects: &[ModelRect], edge: Edge) -> Vec<ModelRect> {
    let Some(shared) = union_of(rects) else {
        return Vec::new();
    };
    rects
        .iter()
        .map(|rect| match edge {
            Edge::Left => ModelRect { x: shared.x, ..*rect },
            Edge::HCenter => ModelRect { x: shared.x + (shared.width - rect.width) / 2, ..*rect },
            Edge::Right => ModelRect { x: shared.x + shared.width - rect.width, ..*rect },
            Edge::Top => ModelRect { y: shared.y, ..*rect },
            Edge::VCenter => ModelRect { y: shared.y + (shared.height - rect.height) / 2, ..*rect },
            Edge::Bottom => ModelRect { y: shared.y + shared.height - rect.height, ..*rect },
        })
        .collect()
}

/// The rectangles spaced evenly along one axis. The outermost two stay where they are.
pub(super) fn distribute_rects(rects: &[ModelRect], horizontal: bool) -> Vec<ModelRect> {
    let count = rects.len();
    if count < 3 {
        return rects.to_vec();
    }
    let start = |rect: &ModelRect| if horizontal { rect.x } else { rect.y };
    let extent = |rect: &ModelRect| if horizontal { rect.width } else { rect.height };
    let mut order: Vec<usize> = (0..count).collect();
    order.sort_by_key(|&index| start(&rects[index]));
    let first = start(&rects[order[0]]);
    let last = rects[order[count - 1]];
    let span = start(&last) + extent(&last) - first;
    let total: i64 = rects.iter().map(extent).sum();
    let free = span - total;
    let mut placed = rects.to_vec();
    let mut before = 0_i64;
    for (position, &index) in order.iter().enumerate() {
        let at = first + before + free * position as i64 / (count as i64 - 1);
        let rect = rects[index];
        placed[index] = if horizontal { ModelRect { x: at, ..rect } } else { ModelRect { y: at, ..rect } };
        before += extent(&rect);
    }
    placed
}

/// The ids with `id` added, or removed when it is already there.
pub(super) fn toggle_id(ids: &[String], id: &str) -> Vec<String> {
    if ids.iter().any(|kept| kept == id) {
        ids.iter().filter(|kept| kept.as_str() != id).cloned().collect()
    } else {
        ids.iter().cloned().chain(std::iter::once(id.to_owned())).collect()
    }
}

/// A node that holds others as one piece the user moves: a group, a table or a component instance (a shape with children is
/// an object of its own).
pub(super) fn is_container(node: &Node) -> bool {
    node.kind == designllm_contracts::NodeKind::Group || node.ext.table.is_some() || node.ext.instance.is_some()
}

pub(super) fn is_locked(node: &Node) -> bool {
    node.ext.locked == Some(true)
}

/// Shift or ctrl held: a click or a marquee adds to the selection instead of replacing it.
pub(super) fn additive_modifier(ctx: &Context) -> bool {
    ctx.input(|input| input.modifiers.shift || input.modifiers.command)
}

/// The node with `id` among `nodes` or their children (a copy), with the id of its parent.
fn locate(nodes: &[Node], id: &str, parent: Option<&str>) -> Option<(Node, Option<String>)> {
    nodes.iter().find_map(|node| {
        if node.id == id {
            Some((node.clone(), parent.map(str::to_owned)))
        } else {
            locate(&node.children, id, Some(node.id.as_str()))
        }
    })
}

/// Where the engine drew each node on a page, in page units, by node id. A stack or grid child's stored bounds
/// are not where it is drawn, so the drawn frame comes from the scene: the node's own region (no text range), else
/// its shape, image or path primitive, else the union of its text-line regions.
pub(super) fn laid_out_frames(page: &RenderPage) -> BTreeMap<String, ModelRect> {
    let mut frames: BTreeMap<String, ModelRect> = BTreeMap::new();
    for source in page.source_map.iter().filter(|source| source.logical_range.is_none()) {
        frames.entry(source.node_id.clone()).or_insert(source.bounds);
    }
    for primitive in &page.primitives {
        if let Primitive::Shape { node_id, bounds, .. } | Primitive::Image { node_id, bounds, .. } | Primitive::Path { node_id, bounds, .. } = primitive {
            frames.entry(node_id.clone()).or_insert(*bounds);
        }
    }
    let mut lines: BTreeMap<String, Vec<ModelRect>> = BTreeMap::new();
    for source in page.source_map.iter().filter(|source| source.logical_range.is_some() && !frames.contains_key(&source.node_id)) {
        lines.entry(source.node_id.clone()).or_default().push(source.bounds);
    }
    for (id, rects) in lines {
        if let Some(union) = union_of(&rects) {
            frames.insert(id, union);
        }
    }
    frames
}

/// Appends `node` and its children with their frames on screen. The drawn frame (`drawn`, page units) wins; a node
/// the scene does not hold uses its stored bounds, relative to its parent's origin `parent` in page units.
pub(super) fn collect_frames(node: &Node, parent: (i64, i64), drawn: &BTreeMap<String, ModelRect>, page: Rect, zoom: f32, out: &mut Vec<(String, Rect)>) {
    let bounds = drawn.get(&node.id).copied().unwrap_or(ModelRect {
        x: parent.0 + node.bounds.x,
        y: parent.1 + node.bounds.y,
        width: node.bounds.width,
        height: node.bounds.height,
    });
    out.push((node.id.clone(), super::selection::frame_on_screen(bounds, page, zoom)));
    for child in &node.children {
        collect_frames(child, (bounds.x, bounds.y), drawn, page, zoom, out);
    }
}

/// The index among `siblings` (drawn frames in child order, the dragged node left out) at which a node dropped with
/// its centre at `drop` goes. A row compares x, a column y; a grid reads rows top to bottom, then x within a row.
pub(super) fn drop_index(mode: &str, siblings: &[ModelRect], drop: (i64, i64)) -> usize {
    let centre = |rect: &ModelRect| (rect.x + rect.width / 2, rect.y + rect.height / 2);
    siblings
        .iter()
        .filter(|rect| {
            let (cx, cy) = centre(rect);
            match mode {
                "row" => cx < drop.0,
                "column" => cy < drop.1,
                _ => rect.y + rect.height <= drop.1 || (drop.1 >= rect.y && cx < drop.0),
            }
        })
        .count()
}

/// What a drag of a node does: moves its frame, or (inside a stack or grid, which places its children) changes its
/// place among its siblings.
pub(super) enum DragDrop {
    /// The parent does not lay out its children: the frame moves.
    Free,
    /// Dropped where it already is.
    Unchanged,
    Reorder(Operation),
}

impl DesktopApp {
    /// A drag of `node_id` by (`dx`, `dy`) page EMU. A child of a stack or grid is placed by the engine, so moving its
    /// stored frame does nothing on the page: the drop point (its drawn centre plus the drag) picks its new index.
    pub(super) fn drag_drop(&self, node_id: &str, dx: i64, dy: i64) -> DragDrop {
        let Some(artifact) = self.artifact.as_ref() else {
            return DragDrop::Free;
        };
        let pages = match &artifact.body {
            ArtifactBody::Canvas(document) => &document.pages,
            ArtifactBody::Slides(document) => &document.pages,
            _ => return DragDrop::Free,
        };
        fn parent_of<'a>(nodes: &'a [Node], id: &str) -> Option<&'a Node> {
            nodes.iter().find_map(|node| if node.children.iter().any(|child| child.id == id) { Some(node) } else { parent_of(&node.children, id) })
        }
        for (page_index, page) in pages.iter().enumerate() {
            let Some(parent) = parent_of(&page.nodes, node_id) else {
                continue;
            };
            let mode = parent.layout.as_ref().map_or("", |layout| layout.mode.as_str());
            if !matches!(mode, "row" | "column" | "grid") {
                return DragDrop::Free;
            }
            let drawn = self.page_textures.get(&(page_index as u32)).map(|texture| texture.frames.clone()).unwrap_or_default();
            let Some(frame) = drawn.get(node_id) else {
                return DragDrop::Unchanged;
            };
            let siblings: Vec<ModelRect> =
                parent.children.iter().filter(|child| child.id != node_id).filter_map(|child| drawn.get(&child.id).copied()).collect();
            let drop = (frame.x + frame.width / 2 + dx, frame.y + frame.height / 2 + dy);
            let index = drop_index(mode, &siblings, drop);
            let current = parent.children.iter().position(|child| child.id == node_id).unwrap_or(index);
            if index == current {
                return DragDrop::Unchanged;
            }
            return DragDrop::Reorder(Operation::MoveNode {
                node_id: node_id.to_owned(),
                page_id: page.id.clone(),
                parent_id: Some(parent.id.clone()),
                index: index as u32,
            });
        }
        DragDrop::Free
    }

    /// The one place that changes the selection's ids. The first id is the primary node the properties show.
    pub(super) fn set_selected_ids(&mut self, ids: Vec<String>) {
        if self.inline_text_edit.as_ref().is_some_and(|edit| ids.len() != 1 || ids[0] != edit.node_id) {
            return;
        }
        // Objects and a board memo are not selected together: what was chosen last is what `Delete` removes.
        if !ids.is_empty() {
            self.selected_board_memo = None;
        }
        self.selected_node = ids.first().cloned();
        self.selected_nodes = ids;
    }

    /// Selects `ids` in the window and at the host (selection.set). Empty clears the selection.
    pub(super) fn select_ids(&mut self, ids: Vec<String>) {
        self.set_selected_ids(ids.clone());
        self.set_selection(designllm_contracts::Selection { node_ids: ids, text_range: None });
        self.sync_style_fields();
        self.sync_text_target();
    }

    /// Every node's frame on the board, nested children included, by node id.
    pub(super) fn node_frames(&self, origin: Pos2, zoom: f32) -> Vec<(String, Rect)> {
        let (Some(artifact), Some(kind)) = (self.artifact.as_ref(), self.active.as_ref().map(|item| item.kind)) else {
            return Vec::new();
        };
        let page_nodes: Vec<&[Node]> = match &artifact.body {
            ArtifactBody::Canvas(document) => document.pages.iter().map(|page| page.nodes.as_slice()).collect(),
            ArtifactBody::Slides(document) => document.pages.iter().map(|page| page.nodes.as_slice()).collect(),
            _ => return Vec::new(),
        };
        let slots = board::arrange(kind, &self.page_sizes(), zoom);
        let mut frames = Vec::new();
        // A page without a texture has no drawn frames (an empty map does not allocate).
        let undrawn = BTreeMap::new();
        for (index, nodes) in page_nodes.iter().enumerate() {
            let Some(slot) = slots.iter().find(|slot| slot.page_index == index as u32) else {
                continue;
            };
            let page = board::to_screen(slot.world, origin, zoom);
            let drawn = self.page_textures.get(&(index as u32)).map_or(&undrawn, |texture| &texture.frames);
            for node in nodes.iter() {
                collect_frames(node, (0, 0), drawn, page, zoom, &mut frames);
            }
        }
        frames
    }

    /// The nodes of every page (Canvas and Slides), page by page.
    fn page_node_lists(&self) -> Vec<&[Node]> {
        match self.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::Canvas(document)) => document.pages.iter().map(|page| page.nodes.as_slice()).collect(),
            Some(ArtifactBody::Slides(document)) => document.pages.iter().map(|page| page.nodes.as_slice()).collect(),
            _ => Vec::new(),
        }
    }

    /// The nodes of page `page_index` whose empty part passes a press on to the board (`passes_press`), nested ones
    /// included. One walk of the page, for the regions of one frame.
    pub(super) fn pass_through_ids(&self, page_index: u32) -> std::collections::BTreeSet<String> {
        fn walk(app: &DesktopApp, node: &Node, out: &mut std::collections::BTreeSet<String>) {
            if app.passes_press(node) {
                out.insert(node.id.clone());
            }
            node.children.iter().for_each(|child| walk(app, child, out));
        }
        let mut out = std::collections::BTreeSet::new();
        if let Some(nodes) = self.page_node_lists().get(page_index as usize) {
            nodes.iter().for_each(|node| walk(self, node, &mut out));
        }
        out
    }

    /// Whether a press on `node`'s own region (not on a child) is the board's: a locked node never moves, and a container
    /// (group, table, component instance) that is not selected lets the drag draw a marquee. A selected container is
    /// dragged as a whole.
    pub(super) fn passes_press(&self, node: &Node) -> bool {
        is_locked(node) || (is_container(node) && !self.selected_nodes.contains(&node.id))
    }

    /// What a marquee over `marquee` selects, in document order: a node it touches; a container only when it covers it
    /// whole (else the children it touches), a table or component instance as one piece, a shape with children as itself;
    /// never a locked node.
    pub(super) fn marquee_selection(&self, frames: &[(String, Rect)], marquee: Rect) -> Vec<String> {
        fn walk(node: &Node, frames: &BTreeMap<&str, Rect>, marquee: Rect, out: &mut Vec<String>) {
            let Some(frame) = frames.get(node.id.as_str()).copied() else {
                return;
            };
            let whole = node.ext.table.is_some() || node.ext.instance.is_some();
            if is_locked(node) {
                node.children.iter().for_each(|child| walk(child, frames, marquee, out));
            } else if is_container(node) && !whole {
                if marquee.contains_rect(frame) {
                    out.push(node.id.clone());
                } else if frame.intersects(marquee) {
                    node.children.iter().for_each(|child| walk(child, frames, marquee, out));
                }
            } else if frame.intersects(marquee) {
                out.push(node.id.clone());
            }
        }
        let frames: BTreeMap<&str, Rect> = frames.iter().map(|(id, frame)| (id.as_str(), *frame)).collect();
        let mut out = Vec::new();
        for nodes in self.page_node_lists() {
            nodes.iter().for_each(|node| walk(node, &frames, marquee, &mut out));
        }
        out
    }

    /// The selected nodes (copies) with their page and their shared parent. Nodes on different pages or under
    /// different parents cannot be aligned or grouped together, so they are refused.
    pub(super) fn selected_siblings(&self) -> Result<(Vec<Node>, designllm_contracts::Id, Option<designllm_contracts::Id>), String> {
        let found = self.selected_located()?;
        let Some((_, parent, page)) = found.first().cloned() else {
            return Err(String::from("항목을 고르세요"));
        };
        if found.iter().any(|(_, other_parent, other_page)| *other_parent != parent || *other_page != page) {
            return Err(String::from("같은 페이지, 같은 상위 항목 안의 항목끼리만 맞출 수 있습니다"));
        }
        Ok((found.into_iter().map(|(node, _, _)| node).collect(), page, parent))
    }

    /// Every selected node with its parent and its page, in selection order.
    fn selected_located(&self) -> Result<Vec<(Node, Option<String>, String)>, String> {
        let artifact = self.artifact.as_ref().ok_or_else(|| String::from("작업물이 열려 있지 않습니다"))?;
        let pages: Vec<(&str, &[Node])> = match &artifact.body {
            ArtifactBody::Canvas(document) => document.pages.iter().map(|page| (page.id.as_str(), page.nodes.as_slice())).collect(),
            ArtifactBody::Slides(document) => document.pages.iter().map(|page| (page.id.as_str(), page.nodes.as_slice())).collect(),
            _ => return Err(String::from("이 작업물에서는 여러 항목을 함께 고칠 수 없습니다")),
        };
        let mut found: Vec<(Node, Option<String>, String)> = Vec::new();
        for id in &self.selected_nodes {
            let hit = pages.iter().find_map(|(page_id, nodes)| {
                locate(nodes, id, None).map(|(node, parent)| (node, parent, (*page_id).to_owned()))
            });
            found.push(hit.ok_or_else(|| String::from("선택한 항목을 찾지 못했습니다"))?);
        }
        Ok(found)
    }

    /// Align the selection to one edge of its shared box (`props.align` with several nodes selected).
    pub(super) fn align_selected(&mut self, edge: &str) -> Result<(), String> {
        let edge = Edge::parse(edge).ok_or_else(|| String::from("맞춤 방향을 알 수 없습니다"))?;
        let (nodes, _, _) = self.selected_siblings()?;
        let before: Vec<ModelRect> = nodes.iter().map(|node| node.bounds).collect();
        let after = align_rects(&before, edge);
        self.geometry_change(&nodes, &before, &after)
    }

    /// Spread the selection evenly along one axis (`props.distribute`).
    pub(super) fn distribute_selected(&mut self, horizontal: bool) -> Result<(), String> {
        let (nodes, _, _) = self.selected_siblings()?;
        let before: Vec<ModelRect> = nodes.iter().map(|node| node.bounds).collect();
        let after = distribute_rects(&before, horizontal);
        self.geometry_change(&nodes, &before, &after)
    }

    /// Sets the x and/or y of every selected node. A mixed field edits only the axis it shows.
    pub(super) fn set_selected_positions(&mut self, x: Option<i64>, y: Option<i64>) -> Result<(), String> {
        let (nodes, _, _) = self.selected_siblings()?;
        let before: Vec<ModelRect> = nodes.iter().map(|node| node.bounds).collect();
        let after: Vec<ModelRect> = before
            .iter()
            .map(|rect| ModelRect { x: x.unwrap_or(rect.x), y: y.unwrap_or(rect.y), ..*rect })
            .collect();
        self.geometry_change(&nodes, &before, &after)
    }

    /// Moves every selected node by (`dx`, `dy`) EMU (`edit.nudge` with several nodes selected), as one change. Each frame is
    /// relative to its own parent, so nodes under different parents or on different pages move by the same offset.
    pub(super) fn move_selected_by(&mut self, dx: i64, dy: i64) -> Result<(), String> {
        let nodes: Vec<Node> = self.selected_located()?.into_iter().map(|(node, _, _)| node).collect();
        let before: Vec<ModelRect> = nodes.iter().map(|node| node.bounds).collect();
        let after: Vec<ModelRect> = before.iter().map(|rect| ModelRect { x: rect.x + dx, y: rect.y + dy, ..*rect }).collect();
        self.geometry_change(&nodes, &before, &after)
    }

    /// Sets the width and/or height of every selected node. A mixed field edits only the dimension it shows.
    pub(super) fn set_selected_sizes(&mut self, width: Option<i64>, height: Option<i64>) -> Result<(), String> {
        let (nodes, _, _) = self.selected_siblings()?;
        let before: Vec<ModelRect> = nodes.iter().map(|node| node.bounds).collect();
        let after: Vec<ModelRect> = before
            .iter()
            .map(|rect| ModelRect { width: width.unwrap_or(rect.width), height: height.unwrap_or(rect.height), ..*rect })
            .collect();
        // Each node follows the zero-extent rules of its own kind (the same check as a single node's `props.size`).
        for (node, size) in nodes.iter().zip(&after) {
            super::page_guides::validate_node_extent(node.kind, size.width, size.height)?;
        }
        self.geometry_change(&nodes, &before, &after)
    }

    /// One change: every node whose bounds moved gets its new frame.
    fn geometry_change(&mut self, nodes: &[Node], before: &[ModelRect], after: &[ModelRect]) -> Result<(), String> {
        let operations: Vec<Operation> = nodes
            .iter()
            .enumerate()
            .filter(|(index, _)| before[*index] != after[*index])
            .map(|(index, node)| Operation::SetGeometry { node_id: node.id.clone(), bounds: after[index] })
            .collect();
        if operations.is_empty() {
            return Ok(());
        }
        self.change(operations, &[Intent::Design])
    }

    /// A drag that starts on empty board draws a marquee; on release the nodes it touches are selected (added to
    /// the selection with shift or ctrl). Drags that start on a node are left to the node.
    pub(super) fn marquee_input(&mut self, ctx: &Context, ui: &mut Ui, response: &Response, palette: &theme::Palette, origin: Pos2, zoom: f32) {
        let space = ctx.input(|input| input.key_down(egui::Key::Space));
        let select_tool = self.view_ref().tool == super::board::Tool::Select;
        if self.marquee.is_none() && select_tool && !space && response.drag_started_by(egui::PointerButton::Primary) {
            let press = ctx.input(|input| input.pointer.press_origin()).or_else(|| response.interact_pointer_pos());
            if let Some(start) = press {
                // A press on a locked node or on the empty part of a container that is not selected still starts the marquee.
                let passing: std::collections::BTreeSet<String> =
                    (0..self.page_node_lists().len() as u32).flat_map(|page| self.pass_through_ids(page)).collect();
                if !self.node_frames(origin, zoom).iter().any(|(id, frame)| frame.contains(start) && !passing.contains(id)) {
                    self.marquee = Some((start, start));
                }
            }
        }
        let Some((start, _)) = self.marquee else {
            return;
        };
        let current = ctx.input(|input| input.pointer.hover_pos()).or_else(|| response.interact_pointer_pos()).unwrap_or(start);
        if response.drag_stopped_by(egui::PointerButton::Primary) {
            self.marquee = None;
            let rect = Rect::from_two_pos(start, current);
            let additive = additive_modifier(ctx);
            let mut ids = if additive { self.selected_nodes.clone() } else { Vec::new() };
            for id in self.marquee_selection(&self.node_frames(origin, zoom), rect) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            if !ids.is_empty() || !additive {
                self.select_ids(ids);
            }
            return;
        }
        self.marquee = Some((start, current));
        let rect = Rect::from_two_pos(start, current);
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::ZERO, palette.accent_soft);
        painter.rect_stroke(rect, CornerRadius::ZERO, Stroke::new(1.0, palette.accent), StrokeKind::Inside);
    }

    /// The outlines of a multi-selection, its shared box with handles, and the toolbar above the box.
    pub(super) fn draw_multi_selection(&mut self, ctx: &Context, ui: &mut Ui, palette: &theme::Palette, origin: Pos2, zoom: f32) {
        if self.selected_nodes.len() < 2 {
            return;
        }
        let frames = self.node_frames(origin, zoom);
        // A move under way, or a drop not drawn yet, shows the outlines with the moved pixels (board.rs `drag_shift`).
        let picked: Vec<Rect> = frames
            .iter()
            .filter(|(id, _)| self.selected_nodes.contains(id))
            .map(|(id, rect)| self.drag_shift(ctx, id).map_or(*rect, |(_, delta)| rect.translate(delta)))
            .collect();
        let Some(shared) = picked.iter().copied().reduce(|a, b| a.union(b)) else {
            return;
        };
        // Outlines, handles and toolbar are drawn by the selection overlay (selection.rs).
        let Some(primary) = self.selected_node_properties() else {
            return;
        };
        self.paint_multi_selection(ctx, ui, palette, &picked, shared, &primary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Vec2;
    use serde_json::json;

    fn rect(x: i64, y: i64, width: i64, height: i64) -> ModelRect {
        ModelRect { x, y, width, height }
    }

    #[test]
    fn align_to_an_edge_moves_each_rectangle_to_the_shared_box() {
        let rects = [rect(10, 0, 40, 20), rect(30, 50, 10, 10)];
        assert_eq!(align_rects(&rects, Edge::Left), [rect(10, 0, 40, 20), rect(10, 50, 10, 10)]);
        assert_eq!(align_rects(&rects, Edge::Bottom), [rect(10, 40, 40, 20), rect(30, 50, 10, 10)]);
        assert_eq!(align_rects(&rects, Edge::HCenter)[1], rect(25, 50, 10, 10), "centred in 10..50");
    }

    #[test]
    fn distribute_keeps_the_outermost_two_and_spaces_the_middle_evenly() {
        let rects = [rect(0, 0, 10, 10), rect(20, 0, 10, 10), rect(100, 0, 10, 10)];
        assert_eq!(distribute_rects(&rects, true), [rect(0, 0, 10, 10), rect(50, 0, 10, 10), rect(100, 0, 10, 10)]);
        let tall = [rect(0, 0, 10, 10), rect(0, 20, 10, 10), rect(0, 100, 10, 10)];
        assert_eq!(distribute_rects(&tall, false)[1], rect(0, 50, 10, 10));
        assert_eq!(distribute_rects(&rects[..2], true), rects[..2], "two rectangles have nothing to spread");
    }

    #[test]
    fn toggling_adds_a_missing_id_and_removes_a_present_one() {
        let ids = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(toggle_id(&ids, "b"), ["a"]);
        assert_eq!(toggle_id(&ids, "c"), ["a", "b", "c"]);
    }

    #[test]
    fn a_child_is_framed_at_its_parent_origin_plus_its_own_bounds() {
        let parent: Node = serde_json::from_value(json!({
            "id": "p", "kind": "rectangle", "bounds": { "x": 1000, "y": 2000, "width": 900, "height": 600 },
            "children": [{ "id": "c", "kind": "text", "text": "가", "bounds": { "x": 50, "y": 60, "width": 300, "height": 50 } }],
        }))
        .expect("a parent with a child parses");
        let page = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::ZERO);
        let mut out = Vec::new();
        collect_frames(&parent, (0, 0), &BTreeMap::new(), page, 1.0, &mut out);
        let direct = super::super::selection::frame_on_screen(rect(1050, 2060, 300, 50), page, 1.0);
        assert_eq!(out[1], ("c".to_owned(), direct));
    }
}
