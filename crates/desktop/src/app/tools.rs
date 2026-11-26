//! The tool bar on the board (GUI_DESIGN §4, §8, §12): the select and hand tools, the shapes and
//! the text, and the page presets from `FRAME_PRESETS`. The floating bar is icon-only with
//! tooltips (rule L2); the add-page menu names each preset (rule L1).

use designllm_contracts::{Feature, EMU_PER_CSS_PX, FRAME_PRESETS};
use eframe::egui::{self, Context, Rect, Ui};
use serde_json::json;

use super::{
    board::Tool,
    chrome::{float_bar, tooltip_for, Bar, FloatPlace},
    DesktopApp,
};
use crate::{commands, theme, widgets};

/// The icon of a frame preset (`preset-*`, §6.1) and its display name (§12).
pub(super) fn preset_name(id: &str) -> &'static str {
    match id {
        "phone" => "휴대폰",
        "phone_large" => "큰 휴대폰",
        "tablet" => "태블릿",
        "desktop" => "데스크톱",
        "social_square" => "소셜 정사각형",
        "social_portrait" => "소셜 세로",
        "story" => "스토리",
        "a4" => "A4",
        "letter" => "Letter",
        "a3" => "A3 포스터",
        "invitation" => "초대장 5×7 in",
        "banner_medium" => "배너 300×250",
        "leaderboard" => "배너 728×90",
        "slide_16_9" => "슬라이드 16:9",
        _ => "사용자 크기",
    }
}

pub(super) fn preset_icon(id: &str) -> &'static str {
    match id {
        "phone" | "phone_large" => "preset-phone",
        "tablet" => "preset-tablet",
        "desktop" => "preset-desktop",
        "social_square" => "preset-square",
        "social_portrait" => "preset-portrait",
        "story" => "preset-story",
        "a4" | "letter" => "preset-paper",
        "a3" => "preset-poster",
        "invitation" => "preset-invitation",
        "banner_medium" => "preset-banner-box",
        "leaderboard" => "preset-banner-wide",
        "slide_16_9" => "preset-slide",
        _ => "preset-custom",
    }
}

/// The page-size menu: every preset as icon, name and size (§12). Choosing one adds a page.
pub(super) fn preset_menu(ui: &mut Ui, palette: &theme::Palette, add: &mut dyn FnMut(&str)) {
    ui.set_width(260.0);
    let mut last_family = None;
    for preset in FRAME_PRESETS {
        if last_family.is_some_and(|family| family != preset.family) {
            widgets::menu_separator(ui, palette);
        }
        last_family = Some(preset.family);
        let width_mm = preset.size.width as f64 / designllm_contracts::EMU_PER_MM as f64;
        let height_mm = preset.size.height as f64 / designllm_contracts::EMU_PER_MM as f64;
        // Screen sizes and slides in CSS px (a 16:9 slide is 1280 × 720), print sizes in mm.
        let size = match preset.natural_unit {
            designllm_contracts::NaturalUnit::Px => format!("{} × {}", preset.size.width / EMU_PER_CSS_PX, preset.size.height / EMU_PER_CSS_PX),
            _ if preset.id == "slide_16_9" => format!("{} × {}", preset.size.width / EMU_PER_CSS_PX, preset.size.height / EMU_PER_CSS_PX),
            _ => format!("{} × {} mm", width_mm.round(), height_mm.round()),
        };
        // Name, and the size as the item's value at the right (L5).
        if widgets::menu_item_value(ui, palette, Some(preset_icon(preset.id)), preset_name(preset.id), &size, false).clicked() {
            add(preset.id);
            ui.close();
        }
    }
}

