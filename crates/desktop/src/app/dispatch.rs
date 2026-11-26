//! Runs registered commands (`commands.rs`). Every button, menu item and shortcut of the shell
//! reaches the engine through `DesktopApp::invoke`, which checks the arguments against the
//! command's schema first. The same entry point serves the keyboard and, once the core's UI
//! channel lands, `ui.command.run`.

use std::collections::{BTreeMap, BTreeSet};

use designllm_contracts::{
    new_id, ArtifactBody, ArtifactKind, BindTarget, Feature, FigureLink, Interaction, Intent, Justify, LayoutSpec, Node, NodeKind,
    Operation, PaddingSide, Page, Rect as ModelRect, Selection, Shadow, Size, Style, StyleField, StylePatch, Support, TokenMode,
    TokenRef, TokenSource, TokenValues, EMU_PER_PT,
};
use eframe::egui::{Context, Pos2, Rect};
use serde_json::{json, Value};

use super::{board::LeftTab, properties::TokenChangeBaseline, DesktopApp, FigureForm, PendingAction};
use crate::commands::{self, CommandSpec, COMMANDS};
#[path = "slides_theme.rs"] mod slides_theme;

const UNIT_PX: f64 = designllm_contracts::EMU_PER_CSS_PX as f64;
/// `nav.open_project` with an id the window's work list does not hold yet (an agent created it a moment ago).
pub(super) const LIST_REREAD: &str = "작업물 목록을 다시 읽는 중입니다. 같은 명령을 다시 실행해 주세요";

fn intents(list: &[Intent]) -> BTreeSet<Intent> {
    list.iter().copied().collect()
}

fn number(args: &Value, key: &str) -> Option<f64> {
    args.get(key).and_then(Value::as_f64)
}

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn token_target(args: &Value) -> Result<BindTarget, String> {
    let target = text(args, "target").ok_or("토큰 대상을 고르세요")?;
    Ok(match target {
        "style_color" => BindTarget::StyleColor,
        "style_fill" => BindTarget::StyleFill,
        "style_stroke" => BindTarget::StyleStroke,
        "style_font_family" => BindTarget::StyleFontFamily,
        "style_font_size" => BindTarget::StyleFontSize,
        "style_corner_radius" => BindTarget::StyleCornerRadius,
        "layout_gap" => BindTarget::LayoutGap,
        "layout_row_gap" => BindTarget::LayoutRowGap,
        "layout_padding" => BindTarget::LayoutPadding,
        "layout_padding_side" => {
            let side = match text(args, "side").ok_or("여백 방향을 고르세요")? {
                "top" => PaddingSide::Top,
                "right" => PaddingSide::Right,
                "bottom" => PaddingSide::Bottom,
                "left" => PaddingSide::Left,
                _ => return Err("여백 방향이 올바르지 않습니다".into()),
            };
            BindTarget::LayoutPaddingSide { side }
        }
        "breakpoint_gap" | "breakpoint_padding" => {
            let min_width_css_px = args
                .get("min_width_css_px")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0)
                .ok_or("중단점 너비는 1 이상의 정수여야 합니다")?;
            if target == "breakpoint_gap" {
                BindTarget::BreakpointGap { min_width_css_px }
            } else {
                BindTarget::BreakpointPadding { min_width_css_px }
            }
        }
        _ => return Err("지원하지 않는 토큰 대상입니다".into()),
    })
}

/// Gives a copied selection and all its children new ids, updating node interactions inside the copy.
fn renew_ids(nodes: &mut [Node]) -> BTreeMap<String, String> {
    fn assign(node: &mut Node, ids: &mut BTreeMap<String, String>, repeat_prefix: Option<(&str, &str)>) {
        // Repeat expansion will recreate these same row and descendant IDs, including interaction targets.
        let next = repeat_prefix.and_then(|(old, new)| node.id.strip_prefix(old).map(|suffix| format!("{new}{suffix}")))
            .unwrap_or_else(new_id);
        let old = std::mem::replace(&mut node.id, next);
        let prefixes = node.ext.repeat.as_ref().map(|_| (format!("{old}-"), format!("{}-", node.id)));
        ids.insert(old, node.id.clone());
        let repeat_prefix = prefixes.as_ref().map(|(old, new)| (old.as_str(), new.as_str())).or(repeat_prefix);
        for child in &mut node.children {
            assign(child, ids, repeat_prefix);
        }
    }
    fn remap(node: &mut Node, ids: &BTreeMap<String, String>) {
        for interaction in &mut node.interactions {
            let target = match interaction {
                Interaction::ToggleVisibility { node_id } | Interaction::SetVariant { node_id, .. } => node_id,
                _ => continue,
            };
            if let Some(new) = ids.get(target) {
                *target = new.clone();
            }
        }
        for child in &mut node.children {
            remap(child, ids);
        }
    }
    let mut ids = BTreeMap::new();
    for node in nodes.iter_mut() {
        assign(node, &mut ids, None);
    }
    for node in nodes.iter_mut() {
        remap(node, &ids);
    }
    ids
}

fn collect_ids(nodes: &[Node], ids: &mut BTreeSet<String>) {
    for node in nodes {
        ids.insert(node.id.clone());
        collect_ids(&node.children, ids);
    }
}

fn node_origin(nodes: &[Node], id: &str, origin: (i64, i64)) -> Option<(i64, i64)> {
    for node in nodes {
        let Some(x) = origin.0.checked_add(node.bounds.x) else { continue };
        let Some(y) = origin.1.checked_add(node.bounds.y) else { continue };
        if node.id == id {
            return Some((x, y));
        }
        if let Some(found) = node_origin(&node.children, id, (x, y)) {
            return Some(found);
        }
    }
    None
}

fn style_copy_patch(style: &Style) -> StylePatch {
    let mut clear = BTreeSet::new();
    for (field, present) in [
        (StyleField::CornerRadius, style.corner_radius.is_some()),
        (StyleField::Opacity, style.opacity_milli.is_some()),
        (StyleField::Shadows, !style.shadows.is_empty()),
        (StyleField::Gradient, style.gradient.is_some()),
        (StyleField::LineHeight, style.line_height_milli.is_some()),
        (StyleField::LetterSpacing, style.letter_spacing.is_some()),
        (StyleField::VerticalAlign, style.vertical_align.is_some()),
        (StyleField::FontWeight, style.font_weight.is_some()),
        (StyleField::StrokeDash, style.stroke_dash.is_some()),
        (StyleField::StrokeCap, style.stroke_cap.is_some()),
        (StyleField::StrokeJoin, style.stroke_join.is_some()),
        (StyleField::Underline, style.underline.is_some()),
        (StyleField::Strikethrough, style.strikethrough.is_some()),
        (StyleField::StrokeStart, style.stroke_start.is_some()),
        (StyleField::StrokeEnd, style.stroke_end.is_some()),
    ] {
        if !present {
            clear.insert(field);
        }
    }
    StylePatch {
        font_family: Some(style.font_family.clone()),
        font_size: Some(style.font_size),
        color: Some(style.color.clone()),
        fill: Some(style.fill.clone()),
        stroke: Some(style.stroke.clone()),
        stroke_width: Some(style.stroke_width),
        bold: Some(style.bold),
        italic: Some(style.italic),
        align: Some(style.align.clone()),
        corner_radius: style.corner_radius,
        opacity_milli: style.opacity_milli,
        shadows: (!style.shadows.is_empty()).then(|| style.shadows.clone()),
        gradient: style.gradient.clone(),
        line_height_milli: style.line_height_milli,
        letter_spacing: style.letter_spacing,
        vertical_align: style.vertical_align,
        font_weight: style.font_weight,
        stroke_dash: style.stroke_dash,
        stroke_cap: style.stroke_cap,
        stroke_join: style.stroke_join,
        underline: style.underline,
        strikethrough: style.strikethrough,
        stroke_start: style.stroke_start,
        stroke_end: style.stroke_end,
        clear,
    }
}

fn copied_figure_links(links: &[FigureLink], copied_ids: &BTreeSet<String>) -> Vec<FigureLink> {
    links.iter().filter(|link| copied_ids.contains(&link.target_node_id)).cloned().collect()
}

fn figure_link_operations(links: Vec<FigureLink>, ids: &BTreeMap<String, String>) -> Result<Vec<Operation>, String> {
    links
        .into_iter()
        .map(|mut link| {
            link.target_node_id = ids
                .get(&link.target_node_id)
                .cloned()
                .ok_or_else(|| "복사한 그림 연결 대상을 찾지 못했습니다".to_owned())?;
            Ok(Operation::UpdateFigureLink { link })
        })
        .collect()
}

fn check_slides_transfer(nodes: &[Node]) -> Result<(), String> {
    for node in nodes {
        if !node.interactions.is_empty() || !node.variants.is_empty() {
            return Err("슬라이드 작업물은 상호작용과 변형을 보존하지 않아 붙여 넣지 않았습니다".into());
        }
        if node.kind == NodeKind::Image && (!node.text.is_empty() || node.style != Style::default()) {
            return Err("슬라이드 엔진은 그림의 텍스트와 스타일을 보존하지 않아 붙여 넣지 않았습니다".into());
        }
        if node.kind == NodeKind::Line && !node.text.is_empty() {
            return Err("슬라이드 엔진은 선의 텍스트를 보존하지 않아 붙여 넣지 않았습니다".into());
        }
        if node.kind != NodeKind::Image && node.asset.is_some() {
            return Err("그림 자산이 도형에 연결돼 있어 슬라이드로 옮기지 않았습니다".into());
        }
        check_slides_transfer(&node.children)?;
    }
    Ok(())
}

fn parent_uses_auto_layout(nodes: &[Node], parent_id: &str) -> bool {
    let Some(parent) = nodes.iter().find_map(|node| super::doc_tree::find_node_in_node(node, parent_id)) else {
        return true;
    };
    parent.layout.as_ref().is_some_and(|layout| layout.mode != "absolute")
}

fn remap_pasted_interactions(
    nodes: &mut [Node],
    copied_ids: &BTreeSet<String>,
    same_artifact: bool,
    source_page_id: &str,
    destination_page_id: &str,
) -> Result<(), String> {
    fn visit(
        node: &mut Node,
        copied_ids: &BTreeSet<String>,
        same_artifact: bool,
        source_page_id: &str,
        destination_page_id: &str,
    ) -> Result<(), String> {
        for interaction in &mut node.interactions {
            match interaction {
                Interaction::ToggleVisibility { node_id } | Interaction::SetVariant { node_id, .. }
                    if !same_artifact && !copied_ids.contains(node_id) =>
                {
                    return Err("다른 작업물의 연결 대상도 함께 복사해야 합니다".into());
                }
                Interaction::NavigatePage { page_id } if !same_artifact => {
                    if page_id == source_page_id {
                        *page_id = destination_page_id.to_owned();
                    } else {
                        return Err("다른 페이지를 가리키는 상호작용은 대상 작업물로 복사할 수 없습니다".into());
                    }
                }
                _ => {}
            }
        }
        for child in &mut node.children {
            visit(child, copied_ids, same_artifact, source_page_id, destination_page_id)?;
        }
        Ok(())
    }
    for node in nodes {
        visit(node, copied_ids, same_artifact, source_page_id, destination_page_id)?;
    }
    Ok(())
}

impl DesktopApp {
    /// Runs a command from the UI. A refusal (bad arguments, the engine's answer) is shown.
    pub(super) fn invoke_ui(&mut self, id: &str, args: Value) {
        if let Err(message) = self.invoke(id, args) {
            self.set_error(message);
        }
    }

    /// Checks the arguments against the command's schema, then runs it.
    pub(super) fn invoke(&mut self, id: &str, args: Value) -> Result<(), String> {
        let spec = commands::find(id).ok_or_else(|| format!("알 수 없는 명령: {id}"))?;
        if !self.command_enabled_for_current_kind(id) {
            return Err(self.web_runtime_refusal(id).unwrap_or("현재 작업물에서는 사용할 수 없는 명령입니다").into());
        }
        if let Some(reason) = self.host_lost_refusal(spec) {
            return Err(reason.into());
        }
        let gui_save_shortcut = id == "file.save" && self.run_context.is_none() && args.as_object().is_some_and(serde_json::Map::is_empty);
        let gui_empty_ai_cancel = id == "ai.cancel" && self.run_context.is_none() && args.as_object().is_some_and(serde_json::Map::is_empty);
        let dialog_path = match id {
            "file.export" | "file.export_selection" => Some("target_path"),
            "nav.open_file" | "add.figure_image" => Some("path"),
            _ => None,
        };
        if let Some(key) = dialog_path.filter(|key| self.run_context.is_none() && args.get(*key).is_none()) {
            let mut dialog_args = args.clone();
            dialog_args[key] = json!("");
            commands::validate(spec, &dialog_args)?;
        } else if !gui_save_shortcut && !gui_empty_ai_cancel {
            commands::validate(spec, &args)?;
        }
        self.dispatch(spec, &commands::without_absent_nulls(spec, args))
    }

