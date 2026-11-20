//! Selection on the board (GUI_DESIGN §5.4, §9.3, §11): the outline and corner handles of the
//! selected node, its floating toolbar, and the explanation of a refused edit next to it.

use designllm_contracts::{
    layout_guides::{ActiveEdge, EmuRect, ResizeHandle},
    ArtifactBody, Node,
};
use eframe::egui::{self, Align2, Color32, Context, CornerRadius, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use serde_json::json;

use super::{properties::NodeProperties, DesktopApp};
use crate::{board, commands, frame, icons, theme, widgets};

fn layer_state_commands(kind: designllm_contracts::ArtifactKind, hidden: bool, locked: bool) -> Vec<&'static str> {
    if kind != designllm_contracts::ArtifactKind::Canvas {
        return Vec::new();
    }
    let mut commands = vec![if locked { "edit.unlock" } else { "edit.lock" }];
    if !locked {
        commands.push(if hidden { "edit.show" } else { "edit.hide" });
    }
    commands
}

/// The Korean label of an engine rule code (tokens `text.lint-rules`). The rule codes themselves never reach the
/// screen. The engine's diagnostics carry no measured values, so the label stands for the `message` sentence
/// (the token's rule: no values → label only).
fn lint_label(code: &str) -> Option<&'static str> {
    // The flow tables' own rules are not in the label table (strings.rs), so they carry their words here.
    match lint_rule(code) {
        "TABLE_BORDER" => Some("표 테두리가 잘림"),
        "TABLE_TEXT" => Some("표 글자가 잘림"),
        // A code the table does not list yet (the flow engines' lowercase codes) reads as the rule it belongs to.
        rule => crate::strings::lint_label(code).or_else(|| crate::strings::lint_label(rule)),
    }
}

/// The token name of an engine rule code: the canvas engine says TOUCH_TARGET_SMALL, CONTRAST_LOW, TEXT_CLIPPED.
fn lint_rule(code: &str) -> &str {
    match code {
        "TOUCH_TARGET_SMALL" => "TOUCH_TARGET",
        "CONTRAST_LOW" => "CONTRAST",
        "TEXT_CLIPPED" => "CLIPPED",
        // Flow (document) diagnostics: the same sentence family, width or height by their `axis` param. A table's
        // own sentence waits for the lead; until then `table_overflow` reads as the width sentence.
        "content_overflow" | "text_overflow" | "table_overflow" => "CLIPPED",
        // A table border cut by a page edge and a split row's text past its cell edge have their own words (`lint_label`).
        "table_border_overflow" => "TABLE_BORDER",
        "table_text_overflow" => "TABLE_TEXT",
        other => other,
    }
}

/// A fix button of a refusal callout: icon, label, and the command it runs (tokens `text.lint-rules.*.fixes`).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct RefusalFix {
    pub icon: &'static str,
    pub label: &'static str,
    pub command: &'static str,
    pub args: serde_json::Value,
}

/// A length param (EMU) in the document's unit (tokens `lint-rules.units`): canvas·slides·web whole px, documents
/// and figures mm with one decimal.
fn lint_length(value: &serde_json::Value, kind: designllm_contracts::ArtifactKind) -> Option<String> {
    // A length may also come as `{emu, axis}` (the canvas engine's CLIPPED `box`).
    let emu = value.as_f64().or_else(|| value.get("emu").and_then(serde_json::Value::as_f64))?;
    let unit = super::properties::unit_for(kind);
    let shown = emu / unit.emu();
    Some(match unit.label() {
        "mm" => format!("{shown:.1} mm"),
        label => format!("{} {label}", shown.round()),
    })
}

/// The `message` sentence of a rule with its params filled in; None when a param it needs is missing.
fn lint_message(diagnostic: &designllm_contracts::Diagnostic, kind: designllm_contracts::ArtifactKind, artifact: Option<&designllm_contracts::Artifact>) -> Option<String> {
    let params = &diagnostic.params;
    let length = |key: &str| params.get(key).and_then(|value| lint_length(value, kind));
    let ratio = |key: &str| params.get(key).and_then(serde_json::Value::as_f64).map(|value| format!("{value:.1}"));
    // Names as the layer tree shows them (`sidebar::node_label`): the parameter when it is a node id, else the
    // parent of the node the rule names; the raw text only when neither is found.
    let label_of = |id: &str| artifact.and_then(|artifact| super::doc_tree::find_node(artifact, id)).map(super::sidebar::node_label);
    let name = |key: &str| {
        let raw = params.get(key).map(|value| value.as_str().map_or_else(|| value.to_string(), str::to_owned));
        let by_id = raw.as_deref().and_then(label_of);
        let by_tree = (key == "parent")
            .then(|| artifact.zip(diagnostic.node_id.as_deref()).and_then(|(artifact, id)| parent_label(artifact, id)))
            .flatten();
        by_id.or(by_tree).or(raw)
    };
    Some(match lint_rule(&diagnostic.code) {
        "TOUCH_TARGET" => {
            let min = length("min")?;
            format!("누르는 영역({} × {})이 최소 {min} × {min}보다 작습니다", length("w")?, length("h")?)
        }
        // tokens `lint-rules.particle-rule`: values sit in parentheses after the word they measure, so no particle
        // follows a value or a name.
        "CONTRAST" => format!("글자 대비({}:1)가 기준 {}:1보다 낮습니다", ratio("ratio")?, ratio("min")?),
        "CLIPPED" => {
            let (text, boxed) = (length("text")?, length("box")?);
            // `axis` flat, or inside `box` (`{axis, emu}`).
            let axis = params.get("axis").or_else(|| params.get("box").and_then(|value| value.get("axis"))).and_then(serde_json::Value::as_str);
            if axis == Some("height") {
                format!("글 높이({text})가 상자 높이({boxed})보다 높습니다")
            } else {
                format!("글 폭({text})이 상자 폭({boxed})보다 넓습니다")
            }
        }
        "CHILD_OUTSIDE_PARENT" => format!("‘{}’ 밖으로 나갑니다", name("parent")?),
        "OUT_OF_PAGE" => String::from("페이지 밖에 있습니다"),
        // The flow table codes' sentences are the words table's (messages.rs).
        "TABLE_BORDER" | "TABLE_TEXT" => super::messages::diagnostic_message(&diagnostic.code)?.to_owned(),
        "TEMPLATE_FIT" => format!("{} × {}에는 이 템플릿이 맞지 않습니다", length("size_w")?, length("size_h")?),
        _ => return None,
    })
}

/// The layer-tree name of the parent of `id` on a canvas or slide page.
fn parent_label(artifact: &designllm_contracts::Artifact, id: &str) -> Option<String> {
    fn parent_of<'a>(nodes: &'a [Node], id: &str) -> Option<&'a Node> {
        nodes.iter().find_map(|node| if node.children.iter().any(|child| child.id == id) { Some(node) } else { parent_of(&node.children, id) })
    }
    let pages: Vec<&[Node]> = match &artifact.body {
        ArtifactBody::Canvas(document) => document.pages.iter().map(|page| page.nodes.as_slice()).collect(),
        ArtifactBody::Slides(document) => document.pages.iter().map(|page| page.nodes.as_slice()).collect(),
        _ => return None,
    };
    pages.into_iter().find_map(|nodes| parent_of(nodes, id)).map(super::sidebar::node_label)
}

/// The fix buttons of a rule whose commands exist, for the node the rule names.
fn lint_fixes(diagnostic: &designllm_contracts::Diagnostic) -> Vec<RefusalFix> {
    let node = diagnostic.node_id.clone();
    let with_node = |mut args: serde_json::Value| {
        if let Some(node) = &node {
            args["node_id"] = json!(node);
        }
        args
    };
    let candidates: Vec<(&'static str, &'static str, &'static str, serde_json::Value)> = match lint_rule(&diagnostic.code) {
        "TOUCH_TARGET" => vec![("size-grow", "크기 키우기", "fix.grow_to_min", json!({}))],
        "CONTRAST" => {
            let min = diagnostic.params.get("min").cloned();
            vec![("text-color", "글자 색 진하게", "fix.darken_text", min.map_or_else(|| json!({}), |min| json!({ "min_ratio": min })))]
        }
        "CLIPPED" => vec![("size-grow", "상자 키우기", "fix.grow_box", json!({})), ("text-shrink", "글자 줄이기", "fix.shrink_text", json!({}))],
        "CHILD_OUTSIDE_PARENT" => vec![("size-grow", "프레임 키우기", "fix.grow_parent", json!({}))],
        "OUT_OF_PAGE" => vec![("layout-free", "페이지 안으로", "fix.move_into_page", json!({}))],
        _ => Vec::new(),
    };
    candidates
        .into_iter()
        .filter(|(_, _, command, _)| commands::available(command) && commands::find(command).is_some())
        .take(2)
        .map(|(icon, label, command, args)| RefusalFix { icon, label, command, args: with_node(args) })
        .collect()
}

/// The callout line and fix buttons of a refused edit (§11, tokens `text.lint-rules`).
pub(super) fn refusal_for(
    error: &designllm_contracts::DltError,
    friendly: &str,
    kind: designllm_contracts::ArtifactKind,
    artifact: Option<&designllm_contracts::Artifact>,
) -> (String, Vec<RefusalFix>) {
    if let Some(diagnostic) = error.diagnostics.iter().find(|diagnostic| lint_label(&diagnostic.code).is_some()) {
        let line = lint_message(diagnostic, kind, artifact).unwrap_or_else(|| lint_label(&diagnostic.code).unwrap_or_default().to_owned());
        // The fix commands work on canvases and slides only.
        let fixes = if matches!(kind, designllm_contracts::ArtifactKind::Canvas | designllm_contracts::ArtifactKind::Slides) { lint_fixes(diagnostic) } else { Vec::new() };
        return (line, fixes);
    }
    (refusal_text(error, friendly), Vec::new())
}

/// The one line of a refused edit's callout without measured values (§11, tokens `text.lint-rules`): the Korean
/// label of the first rule the engine names; else the reason in Korean the window already has; else
/// `이 편집은 적용되지 않았습니다`. English codes and messages are never shown.
pub(super) fn refusal_text(error: &designllm_contracts::DltError, friendly: &str) -> String {
    let korean = |text: &str| text.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c));
    let shouts_a_code = |text: &str| text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').any(|word| word.len() > 3 && word.contains('_') && word.chars().all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit()));
    if let Some(label) = error.diagnostics.iter().find_map(|diagnostic| lint_label(&diagnostic.code)) {
        return label.to_owned();
    }
    if let Some(message) = error.diagnostics.iter().map(|diagnostic| diagnostic.message.as_str()).find(|message| korean(message) && !shouts_a_code(message)) {
        return message.to_owned();
    }
    if korean(friendly) && !shouts_a_code(friendly) {
        return friendly.to_owned();
    }
    String::from("이 편집은 적용되지 않았습니다")
}