/// The content icon picker inside the insert menu: a search field (Korean or English keywords) and a grid of 32 px
/// buttons, the icon's purpose as the hover name. Returns the icon clicked.
pub(super) fn icon_picker(ui: &mut Ui, palette: &theme::Palette) -> Option<String> {
    let query_id = egui::Id::new("icon-picker-query");
    let mut query: String = ui.data(|data| data.get_temp(query_id)).unwrap_or_default();
    ui.add_space(4.0);
    let field = widgets::text_field(ui, palette, &mut query, "", ui.available_width());
    if field.changed() {
        ui.data_mut(|data| data.insert_temp(query_id, query.clone()));
    }
    ui.add_space(4.0);
    let mut chosen = None;
    let found = crate::content_icons::search(&query);
    egui::ScrollArea::vertical().max_height(176.0).show(ui, |ui| {
        let columns = ((ui.available_width() + 2.0) / (theme::ICON_BUTTON + 2.0)).floor().max(1.0) as usize;
        for row in found.chunks(columns) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(2.0, 2.0);
                for entry in row {
                    let (rect, response) = ui.allocate_exact_size(egui::Vec2::splat(theme::ICON_BUTTON), egui::Sense::click());
                    if response.hovered() {
                        ui.painter().rect_filled(rect, egui::CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_2);
                    }
                    crate::icons::paint(ui, Rect::from_center_size(rect.center(), egui::Vec2::splat(theme::ICON_IN_BUTTON)), &format!("content:{}", entry.name), palette.ink);
                    if response.on_hover_text(crate::i18n::tr(&entry.purpose)).clicked() {
                        chosen = Some(entry.name.clone());
                    }
                }
            });
        }
    });
    chosen
}