    /// Zooms the view to the selected nodes (`view.zoom_selection`, in a Canvas or Slides editor): the zoom fits their union on the
    /// board, up to the largest zoom, and centres it there. The pages are placed again at each zoom, since the gaps between them
    /// depend on it.
    fn zoom_to_selection(&mut self, board_rect: Rect) -> Result<(), String> {
        // The selected nodes' union in board units at `zoom`: `node_frames` at origin 0 gives screen units at that zoom.
        let selected_at = |zoom: f32| -> Option<Rect> {
            let mut content: Option<Rect> = None;
            for (id, frame) in self.node_frames(Pos2::ZERO, zoom) {
                if self.selected_nodes.contains(&id) {
                    let board_frame = Rect::from_min_size((frame.min.to_vec2() / zoom).to_pos2(), frame.size() / zoom);
                    content = Some(content.map_or(board_frame, |so_far| so_far.union(board_frame)));
                }
            }
            content
        };
        let not_found = || String::from("선택한 항목의 위치를 찾지 못했습니다");
        let (zoom, _) = super::board::settle_zoom(&selected_at, |content| crate::board::fit_zoom(content, board_rect, crate::board::ZOOM_MAX), 6)
            .ok_or_else(not_found)?;
        // The centring uses the selection at the zoom it gets: the pages are placed for that zoom.
        let content = selected_at(zoom).ok_or_else(not_found)?;
        let origin = crate::board::centred_origin(content, board_rect, zoom);
        let view = self.view();
        view.zoom = zoom;
        view.pan = origin - board_rect.min;
        view.fitted = true;
        view.zoom_changed = std::time::Instant::now();
        Ok(())
    }

    /// Whether the editor (not the home screen) is showing.
    pub(super) fn screen_is_editor(&self) -> bool {
        matches!(self.screen, super::Screen::Editor)
    }

    /// Clears the selection.
    pub(super) fn select_clear(&mut self) {
        self.set_selected_ids(Vec::new());
        self.set_selection(Selection { node_ids: Vec::new(), text_range: None });
    }

    /// Keyboard shortcuts of the registry. A bare letter does nothing while a text field has the
    /// keyboard, so that typing never triggers a tool.
    pub(super) fn handle_shortcuts(&mut self, ctx: &Context) {
        // The lost-host notice covers the window, and a dialog that takes keys takes them first (68d): no shortcut runs behind
        // either.
        if self.host_lost() || self.key_dialog_open() {
            return;
        }
        // F2 opens the selected table's or text's text before `edit.rename` sees it (68 item 11).
        self.take_f2_for_text_edit(ctx);
        self.handle_nudge_keys(ctx);
        let typing = ctx.wants_keyboard_input();
        for spec in COMMANDS {
            if !self.command_enabled_for_current_kind(spec.id) {
                continue;
            }
            let Some(shortcut) = commands::keyboard_shortcut(spec) else {
                continue;
            };
            if typing && commands::is_bare_key(spec) {
                continue;
            }
            if typing && matches!(spec.id, "edit.copy" | "edit.paste" | "edit.copy_properties" | "edit.paste_properties") {
                continue;
            }
            // Esc confirms the open board memo or title editor (`draw_editing_memo`), so it does not clear the selection.
            if self.board_memo_editor_shown() && spec.id == "select.clear" {
                continue;
            }
            // Esc closes an open menu (a node's or a table's right-click menu, 68 item 16), whose items act on the selection.
            if spec.id == "select.clear" && eframe::egui::Popup::is_any_open(ctx) {
                continue;
            }
            // Esc cancels a toolbox tile's drag or closes its icon picker (68g, `toolbox.rs`), so it does not clear the selection.
            if spec.id == "select.clear" && super::toolbox::takes_escape(ctx) {
                continue;
            }
            // Esc ends the open inline text edit (`draw_inline_text_editor`; an IME's Esc there cancels the composition), so
            // `select.clear` does not take it first. egui takes the text field's focus on Esc, so `typing` above is false here and
            // the bare-key guard does not stop it. An edit the editor cannot end on Esc (a conflict, a target that changed, a page
            // not drawn) keeps its draft on Esc, and the selection stays its node: `select.clear` could not change it during an
            // edit either (`set_selected_ids`, `set_selection`). Its conflict notice's buttons decide what happens to the draft.
            if self.inline_text_edit.is_some() && self.board_rect.is_some() && spec.id == "select.clear" {
                continue;
            }
            if !self.shortcut_applies(spec) {
                continue;
            }
            if ctx.input_mut(|input| input.consume_shortcut(&shortcut)) {
                let args = self.default_args(spec.id);
                self.invoke_ui(spec.id, args);
            }
        }
    }

    /// Whether a shortcut means something on the current screen.
    fn shortcut_applies(&self, spec: &CommandSpec) -> bool {
        match spec.id {
            "home.create" => !self.screen_is_editor(),
            "ai.request" => self.screen_is_editor(),
            _ => true,
        }
    }

    /// Arguments that a shortcut supplies from the current state.
    fn default_args(&self, id: &str) -> Value {
        match id {
            "ai.request" => json!({ "prompt": self.ai_prompt }),
            "home.create" => json!({ "tile": self.home.tile, "prompt": self.home.prompt }),
            _ => json!({}),
        }
    }

    /// The selected node ids (the UI selects one node at a time).
    fn selected_ids(&self) -> Vec<String> {
        self.selected_nodes.clone()
    }

    /// The nodes `edit.delete` removes, or the reason it cannot run now. The UI channel lists the command from the same check.
    pub(super) fn delete_targets(&self) -> Result<Vec<String>, String> {
        let ids = self.selected_ids();
        if ids.is_empty() {
            return Err("지울 대상을 고르세요".into());
        }
        Ok(ids)
    }

    /// The one picture `props.image_fit` acts on (the checks of the run). The UI channel lists the command from the same check.
    pub(super) fn selected_canvas_image_id(&self) -> Result<String, String> {
        if self.active.as_ref().map(|active| active.kind) != Some(ArtifactKind::Canvas) {
            return Err("캔버스 이미지를 고르세요".into());
        }
        if self.selected_nodes.len() > 1 {
            return Err("이미지 하나만 고르세요".into());
        }
        let (node, _, _) = self.selected_with_page().ok_or("이미지를 고르세요")?;
        if node.kind != NodeKind::Image {
            return Err("이미지를 고르세요".into());
        }
        Ok(node.id)
    }

    /// The one picture or video `props.image_alt` acts on (the checks of the run). The UI channel lists the command from the same check.
    pub(super) fn selected_canvas_alt_media_id(&self) -> Result<String, String> {
        if self.active.as_ref().map(|active| active.kind) != Some(ArtifactKind::Canvas) {
            return Err("캔버스 이미지 또는 동영상을 고르세요".into());
        }
        if self.selected_nodes.len() > 1 {
            return Err("이미지 또는 동영상 하나만 고르세요".into());
        }
        let (node, _, _) = self.selected_with_page().ok_or("이미지 또는 동영상을 고르세요")?;
        if !matches!(node.kind, NodeKind::Image | NodeKind::Video) {
            return Err("이미지 또는 동영상을 고르세요".into());
        }
        Ok(node.id)
    }

    /// The picture `props.image_fit` acts on, or the reason it cannot act now. Its run and its listing in `ui_channel` both ask this,
    /// so the listing is enabled exactly when the run passes.
    pub(super) fn image_fit_target(&self) -> Result<String, String> {
        let node_id = self.selected_canvas_image_id()?;
        self.picture_change_ready(&node_id)?;
        Ok(node_id)
    }

    /// The node `props.image_alt` acts on, or the reason it cannot act now (see `image_fit_target`).
    pub(super) fn image_alt_target(&self) -> Result<String, String> {
        let node_id = self.selected_canvas_alt_media_id()?;
        self.picture_change_ready(&node_id)?;
        Ok(node_id)
    }

    /// What both picture commands need before they change the node: no replacement or property change of it on its way, an open
    /// editor, and a document the window has read.
    fn picture_change_ready(&self, node_id: &str) -> Result<(), String> {
        if self.image_property_change_pending(node_id) {
            return Err("이미지 교체 또는 이미지 속성 변경이 끝날 때까지 기다리세요".into());
        }
        if !self.screen_is_editor() {
            return Err("작업물을 열어 주세요".into());
        }
        if self.context.is_none() {
            return Err("작업물을 아직 읽는 중입니다".into());
        }
        Ok(())
    }

    /// The selected node and the page that holds it.
    fn selected_with_page(&self) -> Option<(Node, u32, designllm_contracts::Id)> {
        let (node, index, page_id) = self.selected_on_page()?;
        Some((node.clone(), index, page_id.clone()))
    }

    /// `selected_with_page` without copies: the UI channel lists the commands that take this target from it, every frame.
    pub(super) fn selected_on_page(&self) -> Option<(&Node, u32, &designllm_contracts::Id)> {
        let artifact = self.artifact.as_ref()?;
        let node = super::doc_tree::find_node(artifact, self.selected_node.as_deref()?)?;
        let pages = match &artifact.body {
            designllm_contracts::ArtifactBody::Canvas(document) => &document.pages,
            designllm_contracts::ArtifactBody::Slides(document) => &document.pages,
            _ => return None,
        };
        for (index, page) in pages.iter().enumerate() {
            if page.nodes.iter().any(|top| super::doc_tree::find_node_in_node(top, &node.id).is_some()) {
                return Some((node, index as u32, &page.id));
            }
        }
        None
    }