/// The page that holds `id`, searching nested children too.
fn page_of(body: &ArtifactBody, id: &str) -> Option<u32> {
    let contains = |nodes: &[Node]| nodes.iter().any(|node| super::doc_tree::find_node_in_node(node, id).is_some());
    match body {
        ArtifactBody::Canvas(document) => document.pages.iter().position(|page| contains(&page.nodes)).map(|index| index as u32),
        ArtifactBody::Slides(document) => document.pages.iter().position(|page| contains(&page.nodes)).map(|index| index as u32),
        _ => None,
    }
}

impl DesktopApp {
    /// Draws the selection outline, its handles, the toolbar and the refusal explanation.
    pub(super) fn draw_selection(&mut self, ctx: &Context, ui: &mut Ui, palette: &theme::Palette, origin: Pos2, zoom: f32) {
        if self.selected_nodes.len() > 1 {
            return;
        }
        if self.draw_figure_panel_selection(ctx, ui, palette, origin, zoom) {
            return;
        }
        let Some(node) = self.selected_node_properties() else {
            return;
        };
        let (Some(artifact), Some(kind)) = (self.artifact.as_ref(), self.active.as_ref().map(|item| item.kind)) else {
            return;
        };
        let Some(page_index) = page_of(&artifact.body, &node.id) else {
            return;
        };
        let slots = board::arrange(kind, &self.page_sizes(), zoom);
        let Some(slot) = slots.iter().find(|slot| slot.page_index == page_index) else {
            return;
        };
        let page = board::to_screen(slot.world, origin, zoom);
        let rect = self
            .node_frames(origin, zoom)
            .into_iter()
            .find_map(|(id, rect)| (id == node.id).then_some(rect))
            .unwrap_or_else(|| frame_on_screen(node.bounds, page, zoom));
        // A refused node keeps its selection, drawn in `danger` (mockup `state-refused`: `.refused`, `.hd.dg`).
        let refused = self.refusal.is_some() && (self.refusal_ids.is_empty() || self.refusal_ids.contains(&node.id));
        let outline = self.resize_handles(ui, &node, rect, page, page_index);
        // A move under way, or a drop not drawn yet, shows the outline with the moved pixels (board.rs `drag_shift`).
        let outline = match self.drag_shift(ctx, &node.id) {
            Some((_, delta)) => outline.translate(delta),
            None => outline,
        };
        paint_selection_box(ui.painter(), outline, if refused { palette.danger } else { palette.accent });
        if let Some(text) = self.refusal.clone().filter(|_| self.refusal_ids.is_empty()) {
            let fixes = self.refusal_fixes.clone();
            let (_, _, _, _, fixed) = refusal_callout_with(ctx, palette, self.board_rect, rect, &text, None, &fixes, None, None, None);
            if let Some(fix) = fixed {
                // A fix is the next edit: the refusal goes away and the fix runs as its own change.
                self.refusal = None;
                self.refusal_fixes.clear();
                let mut args = fix.args;
                if args.get("node_id").is_none() {
                    args["node_id"] = json!(node.id);
                }
                self.invoke_ui(fix.command, args);
            }
        }
        if let Some(board_rect) = self.board_rect {
            let tools = node_tools(&node);
            let mut toolbar = self.place_toolbar(board_rect, rect, toolbar_size(ui, &tools));
            // tokens.json `ctx.avoid-tag`: a toolbar above the node that meets the agent's name tag moves up so that
            // its bottom is 6 above the tag.
            if let Some(reservation) = self.reservations.iter().find(|reservation| reservation.node_ids.first() == Some(&node.id)) {
                let tag = tag_rect(ui.painter(), rect.left_top(), &self.reservation_name(reservation));
                if toolbar.bottom() <= rect.top() && toolbar.x_range().intersects(tag.x_range()) && toolbar.bottom() > tag.top() - 6.0 {
                    toolbar = toolbar.translate(Vec2::new(0.0, tag.top() - 6.0 - toolbar.bottom()));
                }
            }
            self.show_toolbar(ctx, palette, "selection-toolbar", toolbar, &tools, &node);
            // tokens `lint-warning.callout`: only for this one selected node, and not on top of a refusal.
            if self.refusal.is_none() {
                self.draw_lint_warnings(ctx, palette, board_rect, rect, toolbar, &node.id, kind);
            }
        }
    }

    /// The resized bounds snapped to the page guides, as a move is (`page_guides::resize_snap`): only the edges the dragged
    /// corner lies on move. Only a canvas with guides shown snaps.
    fn snap_resize_bounds(&self, page_index: u32, bounds: designllm_contracts::Rect, handle: ResizeHandle, zoom: f32, pixels_per_point: f32) -> designllm_contracts::Rect {
        if !self.view_ref().snap_guides {
            return bounds;
        }
        let Some(ArtifactBody::Canvas(document)) = self.artifact.as_ref().map(|artifact| &artifact.body) else {
            return bounds;
        };
        let Some(page) = document.pages.get(page_index as usize) else {
            return bounds;
        };
        let Some(layout) = super::page_guides::page_guide_layout(document, page) else {
            return bounds;
        };
        let frame = EmuRect { x_emu: bounds.x, y_emu: bounds.y, width_emu: bounds.width, height_emu: bounds.height };
        match super::page_guides::resize_snap(Some(&layout), frame, handle, zoom, pixels_per_point) {
            Ok(snap) => designllm_contracts::Rect { x: snap.rect.x_emu, y: snap.rect.y_emu, width: snap.rect.width_emu, height: snap.rect.height_emu },
            Err(_) => bounds,
        }
    }

    /// Where a corner resize under way would put the node: resized, turned over if the corner passed the opposite one, and
    /// snapped as the drop will be. `None` when it would have no size.
    fn resize_result(&self, gesture: &ResizeGesture, pixels_per_point: f32) -> Option<designllm_contracts::Rect> {
        let (bounds, handle) = resized_bounds(gesture.start, gesture.corner, gesture.delta, gesture.page_rect, gesture.page_size)?;
        Some(self.snap_resize_bounds(gesture.page_index, bounds, handle, self.view_ref().zoom, pixels_per_point))
    }

    /// The corner handles of a selected page-level node resize it: the dragged corner moves, the opposite one stays,
    /// and the drop is one change (one undo step). The handles are interacted after the node regions, so they take
    /// a press on a corner before the body's move; on a small node they shrink to a third of its side, so its body
    /// stays draggable. A locked node has none, and while the board pans they take no press. Returns the outline to
    /// draw (where the drop would put the node while a resize is under way).
    fn resize_handles(&mut self, ui: &mut Ui, node: &NodeProperties, rect: Rect, page: Rect, page_index: u32) -> Rect {
        let Some((page_nodes, page_size)) = self.artifact.as_ref().and_then(|artifact| page_nodes_and_size(&artifact.body, page_index)) else {
            return rect;
        };
        let Some(page_node) = page_nodes.iter().find(|candidate| candidate.id == node.id) else {
            return rect;
        };
        let gesture_id = egui::Id::new("selection-resize");
        if super::multi::is_locked(page_node) || self.board_pan_mode(ui.ctx()) {
            ui.data_mut(|data| data.remove::<ResizeGesture>(gesture_id));
            return rect;
        }
        // A gesture of another node (the selection moved on) is over.
        let mut gesture: Option<ResizeGesture> = ui.data(|data| data.get_temp::<ResizeGesture>(gesture_id)).filter(|gesture| gesture.node_id == node.id);
        let pixels_per_point = ui.ctx().pixels_per_point();
        let side = (rect.width().min(rect.height()) / 3.0).clamp(4.0, 12.0);
        let corners = [rect.left_top(), rect.right_top(), rect.left_bottom(), rect.right_bottom()];
        for (index, corner) in corners.into_iter().enumerate() {
            let hit = Rect::from_center_size(corner, Vec2::splat(side));
            let response = ui.interact(hit, egui::Id::new(("selection-handle", node.id.as_str(), index)), Sense::drag());
            super::board::note_board_press(ui.ctx(), &response);
            if response.drag_started_by(egui::PointerButton::Primary) {
                let press = ui.input(|input| input.pointer.press_origin()).unwrap_or(corner);
                gesture = Some(ResizeGesture {
                    node_id: node.id.clone(),
                    page_index,
                    corner: index,
                    start: node.bounds,
                    press,
                    delta: Vec2::ZERO,
                    page_rect: page,
                    page_size,
                });
                self.region_clicked = true;
                // A resize on another page makes it current, as a click there does.
                self.focus_page(page_index);
            }
            if response.dragged_by(egui::PointerButton::Primary) {
                if let (Some(current), Some(pointer)) = (gesture.as_mut(), response.interact_pointer_pos()) {
                    if current.corner == index {
                        current.delta = pointer - current.press;
                    }
                }
            }
            if response.drag_stopped_by(egui::PointerButton::Primary) && gesture.as_ref().is_some_and(|g| g.corner == index) {
                if let Some(done) = gesture.take() {
                    if let Some(bounds) = self.resize_result(&done, pixels_per_point).filter(|bounds| *bounds != done.start) {
                        let operation = designllm_contracts::Operation::SetGeometry { node_id: done.node_id, bounds };
                        if let Err(message) = self.change(vec![operation], &[designllm_contracts::Intent::Design]) {
                            self.set_error(message);
                        }
                    }
                }
            }
        }
        // A release the handle did not see (the window lost the pointer) ends the gesture without a change.
        if gesture.is_some() && !ui.input(|input| input.pointer.primary_down()) {
            gesture = None;
        }
        let outline = gesture
            .as_ref()
            .and_then(|current| self.resize_result(current, pixels_per_point))
            .map_or(rect, |bounds| super::page_to_screen(bounds, page_size, page));
        ui.data_mut(|data| match gesture {
            Some(current) => data.insert_temp(gesture_id, current),
            None => data.remove::<ResizeGesture>(gesture_id),
        });
        outline
    }