impl DesktopApp {
    /// The bar at the bottom centre of the board: tools for canvases and slides.
    pub(super) fn draw_tool_group(&mut self, ctx: &Context, palette: &theme::Palette, board: Rect) {
        let kind = self.active.as_ref().map(|item| item.kind);
        if matches!(kind, Some(designllm_contracts::ArtifactKind::Docx | designllm_contracts::ArtifactKind::Hwpx | designllm_contracts::ArtifactKind::Hwp)) {
            self.draw_document_tool_group(ctx, palette, board);
            return;
        }
        if kind == Some(designllm_contracts::ArtifactKind::Figure) {
            self.draw_figure_tool_group(ctx, palette, board);
            return;
        }
        if !matches!(kind, Some(designllm_contracts::ArtifactKind::Canvas | designllm_contracts::ArtifactKind::Slides)) {
            return;
        }
        let view = self.view_ref();
        let slides = kind == Some(designllm_contracts::ArtifactKind::Slides);
        let text_ok = self.feature_write_supported(Feature::Text);
        let shape_ok = self.feature_write_supported(Feature::Geometry);
        let images_ok = self.feature_write_supported(Feature::Images);
        // tokens / canvas-board: select · hand │ text · frame · shape ▾ · (pen ▾ · memo when built) │ insert ▾.
        float_bar(ctx, palette, "tool-group", board, FloatPlace::BottomCenter, Bar::Tools, |ui| {
            if widgets::icon_button(ui, palette, "tool-select", "선택  V", view.tool == Tool::Select).clicked() {
                self.view().tool = Tool::Select;
            }
            if widgets::icon_button(ui, palette, "tool-hand", "손  H", view.tool == Tool::Hand).clicked() {
                self.view().tool = Tool::Hand;
            }
            widgets::group_separator(ui, palette);
            if text_ok && widgets::icon_button(ui, palette, "tool-text", &tooltip_for("add.text"), false).clicked() {
                self.invoke_ui("add.text", json!({}));
            }
            if shape_ok && !slides && widgets::icon_button(ui, palette, "tool-frame", &tooltip_for("add.frame"), false).clicked() {
                self.invoke_ui("add.frame", json!({}));
            }
            if shape_ok {
                if widgets::icon_button(ui, palette, "tool-rect", &tooltip_for("add.rect"), false).clicked() {
                    self.invoke_ui("add.rect", json!({}));
                }
                let chevron = widgets::split_chevron(ui, palette, "도형 고르기");
                egui::Popup::from_toggle_button_response(&chevron)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
                    .show(|ui| {
                        ui.set_width(200.0);
                        for (command, icon) in [("add.rect", "tool-rect"), ("add.oval", "tool-oval"), ("add.line", "tool-line"), ("add.arrow", "tool-arrow")] {
                            let Some(spec) = commands::find(command).filter(|_| commands::available(command)) else {
                                continue;
                            };
                            if widgets::menu_item(ui, palette, Some(icon), spec.label, spec.shortcut, false).clicked() {
                                self.invoke_ui(command, json!({}));
                            }
                        }
                    });
            }
            if commands::available("add.pen")
                && widgets::icon_button(ui, palette, "tool-pen", &tooltip_for("add.pen"), false).clicked()
            {
                self.invoke_ui("add.pen", json!({}));
            }
            if kind == Some(designllm_contracts::ArtifactKind::Canvas)
                && widgets::icon_button(
                    ui,
                    palette,
                    "tool-memo",
                    "보드 메모  N",
                    view.tool == Tool::Memo,
                )
                .clicked()
            {
                self.view().tool = if view.tool == Tool::Memo { Tool::Select } else { Tool::Memo };
            }
            if kind == Some(designllm_contracts::ArtifactKind::Canvas) {
                if widgets::icon_button(ui, palette, "layout-grid", &tooltip_for("view.toggle_guides"), view.guides_visible).clicked() {
                    self.invoke_ui("view.toggle_guides", json!({}));
                }
                if widgets::icon_button(ui, palette, "align-h-center", &tooltip_for("view.toggle_snap"), view.snap_guides).clicked() {
                    self.invoke_ui("view.toggle_snap", json!({}));
                }
            }
            widgets::group_separator(ui, palette);
            let insert = widgets::icon_button(ui, palette, "add", "넣기", false);
            let chevron = widgets::split_chevron(ui, palette, "넣을 것 고르기");
            // The chevron opens the same menu as the button.
            if chevron.clicked() {
                egui::Popup::toggle_id(ui.ctx(), egui::Popup::default_response_id(&insert));
            }
            egui::Popup::from_toggle_button_response(&insert)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| self.insert_menu(ui, palette, images_ok, slides));
        });
    }

    /// The insert menu of the canvas and slide bar: picture, paper figures, then a new page of a preset.
    fn insert_menu(&mut self, ui: &mut Ui, palette: &theme::Palette, images_ok: bool, slides: bool) {
        ui.set_width(260.0);
        if images_ok && widgets::menu_item(ui, palette, Some("ins-image"), "이미지", "Ctrl+Shift+K", false).clicked() {
            self.invoke_ui("add.image", json!({}));
            ui.close();
        }
        if images_ok && !slides {
            if self.video_insert_for_active().is_some() {
                ui.small(crate::i18n::tr(&self.video_insert_status_for_active()));
                if ui.add_enabled_ui(self.video_insert_can_cancel(), |ui| widgets::labeled_button(ui, palette, None, "동영상 추가 취소", false)).inner.clicked() {
                    if let Err(message) = self.cancel_canvas_video_insert() { self.set_error(message); }
                }
            } else if widgets::menu_item(ui, palette, Some("ins-video"), "동영상", "", false).clicked() {
                if let Err(message) = self.begin_canvas_video_insert() { self.set_error(message); }
                ui.close();
            }
        }
        if !slides && self.feature_write_supported(Feature::Tables) && commands::available("canvas.table.add")
            && widgets::menu_item(ui, palette, Some("ins-table"), "표", "", false).clicked()
        {
            self.invoke_ui("canvas.table.add", json!({}));
            ui.close();
        }
        if self.feature_write_supported(Feature::VectorPaths) {
            let open_id = egui::Id::new("icon-picker-open");
            let open = ui.data(|data| data.get_temp::<bool>(open_id)).unwrap_or(false);
            if widgets::menu_item(ui, palette, Some("ins-icon"), "아이콘", "", open).clicked() {
                ui.data_mut(|data| data.insert_temp(open_id, !open));
            }
            if open {
                if let Some(icon) = icon_picker(ui, palette) {
                    ui.data_mut(|data| data.insert_temp(open_id, false));
                    self.invoke_ui("add.icon", json!({ "icon": icon }));
                    ui.close();
                }
            }
            let artifact_id = self.artifact.as_ref().map_or("", |artifact| artifact.id.as_str());
            let (path_open_id, path_draft_id) = (path_form_open_id(artifact_id), path_form_draft_id(artifact_id));
            let path_open = ui.data(|data| data.get_temp::<bool>(path_open_id)).unwrap_or(false);
            if widgets::menu_item(ui, palette, Some("tool-pen"), "경로", "", path_open).clicked() {
                ui.data_mut(|data| data.insert_temp(path_open_id, !path_open));
            }
            if path_open {
                self.path_form(ui, palette, path_open_id, path_draft_id);
            }
        }
        if self.figure_insertion_blocked().is_none() {
            let figures: Vec<(String, String)> = self
                .artifacts
                .iter()
                .filter(|summary| summary.kind == designllm_contracts::ArtifactKind::Figure)
                .map(|summary| (summary.id.clone(), summary.title.clone()))
                .collect();
            for (id, title) in figures {
                if widgets::menu_item_verbatim(ui, palette, Some("ins-figure"), &title, false).clicked() {
                    self.invoke_ui("add.figure_link", json!({ "figure_id": id }));
                    ui.close();
                }
            }
        }
        widgets::menu_separator(ui, palette);
        if slides {
            if widgets::menu_item(ui, palette, Some("add-slide"), "슬라이드 추가", "Ctrl+M", false).clicked() {
                self.invoke_ui("add.slide", json!({}));
                ui.close();
            }
        } else {
            preset_menu(ui, palette, &mut |preset| {
                self.invoke_ui("add.page", json!({ "preset": preset }));
            });
        }
    }

    /// The path form under the path item: the path data over its view box, a button to place it, and the refusal of
    /// the last try in red. The menu closes once the path is sent. The text stays in the form until the host answers:
    /// `path_add_placed` clears it, and `path_add_refused` puts it back here, open, with the reason.
    fn path_form(&mut self, ui: &mut Ui, palette: &theme::Palette, open_id: egui::Id, draft_id: egui::Id) {
        // Taken out of the menu's memory for this frame and put back at its end: the text is moved, not cloned, each frame.
        let mut draft = ui.data_mut(|data| data.remove_temp::<PathDraft>(draft_id)).unwrap_or_else(PathDraft::new);
        let mut changed = false;
        let mut place = false;
        ui.add_space(4.0);
        let width = ui.available_width();
        changed |= widgets::edit_area(ui, palette, &mut draft.d, widgets::Field {
            hint: "M 0 0 L 24 24 Z",
            width,
            rows: 4,
            ..Default::default()
        })
        .changed();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let width = ui.available_width() - 80.0;
            changed |= widgets::edit_line(ui, palette, &mut draft.view_box, widgets::Field { width, ..Default::default() }).changed();
            place = widgets::labeled_button(ui, palette, None, "넣기", true).clicked();
        });
        if changed {
            draft.error = None;
        }
        if place {
            match self.invoke("add.path", json!({ "d": draft.d, "view_box": draft.view_box })) {
                Ok(()) => {
                    ui.data_mut(|data| data.insert_temp(open_id, false));
                    ui.close();
                }
                Err(message) => draft.error = Some(message),
            }
        }
        if let Some(error) = &draft.error {
            ui.add_space(4.0);
            ui.colored_label(palette.danger, error.as_str());
        }
        ui.data_mut(|data| data.insert_temp(draft_id, draft));
    }
}