    /// The units of the document kind (px or mm) as EMU per unit.
    /// The colour that paints nothing, in the words of the open document's engine: the canvas engine takes `transparent` only
    /// (canvas `common.rs` `valid_color`), the slide and text engines take `none`.
    fn no_paint(&self) -> &'static str {
        if self.active.as_ref().is_some_and(|item| item.kind == ArtifactKind::Canvas) { "transparent" } else { "none" }
    }

    fn emu_per_unit(&self) -> f64 {
        let kind = self.active.as_ref().map_or(ArtifactKind::Canvas, |item| item.kind);
        super::properties::unit_for(kind).emu()
    }

    pub(super) fn change(&mut self, operations: Vec<Operation>, allowed: &[Intent]) -> Result<(), String> {
        if self.context.is_none() {
            return Err("작업물을 아직 읽는 중입니다".into());
        }
        self.apply_operations(operations, intents(allowed));
        Ok(())
    }

    fn token_change(&mut self, operations: Vec<Operation>, baseline: TokenChangeBaseline) -> Result<(), String> {
        if self.context.is_none() {
            return Err("작업물을 아직 읽는 중입니다".into());
        }
        let baseline = self.run_context.is_none().then_some(baseline);
        self.apply_operations_for(operations, intents(&[Intent::Design]), PendingAction::TokenChange { baseline });
        Ok(())
    }

    /// The artifact a design-system command acts on, its revision, and the intents it may use. A publisher is a Canvas (`link`
    /// false). A consumer is a Canvas or a WebCode project. A link's default intents are the ones the UI listing publishes for it
    /// (`commands::design_system_link_intents`), so a queued agent run and a GUI link need the same intents.
    fn design_system_context(&self, link: bool) -> Result<(String, String, u64, String, BTreeSet<Intent>), String> {
        let (Some(active), Some(artifact), Some(context)) = (&self.active, &self.artifact, &self.context) else {
            return Err("작업물을 아직 읽는 중입니다".into());
        };
        // A link also reaches WebCode and the named styles of Word and HWPX documents (task 49d); publishing stays Canvas.
        let accepted = |kind: ArtifactKind| {
            kind == ArtifactKind::Canvas || (link && matches!(kind, ArtifactKind::WebCode | ArtifactKind::Docx | ArtifactKind::Hwpx))
        };
        if !accepted(active.kind)
            || !accepted(artifact.kind)
            || active.id != artifact.id
            || artifact.id != context.artifact_id
            || active.revision != artifact.revision
            || artifact.revision != context.revision
        {
            return Err("작업물 상태가 바뀌었습니다. 최신 내용을 다시 읽은 뒤 다시 시도해 주세요".into());
        }
        let default_intents = if link { commands::design_system_link_intents(artifact.kind) } else { intents(&[Intent::Design]) };
        let (actor, allowed) = self.run_context.as_ref().map_or_else(
            || (String::from("desktop"), default_intents),
            |run| (run.actor.clone(), run.allowed_intents.clone()),
        );
        if !allowed.contains(&Intent::Design) {
            return Err("디자인 권한이 없어 공유 토큰을 바꾸지 않았습니다".into());
        }
        Ok((context.workspace_id.clone(), active.id.clone(), context.revision, actor, allowed))
    }

    fn design_system_publish(&mut self, args: &Value) -> Result<(), String> {
        let publish = args.get("publish").and_then(Value::as_bool).ok_or("공개 여부를 지정해 주세요")?;
        let (workspace_id, artifact_id, expected_revision, actor, allowed_intents) = self.design_system_context(false)?;
        let params = json!({
            "request_id": new_id(),
            "actor": actor,
            "workspace_id": workspace_id,
            "artifact_id": artifact_id,
            "expected_revision": expected_revision,
            "publish": publish,
            "allowed_intents": allowed_intents,
        });
        self.call("design_system.publish", params, PendingAction::DesignSystemChange { refresh_systems: true });
        Ok(())
    }

    fn design_system_set(&mut self, args: &Value) -> Result<(), String> {
        let design_system = match args.get("design_system") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) if !id.trim().is_empty() => Some(id.clone()),
            _ => return Err("공유 디자인 시스템을 고르거나 연결 해제를 선택하세요".into()),
        };
        let expected_system_revision = args.get("expected_system_revision").and_then(Value::as_u64);
        if design_system.is_none() && expected_system_revision.is_some() {
            return Err("연결 해제에는 공유 원본 revision을 지정하지 마세요".into());
        }
        let (workspace_id, artifact_id, expected_revision, actor, allowed_intents) = self.design_system_context(true)?;
        let mut params = json!({
            "request_id": new_id(),
            "actor": actor,
            "workspace_id": workspace_id,
            "artifact_id": artifact_id,
            "expected_revision": expected_revision,
            "design_system": design_system,
            "allowed_intents": allowed_intents,
        });
        if let Some(revision) = expected_system_revision {
            params["expected_system_revision"] = json!(revision);
        }
        if let Some(policy) = args.get("direct_formatting").filter(|value| !value.is_null()) {
            params["direct_formatting"] = policy.clone();
        }
        self.call("design_system.set", params, PendingAction::DesignSystemChange { refresh_systems: false });
        Ok(())
    }

    fn style_change(&mut self, patch: StylePatch) -> Result<(), String> {
        let ids = self.selected_ids();
        if ids.is_empty() {
            return Err("바꿀 대상을 고르세요".into());
        }
        self.change(vec![Operation::SetStyle { node_ids: ids, patch }], &[Intent::Design])
    }

    pub(super) fn ensure_interaction_revision_matches_context(&self) -> Result<(), String> {
        let artifact = self.artifact.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
        let context = self.context.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
        if artifact.id != context.artifact_id
            || self.active.as_ref().is_none_or(|active| active.id != artifact.id)
            || artifact.revision != context.revision
        {
            return Err("작업물 상태가 바뀌었습니다. 최신 내용을 다시 불러온 뒤 다시 시도해 주세요".into());
        }
        Ok(())
    }

    fn canvas_interaction_set(&mut self, args: &Value) -> Result<(), String> {
        let ids = self.selected_ids();
        let [node_id] = ids.as_slice() else {
            return Err("다른 클릭 동작은 한 요소씩 편집하세요".into());
        };
        let index = args
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or("동작을 찾지 못했습니다")?;
        let action = text(args, "action").ok_or("동작을 고르세요")?;
        let interaction = match action {
            "navigate_page" => Interaction::NavigatePage {
                page_id: text(args, "target_id").filter(|id| !id.is_empty()).ok_or("이동할 페이지를 고르세요")?.into(),
            },
            "toggle_visibility" => Interaction::ToggleVisibility {
                node_id: text(args, "target_id").filter(|id| !id.is_empty()).ok_or("표시를 바꿀 요소를 고르세요")?.into(),
            },
            "set_variant" => Interaction::SetVariant {
                node_id: text(args, "target_id").filter(|id| !id.is_empty()).ok_or("상태를 바꿀 요소를 고르세요")?.into(),
                variant: text(args, "variant").filter(|variant| !variant.is_empty()).ok_or("상태를 고르세요")?.into(),
            },
            "open_url" => {
                let url = text(args, "url").unwrap_or_default();
                if designllm_contracts::interaction_url::check_new_interaction_url(url).is_err() {
                    return Err("HTTPS, mailto 또는 tel 주소를 입력하세요".into());
                }
                Interaction::OpenUrl { url: url.to_owned() }
            }
            "back" => Interaction::Back,
            _ => return Err("지원하지 않는 클릭 동작입니다".into()),
        };
        self.ensure_interaction_revision_matches_context()?;
        let artifact = self.artifact.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
        let ArtifactBody::Canvas(_) = &artifact.body else {
            return Err("캔버스에서만 클릭 동작을 편집할 수 있습니다".into());
        };
        let node = super::doc_tree::find_node(artifact, node_id).ok_or("선택한 요소를 찾지 못했습니다")?;
        let mut interactions = node.interactions.clone();
        if index > interactions.len() {
            return Err("동작을 찾지 못했습니다".into());
        }
        if matches!(&interaction, Interaction::OpenUrl { .. })
            && interactions.iter().enumerate().any(|(current, existing)| current != index && matches!(existing, Interaction::OpenUrl { .. }))
        {
            return Err("요소당 URL 링크는 하나만 설정할 수 있습니다".into());
        }
        if index == interactions.len() {
            interactions.push(interaction);
        } else {
            let current = serde_json::to_value(&interactions[index]).map_err(|_| "클릭 동작을 읽지 못했습니다")?;
            let next = serde_json::to_value(&interaction).map_err(|_| "클릭 동작을 읽지 못했습니다")?;
            if current == next {
                return Ok(());
            }
            interactions[index] = interaction;
        }
        self.change(vec![Operation::SetInteractions { node_id: node_id.clone(), interactions }], &[Intent::Content])
    }

    fn canvas_interaction_remove(&mut self, args: &Value) -> Result<(), String> {
        let ids = self.selected_ids();
        let [node_id] = ids.as_slice() else {
            return Err("다른 클릭 동작은 한 요소씩 편집하세요".into());
        };
        let index = args
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or("동작을 찾지 못했습니다")?;
        self.ensure_interaction_revision_matches_context()?;
        let artifact = self.artifact.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
        let ArtifactBody::Canvas(_) = &artifact.body else {
            return Err("캔버스에서만 클릭 동작을 편집할 수 있습니다".into());
        };
        let node = super::doc_tree::find_node(artifact, node_id).ok_or("선택한 요소를 찾지 못했습니다")?;
        let mut interactions = node.interactions.clone();
        if index >= interactions.len() {
            return Err("동작을 찾지 못했습니다".into());
        }
        interactions.remove(index);
        self.change(vec![Operation::SetInteractions { node_id: node_id.clone(), interactions }], &[Intent::Content])
    }

    fn canvas_open_url(&mut self, url: &str) -> Result<(), String> {
        let ids = self.selected_ids();
        if ids.is_empty() {
            return Err("링크를 적용할 요소를 선택하세요".into());
        }
        if !url.is_empty() && designllm_contracts::interaction_url::check_new_interaction_url(url).is_err() {
            return Err("HTTPS, mailto 또는 tel 주소를 입력하세요".into());
        }
        self.ensure_interaction_revision_matches_context()?;
        let operations = {
            let artifact = self.artifact.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
            let ArtifactBody::Canvas(_) = &artifact.body else {
                return Err("캔버스에서만 링크를 편집할 수 있습니다".into());
            };
            let mut operations = Vec::with_capacity(ids.len());
            for node_id in ids.iter().cloned() {
                let node = super::doc_tree::find_node(artifact, &node_id).ok_or("선택한 요소를 찾지 못했습니다")?;
                let mut interactions = node.interactions.clone();
                let open_urls: Vec<usize> = interactions
                    .iter()
                    .enumerate()
                    .filter_map(|(index, interaction)| matches!(interaction, Interaction::OpenUrl { .. }).then_some(index))
                    .collect();
                if url.is_empty() {
                    if open_urls.is_empty() {
                        continue;
                    }
                    interactions.retain(|interaction| !matches!(interaction, Interaction::OpenUrl { .. }));
                } else if open_urls.len() > 1 {
                    return Err("요소에 URL 동작이 여러 개 있어 수정할 수 없습니다".into());
                } else if let Some(index) = open_urls.first().copied() {
                    let Interaction::OpenUrl { url: current } = &interactions[index] else { unreachable!() };
                    if current == url {
                        continue;
                    }
                    interactions[index] = Interaction::OpenUrl { url: url.to_owned() };
                } else {
                    interactions.push(Interaction::OpenUrl { url: url.to_owned() });
                }
                operations.push(Operation::SetInteractions { node_id, interactions });
            }
            operations
        };
        if operations.is_empty() {
            return Ok(());
        }
        let (expected_selection, expected_page, expected_selection_generation) = self.selection_commit_guard()?;
        self.enqueue_selecting_commit(
            operations,
            intents(&[Intent::Content]),
            PendingAction::ApplyWithSelectionIfUnchanged { node_ids: ids, expected_selection, expected_page, expected_selection_generation },
        );
        Ok(())
    }

    fn dispatch(&mut self, spec: &CommandSpec, args: &Value) -> Result<(), String> {
        let id = spec.id;
        match id {
            // Screens and navigation.
            "nav.home" => {
                self.close_document_preview();
                self.cancel_canvas_image_replacement();
                self.screen = super::Screen::Home;
                Ok(())
            }
            "nav.new" => {
                self.close_document_preview();
                self.cancel_canvas_image_replacement();
                self.screen = super::Screen::Home;
                self.home.focus_request = true;
                Ok(())
            }
            "nav.open_project" => {
                let artifact_id = text(args, "artifact_id").unwrap_or_default();
                let Some(summary) = self.artifacts.iter().find(|item| item.id == artifact_id).cloned() else {
                    // A work item made elsewhere a moment ago (an agent's artifact_create) is not in the list until the next
                    // event poll: the list is read again once, and the repeated command finds it.
                    if self.list_reread_for.as_deref() == Some(artifact_id) {
                        self.list_reread_for = None;
                        return Err("작업물을 찾지 못했습니다".into());
                    }
                    self.list_reread_for = Some(artifact_id.to_owned());
                    self.call("workspace.list", json!({}), PendingAction::WorkspaceList);
                    return Err(LIST_REREAD.into());
                };
                self.list_reread_for = None;
                self.screen = super::Screen::Editor;
                self.activate(summary);
                Ok(())
            }
            "nav.open_file" => {
                if let Some(path) = text(args, "path") {
                    let kind = match args.get("kind") {
                        Some(kind) => Some(serde_json::from_value::<ArtifactKind>(kind.clone()).map_err(|_| "알 수 없는 작업물 종류입니다")?),
                        None => None,
                    };
                    self.open_path = path.to_owned();
                    self.open_file_as(kind);
                } else if self.run_context.is_some() {
                    return Err("열 파일의 절대 경로를 path로 지정해 주세요".into());
                } else if let Some(path) = crate::dialogs::open_document(self.parent_hwnd) {
                    self.open_path = path.to_string_lossy().into_owned();
                    self.open_file();
                }
                Ok(())
            }
            "nav.toggle_left" => {
                let open = self.view_ref().left_open;
                self.view().left_open = !open;
                Ok(())
            }
            "nav.toggle_right" => {
                let open = self.view_ref().right_open;
                self.view().right_open = !open;
                Ok(())
            }
            "nav.toggle_panels" => {
                let view = self.view();
                let open = !(view.left_open || view.right_open);
                view.left_open = open;
                view.right_open = open;
                Ok(())
            }
            "nav.ai" => {
                self.view().left_tab = LeftTab::Ai;
                self.view().left_open = true;
                Ok(())
            }
            "nav.tab" => {
                let tab = text(args, "tab").unwrap_or("layers");
                let target = match tab {
                    "ai" => LeftTab::Ai,
                    // The toolbox tab is a canvas's or a slide deck's (68g); elsewhere there is none to show.
                    "tools" if !self.toolbox_shown() => return Err("도구 탭은 캔버스와 슬라이드에만 있습니다".into()),
                    "tools" => LeftTab::Tools,
                    _ => LeftTab::Layers,
                };
                self.view().left_tab = target;
                Ok(())
            }
            "layer.find" => {
                let view = self.view();
                view.left_tab = LeftTab::Layers;
                view.left_open = true;
                self.layer_find_open = true;
                self.layer_find_focus = true;
                if let Some(query) = text(args, "text") {
                    self.layer_find_query = query.to_owned();
                }
                Ok(())
            }
            "layer.find.next" | "layer.find.previous" => {
                let forward = id == "layer.find.next";
                if self.active.as_ref().is_some_and(|active| active.kind == ArtifactKind::Canvas) {
                    return self.move_canvas_find_match(forward);
                }
                self.move_flow_find_match(forward);
                Ok(())
            }
            "nav.page" => {
                let index = number(args, "page_index").unwrap_or(0.0) as u32;
                self.remember_pending_page_reorder_selection(index);
                self.view().page_index = index;
                self.page_index = index;
                Ok(())
            }
            "nav.focus_request" => {
                // In the editor the request box is the AI tab's (as Ctrl+J opens it); on the home screen it is the home one.
                if self.screen_is_editor() {
                    self.view().left_tab = LeftTab::Ai;
                    self.view().left_open = true;
                    self.ai_focus_request = true;
                    return Ok(());
                }
                self.cancel_canvas_image_replacement();
                self.screen = super::Screen::Home;
                self.home.focus_request = true;
                Ok(())
            }
            // View.
            "view.zoom_in" | "view.zoom_out" | "view.zoom_100" | "view.zoom_set" | "view.zoom_fit" | "view.zoom_selection" => {
                let Some(rect) = self.board_rect else {
                    return Ok(());
                };
                match id {
                    "view.zoom_in" => self.zoom_about(rect, rect.center(), 1.25),
                    "view.zoom_out" => self.zoom_about(rect, rect.center(), 0.8),
                    "view.zoom_100" => self.set_zoom(rect, 1.0),
                    "view.zoom_set" => self.set_zoom(rect, (number(args, "percent").unwrap_or(100.0) / 100.0) as f32),
                    "view.zoom_fit" => {
                        self.view().fitted = false;
                    }
                    "view.zoom_selection" => return self.zoom_to_selection(rect),
                    _ => {
                        self.view().fitted = false;
                    }
                }
                Ok(())
            }
            "view.zoom_page" => {
                let index = number(args, "page_index").unwrap_or(0.0) as u32;
                if index as usize >= self.page_sizes().len() {
                    return Err("페이지를 찾지 못했습니다".into());
                }
                self.remember_pending_page_reorder_selection(index);
                if let Some(rect) = self.board_rect {
                    self.fit_page(rect, index);
                }
                Ok(())
            }
            "view.play" => self.play_canvas(),
            "web.refresh" => Err(format!("{}: 이 빌드에서 아직 지원하지 않습니다", spec.label)),
            // Selection.
            "select.node" => {
                let node_id = text(args, "node_id").unwrap_or_default().to_owned();
                self.select_node(node_id);
                Ok(())
            }
            "select.many" => {
                let ids: Vec<String> = args
                    .get("node_ids")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                self.select_ids(ids);
                Ok(())
            }
            "props.distribute" => {
                let horizontal = text(args, "axis").unwrap_or("h") == "h";
                self.distribute_selected(horizontal)
            }
            "select.clear" => {
                self.select_clear();
                Ok(())
            }
            "select.next" => self.select_adjacent_layer(true),
            "select.previous" => self.select_adjacent_layer(false),
            // History.
            "edit.undo" => self.history(false),
            "edit.redo" => self.history(true),
            // Node operations.
            "edit.delete" => {
                let ids = self.delete_targets()?;
                let operations = ids.into_iter().map(|node_id| Operation::RemoveNode { node_id }).collect();
                self.select_clear();
                self.change(operations, &[Intent::Content, Intent::Design])
            }
            "edit.duplicate" => {
                self.duplicate_selected()
            }
            "edit.copy" => self.copy_selected(),
            "edit.paste" => self.paste_nodes(),
            "edit.copy_properties" => self.copy_properties(),
            "edit.paste_properties" => self.paste_properties(),
            "edit.order_forward" | "edit.order_backward" | "edit.order_front" | "edit.order_back" => {
                let (node, _, page_id) = self.selected_with_page().ok_or("순서를 바꿀 대상을 고르세요")?;
                let count = self.page_node_count(&page_id);
                let current = self.page_node_index(&page_id, &node.id).unwrap_or(0);
                let index = match id {
                    "edit.order_forward" => (current + 1).min(count.saturating_sub(1)),
                    "edit.order_backward" => current.saturating_sub(1),
                    "edit.order_front" => count.saturating_sub(1),
                    _ => 0,
                };
                self.change(vec![Operation::SetZOrder { node_id: node.id, index: index as u32 }], &[Intent::Content])
            }
            "layer.move" => {
                let node_id = text(args, "node_id").unwrap_or_default().to_owned();
                let index = number(args, "index").unwrap_or(0.0).max(0.0) as u32;
                self.change(vec![Operation::SetZOrder { node_id, index }], &[Intent::Content])
            }
            "edit.lock" | "edit.unlock" | "edit.hide" | "edit.show" => {
                let ids = self.selected_ids();
                if ids.is_empty() {
                    return Err("대상을 고르세요".into());
                }
                let operation = match id {
                    "edit.lock" | "edit.unlock" => Operation::SetNodeAppearance {
                        node_ids: ids,
                        image_fit: None,
                        locked: Some(id == "edit.lock"),
                    },
                    "edit.hide" | "edit.show" => Operation::SetNodeInfo {
                        node_ids: ids,
                        alt: None,
                        name: None,
                        hidden: Some(id == "edit.hide"),
                    },
                    _ => unreachable!(),
                };
                // Lock is a design change (SetNodeAppearance); hide is content (SetNodeInfo), per the contract table.
                let intent = if matches!(id, "edit.lock" | "edit.unlock") { Intent::Design } else { Intent::Content };
                self.change(vec![operation], &[intent])
            }
            "edit.nudge" => {
                let scale = self.emu_per_unit();
                let dx = (number(args, "x").unwrap_or(0.0) * scale).round() as i64;
                let dy = (number(args, "y").unwrap_or(0.0) * scale).round() as i64;
                // The whole selection moves, as one change (props.position does the same for several nodes).
                if self.selected_nodes.len() > 1 {
                    return self.move_selected_by(dx, dy);
                }
                let (node, _, _) = self.selected_with_page().ok_or("옮길 대상을 고르세요")?;
                let bounds = ModelRect { x: node.bounds.x + dx, y: node.bounds.y + dy, ..node.bounds };
                self.change(vec![Operation::SetGeometry { node_id: node.id, bounds }], &[Intent::Design])
            }
            "edit.rename" => {
                let name = text(args, "name").unwrap_or_default().to_owned();
                // A layer row names its own node (`node_id`); without one, the selection is renamed.
                let ids = match text(args, "node_id").filter(|id| !id.is_empty()) {
                    Some(node_id) => {
                        let artifact = self.artifact.as_ref().ok_or("선택한 요소를 찾지 못했습니다")?;
                        super::doc_tree::find_node(artifact, node_id).ok_or("선택한 요소를 찾지 못했습니다")?;
                        vec![node_id.to_owned()]
                    }
                    None => self.selected_ids(),
                };
                if ids.is_empty() {
                    return Err("이름을 바꿀 대상을 고르세요".into());
                }
                self.change(vec![Operation::SetNodeInfo { node_ids: ids, alt: None, name: Some(name), hidden: None }], &[Intent::Content])
            }
            // Adding.
            "add.text" => self.add_node_kind(NodeKind::Text, args),
            "add.rect" | "add.frame" => self.add_node_kind(NodeKind::Rectangle, args),
            "add.oval" => self.add_node_kind(NodeKind::Ellipse, args),
            "add.line" => self.add_node_kind(NodeKind::Line, args),
            "add.image" => {
                // A text document takes the picture after the selected block (an empty page id means the flow).
                let flow = matches!(self.artifact.as_ref().map(|artifact| &artifact.body), Some(designllm_contracts::ArtifactBody::Flow(_)));
                if flow && args.get("at").is_some() {
                    return Err("문서에서는 그림 위치를 정할 수 없습니다".into());
                }
                // `at` makes its page the current one (68g).
                let at = if flow { None } else { self.take_add_at(args)? };
                let page_id = if flow { String::new() } else { self.current_page_id().ok_or("이미지를 넣을 페이지를 고르세요")?.1 };
                if let Some(path) = crate::dialogs::open_image(self.parent_hwnd) {
                    let center = at.map(|at| at.center);
                    self.call("asset.import", json!({ "path": path.to_string_lossy() }), PendingAction::ImageAsset { page_id, center });
                }
                Ok(())
            }
            "add.paragraph" => {
                self.add_paragraph();
                Ok(())
            }
            "add.figure_link" => {
                let figure_id = match text(args, "figure_id") {
                    Some(figure_id) => figure_id.to_owned(),
                    None => self
                        .artifacts
                        .iter()
                        .find(|item| item.kind == ArtifactKind::Figure)
                        .map(|item| item.id.clone())
                        .ok_or("이 작업공간에는 논문 그림이 없습니다")?,
                };
                if self.figure_insertion_blocked().is_some() {
                    return Err("이 문서는 논문 그림을 넣을 수 없습니다".into());
                }
                self.begin_figure_insert(&figure_id);
                Ok(())
            }
            "add.page" | "add.slide" => {
                let preset = text(args, "preset").unwrap_or("");
                self.add_page_preset(preset)
            }
            "add.chart" | "add.csv" => {
                // With a path (an agent's run names its file) the table is read into the figure without the dialog
                // (payload.rs `csv_dataset_read`); the agent then describes the chart with figure.plot.
                if let Some(path) = text(args, "path").filter(|path| !path.trim().is_empty()) {
                    if self.figure_document().is_none() {
                        return Err("논문 그림이 열려 있지 않습니다".into());
                    }
                    if !self.feature_edit_supported(Feature::Dataset) {
                        return Err("이 작업물에는 자료를 넣을 수 없습니다".into());
                    }
                    self.call("asset.import", json!({ "path": path.trim() }), PendingAction::CsvAssetImport);
                    return Ok(());
                }
                self.open_figure_form(FigureForm::Chart);
                Ok(())
            }
            "add.evidence" => {
                self.open_figure_form(FigureForm::Evidence);
                Ok(())
            }
            // Properties.
            "props.position" => {
                if self.selected_nodes.len() > 1 {
                    let scale = self.emu_per_unit();
                    let x = number(args, "x").map(|value| (value * scale).round() as i64);
                    let y = number(args, "y").map(|value| (value * scale).round() as i64);
                    return self.set_selected_positions(x, y);
                }
                let (node, _, _) = self.selected_with_page().ok_or("위치를 바꿀 대상을 고르세요")?;
                let scale = self.emu_per_unit();
                let x = number(args, "x").map_or(node.bounds.x, |value| (value * scale).round() as i64);
                let y = number(args, "y").map_or(node.bounds.y, |value| (value * scale).round() as i64);
                let bounds = ModelRect { x, y, ..node.bounds };
                self.change(vec![Operation::SetGeometry { node_id: node.id, bounds }], &[Intent::Design])
            }
            "canvas.open_url" => self.canvas_open_url(text(args, "url").unwrap_or_default()),
            "canvas.interaction.set" => self.canvas_interaction_set(args),
            "canvas.interaction.remove" => self.canvas_interaction_remove(args),
            "props.size" => {
                let scale = self.emu_per_unit();
                let width = number(args, "width").map(|value| (value * scale).round() as i64);
                let height = number(args, "height").map(|value| (value * scale).round() as i64);
                if self.selected_nodes.len() > 1 {
                    return self.set_selected_sizes(width, height);
                }
                let (node, _, _) = self.selected_with_page().ok_or("크기를 바꿀 대상을 고르세요")?;
                let width = width.unwrap_or(node.bounds.width);
                let height = height.unwrap_or(node.bounds.height);
                super::page_guides::validate_node_extent(node.kind, width, height)?;
                let bounds = ModelRect { width, height, ..node.bounds };
                self.change(vec![Operation::SetGeometry { node_id: node.id, bounds }], &[Intent::Design])
            }
            "props.opacity" => {
                let percent = number(args, "percent").unwrap_or(100.0).clamp(0.0, 100.0);
                self.style_change(StylePatch { opacity_milli: Some((percent * 10.0).round() as u32), ..Default::default() })
            }
            "props.image_fit" => {
                let fit = match text(args, "fit").ok_or("이미지 맞춤 방식을 고르세요")? {
                    "fill" => designllm_contracts::ImageFit::Fill,
                    "contain" => designllm_contracts::ImageFit::Contain,
                    "cover" => designllm_contracts::ImageFit::Cover,
                    _ => return Err("이미지 맞춤 방식은 fill, contain 또는 cover여야 합니다".into()),
                };
                let node_id = self.image_fit_target()?;
                self.change(vec![Operation::SetNodeAppearance { node_ids: vec![node_id], image_fit: Some(fit), locked: None }], &[Intent::Design])
            }
            "props.image_alt" => {
                let node_id = self.image_alt_target()?;
                let alt = text(args, "alt").ok_or("대체 텍스트를 지정하세요")?.to_owned();
                self.change(vec![Operation::SetNodeInfo { node_ids: vec![node_id], alt: Some(alt), name: None, hidden: None }], &[Intent::Content])
            }
            "props.corner_radius" => {
                let px = number(args, "value").unwrap_or(0.0).max(0.0);
                self.style_change(StylePatch { corner_radius: Some((px * UNIT_PX).round() as i64), ..Default::default() })
            }
            "props.fill" => {
                let color = text(args, "color").unwrap_or("#FFFFFF").to_owned();
                self.style_change(StylePatch { fill: Some(color), ..Default::default() })
            }
            "props.fill_remove" => self.style_change(StylePatch { fill: Some(self.no_paint().into()), ..Default::default() }),
            "props.stroke" => {
                let color = text(args, "color").unwrap_or("#566479").to_owned();
                self.style_change(StylePatch { stroke: Some(color), ..Default::default() })
            }
            "props.stroke_width" => {
                let emu = number(args, "value").unwrap_or(1.0).max(0.0);
                self.style_change(StylePatch { stroke_width: Some(emu.round() as i64), ..Default::default() })
            }
            "props.stroke_remove" => self.style_change(StylePatch { stroke: Some(self.no_paint().into()), stroke_width: Some(0), ..Default::default() }),
            "props.shadow" => {
                let scale = UNIT_PX;
                let shadow = Shadow {
                    x: (number(args, "x").unwrap_or(0.0) * scale).round() as i64,
                    y: (number(args, "y").unwrap_or(4.0) * scale).round() as i64,
                    blur: (number(args, "blur").unwrap_or(12.0) * scale).round() as i64,
                    spread: (number(args, "spread").unwrap_or(0.0) * scale).round() as i64,
                    color: shadow_color(text(args, "color").unwrap_or("#101622"), number(args, "opacity")),
                };
                self.style_change(StylePatch { shadows: Some(vec![shadow]), ..Default::default() })
            }
            "props.shadow_remove" => self.style_change(StylePatch { shadows: Some(Vec::new()), ..Default::default() }),
            "props.text_style" => {
                let patch = StylePatch {
                    bold: args.get("bold").and_then(Value::as_bool),
                    italic: args.get("italic").and_then(Value::as_bool),
                    font_size: number(args, "font_size").map(|pt| (pt * EMU_PER_PT as f64).round() as i64),
                    color: text(args, "color").map(str::to_owned),
                    ..Default::default()
                };
                self.style_change(patch)
            }
            "canvas.token.put" => {
                let token_id = text(args, "token_id").filter(|value| !value.trim().is_empty()).ok_or("토큰 ID를 입력하세요")?.to_owned();
                let name = text(args, "name").filter(|value| !value.trim().is_empty()).ok_or("토큰 이름을 입력하세요")?.to_owned();
                let values = args
                    .get("values")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<TokenValues>(value).ok())
                    .ok_or("토큰 값 형식이 올바르지 않습니다")?;
                self.token_change(
                    vec![Operation::PutToken { token_id: token_id.clone(), name: name.clone(), values: values.clone() }],
                    TokenChangeBaseline::Put { token_id, name, values },
                )
            }
            "canvas.token.remove" => {
                let token_id = text(args, "token_id").filter(|value| !value.trim().is_empty()).ok_or("토큰을 고르세요")?.to_owned();
                self.token_change(vec![Operation::RemoveToken { token_id }], TokenChangeBaseline::Remove)
            }
            "canvas.token.mode" => {
                let mode = match text(args, "mode").ok_or("토큰 모드를 고르세요")? {
                    "light" => TokenMode::Light,
                    "dark" => TokenMode::Dark,
                    _ => return Err("라이트 또는 다크 모드를 고르세요".into()),
                };
                self.token_change(vec![Operation::SetTokenMode { mode }], TokenChangeBaseline::Advance)
            }
            "canvas.design_system.list" => {
                self.call("design_system.list", json!({}), PendingAction::DesignSystemList);
                Ok(())
            }
            "canvas.design_system.publish" => self.design_system_publish(args),
            "canvas.design_system.set" => self.design_system_set(args),
            "canvas.token.bind" => {
                let node_ids = self.selected_ids();
                if node_ids.is_empty() {
                    return Err("토큰을 연결할 캔버스 요소를 선택하세요".into());
                }
                let token_id = text(args, "token_id").filter(|value| !value.trim().is_empty()).ok_or("토큰을 고르세요")?.to_owned();
                let source = match text(args, "source").unwrap_or("local") {
                    "local" => TokenSource::Local,
                    "shared" => {
                        let artifact = self.artifact.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
                        let reference = artifact.design_system.as_ref().ok_or("공유 원본을 먼저 연결하세요")?;
                        if !self.canvas_token_snapshot_current()
                            || self.token_panel.shared_source_id.as_deref() != Some(reference.artifact_id.as_str())
                            || self.token_panel.shared_source_revision != Some(reference.revision)
                            || !self.token_panel.shared_source_loaded
                            || !self.token_panel.shared_tokens.contains_key(&token_id)
                        {
                            return Err("공유 토큰 목록이 최신이 아닙니다. 원본을 다시 읽어 주세요".into());
                        }
                        TokenSource::Shared
                    }
                    _ => return Err("토큰 출처는 local 또는 shared여야 합니다".into()),
                };
                self.token_change(
                    vec![Operation::BindToken {
                        node_ids,
                        target: token_target(args)?,
                        token: TokenRef { source, token_id },
                    }],
                    TokenChangeBaseline::Advance,
                )
            }
            "canvas.token.unbind" => {
                let node_ids = self.selected_ids();
                if node_ids.is_empty() {
                    return Err("토큰 연결을 해제할 캔버스 요소를 선택하세요".into());
                }
                self.token_change(
                    vec![Operation::UnbindToken { node_ids, target: token_target(args)? }],
                    TokenChangeBaseline::Advance,
                )
            }
            "props.text_align" => {
                let align = text(args, "align").unwrap_or("left").to_owned();
                self.style_change(StylePatch { align: Some(align), ..Default::default() })
            }
            "edit.group" => self.wrap_selected(NodeKind::Group, None),
            "edit.stack" => self.wrap_selected(NodeKind::Rectangle, Some(LayoutSpec { mode: "row".into(), ..LayoutSpec::default() })),
            "edit.ungroup" => self.ungroup_selected(),
            "props.layout_mode" => {
                let choice = text(args, "mode").unwrap_or("free").to_owned();
                self.edit_layout(move |layout| {
                    layout.mode = model_layout_mode(&choice, &layout.mode).to_owned();
                    // The engine's grid rules (canvas model.rs validate_layout): a column count (the properties panel shows 2
                    // until one is set), a fixed or fill width (a hugging stack keeps its current width), no distribution.
                    if layout.mode == "grid" {
                        if layout.grid_columns.is_none() {
                            layout.grid_columns = Some(2);
                        }
                        if layout.width_mode == "hug" {
                            layout.width_mode = "fixed".into();
                        }
                        layout.justify = None;
                        layout.breakpoints.iter_mut().for_each(|breakpoint| breakpoint.justify = None);
                    }
                })
            }
            "props.layout_direction" => {
                let column = text(args, "direction") == Some("column");
                self.edit_layout(move |layout| layout.mode = String::from(if column { "column" } else { "row" }))
            }
            "props.layout_justify" => {
                let justify = justify_of(text(args, "justify").unwrap_or("start"));
                self.edit_layout(move |layout| layout.justify = Some(justify))
            }
            "props.layout_align" => {
                let align = text(args, "align").unwrap_or("start").to_owned();
                self.edit_layout(move |layout| layout.align = align)
            }
            "props.layout_gap" => {
                let gap = (number(args, "value").unwrap_or(0.0).max(0.0) * UNIT_PX).round() as i64;
                self.edit_layout(move |layout| layout.gap = gap)
            }
            "props.layout_padding" => {
                let padding = (number(args, "value").unwrap_or(0.0).max(0.0) * UNIT_PX).round() as i64;
                self.edit_layout(move |layout| {
                    layout.padding = padding;
                    layout.padding_sides = None;
                })
            }
            "props.size_mode" => {
                let width = text(args, "width_mode").map(str::to_owned);
                let height = text(args, "height_mode").map(str::to_owned);
                // Every selected node takes the mode, in one change set.
                if self.selected_nodes.len() > 1 {
                    let artifact = self.artifact.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
                    let operations: Vec<Operation> = self
                        .selected_nodes
                        .iter()
                        .filter_map(|id| super::doc_tree::find_node(artifact, id))
                        .map(|node| {
                            let mut layout = node.layout.clone().unwrap_or_default();
                            if let Some(width) = &width {
                                layout.width_mode = width.clone();
                            }
                            if let Some(height) = &height {
                                layout.height_mode = height.clone();
                            }
                            Operation::SetLayout { node_id: node.id.clone(), layout }
                        })
                        .collect();
                    return self.change(operations, &[Intent::Design]);
                }
                self.edit_layout(move |layout| {
                    if let Some(width) = width {
                        layout.width_mode = width;
                    }
                    if let Some(height) = height {
                        layout.height_mode = height;
                    }
                })
            }
            "ai.undo_change" => self.undo_batch(args),
            "props.align" => {
                let edge = text(args, "edge").unwrap_or("left").to_owned();
                if self.selected_nodes.len() > 1 {
                    return self.align_selected(&edge);
                }
                let (node, index, _) = self.selected_with_page().ok_or("정렬할 대상을 고르세요")?;
                let size = self.page_sizes().get(index as usize).copied().unwrap_or(Size::a4());
                let bounds = node.bounds;
                let aligned = match edge.as_str() {
                    "left" => ModelRect { x: 0, ..bounds },
                    "h-center" => ModelRect { x: (size.width - bounds.width) / 2, ..bounds },
                    "right" => ModelRect { x: size.width - bounds.width, ..bounds },
                    "top" => ModelRect { y: 0, ..bounds },
                    "v-center" => ModelRect { y: (size.height - bounds.height) / 2, ..bounds },
                    _ => ModelRect { y: size.height - bounds.height, ..bounds },
                };
                self.change(vec![Operation::SetGeometry { node_id: node.id, bounds: aligned }], &[Intent::Design])
            }
            "props.page_size" => {
                let (_, page_id) = self.current_page_id().ok_or("페이지를 고르세요")?;
                let scale = self.emu_per_unit();
                let size = if let Some(preset) = text(args, "preset") {
                    designllm_contracts::frame_preset(preset).ok_or("없는 크기입니다")?.size
                } else {
                    let width = number(args, "width").unwrap_or(0.0) * scale;
                    let height = number(args, "height").unwrap_or(0.0) * scale;
                    Size { width: width.round() as i64, height: height.round() as i64 }
                };
                self.change(vec![Operation::SetPageSize { page_id, size }], &[Intent::Design])
            }
            "props.rename_page" => {
                let (_, page_id) = self.current_page_id().ok_or("페이지를 고르세요")?;
                let name = text(args, "name").unwrap_or_default().to_owned();
                self.change(vec![Operation::RenamePage { page_id, name: Some(name) }], &[Intent::Content])
            }
            // Text.
            "text.set" => {
                let node_id = text(args, "node_id").unwrap_or_default().to_owned();
                let value = text(args, "text").unwrap_or_default().to_owned();
                self.equation_check(&node_id, &value)?;
                self.change(vec![Operation::SetText { node_id, text: value }], &[Intent::Content])
            }
            // AI requests.
            "ai.request" => {
                let prompt = text(args, "prompt").unwrap_or_default().trim().to_owned();
                if prompt.is_empty() {
                    return Err("요청 내용을 적어 주세요".into());
                }
                if let Some(reason) = self.ai_request_block(&prompt) {
                    return Err(reason.into());
                }
                self.ai_prompt = prompt;
                self.start_ai_job();
                Ok(())
            }
            "ai.cancel" => {
                if self.run_context.is_some() || args.get("job_id").is_some() {
                    self.cancel_job_for_ui(args)
                } else {
                    self.cancel_job();
                    Ok(())
                }
            }
            "ai.retry" => {
                let prompt = self.ai_prompt.clone();
                if let Some(reason) = self.ai_request_block(&prompt) {
                    return Err(reason.into());
                }
                self.start_ai_job();
                Ok(())
            }
            "ai.toggle_intent" => {
                if let Some(mode) = text(args, "mode") {
                    let (content, design) = match mode {
                        "both" => (true, true),
                        "design_only" => (false, true),
                        "content_only" => (true, false),
                        _ => return Err("mode는 both, design_only, content_only 중 하나여야 합니다".into()),
                    };
                    // A segment names the whole set the window sends, so the data intent (not one of the segments) is off.
                    self.allow_content = content;
                    self.allow_design = design;
                    self.allow_data = false;
                    return Ok(());
                }
                let Some(intent) = text(args, "intent") else {
                    return Err("intent 또는 mode를 지정해 주세요".into());
                };
                match intent {
                    // Each toggle may turn off only while another intent stays on, so the request always has one.
                    "design" => {
                        if !self.allow_design || self.allow_content || self.allow_data {
                            self.allow_design = !self.allow_design;
                        }
                    }
                    "data" => {
                        if !self.allow_data || self.allow_content || self.allow_design {
                            self.allow_data = !self.allow_data;
                        }
                    }
                    "content" => {
                        if !self.allow_content || self.allow_design || self.allow_data {
                            self.allow_content = !self.allow_content;
                        }
                    }
                    _ => return Err("intent는 content, design, data 중 하나여야 합니다".into()),
                }
                Ok(())
            }
            "ai.deliver_as" | "proposal.select" | "proposal.discard" | "proposal.apply" => self.proposal_command(id, args),
            "figure.output_profile" => self.apply_figure_output_profile(args),
            // Files.
            "file.save" => {
                if self.run_context.is_some() || args.get("target_path").is_some() {
                    self.save_active_to(args)
                } else {
                    self.save_active();
                    Ok(())
                }
            }
            "file.save_as" => {
                let artifact = self.active.as_ref().ok_or("작업물이 열려 있지 않습니다")?;
                if let Some(path) = crate::dialogs::save_document(self.parent_hwnd, artifact.title.clone(), artifact.kind) {
                    if self.home.project_folder.is_some() {
                        return self.save_active_to(&json!({ "target_path": path }));
                    }
                    self.save_path = path.to_string_lossy().into_owned();
                    self.save_active();
                }
                Ok(())
            }
            "file.export" => {
                if args.get("dpi").is_some_and(|dpi| !dpi.as_u64().is_some_and(|dpi| (72..=1200).contains(&dpi))) {
                    return Err("dpi는 72부터 1200까지의 정수여야 합니다".into());
                }
                if let Some(dpi) = args.get("dpi").and_then(Value::as_u64) { self.view().figure_export_dpi = dpi as u32; }
                if self.run_context.is_some() || args.get("target_path").is_some() {
                    return self.export_active_to(args);
                }
                let format = text(args, "format").unwrap_or("pdf").to_owned();
                self.export_format = format.clone();
                if let Some(scale) = number(args, "scale") {
                    self.export_scale = scale as u32;
                }
                if let Some(scope) = text(args, "scope") {
                    self.export_scope = scope.to_owned();
                }
                if let Some(background) = text(args, "background") {
                    self.export_transparent = background == "transparent";
                }
                // A page range is checked before the save dialog opens.
                let pages = text(args, "pages").unwrap_or_default();
                if self.export_scope == "range" {
                    self.check_page_range(&format, pages)?;
                }
                let extension = format.clone();
                if let Some(path) = crate::dialogs::save_export(self.parent_hwnd, &extension) {
                    self.export_path = path.to_string_lossy().into_owned();
                    // The settings just taken from the arguments, as the popover's own export sends them.
                    let mut options = self.popover_export_args(pages);
                    if args.get("dpi").is_none() && args.get("scale").is_some() {
                        options.as_object_mut().expect("export options are an object").remove("dpi");
                    }
                    self.export_active_with(Some(&options));
                }
                Ok(())
            }
            // The properties panel's `내보내기 ›`: PNG or SVG of the selected nodes only, through the export path.
            "file.export_selection" => {
                if self.selected_nodes.is_empty() {
                    return Err("내보낼 대상을 고르세요".into());
                }
                let mut export = args.clone();
                export["scope"] = json!("selection");
                self.invoke("file.export", export)
            }
            "file.dismiss_loss" => {
                self.apply_export_losses();
                Ok(())
            }
            "web.preview" => {
                self.start_materialized_web_preview();
                Ok(())
            }
            // Home.
            "home.create" => self.create_from_home_command(args),
            "home.view" => {
                self.home.list_view = text(args, "view") != Some("grid");
                Ok(())
            }
            "home.starred_only" => {
                self.home.starred_only = args.get("on").and_then(Value::as_bool).unwrap_or(false);
                Ok(())
            }
            "home.attach" => {
                if let Some(path) = text(args, "path") {
                    self.home.attachments.push(path.to_owned());
                }
                Ok(())
            }
            "home.mode" => {
                self.home.code_mode = text(args, "mode") == Some("code");
                Ok(())
            }
            "home.star" => {
                let artifact_id = text(args, "artifact_id").unwrap_or_default().to_owned();
                if !self.home.starred.remove(&artifact_id) {
                    self.home.starred.insert(artifact_id);
                }
                self.save_settings();
                Ok(())
            }
            // No design system exists before D11: the only choice is none.
            "home.design_system" => {
                self.home.design_system = text(args, "design_system").unwrap_or_default().to_owned();
                Ok(())
            }
            "home.tile" => {
                self.home.tile = text(args, "tile").unwrap_or("blank").to_owned();
                self.home_preset.clear();
                Ok(())
            }
            "home.tab" => {
                self.home.tab = match text(args, "tab") {
                    Some("design_systems") => super::home::HomeTab::DesignSystems,
                    Some("templates") => super::home::HomeTab::Templates,
                    _ => super::home::HomeTab::Projects,
                };
                Ok(())
            }
            "home.preset" => {
                // A user size (`사용자 크기`) is kept as `custom:<width>x<height>` in the tile's units (px, or mm for print).
                if self.home.tile == "resume" {
                    if args.get("width").is_some() || args.get("height").is_some() {
                        return Err("이력서 템플릿은 A4 크기만 지원합니다".into());
                    }
                    let preset = text(args, "preset").unwrap_or_default();
                    if !preset.is_empty() && preset != "a4" {
                        return Err("이력서 템플릿은 A4 크기만 지원합니다".into());
                    }
                    self.home_preset = preset.to_owned();
                    return Ok(());
                }
                if let (Some(width), Some(height)) = (number(args, "width"), number(args, "height")) {
                    self.home_preset = format!("custom:{}x{}", width.round(), height.round());
                    return Ok(());
                }
                let preset = text(args, "preset").unwrap_or_default().to_owned();
                if !preset.is_empty() && designllm_contracts::frame_preset(&preset).is_none()
                    && super::home::custom_size(&preset).is_none()
                    && !(matches!(self.home.tile.as_str(), "figure" | "diagram") && designllm_contracts::figure_rules::figure_output_profile(&preset).is_some()) {
                    return Err("없는 크기입니다".into());
                }
                self.home_preset = preset;
                Ok(())
            }
            "home.runner" => {
                // The editor's runner stays as it was while one of its jobs runs: the job keeps its own runner. The home screen's
                // runner is not held back by a job of an open document.
                if self.screen_is_editor() && self.ai_job_wait().is_some() {
                    return Err("AI 작업이 진행 중이라 실행기를 바꿀 수 없습니다".into());
                }
                let mut runtime_changed = false;
                if let Some(runtime) = text(args, "runtime") {
                    if !self.runner_models.available(runtime) { return Err("로그인된 AI 실행기를 고르세요".into()); }
                    runtime_changed = super::runner_models::runtime_kind(runtime) != super::runner_models::runtime_kind(&self.ai_runtime);
                    self.ai_runtime = runtime.to_owned();
                }
                if let Some(model) = text(args, "model") {
                    self.ai_model = model.trim().to_owned();
                }
                if let Some(effort) = text(args, "effort") {
                    self.ai_effort = effort.to_owned();
                }
                // A runtime change keeps the model only when the new runtime lists it (a model given in the same call is kept as
                // given); the listed model's effort is fitted to it (`runner_models.rs`).
                self.fit_runner(runtime_changed, text(args, "model").is_some(), text(args, "effort").is_some());
                self.save_settings();
                Ok(())
            }
            // A template is a canvas made by the engine (`artifact.create` with `template`); the editor opens on it.
            "home.use_template" => {
                let template = text(args, "template").unwrap_or("mobile_app").to_owned();
                let title = text(args, "title").map(str::to_owned).unwrap_or_else(|| super::state::home_template_title(&template).to_owned());
                let kind = match text(args, "kind") {
                    Some(kind) => serde_json::from_value(json!(kind)).map_err(|_| format!("unknown template kind {kind}"))?,
                    None => self.home.feeds.templates.iter().find(|info| info.id == template).map(|info| info.kind)
                        .or_else(|| (template == "webgl_animation").then_some(ArtifactKind::WebCode))
                        .unwrap_or(ArtifactKind::Canvas),
                };
                let request = designllm_contracts::CreateArtifactRequest {
                    request_id: new_id(),
                    actor: "desktop".into(),
                    options: designllm_contracts::CreateRequest { kind, title, size: None, template: Some(template) },
                };
                let params = serde_json::to_value(request).map_err(|error| error.to_string())?;
                self.call("artifact.create", params, PendingAction::Create);
                Ok(())
            }
            "home.search" => {
                self.home.search = text(args, "text").unwrap_or_default().to_owned();
                Ok(())
            }
            _ => self
                .dispatch_kind(spec, args)
                .or_else(|| self.dispatch_effects(spec, args))
                .or_else(|| self.dispatch_fixes(spec, args))
                .or_else(|| self.dispatch_figure_r07(spec, args))
                .or_else(|| self.dispatch_figure_math(spec, args))
                .or_else(|| self.dispatch_operation_commands(spec, args))
                .or_else(|| self.dispatch_repeat_groups(spec, args))
                .or_else(|| self.dispatch_slides_theme(spec, args))
                .or_else(|| self.dispatch_design_theme(spec, args))
                .or_else(|| self.dispatch_web_mapping(spec, args))
                .unwrap_or_else(|| Err(format!("{}: 아직 연결되지 않았습니다", spec.label))),
        }
    }

    fn add_node_kind(&mut self, kind: NodeKind, args: &Value) -> Result<(), String> {
        let at = self.take_add_at(args)?;
        self.add_node(kind, at)
    }

    fn current_page_id(&self) -> Option<(usize, designllm_contracts::Id)> {
        let index = self.view_ref().page_index as usize;
        let artifact = self.artifact.as_ref()?;
        match &artifact.body {
            designllm_contracts::ArtifactBody::Canvas(document) => document.pages.get(index).map(|page| (index, page.id.clone())),
            designllm_contracts::ArtifactBody::Slides(document) => document.pages.get(index).map(|page| (index, page.id.clone())),
            _ => None,
        }
    }

    fn page_node_count(&self, page_id: &str) -> usize {
        self.page_nodes(page_id).map_or(0, |nodes| nodes.len())
    }

    fn page_node_index(&self, page_id: &str, node_id: &str) -> Option<usize> {
        self.page_nodes(page_id)?.iter().position(|node| node.id == node_id)
    }

    fn page_nodes(&self, page_id: &str) -> Option<&Vec<Node>> {
        let artifact = self.artifact.as_ref()?;
        match &artifact.body {
            designllm_contracts::ArtifactBody::Canvas(document) => document.pages.iter().find(|page| page.id == page_id).map(|page| &page.nodes),
            designllm_contracts::ArtifactBody::Slides(document) => document.pages.iter().find(|page| page.id == page_id).map(|page| &page.nodes),
            _ => None,
        }
    }

    /// Adds a page of a frame preset at the end of the document.
    fn add_page_preset(&mut self, preset: &str) -> Result<(), String> {
        let size = if preset.is_empty() {
            None
        } else {
            Some(designllm_contracts::frame_preset(preset).ok_or("없는 크기입니다")?.size)
        };
        let Some(artifact) = self.artifact.as_ref() else {
            return Err("작업물을 아직 읽는 중입니다".into());
        };
        let (index, default) = match &artifact.body {
            designllm_contracts::ArtifactBody::Canvas(document) => (document.pages.len() as u32, document.pages.first().map(|page| page.size)),
            designllm_contracts::ArtifactBody::Slides(document) => (document.pages.len() as u32, document.pages.first().map(|page| page.size)),
            _ => return Err("이 작업물에는 페이지를 추가할 수 없습니다".into()),
        };
        let size = size.or(default).unwrap_or_else(Size::a4);
        let page = Page::new(new_id(), size);
        self.change(vec![Operation::InsertPage { index, page }], &[Intent::Content])
    }

}