    /// The design-check warning callout (tokens `component.lint-warning.callout`): the warnings of the one selected
    /// node, one line each (`state-warn` 16 + the `text.lint-rules` sentence) with up to two fix buttons under it, in
    /// rule order, three shown and `{n}개 더` for the rest. Same box and place as the refusal callout, in `warn-soft`
    /// with a 24% `warn` ring, kept clear of the selection toolbar. It never blocks an edit; a fixed warning leaves
    /// the list when the engine stops reporting it, and the box goes with the last one.
    #[allow(clippy::too_many_arguments)]
    fn draw_lint_warnings(&mut self, ctx: &Context, palette: &theme::Palette, board: Rect, frame: Rect, toolbar: Rect, node_id: &str, kind: designllm_contracts::ArtifactKind) {
        let mut warnings: Vec<designllm_contracts::Diagnostic> = self
            .lint_warnings
            .iter()
            .filter(|diagnostic| !diagnostic.blocking && diagnostic.node_id.as_deref() == Some(node_id))
            .cloned()
            .collect();
        if warnings.is_empty() {
            return;
        }
        let rank = |code: &str| match lint_rule(code) {
            "CLIPPED" => 0,
            "CHILD_OUTSIDE_PARENT" => 1,
            "OUT_OF_PAGE" => 2,
            "TOUCH_TARGET" => 3,
            "CONTRAST" => 4,
            _ => 5,
        };
        warnings.sort_by_key(|diagnostic| rank(&diagnostic.code));
        let expanded_id = egui::Id::new(("lint-warnings-expanded", node_id.to_owned()));
        let expanded = ctx.data(|data| data.get_temp::<bool>(expanded_id)).unwrap_or(false);
        let shown = if expanded { warnings.len() } else { warnings.len().min(3) };
        let more = warnings.len() - shown;
        let lines: Vec<(String, Vec<RefusalFix>)> = warnings[..shown]
            .iter()
            .map(|diagnostic| {
                let line = lint_message(diagnostic, kind, self.artifact.as_ref())
                    .or_else(|| lint_label(&diagnostic.code).map(str::to_owned))
                    .unwrap_or_else(|| {
                        let mut error = designllm_contracts::DltError::new(designllm_contracts::ErrorCode::InvalidArgument, "");
                        error.diagnostics = vec![diagnostic.clone()];
                        refusal_text(&error, "")
                    });
                let fixes = if matches!(kind, designllm_contracts::ArtifactKind::Canvas | designllm_contracts::ArtifactKind::Slides) { lint_fixes(diagnostic) } else { Vec::new() };
                (line, fixes)
            })
            .collect();
        // Height estimate for the flip: per line 16 (+ 8 + 28 with buttons), 8 between, `{n}개 더` 16, padding 8 + 10.
        let height: f32 = 18.0
            + lines.iter().map(|(_, fixes)| 16.0 + if fixes.is_empty() { 0.0 } else { 36.0 }).sum::<f32>()
            + 8.0 * lines.len().saturating_sub(1) as f32
            + if more > 0 { 24.0 } else { 0.0 };
        let mut outer = frame;
        if self.reservations.iter().any(|reservation| reservation.node_ids.iter().any(|id| id == node_id)) {
            outer = frame.expand(AGENT_OUTSET);
        }
        let below_top = if toolbar.top() >= outer.bottom() { toolbar.bottom() + 8.0 } else { outer.bottom() + 8.0 };
        let (anchor, pivot) = if below_top + height <= board.bottom() - 12.0 {
            (Pos2::new(outer.left(), below_top), Align2::LEFT_TOP)
        } else {
            let above_bottom = if toolbar.bottom() <= outer.top() { toolbar.top() - 8.0 } else { outer.top() - 8.0 };
            (Pos2::new(outer.left(), above_bottom), Align2::LEFT_BOTTOM)
        };
        let ring = theme::with_alpha(palette.warn, 0.24);
        let mut fixed: Option<RefusalFix> = None;
        let mut expand = false;
        egui::Area::new(egui::Id::new("lint-warnings"))
            .order(egui::Order::Foreground)
            .pivot(pivot)
            .fixed_pos(anchor)
            .fade_in(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(palette.warn_soft)
                    .corner_radius(CornerRadius::same(theme::RADIUS_CHIP))
                    .stroke(Stroke::new(1.0, ring))
                    .shadow(theme::float_shadow()[1])
                    .inner_margin(egui::Margin { left: 10, right: 10, top: 8, bottom: 10 })
                    .show(ui, |ui| {
                        ui.set_max_width(280.0 - 20.0);
                        ui.spacing_mut().item_spacing = Vec2::new(6.0, 8.0);
                        for (line, fixes) in &lines {
                            ui.horizontal(|ui| {
                                let (icon_rect, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                                icons::paint(ui, icon_rect, "state-warn", palette.warn);
                                ui.add(egui::Label::new(egui::RichText::new(line).font(theme::display_font(false, 12.0)).color(palette.ink)).wrap());
                            });
                            if !fixes.is_empty() {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    for fix in fixes.iter().take(2) {
                                        if callout_button_ring(ui, palette, fix.icon, fix.label, ring).clicked() {
                                            fixed = Some(fix.clone());
                                        }
                                    }
                                });
                            }
                        }
                        if more > 0 {
                            let label = format!("{more}개 더");
                            if widgets::text_button(ui, palette, &label, palette.ink_2).clicked() {
                                expand = true;
                            }
                        }
                    });
            });
        if expand {
            ctx.data_mut(|data| data.insert_temp(expanded_id, true));
        }
        if let Some(fix) = fixed {
            let mut args = fix.args;
            if args.get("node_id").is_none() {
                args["node_id"] = json!(node_id);
            }
            self.invoke_ui(fix.command, args);
        }
    }

    /// The outlines of a multi-selection (mockup `state-multiselect`): each node 1 px `accent`, the shared box 1.5 px
    /// with four handles, and the multi-selection toolbar above the box.
    pub(super) fn paint_multi_selection(
        &mut self,
        ctx: &Context,
        ui: &mut Ui,
        palette: &theme::Palette,
        picked: &[Rect],
        shared: Rect,
        primary: &NodeProperties,
    ) {
        // The board's frame list holds top-level frames; a nested node (a stack's child) is placed from its bounds,
        // so every selected node gets its outline and the shared box covers them all.
        let (picked, shared) = match (self.board_rect, self.active.as_ref()) {
            (Some(board_rect), Some(_)) => {
                let view = self.view_ref();
                let origin = board_rect.min + view.pan;
                let frames = self.node_frames(origin, view.zoom);
                let rects: Vec<Rect> = self
                    .selected_nodes
                    .iter()
                    .filter_map(|id| {
                        frames
                            .iter()
                            .find_map(|(frame_id, rect)| (frame_id == id).then_some(*rect))
                            .or_else(|| self.node_rect_on_board(id, origin, view.zoom))
                    })
                    .collect();
                let union = rects.iter().copied().reduce(|a, b| a.union(b)).unwrap_or(shared);
                (rects, union)
            }
            _ => (picked.to_vec(), shared),
        };
        let picked = picked.as_slice();
        let painter = ui.painter();
        for rect in picked {
            painter.rect_stroke(*rect, CornerRadius::ZERO, Stroke::new(1.0, palette.accent), StrokeKind::Inside);
        }
        paint_selection_box(painter, shared, palette.accent);
        let Some(board_rect) = self.board_rect else {
            return;
        };
        let mut tools = vec![Tool::Split { icon: "align-left", action: Action::Align }];
        if self.selected_nodes.len() >= 3 {
            tools.push(Tool::Split { icon: "distribute-v", action: Action::Distribute });
        }
        if commands::available("edit.group") {
            tools.push(Tool::Icon { icon: "group", action: Action::Group, active: false });
        }
        tools.extend([Tool::Sep, Tool::Icon { icon: "ai-agent", action: Action::Ai, active: false }, Tool::Icon { icon: "more", action: Action::More, active: false }]);
        let toolbar = self.place_toolbar(board_rect, shared, toolbar_size(ui, &tools));
        self.show_toolbar(ctx, palette, "multi-toolbar", toolbar, &tools, primary);
    }

    /// A selected figure panel (§9.3 차트 패널, mockup `figure-board`): the 1.5 px selection box with its handles on
    /// the panel's frame and, above it, `차트 종류 ▾ │ AI · ⋯` (a diagram or image panel: `AI · ⋯`). Returns true when
    /// the selection is a figure panel.
    fn draw_figure_panel_selection(&mut self, ctx: &Context, ui: &mut Ui, palette: &theme::Palette, origin: Pos2, zoom: f32) -> bool {
        let Some(id) = self.selected_node.clone() else {
            return false;
        };
        let Some(figure) = self.figure_document() else {
            return false;
        };
        let Some(panel) = figure.panels.iter().find(|panel| panel.id == id).cloned() else {
            return false;
        };
        let Some(kind) = self.active.as_ref().map(|item| item.kind) else {
            return false;
        };
        let slots = board::arrange(kind, &self.page_sizes(), zoom);
        let Some(slot) = slots.first() else {
            return false;
        };
        let page = board::to_screen(slot.world, origin, zoom);
        let rect = frame_on_screen(panel.bounds, page, zoom);
        paint_selection_box(ui.painter(), rect, palette.accent);
        let Some(board_rect) = self.board_rect else {
            return true;
        };
        let chart_kind = match &panel.body {
            designllm_contracts::PanelBody::Plot(plot) => Some(match plot.kind.as_str() {
                "bar" => "막대",
                "scatter" => "점",
                _ => "선",
            }),
            _ => None,
        };
        let font = theme::display_font(false, 13.0);
        let kind_width = chart_kind.map(|label| 10.0 + ui.painter().layout_no_wrap(label.to_owned(), font.clone(), palette.ink).size().x + 2.0 + 16.0 + 4.0);
        let sheet_toggle = commands::available("figure.toggle_sheet");
        let sheet_width = if sheet_toggle { 32.0 + 2.0 } else { 0.0 };
        let width = kind_width.map_or(0.0, |width| width + 2.0 + sheet_width + 9.0 + 2.0) + 32.0 + 2.0 + 32.0 + 8.0 + 2.0;
        let at = self.place_toolbar(board_rect, rect, Vec2::new(width, 42.0));
        let plot_editable = self.feature_edit_supported(designllm_contracts::Feature::Plot);
        egui::Area::new(egui::Id::new("figure-panel-toolbar"))
            .order(egui::Order::Foreground)
            .pivot(Align2::CENTER_TOP)
            .fixed_pos(at.center_top())
            .constrain(false)
            .fade_in(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(palette.surface)
                    .stroke(Stroke::new(1.0, palette.line))
                    .corner_radius(CornerRadius::same(theme::RADIUS_FLOAT))
                    .shadow(theme::float_shadow()[1])
                    .inner_margin(egui::Margin::same(4))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            if let (Some(label), Some(kind_width), true) = (chart_kind, kind_width, plot_editable) {
                                let (button, response) = ui.allocate_exact_size(Vec2::new(kind_width, 32.0), Sense::click());
                                if response.hovered() {
                                    ui.painter().rect_filled(button, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_2);
                                }
                                ui.painter().text(Pos2::new(button.left() + 10.0, button.center().y), Align2::LEFT_CENTER, label, font.clone(), palette.ink);
                                icons::paint(ui, Rect::from_min_size(Pos2::new(button.right() - 20.0, button.center().y - 8.0), Vec2::splat(16.0)), "chev-down", palette.ink_3);
                                let response = response.on_hover_text("차트 종류");
                                menu(&response, |ui| {
                                    for (value, name) in [("line", "선"), ("bar", "막대"), ("scatter", "점")] {
                                        let chosen = name == label;
                                        if widgets::menu_item(ui, palette, Some("ins-chart"), name, "", chosen).clicked() {
                                            if !chosen {
                                                self.invoke_ui("figure.plot", json!({ "panel_id": panel.id, "kind": value }));
                                            }
                                            ui.close();
                                        }
                                    }
                                });
                                // The data sheet under the board (§8): on while it is shown.
                                if sheet_toggle {
                                    let shown = !self.view_ref().sheet_hidden;
                                    if tool_button(ui, palette, Some("ins-data"), shown, "데이터 시트").clicked() {
                                        self.invoke_ui("figure.toggle_sheet", json!({}));
                                    }
                                }
                                let (sep, _) = ui.allocate_exact_size(Vec2::new(1.0 + 2.0 * theme::SEPARATOR_MARGIN_X, 32.0), Sense::hover());
                                ui.painter().rect_filled(Rect::from_center_size(sep.center(), Vec2::new(1.0, theme::SEPARATOR_HEIGHT)), CornerRadius::ZERO, palette.line);
                            }
                            if tool_button(ui, palette, Some("ai-agent"), false, Action::Ai.hint()).clicked() {
                                self.invoke_ui("nav.ai", json!({}));
                            }
                            let more = tool_button(ui, palette, Some("more"), false, Action::More.hint());
                            menu(&more, |ui| {
                                for (command, args) in [("figure.arrange_panels", json!({})), ("figure.remove_panel", json!({ "panel_id": panel.id }))] {
                                    if !commands::available(command) {
                                        continue;
                                    }
                                    let Some(spec) = commands::find(command) else {
                                        continue;
                                    };
                                    if widgets::menu_item(ui, palette, None, spec.label, spec.shortcut, false).clicked() {
                                        self.invoke_ui(command, args);
                                        ui.close();
                                    }
                                }
                            });
                        });
                    });
            });
        true
    }

    /// Where the selection toolbar goes (§4, tokens `ctx.flip`): above the selection, or below it when the place
    /// above meets the board's top edge, the view group, or the current page's title strip with its ⤢ ⇪ buttons.
    fn place_toolbar(&self, board_rect: Rect, selection: Rect, size: Vec2) -> Rect {
        let mut keep_clear: Vec<Rect> = self.view_group_rect.into_iter().collect();
        if let Some(active) = self.active.as_ref() {
            let view = self.view_ref();
            let origin = board_rect.min + view.pan;
            let slots = board::arrange(active.kind, &self.page_sizes(), view.zoom);
            if let Some(slot) = slots.iter().find(|slot| slot.page_index == view.page_index) {
                let page = board::to_screen(slot.world, origin, view.zoom);
                // tokens `ctx.flip`: only the title row's ⤢ ⇪ buttons (two 24 px minis and 2 between them at the row's
                // right end, 2 above the 20 px row that sits 4 above the page). Covering the title text is allowed.
                let width = 2.0 * theme::MINI_BUTTON + 2.0;
                let top = page.top() - theme::ARTBOARD_TITLE_GAP - theme::ARTBOARD_TITLE_HEIGHT - 2.0;
                keep_clear.push(Rect::from_min_size(Pos2::new(page.right() - width, top), Vec2::new(width, theme::MINI_BUTTON)));
            }
        }
        let above = frame::selection_toolbar(selection, board_rect, size, None);
        let flipped = above.top() >= selection.bottom();
        if flipped || !keep_clear.iter().any(|rect| rect.intersects(above)) {
            return above;
        }
        let below = Rect::from_min_size(Pos2::new(above.left(), selection.bottom() + 10.0), size);
        if below.bottom() <= board_rect.bottom() - 12.0 {
            below
        } else {
            above
        }
    }

    /// The floating selection toolbar (§9.3, mockup `.float.ctx`): a white group 42 px high, 4 px padding, 2 px between
    /// controls, icons in `ink-2`. Every control acts; the ones that set a value open a small field next to it.
    fn show_toolbar(&mut self, ctx: &Context, palette: &theme::Palette, id: &str, at: Rect, tools: &[Tool], node: &NodeProperties) {
        // Centred on the selection by its real width (tokens `ctx.x`), not by the estimate used to place it.
        egui::Area::new(egui::Id::new(id))
            .order(egui::Order::Foreground)
            .pivot(Align2::CENTER_TOP)
            .fixed_pos(at.center_top())
            .constrain(false)
            .fade_in(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(palette.surface)
                    .stroke(Stroke::new(1.0, palette.line))
                    .corner_radius(CornerRadius::same(theme::RADIUS_FLOAT))
                    .shadow(theme::float_shadow()[1])
                    .inner_margin(egui::Margin::same(4))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            for tool in tools {
                                self.toolbar_item(ui, palette, tool, node);
                            }
                        });
                    });
            });
    }

    fn toolbar_item(&mut self, ui: &mut Ui, palette: &theme::Palette, tool: &Tool, node: &NodeProperties) {
        let id = |key: &str| egui::Id::new(("toolbar-field", node.id.as_str(), key.to_owned()));
        match tool {
            Tool::Sep => {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(1.0 + 2.0 * theme::SEPARATOR_MARGIN_X, 32.0), Sense::hover());
                ui.painter().rect_filled(Rect::from_center_size(rect.center(), Vec2::new(1.0, theme::SEPARATOR_HEIGHT)), CornerRadius::ZERO, palette.line);
            }
            Tool::Fill(colour) => {
                let response = tool_button(ui, palette, None, false, "채우기");
                let sample = Rect::from_center_size(response.rect.center(), Vec2::splat(18.0));
                ui.painter().rect_filled(sample, CornerRadius::same(5), *colour);
                ui.painter().rect_stroke(sample, CornerRadius::same(theme::RADIUS_MINI), Stroke::new(1.0, palette.swatch_ring), StrokeKind::Inside);
                popup(&response, |ui| {
                    if let Some(new) = super::properties::row(ui, palette, Some("색"), |ui| {
                        let width = ui.available_width();
                        super::properties::colour_field(ui, palette, id("fill"), width, Some(&node.style.fill))
                    }) {
                        self.invoke_ui("props.fill", json!({ "color": new }));
                    }
                });
            }
            Tool::Size(label) => {
                let font = theme::display_font(false, 13.0);
                let text = ui.painter().layout_no_wrap(label.clone(), font.clone(), palette.ink).size().x;
                let (rect, response) = ui.allocate_exact_size(Vec2::new(10.0 + text + 2.0 + 16.0 + 4.0, 32.0), Sense::click());
                if response.hovered() {
                    ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_2);
                }
                ui.painter().text(Pos2::new(rect.left() + 10.0, rect.center().y), Align2::LEFT_CENTER, label, font, palette.ink);
                icons::paint(ui, Rect::from_min_size(Pos2::new(rect.right() - 20.0, rect.center().y - 8.0), Vec2::splat(16.0)), "chev-down", palette.ink_3);
                let response = response.on_hover_text("글자 크기");
                popup(&response, |ui| {
                    let size = node.style.font_size as f64 / designllm_contracts::EMU_PER_PT as f64;
                    if let Some(new) = super::properties::row(ui, palette, Some("크기"), |ui| {
                        let width = ui.available_width();
                        super::properties::field(ui, palette, id("font"), width, "", Some(size), "pt", 1, true)
                    }) {
                        self.invoke_ui("props.text_style", json!({ "font_size": new.max(1.0) }));
                    }
                });
            }
            Tool::Icon { icon, action, active } => {
                let response = tool_button(ui, palette, Some(icon), *active, action.hint());
                self.toolbar_action(palette, *action, &response, node);
            }
            Tool::Split { icon, action } => {
                let main = tool_button(ui, palette, Some(icon), false, action.hint());
                let (chev, chev_response) = {
                    let saved = ui.spacing().item_spacing.x;
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.add_space(-2.0);
                    let (rect, response) = ui.allocate_exact_size(Vec2::new(theme::SPLIT_CHEVRON_WIDTH, 32.0), Sense::click());
                    ui.spacing_mut().item_spacing.x = saved;
                    (rect, response)
                };
                if chev_response.hovered() {
                    ui.painter().rect_filled(chev, CornerRadius::same(5), palette.surface_2);
                }
                icons::paint(ui, Rect::from_center_size(chev.center(), Vec2::splat(16.0)), "chev-down", palette.ink_3);
                match action {
                    Action::Align => {
                        if main.clicked() {
                            self.invoke_ui("props.align", json!({ "edge": "left" }));
                        }
                        menu(&chev_response, |ui| {
                            for (icon, label, edge, keys) in [
                                ("align-left", "왼쪽", "left", "Alt+A"),
                                ("align-h-center", "가운데", "h-center", "Alt+H"),
                                ("align-right", "오른쪽", "right", "Alt+D"),
                                ("align-top", "위쪽", "top", "Alt+W"),
                                ("align-v-center", "중간", "v-center", "Alt+V"),
                                ("align-bottom", "아래쪽", "bottom", "Alt+S"),
                            ] {
                                if widgets::menu_item(ui, palette, Some(icon), label, keys, false).clicked() {
                                    self.invoke_ui("props.align", json!({ "edge": edge }));
                                    ui.close();
                                }
                            }
                        });
                    }
                    Action::Distribute => {
                        if main.clicked() {
                            self.invoke_ui("props.distribute", json!({ "axis": "v" }));
                        }
                        menu(&chev_response, |ui| {
                            for (icon, label, axis, keys) in [("distribute-h", "가로 분배", "h", "Alt+Shift+H"), ("distribute-v", "세로 분배", "v", "Alt+Shift+V")] {
                                if widgets::menu_item(ui, palette, Some(icon), label, keys, false).clicked() {
                                    self.invoke_ui("props.distribute", json!({ "axis": axis }));
                                    ui.close();
                                }
                            }
                        });
                    }
                    _ => {
                        // Text alignment: both halves open the four choices.
                        let current = node.style.align.as_str();
                        for response in [&main, &chev_response] {
                            menu(response, |ui| {
                                for (icon, label, align) in [
                                    ("text-align-left", "왼쪽", "left"),
                                    ("text-align-center", "가운데", "center"),
                                    ("text-align-right", "오른쪽", "right"),
                                    ("text-align-justify", "양쪽", "justify"),
                                ] {
                                    let chosen = current == align || (current.is_empty() && align == "left");
                                    if widgets::menu_item(ui, palette, Some(icon), label, "", chosen).clicked() {
                                        if !chosen {
                                            self.invoke_ui("props.text_align", json!({ "align": align }));
                                        }
                                        ui.close();
                                    }
                                }
                            });
                        }
                    }
                }
            }
        }
    }

    fn toolbar_action(&mut self, palette: &theme::Palette, action: Action, response: &egui::Response, node: &NodeProperties) {
        let id = |key: &str| egui::Id::new(("toolbar-field", node.id.as_str(), key.to_owned()));
        let style = &node.style;
        match action {
            Action::Stroke => popup(response, |ui| {
                let colour = super::properties::paints(&style.stroke).then(|| style.stroke.clone()).unwrap_or_else(|| String::from("#566479"));
                if let Some(new) = super::properties::row(ui, palette, Some("색"), |ui| {
                    let width = ui.available_width();
                    super::properties::colour_field(ui, palette, id("stroke"), width, Some(&colour))
                }) {
                    self.invoke_ui("props.stroke", json!({ "color": new }));
                }
                let kind = self.active.as_ref().map_or(designllm_contracts::ArtifactKind::Canvas, |item| item.kind);
                let (per, label, decimals) = super::properties::stroke_unit(super::properties::unit_for(kind));
                let width = style.stroke_width as f64 / per;
                if let Some(new) = super::properties::row(ui, palette, Some("두께"), |ui| {
                    let room = ui.available_width();
                    super::properties::field(ui, palette, id("stroke-width"), room, "", Some(width), label, decimals, true)
                }) {
                    if !super::properties::paints(&style.stroke) {
                        self.invoke_ui("props.stroke", json!({ "color": colour }));
                    }
                    self.invoke_ui("props.stroke_width", json!({ "value": (new.max(0.0) * per).round() }));
                }
                // tokens `component.stroke-rows.toolbar-popover`: the same 모양 row as the panel.
                if commands::available("props.stroke_style") && self.feature_edit_supported(designllm_contracts::Feature::Effects) {
                    if let Some(dash) = super::properties::stroke_dash_row(ui, palette, &[], node) {
                        self.invoke_ui("props.stroke_style", json!({ "dash": dash }));
                    }
                }
            }),
            Action::Radius => popup(response, |ui| {
                let radius = style.corner_radius.unwrap_or(0) as f64 / super::properties::EMU_PER_PX;
                if let Some(new) = super::properties::row(ui, palette, Some("모서리"), |ui| {
                    let width = ui.available_width();
                    super::properties::field(ui, palette, id("radius"), width, "", Some(radius), "px", 0, true)
                }) {
                    self.invoke_ui("props.corner_radius", json!({ "value": new.max(0.0) }));
                }
            }),
            Action::Gap => popup(response, |ui| {
                let gap = node.layout.as_ref().map_or(0, |layout| layout.gap) as f64 / super::properties::EMU_PER_PX;
                if let Some(new) = super::properties::row(ui, palette, Some("간격"), |ui| {
                    let width = ui.available_width();
                    super::properties::field(ui, palette, id("gap"), width, "", Some(gap), "px", 0, true)
                }) {
                    self.invoke_ui("props.layout_gap", json!({ "value": new.max(0.0) }));
                }
            }),
            Action::TextColor => popup(response, |ui| {
                if let Some(new) = super::properties::row(ui, palette, Some("색"), |ui| {
                    let width = ui.available_width();
                    super::properties::colour_field(ui, palette, id("text-color"), width, Some(&style.color))
                }) {
                    self.invoke_ui("props.text_style", json!({ "color": new }));
                }
            }),
            Action::Direction if response.clicked() => {
                let column = node.layout.as_ref().is_some_and(|layout| layout.mode == "column");
                self.invoke_ui("props.layout_direction", json!({ "direction": if column { "row" } else { "column" } }));
            }
            Action::Bold if response.clicked() => self.invoke_ui("props.text_style", json!({ "bold": !style.bold })),
            Action::Italic if response.clicked() => self.invoke_ui("props.text_style", json!({ "italic": !style.italic })),
            Action::Group if response.clicked() => self.invoke_ui("edit.group", json!({})),
            Action::Ai if response.clicked() => self.invoke_ui("nav.ai", json!({})),
            Action::More => {
                let many = self.selected_nodes.len() > 1;
                let group = node.kind == designllm_contracts::NodeKind::Group;
                let locked = node.locked == Some(true);
                let hidden = node.hidden == Some(true);
                let kind = self.active.as_ref().map_or(designllm_contracts::ArtifactKind::Canvas, |item| item.kind);
                menu(response, |ui| {
                    let mut entries: Vec<Option<&str>> = Vec::new();
                    if !many {
                        entries.push(Some("edit.duplicate"));
                    }
                    entries.extend([Some("edit.copy"), Some("edit.paste"), Some("edit.copy_properties"), Some("edit.paste_properties"), None]);
                    entries.extend([Some("edit.group"), Some("edit.ungroup"), Some("edit.stack"), None]);
                    if !many {
                        entries.extend([Some("edit.order_forward"), Some("edit.order_backward"), Some("edit.order_front"), Some("edit.order_back"), None]);
                    }
                    entries.extend(layer_state_commands(kind, hidden, locked).into_iter().map(Some));
                    entries.extend([None, Some("edit.delete")]);
                    for entry in entries {
                        let Some(command) = entry else {
                            ui.separator();
                            continue;
                        };
                        if !commands::available(command) || (command == "edit.ungroup" && !group) || (command == "edit.group" && !many && group) {
                            continue;
                        }
                        let Some(spec) = commands::find(command) else {
                            continue;
                        };
                        if widgets::menu_item(ui, palette, None, spec.label, spec.shortcut, false).clicked() {
                            self.invoke_ui(command, json!({}));
                            ui.close();
                        }
                    }
                });
            }
            _ => {}
        }
    }
}