/// The path form's memory between frames: where the draft of the document `artifact_id` is. Each document keeps its own.
pub(super) fn path_form_draft_id(artifact_id: &str) -> egui::Id {
    egui::Id::new(("path-form-draft", artifact_id))
}

/// The path form's memory between frames: whether it is open under the path item of the document `artifact_id`.
pub(super) fn path_form_open_id(artifact_id: &str) -> egui::Id {
    egui::Id::new(("path-form-open", artifact_id))
}

/// The host placed the path sent with this text to the document `artifact_id`: its form forgets the text, unless the user
/// has typed since.
pub(super) fn path_add_placed(ctx: &Context, artifact_id: &str, d: &str, view_box: &str) {
    let draft_id = path_form_draft_id(artifact_id);
    ctx.data_mut(|data| {
        let Some(draft) = data.remove_temp::<PathDraft>(draft_id) else { return };
        if draft.d != d || draft.view_box != view_box {
            data.insert_temp(draft_id, draft);
        }
    });
}

/// The host refused the path sent with this text to the document `artifact_id`: the text goes back into that document's
/// form, open under the path item, with the reason. If the user has typed since, their text stays and the reason only
/// reaches the status line.
pub(super) fn path_add_refused(ctx: &Context, artifact_id: &str, d: &str, view_box: &str, reason: &str) {
    let draft_id = path_form_draft_id(artifact_id);
    ctx.data_mut(|data| {
        let mut draft = data
            .remove_temp::<PathDraft>(draft_id)
            .unwrap_or_else(|| PathDraft { d: d.to_owned(), view_box: view_box.to_owned(), error: None });
        if draft.d == d && draft.view_box == view_box {
            draft.error = Some(reason.to_owned());
            data.insert_temp(path_form_open_id(artifact_id), true);
        }
        data.insert_temp(draft_id, draft);
    });
}