impl DesktopApp {
    /// Adds an imported picture to the page as an image node (320 × 240 px at the top left, or centred on `center` in EMU).
    pub(super) fn insert_image(&mut self, page_id: &str, value: Value, center: Option<(i64, i64)>) {
        let Ok(asset) = serde_json::from_value::<designllm_contracts::AssetRef>(value) else {
            self.set_error("이미지를 가져오지 못했습니다".into());
            return;
        };
        let (width, height) = (320 * UNIT_PX as i64, 240 * UNIT_PX as i64);
        let (x, y) = match center {
            Some((cx, cy)) => (cx - width / 2, cy - height / 2),
            None => (40 * UNIT_PX as i64, 40 * UNIT_PX as i64),
        };
        let node = Node {
            id: new_id(),
            kind: NodeKind::Image,
            bounds: ModelRect { x, y, width, height },
            text: String::new(),
            style: Style::default(),
            asset: Some(asset),
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: std::collections::BTreeMap::new(),
            ext: Default::default(),
        };
        let operation = if page_id.is_empty() {
            // A flowing document: after the selected block, as wide as the picture at 96 dpi but not wider than the text.
            let Some(designllm_contracts::ArtifactBody::Flow(document)) = self.artifact.as_ref().map(|artifact| &artifact.body) else {
                return;
            };
            let anchor = self.selection_anchor();
            let Some(super::links::Placement::Flow { section_id, index, content_width }) = super::links::flow_placement(document, anchor.as_deref()) else {
                self.set_error("이미지를 넣을 위치를 찾지 못했습니다".into());
                return;
            };
            let mut node = node;
            node.bounds = ModelRect { x: 0, y: 0, width: node.bounds.width.min(content_width), height: node.bounds.height * node.bounds.width.min(content_width) / node.bounds.width };
            Operation::InsertFlowBlock { section_id, index, block: designllm_contracts::FlowBlock::Image(node) }
        } else {
            Operation::AddNode { page_id: page_id.to_owned(), parent_id: None, node }
        };
        if let Err(message) = self.change(vec![operation], &[Intent::Content]) {
            self.set_error(message);
        }
    }
}