/// What a toolbar control does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Stroke,
    Radius,
    Direction,
    Gap,
    Bold,
    Italic,
    TextColor,
    TextAlign,
    Align,
    Distribute,
    Group,
    Ai,
    More,
}

impl Action {
    /// The hover text: the control's name and shortcut (§7 L2).
    fn hint(self) -> &'static str {
        match self {
            Self::Stroke => "테두리",
            Self::Radius => "모서리",
            Self::Direction => "스택 방향",
            Self::Gap => "간격",
            Self::Bold => "굵게  Ctrl+B",
            Self::Italic => "기울임  Ctrl+I",
            Self::TextColor => "글자 색",
            Self::TextAlign => "정렬",
            Self::Align => "정렬",
            Self::Distribute => "분배",
            Self::Group => "그룹  Ctrl+G",
            Self::Ai => "AI에게 요청  Ctrl+J",
            Self::More => "더 보기",
        }
    }
}

/// One control of the selection toolbar.
#[derive(Clone, Debug)]
enum Tool {
    Fill(Color32),
    Size(String),
    Icon { icon: &'static str, action: Action, active: bool },
    Split { icon: &'static str, action: Action },
    Sep,
}

/// The toolbar of one selected node by its kind (§9.3).
fn node_tools(node: &NodeProperties) -> Vec<Tool> {
    use designllm_contracts::NodeKind;
    let icon = |icon: &'static str, action: Action| Tool::Icon { icon, action, active: false };
    let mut tools = Vec::new();
    match node.kind {
        NodeKind::Text => {
            let size = node.style.font_size as f64 / designllm_contracts::EMU_PER_PT as f64;
            tools.push(Tool::Size(widgets::format_number(size, 1)));
            tools.push(Tool::Sep);
            tools.push(Tool::Icon { icon: "bold", action: Action::Bold, active: node.style.bold });
            tools.push(Tool::Icon { icon: "italic", action: Action::Italic, active: node.style.italic });
            tools.push(icon("text-color", Action::TextColor));
            tools.push(Tool::Sep);
            let align = match node.style.align.as_str() {
                "center" => "text-align-center",
                "right" => "text-align-right",
                "justify" => "text-align-justify",
                _ => "text-align-left",
            };
            tools.push(Tool::Split { icon: align, action: Action::TextAlign });
            tools.push(Tool::Sep);
        }
        NodeKind::Rectangle | NodeKind::Ellipse | NodeKind::Path => {
            let fill = widgets::parse_hex(&node.style.fill).unwrap_or(Color32::WHITE);
            tools.push(Tool::Fill(fill));
            tools.push(icon("border-weight", Action::Stroke));
            if node.kind == NodeKind::Rectangle {
                tools.push(icon("radius", Action::Radius));
            }
            let frame = node.kind == NodeKind::Rectangle && (node.has_children || node.layout.is_some());
            let mode = node.layout.as_ref().map_or("", |layout| layout.mode.as_str());
            if frame && matches!(mode, "row" | "column") {
                tools.push(Tool::Sep);
                tools.push(icon(if mode == "column" { "dir-col" } else { "dir-row" }, Action::Direction));
                tools.push(icon("gap", Action::Gap));
            }
            tools.push(Tool::Sep);
        }
        NodeKind::Image => {
            tools.push(icon("radius", Action::Radius));
            tools.push(Tool::Sep);
        }
        NodeKind::Line => {
            tools.push(icon("border-weight", Action::Stroke));
            tools.push(Tool::Sep);
        }
        NodeKind::Group | NodeKind::Video => {}
    }
    tools.push(icon("ai-agent", Action::Ai));
    tools.push(icon("more", Action::More));
    tools
}

/// The size of the toolbar for `tools`: the controls, 2 px between them, 4 px padding and the 1 px border.
fn toolbar_size(ui: &Ui, tools: &[Tool]) -> Vec2 {
    let widths: f32 = tools
        .iter()
        .map(|tool| match tool {
            Tool::Sep => 9.0,
            Tool::Split { .. } => 46.0,
            Tool::Size(label) => 10.0 + ui.painter().layout_no_wrap(label.clone(), theme::display_font(false, 13.0), Color32::BLACK).size().x + 2.0 + 16.0 + 4.0,
            _ => 32.0,
        })
        .sum();
    let gaps = 2.0 * tools.len().saturating_sub(1) as f32;
    Vec2::new(widths + gaps + 8.0 + 2.0, 42.0)
}

/// A 32 px toolbar button (mockup `.ib`): a 24 px icon in `ink-2`, `surface-2` on hover, `accent` on `accent-soft`
/// when on. The hover text is its name and shortcut.
fn tool_button(ui: &mut Ui, palette: &theme::Palette, icon: Option<&str>, active: bool, hint: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(theme::ICON_BUTTON), Sense::click());
    let corner = CornerRadius::same(theme::RADIUS_CONTROL);
    if active {
        ui.painter().rect_filled(rect, corner, palette.accent_soft);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, corner, palette.surface_2);
    }
    if let Some(icon) = icon {
        icons::paint(ui, Rect::from_center_size(rect.center(), Vec2::splat(theme::ICON_IN_BUTTON)), icon, if active { palette.accent } else { palette.ink_2 });
    }
    response.on_hover_text(hint)
}