/// The path form's entries and its last refusal, kept in the menu's temporary memory between frames.
#[derive(Clone)]
pub(super) struct PathDraft {
    pub(super) d: String,
    pub(super) view_box: String,
    pub(super) error: Option<String>,
}

impl PathDraft {
    pub(super) fn new() -> Self {
        Self { d: String::new(), view_box: "0 0 24 24".into(), error: None }
    }
}

impl Default for PathDraft {
    fn default() -> Self {
        Self::new()
    }
}

impl DesktopApp {
    /// The bar for a paper figure: labelled, because these are not picture tools (§8): diagram
    /// panel, chart, evidence and the table of the figure.
    /// The bar for a paper figure (figure-board): select · hand │ chart · diagram (labelled) │ box · circle ·
    /// connector │ evidence (labelled).
    fn draw_figure_tool_group(&mut self, ctx: &Context, palette: &theme::Palette, board: Rect) {
        let view = self.view_ref();
        let panels_ok = self.feature_edit_supported(Feature::FigurePanels);
        let diagram_ok = self.feature_edit_supported(Feature::Diagram);
        let evidence_ok = self.feature_edit_supported(Feature::Evidence);
        let has_diagram = self
            .figure_document()
            .is_some_and(|figure| figure.panels.iter().any(|panel| matches!(panel.body, designllm_contracts::PanelBody::Diagram(_))));
        float_bar(ctx, palette, "figure-tool-group", board, FloatPlace::BottomCenter, Bar::Tools, |ui| {
            if widgets::icon_button(ui, palette, "tool-select", "선택  V", view.tool == Tool::Select).clicked() {
                self.view().tool = Tool::Select;
            }
            if widgets::icon_button(ui, palette, "tool-hand", "손  H", view.tool == Tool::Hand).clicked() {
                self.view().tool = Tool::Hand;
            }
            widgets::group_separator(ui, palette);
            if panels_ok && widgets::labeled_button(ui, palette, Some("ins-chart"), "차트", false).clicked() {
                self.invoke_ui("add.chart", json!({}));
            }
            if panels_ok && widgets::labeled_button(ui, palette, Some("ins-diagram"), "구조도", false).clicked() {
                self.invoke_ui("add.figure_panel", json!({}));
            }
            if panels_ok && widgets::labeled_button(ui, palette, Some("ins-image"), "이미지", false).clicked() {
                self.invoke_ui("add.figure_image", json!({}));
            }
            if diagram_ok && has_diagram {
                widgets::group_separator(ui, palette);
                if widgets::icon_button(ui, palette, "tool-rect", "상자", false).clicked() {
                    self.invoke_ui("add.diagram_node", json!({ "kind": "rect" }));
                }
                if widgets::icon_button(ui, palette, "tool-oval", "원", false).clicked() {
                    self.invoke_ui("add.diagram_node", json!({ "kind": "oval" }));
                }
                if widgets::icon_button(ui, palette, "tool-connector", &tooltip_for("add.connector"), false).clicked() {
                    self.invoke_ui("add.connector", json!({}));
                }
            }
            if evidence_ok {
                widgets::group_separator(ui, palette);
                if widgets::labeled_button(ui, palette, Some("add-evidence"), "근거", false).clicked() {
                    self.invoke_ui("add.evidence", json!({}));
                }
            }
        });
    }
}

impl DesktopApp {
    /// The bar under a text document (docx-page): paragraph · table · picture · paper figure, all labelled. The table
    /// waits for its command (`add.table`).
    fn draw_document_tool_group(&mut self, ctx: &Context, palette: &theme::Palette, board: Rect) {
        let images_ok = self.feature_write_supported(Feature::Images);
        float_bar(ctx, palette, "document-tool-group", board, FloatPlace::BottomCenter, Bar::Tools, |ui| {
            if widgets::labeled_button(ui, palette, Some("ins-paragraph"), "문단", false).clicked() {
                self.invoke_ui("add.paragraph", json!({}));
            }
            if commands::available("add.table") && widgets::labeled_button(ui, palette, Some("ins-table"), "표", false).clicked() {
                self.invoke_ui("add.table", json!({}));
            }
            if images_ok && widgets::labeled_button(ui, palette, Some("ins-image"), "이미지", false).clicked() {
                self.invoke_ui("add.image", json!({}));
            }
            self.draw_figure_insertion(ui);
        });
    }
}