impl DesktopApp {
    fn duplicate_selected(&mut self) -> Result<(), String> {
        let (mut nodes, page_id, parent_id) = self.selected_siblings()?;
        let artifact = self.artifact.as_ref().ok_or("작업물이 열려 있지 않습니다")?;
        if artifact.kind == ArtifactKind::Slides && artifact.origin != "created" {
            return Err("가져온 PPTX 객체의 원본 서식·배치를 보존할 수 없어 복제하지 않았습니다".into());
        }
        let mut copied_ids = BTreeSet::new();
        collect_ids(&nodes, &mut copied_ids);
        let figure_links = copied_figure_links(&artifact.figure_links, &copied_ids);
        let id_map = renew_ids(&mut nodes);
        let ids: Vec<String> = nodes.iter().map(|node| node.id.clone()).collect();
        let offset = (20.0 * self.emu_per_unit()) as i64;
        let mut operations = nodes
            .into_iter()
            .map(|mut node| {
                node.bounds.x = node.bounds.x.checked_add(offset).ok_or("복제 위치가 범위를 벗어났습니다")?;
                node.bounds.y = node.bounds.y.checked_add(offset).ok_or("복제 위치가 범위를 벗어났습니다")?;
                Ok(Operation::AddNode { page_id: page_id.clone(), parent_id: parent_id.clone(), node })
            })
            .collect::<Result<Vec<_>, &str>>()?;
        operations.extend(figure_link_operations(figure_links, &id_map)?);
        self.select_after_commit = Some(ids);
        self.select_after_commit_ticket = None;
        if let Err(error) = self.change(operations, &[Intent::Content]) {
            self.select_after_commit = None;
            self.select_after_commit_ticket = None;
            return Err(error);
        }
        Ok(())
    }