/// A small popover under a toolbar control with its value fields.
fn popup(response: &egui::Response, add: impl FnOnce(&mut Ui)) {
    egui::Popup::from_toggle_button_response(response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_width(240.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui);
        });
}

/// A menu under a toolbar control.
fn menu(response: &egui::Response, add: impl FnOnce(&mut Ui)) {
    egui::Popup::from_toggle_button_response(response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_width(240.0);
            add(ui);
        });
}

/// A corner-handle resize under way: the node, the corner (0 left-top, 1 right-top, 2 left-bottom, 3 right-bottom),
/// its bounds when the press began, and the pointer's move since then (screen points).
#[derive(Clone)]
struct ResizeGesture {
    node_id: String,
    page_index: u32,
    corner: usize,
    start: designllm_contracts::Rect,
    press: Pos2,
    delta: Vec2,
    page_rect: Rect,
    page_size: designllm_contracts::Size,
}

fn page_nodes_and_size(body: &ArtifactBody, page_index: u32) -> Option<(&[Node], designllm_contracts::Size)> {
    match body {
        ArtifactBody::Canvas(document) => document.pages.get(page_index as usize).map(|page| (page.nodes.as_slice(), page.size)),
        ArtifactBody::Slides(document) => document.pages.get(page_index as usize).map(|page| (page.nodes.as_slice(), page.size)),
        _ => None,
    }
}