    fn copy_selected(&mut self) -> Result<(), String> {
        let (nodes, page_id, parent_id) = self.selected_siblings()?;
        let artifact = self.artifact.as_ref().ok_or("작업물이 열려 있지 않습니다")?;
        if artifact.kind == ArtifactKind::Slides && artifact.origin != "created" {
            return Err("가져온 PPTX 객체의 원본 서식·배치를 보존할 수 없어 복사하지 않았습니다".into());
        }
        let page_nodes = match &artifact.body {
            designllm_contracts::ArtifactBody::Canvas(document) => document.pages.iter().find(|page| page.id == page_id).map(|page| page.nodes.as_slice()),
            designllm_contracts::ArtifactBody::Slides(document) => document.pages.iter().find(|page| page.id == page_id).map(|page| page.nodes.as_slice()),
            _ => None,
        }.ok_or("복사할 페이지를 찾지 못했습니다")?;
        let origin = match parent_id.as_deref() {
            Some(id) if parent_uses_auto_layout(page_nodes, id) => {
                return Err("자동 배치 프레임 안의 항목은 따로 복사할 수 없습니다".into());
            }
            Some(id) => node_origin(page_nodes, id, (0, 0)).ok_or("상위 항목 위치를 찾지 못했습니다")?,
            None => (0, 0),
        };
        let mut nodes = nodes;
        for node in &mut nodes {
            node.bounds.x = node.bounds.x.checked_add(origin.0).ok_or("복사 위치가 범위를 벗어났습니다")?;
            node.bounds.y = node.bounds.y.checked_add(origin.1).ok_or("복사 위치가 범위를 벗어났습니다")?;
        }
        let mut copied_ids = BTreeSet::new();
        collect_ids(&nodes, &mut copied_ids);
        self.node_clipboard = Some(super::NodeClipboard {
            artifact_id: artifact.id.clone(),
            artifact_origin: artifact.origin.clone(),
            page_id,
            kind: artifact.kind,
            nodes,
            figure_links: copied_figure_links(&artifact.figure_links, &copied_ids),
        });
        Ok(())
    }