/// The bounds after a corner moved by `delta` (screen points on a page drawn `page_rect` wide): the opposite corner stays.
/// A corner dragged past the opposite one turns the box over, so the result is normalised, with the edges the moved corner
/// now lies on (for the guide snap). `None` when the result would have no size.
fn resized_bounds(start: designllm_contracts::Rect, corner: usize, delta: Vec2, page_rect: Rect, page_size: designllm_contracts::Size) -> Option<(designllm_contracts::Rect, ResizeHandle)> {
    if page_rect.width() <= 0.0 || page_rect.height() <= 0.0 {
        return None;
    }
    let dx = (f64::from(delta.x) * page_size.width as f64 / f64::from(page_rect.width())).round() as i64;
    let dy = (f64::from(delta.y) * page_size.height as f64 / f64::from(page_rect.height())).round() as i64;
    let (left, top, right, bottom) = (start.x, start.y, start.x + start.width, start.y + start.height);
    let (moved_x, fixed_x) = if corner % 2 == 0 { (left + dx, right) } else { (right + dx, left) };
    let (moved_y, fixed_y) = if corner < 2 { (top + dy, bottom) } else { (bottom + dy, top) };
    let bounds = designllm_contracts::Rect {
        x: moved_x.min(fixed_x),
        y: moved_y.min(fixed_y),
        width: (moved_x - fixed_x).abs(),
        height: (moved_y - fixed_y).abs(),
    };
    let handle = ResizeHandle {
        x: Some(if moved_x < fixed_x { ActiveEdge::Start } else { ActiveEdge::End }),
        y: Some(if moved_y < fixed_y { ActiveEdge::Start } else { ActiveEdge::End }),
    };
    (bounds.width >= 1 && bounds.height >= 1).then_some((bounds, handle))
}

/// The selection outline (§5.4): 1.5 px on the frame and four 8 px white handles centred on its corners.
fn paint_selection_box(painter: &egui::Painter, rect: Rect, colour: Color32) {
    painter.rect_stroke(rect, CornerRadius::ZERO, Stroke::new(1.5, colour), StrokeKind::Inside);
    for corner in [rect.left_top(), rect.right_top(), rect.left_bottom(), rect.right_bottom()] {
        let handle = Rect::from_center_size(corner, Vec2::splat(8.0));
        painter.rect_filled(handle, CornerRadius::same(2), Color32::WHITE);
        painter.rect_stroke(handle, CornerRadius::same(2), Stroke::new(1.5, colour), StrokeKind::Inside);
    }
}

/// The refusal callout with fix or held-edit buttons (tokens `refusal-callout.actions`: at most 2, 6 apart). Returns
/// (stop clicked, retry clicked, copy clicked, cancel clicked, fix clicked).
#[allow(clippy::too_many_arguments)]
fn refusal_callout_with(
    ctx: &Context,
    palette: &theme::Palette,
    board: Option<Rect>,
    frame: Rect,
    reason: &str,
    stop: Option<&str>,
    fixes: &[RefusalFix],
    retry: Option<&str>,
    copy: Option<&str>,
    cancel: Option<&str>,
) -> (bool, bool, bool, bool, Option<RefusalFix>) {
    let mut fixed = None;
    let mut retry_clicked = false;
    let mut copy_clicked = false;
    let mut cancel_clicked = false;
    let buttons = stop.is_some() || retry.is_some() || copy.is_some() || cancel.is_some() || !fixes.is_empty();
    let height = 8.0 + 16.0 + 10.0 + if buttons { 8.0 + 28.0 } else { 0.0 };
    let below = Pos2::new(frame.left(), frame.bottom() + 8.0);
    let room_below = board.is_none_or(|board| below.y + height <= board.bottom() - 12.0);
    let (anchor, pivot) = if room_below { (below, Align2::LEFT_TOP) } else { (Pos2::new(frame.left(), frame.top() - 8.0), Align2::LEFT_BOTTOM) };
    let ring = palette.derived(theme::Derived::DangerRing);
    let mut clicked = false;
    egui::Area::new(egui::Id::new("refusal"))
        .order(egui::Order::Foreground)
        .pivot(pivot)
        .fixed_pos(anchor)
        .fade_in(false)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(palette.danger_soft)
                .corner_radius(CornerRadius::same(theme::RADIUS_CHIP))
                .stroke(Stroke::new(1.0, ring))
                .shadow(theme::float_shadow()[1])
                .inner_margin(egui::Margin { left: 10, right: 10, top: 8, bottom: 10 })
                .show(ui, |ui| {
                    ui.set_max_width(280.0 - 20.0);
                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 8.0);
                    ui.horizontal(|ui| {
                        let (icon_rect, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                        icons::paint(ui, icon_rect, "state-error", palette.danger);
                        ui.add(egui::Label::new(egui::RichText::new(reason).font(theme::display_font(false, 12.0)).color(palette.ink)).wrap());
                    });
                    if buttons {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            for fix in fixes.iter().take(2) {
                                if callout_button(ui, palette, fix.icon, fix.label).clicked() {
                                    fixed = Some(fix.clone());
                                }
                            }
                            if let Some(label) = retry {
                                retry_clicked = callout_button(ui, palette, "tool-pencil", label).clicked();
                            }
                            if let Some(label) = copy {
                                copy_clicked = callout_button(ui, palette, "copy", label).clicked();
                            }
                            if let Some(label) = cancel {
                                cancel_clicked = callout_button(ui, palette, "close", label).clicked();
                            }
                            if let Some(label) = stop {
                                clicked = callout_button(ui, palette, "stop", label).clicked();
                            }
                        });
                    }
                });
        });
    (clicked, retry_clicked, copy_clicked, cancel_clicked, fixed)
}

/// A white action button in the refused callout (mockup `.cbtn`): 28 px, a 24% `danger` ring, a 16 px `ink-2` icon
/// and a 12 px `ink` label.
fn callout_button(ui: &mut Ui, palette: &theme::Palette, icon: &str, label: &str) -> egui::Response {
    callout_button_ring(ui, palette, icon, label, palette.derived(theme::Derived::DangerRing))
}

/// A callout button with the ring of its box (`danger-ring` on a refusal, `warn-ring` on a warning).
fn callout_button_ring(ui: &mut Ui, palette: &theme::Palette, icon: &str, label: &str, ring: Color32) -> egui::Response {
    let font = theme::display_font(false, 12.0);
    let text_width = ui.painter().layout_no_wrap(label.to_owned(), font.clone(), palette.ink).size().x;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(8.0 + 16.0 + 6.0 + text_width + 10.0, 28.0), Sense::click());
    let fill = if response.hovered() { palette.surface_2 } else { palette.field };
    // The ring sits outside the 28 px button, as the mockup's `box-shadow: 0 0 0 1px` does (outer 30).
    ui.painter().rect(rect, CornerRadius::same(theme::RADIUS_CONTROL), fill, Stroke::new(1.0, ring), StrokeKind::Outside);
    icons::paint(ui, Rect::from_min_size(rect.min + Vec2::new(8.0, 6.0), Vec2::splat(16.0)), icon, palette.ink_2);
    ui.painter().text(rect.min + Vec2::new(30.0, 14.0), Align2::LEFT_CENTER, label, font, palette.ink);
    response
}

/// The screen rectangle of a node's frame. `bounds` are page EMU; `page` is the page on screen. The
/// frame comes from the node, never from the core's text-line regions (those are for text hit tests).
pub(super) fn frame_on_screen(bounds: designllm_contracts::Rect, page: Rect, zoom: f32) -> Rect {
    let scale = 1.0 / board::EMU_PER_UNIT;
    let origin = Vec2::new(bounds.x as f32 * scale, bounds.y as f32 * scale) * zoom;
    let size = Vec2::new(bounds.width as f32 * scale, bounds.height as f32 * scale) * zoom;
    Rect::from_min_size(page.min + origin, size)
}

/// The node with `id` in a canvas or slide page, nested children included.
fn node_in_body<'a>(body: &'a ArtifactBody, id: &str) -> Option<&'a Node> {
    let find = |nodes: &'a [Node]| nodes.iter().find_map(|node| super::doc_tree::find_node_in_node(node, id));
    match body {
        ArtifactBody::Canvas(document) => document.pages.iter().find_map(|page| find(&page.nodes)),
        ArtifactBody::Slides(document) => document.pages.iter().find_map(|page| find(&page.nodes)),
        _ => None,
    }
}

/// A dashed line from `from` to `to`: strokes `dash` long with `gap` between them.
fn dashed_line(painter: &egui::Painter, from: Pos2, to: Pos2, stroke: Stroke, dash: f32, gap: f32) {
    let delta = to - from;
    let length = delta.length();
    if length <= 0.0 {
        return;
    }
    let direction = delta / length;
    let mut at = 0.0;
    while at < length {
        let end = (at + dash).min(length);
        painter.line_segment([from + direction * at, from + direction * end], stroke);
        at += dash + gap;
    }
}

/// A dashed rectangle outline.
fn dashed_rect(painter: &egui::Painter, rect: Rect, stroke: Stroke, dash: f32, gap: f32) {
    let corners = [rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom()];
    for (from, to) in corners.iter().zip(corners.iter().cycle().skip(1)) {
        dashed_line(painter, *from, *to, stroke, dash, gap);
    }
}

/// The agent's name on a tag above a node's outline.
/// The colour of an agent's marks: Claude purple, Codex green, any other runtime the third agent colour.
fn agent_colour(runtime: &str, palette: &theme::Palette) -> Color32 {
    match runtime {
        "claude" => palette.agents[0],
        "codex" => palette.agents[1],
        _ => palette.agents[2],
    }
}

/// The agent's name as its mark shows it: the known runtimes by their brand names, anything else as given.
fn agent_display_name(runtime: &str) -> String {
    match runtime.to_ascii_lowercase().as_str() {
        "claude" => String::from("Claude"),
        "codex" => String::from("Codex CLI"),
        _ => runtime.to_owned(),
    }
}

/// How far the agent outline's outer edge lies outside the node frame (tokens.json `stroke.agent`).
const AGENT_OUTSET: f32 = 3.0;

/// The rectangle of an agent's name tag for a node frame (tokens.json `agent-tag`): 18 high, 6 px side padding,
/// left = frame.left − 3, bottom = frame.top − 5 (2 above the outline's outer edge).
fn tag_rect(painter: &egui::Painter, frame_top_left: Pos2, text: &str) -> Rect {
    let width = painter.layout_no_wrap(text.to_owned(), theme::display_font(true, 11.0), Color32::WHITE).size().x + 12.0;
    Rect::from_min_size(Pos2::new(frame_top_left.x - AGENT_OUTSET, frame_top_left.y - AGENT_OUTSET - 2.0 - 18.0), Vec2::new(width, 18.0))
}

/// The agent's name tag (§5.5, mockup `.atag`): 11 px bold `on-agent` text on the agent colour, corners 4·4·4·0.
fn name_tag(painter: &egui::Painter, frame_top_left: Pos2, text: &str, fill: Color32, ink: Color32) {
    let tag = tag_rect(painter, frame_top_left, text);
    let galley = painter.layout_no_wrap(text.to_owned(), theme::display_font(true, 11.0), ink);
    painter.rect_filled(tag, CornerRadius { nw: 4, ne: 4, se: 4, sw: 0 }, fill);
    let text_top = tag.center().y - galley.size().y / 2.0;
    painter.galley(Pos2::new(tag.left() + 6.0, text_top), galley, ink);
}

impl DesktopApp {
    /// Reserved nodes (GUI_DESIGN §5.5): each job reservation from `targets.list` and `targets.reserved` gets a
    /// dashed outline in the agent's colour and a name tag. The tag names the job's client, else its runtime.
    pub(super) fn draw_reservations(&self, ui: &mut Ui, palette: &theme::Palette, origin: Pos2, zoom: f32) {
        if self.reservations.is_empty() {
            return;
        }
        // The same frames the selection and the refusal outline use, so all three share one boundary B.
        let frames = self.node_frames(origin, zoom);
        let painter = ui.painter();
        for reservation in &self.reservations {
            let name = self.reservation_name(reservation);
            let colour = agent_colour(&self.reservation_runtime(reservation), palette);
            let mut tag_at: Option<Pos2> = None;
            for id in &reservation.node_ids {
                let found = frames.iter().find_map(|(frame_id, rect)| (frame_id == id).then_some(*rect));
                let Some(frame) = found.or_else(|| self.node_rect_on_board(id, origin, zoom)) else {
                    continue;
                };
                // tokens.json `stroke.agent`: 1.5 px inside a rectangle whose outer edge is B + 3 on every side, so it
                // never lies on the selection line at B. The outer edge is put on whole pixels so that all four sides
                // land exactly 3 out.
                let outer = Rect::from_min_max(frame.min.round(), frame.max.round()).expand(AGENT_OUTSET);
                dashed_rect(painter, outer.shrink(0.75), Stroke::new(1.5, colour), 4.0, 3.0);
                tag_at.get_or_insert(frame.min.round());
            }
            if let Some(at) = tag_at {
                name_tag(painter, at, &name, colour, palette.on_agent);
            }
        }
    }

    /// The runtime behind a reservation in lower case: this window's job's runtime, else the one its owner names.
    fn reservation_runtime(&self, reservation: &super::JobReservation) -> String {
        if let Some(job) = self.job.as_ref().filter(|job| job.id == reservation.job_id) {
            if !job.runtime.is_empty() {
                return job.runtime.to_ascii_lowercase();
            }
        }
        reservation.owner.rsplit_once('(').and_then(|(_, rest)| rest.strip_suffix(')')).unwrap_or_default().to_ascii_lowercase()
    }

    /// The reason a reserved node refuses an edit, as the refusal callout states it: the agent that holds it.
    pub(super) fn busy_reason(&self, ids: &[String]) -> String {
        self.reservations
            .iter()
            .find(|reservation| reservation.node_ids.iter().any(|id| ids.contains(id)))
            .map_or_else(|| String::from("편집 중인 항목입니다"), |reservation| format!("{} 작업 중인 항목입니다", self.reservation_name(reservation)))
    }

    /// The agent behind a reservation: this window's job when it is that job (client name, else runtime),
    /// else the runtime its owner names ("job <id> (<runtime>)").
    fn reservation_name(&self, reservation: &super::JobReservation) -> String {
        if let Some(job) = self.job.as_ref().filter(|job| job.id == reservation.job_id) {
            if let Some(client) = job.client_name.as_deref().filter(|name| !name.is_empty()) {
                return client.to_owned();
            }
            if !job.runtime.is_empty() {
                return agent_display_name(&job.runtime);
            }
        }
        reservation
            .owner
            .rsplit_once('(')
            .and_then(|(_, rest)| rest.strip_suffix(')'))
            .filter(|runtime| !runtime.is_empty())
            .map_or_else(|| String::from("AI"), agent_display_name)
    }

    /// The screen rectangle of a node's frame on the board, when the node is on a page.
    fn node_rect_on_board(&self, id: &str, origin: Pos2, zoom: f32) -> Option<Rect> {
        // The laid-out frame first (stack and grid children sit where their parent placed them), then the bounds.
        if let Some(rect) = self.node_frames(origin, zoom).into_iter().find_map(|(frame_id, rect)| (frame_id == id).then_some(rect)) {
            return Some(rect);
        }
        let artifact = self.artifact.as_ref()?;
        let kind = self.active.as_ref()?.kind;
        let node = node_in_body(&artifact.body, id)?;
        let page_index = page_of(&artifact.body, id)?;
        let slots = board::arrange(kind, &self.page_sizes(), zoom);
        let slot = slots.iter().find(|slot| slot.page_index == page_index)?;
        let page = board::to_screen(slot.world, origin, zoom);
        Some(frame_on_screen(node.bounds, page, zoom))
    }