    fn paste_nodes(&mut self) -> Result<(), String> {
        let clipboard = self.node_clipboard.clone().ok_or("복사한 항목이 없습니다")?;
        let (artifact_id, kind) = {
            let active = self.active.as_ref().ok_or("작업물이 열려 있지 않습니다")?;
            (active.id.clone(), active.kind)
        };
        if !matches!(kind, ArtifactKind::Canvas | ArtifactKind::Slides) || !matches!(clipboard.kind, ArtifactKind::Canvas | ArtifactKind::Slides) {
            return Err("캔버스와 슬라이드 사이에서만 항목을 붙여 넣을 수 있습니다".into());
        }
        if clipboard.kind == ArtifactKind::Slides && clipboard.artifact_origin != "created" {
            return Err("가져온 PPTX 객체의 원본 서식·배치를 보존할 수 없어 붙여 넣지 않았습니다".into());
        }
        let (_, page_id) = self.current_page_id().ok_or("붙여 넣을 페이지를 고르세요")?;
        if kind == ArtifactKind::Slides {
            check_slides_transfer(&clipboard.nodes)?;
        }
        let same_artifact = artifact_id == clipboard.artifact_id;
        let mut nodes = clipboard.nodes;
        let mut copied_ids = BTreeSet::new();
        collect_ids(&nodes, &mut copied_ids);
        remap_pasted_interactions(&mut nodes, &copied_ids, same_artifact, &clipboard.page_id, &page_id)?;
        let id_map = renew_ids(&mut nodes);
        let ids: Vec<String> = nodes.iter().map(|node| node.id.clone()).collect();
        let offset = (20.0 * self.emu_per_unit()) as i64;
        let mut operations = nodes
            .into_iter()
            .map(|mut node| {
                node.bounds.x = node.bounds.x.checked_add(offset).ok_or("붙여 넣을 위치가 범위를 벗어났습니다")?;
                node.bounds.y = node.bounds.y.checked_add(offset).ok_or("붙여 넣을 위치가 범위를 벗어났습니다")?;
                Ok(Operation::AddNode { page_id: page_id.clone(), parent_id: None, node })
            })
            .collect::<Result<Vec<_>, &str>>()?;
        operations.extend(figure_link_operations(clipboard.figure_links, &id_map)?);
        self.select_after_commit = Some(ids);
        self.select_after_commit_ticket = None;
        if let Err(error) = self.change(operations, &[Intent::Content]) {
            self.select_after_commit = None;
            self.select_after_commit_ticket = None;
            return Err(error);
        }
        Ok(())
    }

    fn copy_properties(&mut self) -> Result<(), String> {
        if !matches!(self.active.as_ref().map(|active| active.kind), Some(ArtifactKind::Canvas | ArtifactKind::Slides | ArtifactKind::Figure)) {
            return Err("이 작업물의 속성은 복사할 수 없습니다".into());
        }
        let node = self.selected_node_copy().ok_or("속성을 복사할 대상을 고르세요")?;
        self.style_clipboard = Some(style_copy_patch(&node.style));
        Ok(())
    }

    fn paste_properties(&mut self) -> Result<(), String> {
        let patch = self.style_clipboard.clone().ok_or("복사한 속성이 없습니다")?;
        let node_ids = self.selected_ids();
        if node_ids.is_empty() {
            return Err("속성을 붙여 넣을 대상을 고르세요".into());
        }
        if !matches!(self.active.as_ref().map(|active| active.kind), Some(ArtifactKind::Canvas | ArtifactKind::Slides | ArtifactKind::Figure)) {
            return Err("이 작업물에는 복사한 속성을 붙여 넣을 수 없습니다".into());
        }
        let supports_effects = self
            .context
            .as_ref()
            .and_then(|context| context.capabilities.get(&Feature::Effects))
            .is_some_and(|support| matches!(support.edit, Support::Supported | Support::Approximate));
        let has_effects = patch.corner_radius.is_some()
            || patch.opacity_milli.is_some()
            || patch.shadows.is_some()
            || patch.gradient.is_some()
            || patch.line_height_milli.is_some()
            || patch.letter_spacing.is_some()
            || patch.vertical_align.is_some()
            || patch.font_weight.is_some()
            || patch.stroke_dash.is_some()
            || patch.stroke_cap.is_some()
            || patch.stroke_join.is_some()
            || patch.underline.is_some()
            || patch.strikethrough.is_some();
        if !supports_effects {
            if has_effects {
                return Err("대상 작업물이 복사한 효과나 글꼴 속성을 지원하지 않습니다".into());
            }
        }
        let artifact = self.artifact.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
        let operations = node_ids
            .into_iter()
            .map(|id| {
                let node = super::doc_tree::find_node(artifact, &id).ok_or("선택한 대상을 찾지 못했습니다")?;
                let mut patch = patch.clone();
                let target_fields = node.style.non_default_v12_fields();
                patch.clear.retain(|field| target_fields.contains(field));
                let text_only_clears = [StyleField::FontWeight, StyleField::Underline, StyleField::Strikethrough];
                if !patch.clear.is_empty() && !supports_effects {
                    return Err("대상 작업물에서 기존 효과를 지울 수 없습니다".into());
                }
                if matches!(node.kind, NodeKind::Image | NodeKind::Line | NodeKind::Path | NodeKind::Group)
                    && patch.clear.iter().any(|field| text_only_clears.contains(field))
                {
                    return Err("대상 종류에서 기존 글꼴 효과를 지울 수 없습니다".into());
                }
                Ok(Operation::SetStyle { node_ids: vec![id], patch })
            })
            .collect::<Result<Vec<_>, &str>>()?;
        self.change(operations, &[Intent::Design])
    }

    /// Changes the selected frame's layout: the same spec with one field changed, as one change set.
    fn edit_layout(&mut self, edit: impl FnOnce(&mut LayoutSpec)) -> Result<(), String> {
        let (node, _, _) = self.selected_with_page().ok_or("레이아웃을 바꿀 대상을 고르세요")?;
        let mut layout = node.layout.clone().unwrap_or_default();
        edit(&mut layout);
        self.change(vec![Operation::SetLayout { node_id: node.id, layout }], &[Intent::Design])
    }

    /// Wraps the selected node in a new container at its frame: a group, or a stack frame.
    fn wrap_selected(&mut self, kind: NodeKind, layout: Option<LayoutSpec>) -> Result<(), String> {
        let (nodes, page_id, parent) = self.selected_siblings()?;
        if parent.is_some() {
            return Err(String::from("상위 항목 안의 항목은 묶을 수 없습니다"));
        }
        let operations = wrap_operations(&nodes, &page_id, kind, layout);
        self.change(operations, &[Intent::Content, Intent::Design])
    }

    /// Puts the selected group's children back on the page, where the group was, and removes it.
    fn ungroup_selected(&mut self) -> Result<(), String> {
        let (group, _, page_id) = self.selected_with_page().ok_or("그룹을 고르세요")?;
        if group.kind != NodeKind::Group || group.children.is_empty() {
            return Err("그룹이 아닙니다".into());
        }
        let base = self.page_node_index(&page_id, &group.id).unwrap_or(0) as u32;
        let operations = ungroup_operations(&group, &page_id, base);
        self.select_clear();
        self.change(operations, &[Intent::Content, Intent::Design])
    }
}

/// The model's `LayoutSpec.mode` for a choice of the layout control: free is `absolute`, a stack keeps the
/// direction it has (`row` or `column`, a new stack runs down), grid is `grid`.
pub(super) fn model_layout_mode(choice: &str, current: &str) -> &'static str {
    match choice {
        "stack" if current == "row" => "row",
        "stack" => "column",
        "grid" => "grid",
        _ => "absolute",
    }
}