    /// A refused edit (TARGET_BUSY) is explained at the node it names: the callout sits under that node.
    pub(super) fn draw_target_refusal(&mut self, ctx: &Context, ui: &mut Ui, palette: &theme::Palette, origin: Pos2, zoom: f32) {
        let Some(text) = self.refusal.clone() else {
            return;
        };
        let held = self.held_refusal_id.as_deref().and_then(|id| self.held_edits.iter().find(|held| held.id == id)).cloned();
        let frame = self.refusal_ids.first().and_then(|id| self.node_rect_on_board(id, origin, zoom)).or_else(|| {
            held.is_some().then(|| self.board_rect.map(|rect| Rect::from_min_size(rect.left_top() + Vec2::splat(16.0), Vec2::splat(1.0)))).flatten()
        });
        let Some(frame) = frame else {
            return;
        };
        // Any job that holds the refused nodes can be stopped from here: job.cancel takes its job id.
        let has_held_edit = held.is_some() || self.held_edits.iter().any(|held| held.nodes.iter().any(|id| self.refusal_ids.contains(id)));
        let stop_job = has_held_edit.then(|| {
            self.reservations
                .iter()
                .find(|reservation| reservation.node_ids.iter().any(|id| self.refusal_ids.contains(id)))
                .map(|reservation| reservation.job_id.clone())
        }).flatten();
        let retry_label = held.as_ref().filter(|held| held.can_retry).and_then(|held| match held.state {
            super::state::HeldEditState::NeedsReview => Some(if held.retry_same_request { "같은 요청 확인" } else { "최신 상태 확인" }),
            super::state::HeldEditState::Ready => Some(if held.retry_same_request { "같은 요청 재전송" } else { "다시 적용" }),
            _ => None,
        });
        let copy_label = held.as_ref().filter(|held| held.state == super::state::HeldEditState::NeedsReview && !held.can_retry).map(|_| "변경 내용 복사");
        let cancel_label = held.as_ref().filter(|held| held.state != super::state::HeldEditState::Sending).map(|_| "보류한 변경 취소");
        // tokens.json `refusal-callout.place`: under the agent outline's outer edge when the node carries one.
        let reserved = self.refusal_ids.iter().any(|id| self.reservations.iter().any(|reservation| reservation.node_ids.contains(id))
            || self.user_reservations.iter().any(|reservation| reservation.node_ids.contains(id)));
        let place = if reserved { frame.expand(AGENT_OUTSET) } else { frame };
        let (stop_clicked, retry_clicked, copy_clicked, cancel_clicked, _) = refusal_callout_with(
            ctx,
            palette,
            self.board_rect,
            place,
            &text,
            stop_job.as_ref().map(|_| "작업 멈추고 편집"),
            &[],
            retry_label,
            copy_label,
            cancel_label,
        );
        if let Some(job_id) = stop_job.filter(|_| stop_clicked) {
            self.stop_job_for_edit(&job_id);
        }
        if let Some(held) = held.as_ref() {
            if retry_clicked {
                match held.state {
                    super::state::HeldEditState::NeedsReview => self.check_held_edit(&held.id),
                    super::state::HeldEditState::Ready => self.confirm_held_edit_retry(&held.id),
                    _ => {}
                }
            }
            if copy_clicked { self.copy_held_edit(ctx, &held.id); }
            if cancel_clicked { self.discard_held_edit(&held.id); }
        }
        // A refused node that is not selected still gets the 1.5 px `danger` outline (§11); a selected one has it from
        // its selection box.
        let selected = self.refusal_ids.iter().any(|id| self.selected_node.as_deref() == Some(id.as_str()) || self.selected_nodes.contains(id));
        if !selected {
            ui.painter().rect_stroke(frame, CornerRadius::ZERO, Stroke::new(1.5, palette.danger), StrokeKind::Inside);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Pos2;
    use serde_json::json;

    #[test]
    fn layer_state_actions_restore_hidden_nodes_after_unlock_and_are_canvas_only() {
        use designllm_contracts::ArtifactKind;

        assert_eq!(layer_state_commands(ArtifactKind::Canvas, false, false), ["edit.lock", "edit.hide"]);
        assert_eq!(layer_state_commands(ArtifactKind::Canvas, true, false), ["edit.lock", "edit.show"]);
        assert_eq!(layer_state_commands(ArtifactKind::Canvas, false, true), ["edit.unlock"]);
        assert_eq!(layer_state_commands(ArtifactKind::Canvas, true, true), ["edit.unlock"], "unlock first; SetNodeInfo is guarded on locked nodes");
        assert!(layer_state_commands(ArtifactKind::Slides, true, true).is_empty(), "Slides does not expose v1.2 hide/lock commands");
    }

    /// A rectangle with text: the core reports its text lines as source regions, inside the frame.
    fn card() -> Node {
        serde_json::from_value(json!({
            "id": "card",
            "kind": "rectangle",
            "text": "기록 시작",
            "bounds": { "x": 2_000_000, "y": 1_000_000, "width": 4_000_000, "height": 1_500_000 },
        }))
        .expect("a rectangle with text parses")
    }

    #[test]
    fn selection_outline_follows_the_node_frame_not_its_text_lines() {
        let node = card();
        let page = Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(900.0, 600.0));
        let zoom = 0.5;
        let frame = frame_on_screen(node.bounds, page, zoom);
        assert!((frame.width() - 4_000_000.0 / 9525.0 * zoom).abs() < 1e-3, "the frame is the node's width");
        assert_eq!(frame.min, page.min + Vec2::new(2_000_000.0 / 9525.0 * zoom, 1_000_000.0 / 9525.0 * zoom));
        // A text line inside the card is narrower than the frame; it must not decide the outline.
        let text_line = Rect::from_min_size(frame.min + Vec2::new(8.0, 8.0), Vec2::new(60.0, 14.0));
        assert!(frame.contains_rect(text_line));
        assert!(frame.width() > text_line.width() * 2.0);
    }

    #[test]
    fn a_refusal_shows_the_korean_rule_label_never_the_code() {
        use designllm_contracts::{Diagnostic, DltError, ErrorCode};
        let diagnostic = |code: &str, message: &str| Diagnostic { code: code.into(), message: message.into(), node_id: None, blocking: true, params: Default::default() };
        let mut error = DltError::new(ErrorCode::InvalidArgument, "change refused: CLIPPED: text overflows");
        error.diagnostics = vec![diagnostic("CLIPPED", "text overflows its box")];
        assert_eq!(refusal_text(&error, "변경이 거부되었습니다"), crate::strings::lint_label("CLIPPED").unwrap());
        error.diagnostics = vec![diagnostic("SOMETHING_NEW", "english only")];
        assert_eq!(refusal_text(&error, "도형이 패널 밖으로 나갑니다"), "도형이 패널 밖으로 나갑니다");
        assert_eq!(refusal_text(&error, "refused: SOMETHING_NEW"), "이 편집은 적용되지 않았습니다");
    }

    #[test]
    fn a_rule_with_its_values_reads_as_the_token_sentence_in_the_document_unit() {
        use designllm_contracts::{ArtifactKind, Diagnostic, DltError, ErrorCode};
        let mut params = std::collections::BTreeMap::new();
        params.insert(String::from("text"), json!(61 * 36_000));
        params.insert(String::from("box"), json!(34 * 36_000));
        let clipped = Diagnostic { code: "CLIPPED".into(), message: "text overflows".into(), node_id: Some("n".into()), blocking: true, params };
        let mut error = DltError::new(ErrorCode::InvalidArgument, "change refused");
        error.diagnostics = vec![clipped];
        let (line, _) = refusal_for(&error, "", ArtifactKind::Figure, None);
        assert_eq!(line, "글 폭(61.0 mm)이 상자 폭(34.0 mm)보다 넓습니다");
        let (canvas, _) = refusal_for(&error, "", ArtifactKind::Canvas, None);
        assert_eq!(canvas, format!("글 폭({} px)이 상자 폭({} px)보다 넓습니다", (61.0 * 36_000.0 / 9525.0_f64).round(), (34.0 * 36_000.0 / 9525.0_f64).round()));
        let mut contrast = std::collections::BTreeMap::new();
        contrast.insert(String::from("ratio"), json!(3.24));
        contrast.insert(String::from("min"), json!(4.5));
        error.diagnostics = vec![Diagnostic { code: "CONTRAST".into(), message: String::new(), node_id: None, blocking: true, params: contrast }];
        assert_eq!(refusal_for(&error, "", ArtifactKind::Canvas, None).0, "글자 대비(3.2:1)가 기준 4.5:1보다 낮습니다");
        error.diagnostics[0].params.clear();
        assert_eq!(refusal_for(&error, "", ArtifactKind::Canvas, None).0, crate::strings::lint_label("CONTRAST").unwrap(), "no values: the label only");
        error.diagnostics[0].code = String::from("CONTRAST_LOW");
        assert_eq!(refusal_for(&error, "", ArtifactKind::Canvas, None).0, crate::strings::lint_label("CONTRAST").unwrap(), "the engine's code is the same rule");
        let mut overflow = std::collections::BTreeMap::new();
        overflow.insert(String::from("text"), json!(120 * 36_000));
        overflow.insert(String::from("box"), json!(100 * 36_000));
        overflow.insert(String::from("axis"), json!("height"));
        error.diagnostics = vec![Diagnostic { code: "content_overflow".into(), message: String::new(), node_id: None, blocking: true, params: overflow }];
        assert_eq!(refusal_for(&error, "", ArtifactKind::Docx, None).0, "글 높이(120.0 mm)가 상자 높이(100.0 mm)보다 높습니다");
        for (code, axis) in [("text_overflow", "width"), ("table_overflow", "width")] {
            error.diagnostics[0].code = code.into();
            error.diagnostics[0].params.insert(String::from("axis"), json!(axis));
            assert_eq!(refusal_for(&error, "", ArtifactKind::Docx, None).0, "글 폭(120.0 mm)이 상자 폭(100.0 mm)보다 넓습니다", "{code}");
        }
        let mut parent = std::collections::BTreeMap::new();
        parent.insert(String::from("parent"), json!("카드"));
        error.diagnostics = vec![Diagnostic { code: "CHILD_OUTSIDE_PARENT".into(), message: String::new(), node_id: None, blocking: true, params: parent }];
        assert_eq!(refusal_for(&error, "", ArtifactKind::Canvas, None).0, "‘카드’ 밖으로 나갑니다");
        error.diagnostics[0].code = String::from("OUT_OF_PAGE");
        assert_eq!(refusal_for(&error, "", ArtifactKind::Canvas, None).0, "페이지 밖에 있습니다");
        // The canvas engine's nested CLIPPED form: `box: {axis, emu}`.
        let mut nested = std::collections::BTreeMap::new();
        nested.insert(String::from("text"), json!(200 * 9525));
        nested.insert(String::from("box"), json!({ "axis": "height", "emu": 120 * 9525 }));
        error.diagnostics = vec![Diagnostic { code: "TEXT_CLIPPED".into(), message: String::new(), node_id: None, blocking: true, params: nested }];
        assert_eq!(refusal_for(&error, "", ArtifactKind::Canvas, None).0, "글 높이(200 px)가 상자 높이(120 px)보다 높습니다");
        assert!(refusal_for(&error, "", ArtifactKind::Docx, None).1.is_empty(), "the fix commands are for canvases and slides");
    }

    #[test]
    fn a_table_border_or_table_text_past_its_edge_has_its_own_words_not_the_clipped_ones() {
        use designllm_contracts::{ArtifactKind, Diagnostic, DltError, ErrorCode};
        let mut params = std::collections::BTreeMap::new();
        params.insert(String::from("axis"), json!("vertical"));
        params.insert(String::from("half"), json!(9525));
        params.insert(String::from("room"), json!(0));
        let border = Diagnostic {
            code: "table_border_overflow".into(),
            message: "Table border on the top page edge is cut: half of its stroke lies outside the page".into(),
            node_id: Some("t".into()),
            blocking: true,
            params,
        };
        let mut error = DltError::new(ErrorCode::ExportLossRequired, "export refused");
        error.diagnostics = vec![border];
        let (line, fixes) = refusal_for(&error, "", ArtifactKind::Docx, None);
        assert_eq!(line, "표 테두리 선의 절반이 쪽 가장자리 밖으로 나갑니다");
        assert_ne!(line, crate::strings::lint_label("CLIPPED").unwrap(), "the clipped words do not describe a table border");
        assert!(fixes.is_empty(), "no fix command is offered for a table border");
        assert_eq!(lint_label("table_border_overflow"), Some("표 테두리가 잘림"));

        // The table text past its cell edge is worded the same way: its own sentence and its own label.
        error.diagnostics = vec![Diagnostic {
            code: "table_text_overflow".into(),
            message: String::new(),
            node_id: None,
            blocking: true,
            params: Default::default(),
        }];
        assert_eq!(refusal_for(&error, "", ArtifactKind::Docx, None).0, "표 글자가 셀 밖으로 나갑니다");
        assert_eq!(lint_label("table_text_overflow"), Some("표 글자가 잘림"));
        assert_eq!(super::super::user_error(&error), "표 글자가 셀 밖으로 나갑니다", "a blocking table text diagnostic names itself too");
    }

    #[test]
    fn a_drag_starts_from_the_node_frame_not_from_a_text_line() {
        let node = card();
        let line = designllm_contracts::Rect { x: 2_100_000, y: 1_100_000, width: 500_000, height: 200_000 };
        assert_eq!(super::super::frame_bounds_in(Some(&node), line), node.bounds);
        assert_eq!(super::super::frame_bounds_in(None, line), line, "an unknown node keeps the given box");
    }
}