/// A shadow colour with the given opacity (percent) as `#RRGGBBAA`; without one the colour stays as given.
pub(super) fn shadow_color(color: &str, opacity: Option<f64>) -> String {
    let base: String = color.trim().trim_start_matches('#').chars().take(6).collect();
    match opacity {
        Some(percent) if base.len() == 6 => {
            let alpha = (percent.clamp(0.0, 100.0) / 100.0 * 255.0).round() as u8;
            format!("#{}{alpha:02X}", base.to_uppercase())
        }
        _ => format!("#{}", color.trim().trim_start_matches('#').to_uppercase()),
    }
}

fn justify_of(name: &str) -> Justify {
    match name {
        "center" => Justify::Center,
        "end" => Justify::End,
        "between" => Justify::SpaceBetween,
        _ => Justify::Start,
    }
}

/// The operations that group `nodes` (on one page) into a new container. The container takes their
/// union as its frame. Canvas child bounds are relative to the parent's origin, so each child is
/// moved in and given bounds relative to the container: it stays where it was on the page.
pub(super) fn wrap_operations(nodes: &[Node], page_id: &str, kind: NodeKind, layout: Option<LayoutSpec>) -> Vec<Operation> {
    let frame = union_frame(nodes.iter().map(|node| node.bounds));
    let (left, top) = (frame.x, frame.y);
    let container_id = new_id();
    let container = Node {
        id: container_id.clone(),
        kind,
        bounds: frame,
        text: String::new(),
        style: Style::default(),
        asset: None,
        children: Vec::new(),
        layout,
        interactions: Vec::new(),
        variants: std::collections::BTreeMap::new(),
        ext: Default::default(),
    };
    let mut operations = vec![Operation::AddNode { page_id: page_id.to_owned(), parent_id: None, node: container }];
    for (index, node) in nodes.iter().enumerate() {
        operations.push(Operation::MoveNode {
            node_id: node.id.clone(),
            page_id: page_id.to_owned(),
            parent_id: Some(container_id.clone()),
            index: index as u32,
        });
        let inside = ModelRect {
            x: node.bounds.x - left,
            y: node.bounds.y - top,
            width: node.bounds.width,
            height: node.bounds.height,
        };
        operations.push(Operation::SetGeometry { node_id: node.id.clone(), bounds: inside });
    }
    operations
}

/// The smallest rectangle that holds every given rectangle.
fn union_frame(frames: impl Iterator<Item = ModelRect>) -> ModelRect {
    let frames: Vec<ModelRect> = frames.collect();
    let left = frames.iter().map(|frame| frame.x).min().unwrap_or(0);
    let top = frames.iter().map(|frame| frame.y).min().unwrap_or(0);
    let right = frames.iter().map(|frame| frame.x + frame.width).max().unwrap_or(0);
    let bottom = frames.iter().map(|frame| frame.y + frame.height).max().unwrap_or(0);
    ModelRect { x: left, y: top, width: right - left, height: bottom - top }
}

/// The operations that put a group's children back on the page from `base` on, each at its page
/// position (the group's origin plus the child's relative bounds), then remove the group.
pub(super) fn ungroup_operations(group: &Node, page_id: &str, base: u32) -> Vec<Operation> {
    let mut operations = Vec::new();
    for (offset, child) in group.children.iter().enumerate() {
        operations.push(Operation::MoveNode {
            node_id: child.id.clone(),
            page_id: page_id.to_owned(),
            parent_id: None,
            index: base + offset as u32,
        });
        let frame = ModelRect {
            x: group.bounds.x + child.bounds.x,
            y: group.bounds.y + child.bounds.y,
            width: child.bounds.width,
            height: child.bounds.height,
        };
        operations.push(Operation::SetGeometry { node_id: child.id.clone(), bounds: frame });
    }
    operations.push(Operation::RemoveNode { node_id: group.id.clone() });
    operations
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(id: &str, x: i64, y: i64) -> Node {
        serde_json::from_value(json!({
            "id": id,
            "kind": "text",
            "text": "가",
            "bounds": { "x": x, "y": y, "width": 300, "height": 50 },
        }))
        .expect("a text node parses")
    }

    #[test]
    fn the_layout_control_choices_map_to_the_model_modes() {
        assert_eq!(model_layout_mode("free", "row"), "absolute");
        assert_eq!(model_layout_mode("stack", "absolute"), "column");
        assert_eq!(model_layout_mode("stack", "row"), "row");
        assert_eq!(model_layout_mode("grid", "column"), "grid");
    }

    #[test]
    fn a_shadow_opacity_becomes_the_alpha_of_its_colour() {
        assert_eq!(shadow_color("#101622", Some(24.0)), "#1016223D");
        assert_eq!(shadow_color("#101622", Some(100.0)), "#101622FF");
        assert_eq!(shadow_color("#101622", None), "#101622");
        assert_eq!(shadow_color("#1016223d", Some(50.0)), "#10162280");
    }

    #[test]
    fn renewing_a_selection_updates_internal_child_and_sibling_interactions() {
        let mut root = text("root", 0, 0);
        root.kind = NodeKind::Group;
        root.children = vec![text("child", 10, 10)];
        root.interactions = vec![Interaction::ToggleVisibility { node_id: "child".into() }];
        root.children[0].interactions = vec![Interaction::SetVariant { node_id: "sibling".into(), variant: "hover".into() }];
        let mut sibling = text("sibling", 40, 0);
        sibling.interactions = vec![Interaction::ToggleVisibility { node_id: "child".into() }];
        sibling.children = Vec::new();
        let mut nodes = vec![root, sibling];

        let ids = renew_ids(&mut nodes);
        let root_id = nodes[0].id.clone();
        let child_id = nodes[0].children[0].id.clone();
        let sibling_id = nodes[1].id.clone();
        assert_eq!(ids["root"], root_id);
        assert_eq!(ids["child"], child_id);
        assert_eq!(ids["sibling"], sibling_id);
        let Interaction::ToggleVisibility { node_id } = &nodes[0].interactions[0] else { panic!("root interaction") };
        assert_eq!(node_id, &child_id);
        let Interaction::SetVariant { node_id, variant } = &nodes[0].children[0].interactions[0] else { panic!("child interaction") };
        assert_eq!((node_id.as_str(), variant.as_str()), (sibling_id.as_str(), "hover"));
        let Interaction::ToggleVisibility { node_id } = &nodes[1].interactions[0] else { panic!("sibling interaction") };
        assert_eq!(node_id, &child_id);
    }

    #[test]
    fn renewing_a_repeat_keeps_links_to_the_rows_the_engine_will_rebuild() {
        let mut list = text("list", 0, 0);
        list.kind = NodeKind::Group;
        list.ext.repeat = Some(designllm_contracts::RepeatSpec {
            table_id: "data".into(), component_page_id: "card".into(), bindings: Vec::new(),
        });
        let mut row = text("list-row-2", 0, 0);
        row.kind = NodeKind::Group;
        row.ext.instance = Some(designllm_contracts::InstanceRef { component_page_id: "card".into(), overrides: Default::default() });
        row.children.push(text("list-row-2::title", 0, 0));
        list.children.push(row);
        list.interactions.push(Interaction::ToggleVisibility { node_id: "list-row-2::title".into() });
        let ids = renew_ids(std::slice::from_mut(&mut list));
        let row_id = format!("{}-row-2", list.id);
        let title_id = format!("{row_id}::title");
        assert_ne!(list.id, "list");
        assert_eq!(list.children[0].id, row_id);
        assert_eq!(list.children[0].children[0].id, title_id);
        assert_eq!(ids["list-row-2::title"], title_id);
        assert_eq!(list.interactions, [Interaction::ToggleVisibility { node_id: title_id }]);
    }

    #[test]
    fn cross_document_page_links_map_only_the_copied_source_page() {
        let mut current = vec![Node {
            interactions: vec![Interaction::NavigatePage { page_id: "source-page".into() }],
            ..text("node", 0, 0)
        }];
        let ids = BTreeSet::from(["node".to_string()]);
        remap_pasted_interactions(&mut current, &ids, false, "source-page", "target-page").unwrap();
        let Interaction::NavigatePage { page_id } = &current[0].interactions[0] else { panic!("page link") };
        assert_eq!(page_id, "target-page");

        let mut other = vec![Node {
            interactions: vec![Interaction::NavigatePage { page_id: "other-source-page".into() }],
            ..text("node", 0, 0)
        }];
        assert!(remap_pasted_interactions(&mut other, &ids, false, "source-page", "target-page").is_err());
    }

    #[test]
    fn wrapping_one_node_gives_its_container_the_frame_and_the_node_its_relative_frame() {
        let node = text("child", 100, 200);
        let operations = wrap_operations(std::slice::from_ref(&node), "page", NodeKind::Rectangle, None);
        let [Operation::AddNode { node: container, .. }, Operation::MoveNode { parent_id, .. }, Operation::SetGeometry { bounds, .. }] = operations.as_slice() else {
            panic!("a container, the move, the relative frame: {operations:?}");
        };
        assert_eq!(container.bounds, node.bounds);
        assert_eq!(parent_id.as_deref(), Some(container.id.as_str()));
        assert_eq!((bounds.x, bounds.y, bounds.width, bounds.height), (0, 0, 300, 50));
    }

    #[test]
    fn wrapping_two_nodes_keeps_each_one_where_it_was_on_the_page() {
        let first = text("a", 100, 200);
        let second = text("b", 500, 260);
        let operations = wrap_operations(&[first.clone(), second.clone()], "page", NodeKind::Group, None);
        let Operation::AddNode { node: container, .. } = &operations[0] else { panic!("the container comes first") };
        assert_eq!((container.bounds.x, container.bounds.y), (100, 200), "the union starts at the top left");
        assert_eq!((container.bounds.width, container.bounds.height), (700, 110));
        let inside: Vec<(String, i64, i64)> = operations
            .iter()
            .filter_map(|operation| match operation {
                Operation::SetGeometry { node_id, bounds } => Some((node_id.clone(), bounds.x, bounds.y)),
                _ => None,
            })
            .collect();
        assert_eq!(inside, vec![("a".to_owned(), 0, 0), ("b".to_owned(), 400, 60)]);
        assert_eq!(container.bounds.x + 400, second.bounds.x);
    }

    #[test]
    fn ungrouping_puts_each_child_back_at_its_page_position_and_removes_the_group() {
        let mut group = text("group", 1000, 2000);
        group.kind = NodeKind::Group;
        group.bounds = ModelRect { x: 1000, y: 2000, width: 900, height: 600 };
        group.children = vec![text("a", 100, 200), text("b", 50, 60)];
        let operations = ungroup_operations(&group, "page", 4);
        assert_eq!(operations.len(), 5);
        let placed: Vec<(String, i64, i64)> = operations
            .iter()
            .filter_map(|operation| match operation {
                Operation::SetGeometry { node_id, bounds } => Some((node_id.clone(), bounds.x, bounds.y)),
                _ => None,
            })
            .collect();
        assert_eq!(placed, vec![("a".to_owned(), 1100, 2200), ("b".to_owned(), 1050, 2060)]);
        assert!(matches!(operations.last(), Some(Operation::RemoveNode { node_id }) if node_id == "group"));
    }
}

impl DesktopApp {
    /// Takes back one change batch (`history.undo_change`), named by the batch's request id.
    fn undo_batch(&mut self, args: &Value) -> Result<(), String> {
        let change_id = text(args, "change_id").unwrap_or_default().to_owned();
        let (workspace_id, artifact_id, revision) = match (self.context.as_ref(), self.active.as_ref()) {
            (Some(context), Some(active)) => (context.workspace_id.clone(), active.id.clone(), active.revision),
            _ => return Err("되돌릴 작업물이 열려 있지 않습니다".into()),
        };
        let (actor, allowed_intents) = self.change_actor_intents([Intent::Content, Intent::Design, Intent::Data].into_iter().collect());
        let mut request = undo_change_request(&workspace_id, &artifact_id, revision, change_id);
        request.actor = actor;
        request.allowed_intents = allowed_intents;
        let params = serde_json::to_value(request).map_err(|error| error.to_string())?;
        self.call("history.undo_change", params, PendingAction::History);
        Ok(())
    }
}

/// The request that takes back one batch of changes, named by the batch's request id.
pub(super) fn undo_change_request(workspace_id: &str, artifact_id: &str, revision: u64, change_id: String) -> designllm_contracts::UndoChangeRequest {
    designllm_contracts::UndoChangeRequest {
        request_id: new_id(),
        actor: "desktop".into(),
        workspace_id: workspace_id.into(),
        artifact_id: artifact_id.into(),
        expected_revision: revision,
        change_id,
        allowed_intents: [Intent::Content, Intent::Design, Intent::Data].into_iter().collect(),
        job_ref: None,
    }
}
