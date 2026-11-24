//! The editor frame (GUI_DESIGN §4, §8, §11): the top bar, the left sidebar (layers and AI tabs),
//! the properties panel, the board and the floating groups on it. Every control here invokes a
//! registered command (`DesktopApp::invoke`); nothing in this file calls the engine directly.

use designllm_contracts::{Artifact, ArtifactBody, ArtifactKind, JobStatus};
use eframe::egui::{
    self, Align, Area, Context, CornerRadius, Frame, Layout, Margin, Order, Pos2, Rect, Response, Sense, Stroke,
    TopBottomPanel, Ui, Vec2,
};
use serde_json::json;

use super::{board::LeftTab, print_range, DesktopApp};
use crate::{
    commands, frame, icons, notice::Severity, theme,
    widgets::{self, AgentState},
};

/// Where each part of the window ended up in one frame (read by the layout tests).
#[cfg_attr(not(test), allow(dead_code))]
pub(super) struct FrameLayout {
    pub top: Rect,
    pub left: Option<Rect>,
    pub right: Option<Rect>,
    pub center: Rect,
}

/// The icon for an artifact kind (§4 top bar, §3 list).
pub(super) fn kind_icon(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Canvas => "kind-canvas",
        ArtifactKind::Slides => "kind-slides",
        ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp => "kind-document",
        ArtifactKind::Figure => "kind-figure",
        ArtifactKind::WebCode => "kind-web",
    }
}

/// The file extension shown after a title for documents that come from a file (§4).
pub(super) fn origin_extension(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Docx => ".docx",
        ArtifactKind::Hwpx => ".hwpx",
        ArtifactKind::Hwp => ".hwp",
        ArtifactKind::Slides => ".pptx",
        _ => "",
    }
}

/// Whether the document is kept as its own file, so that it has a save button (§4, §12).
pub(super) fn is_file_backed(kind: ArtifactKind) -> bool {
    matches!(kind, ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp | ArtifactKind::Slides)
}

/// The agent letter and colour slot for a runtime name (§5.5). Claude is `C`, Codex is `X`.
pub(super) fn agent_letter(runtime: &str) -> &'static str {
    if runtime == "codex" { "X" } else { "C" }
}

/// The state word of an agent mark in a tooltip (§5.5 states).
fn agent_state_word(state: AgentState) -> &'static str {
    match state {
        AgentState::Waiting => "대기",
        AgentState::Running => "실행 중",
        AgentState::Review => "검토 중",
        AgentState::Done => "완료",
        AgentState::Failed => "실패",
    }
}

/// Whether the host still runs a job of this status (queued, running or validating).
pub(super) fn job_status_running(status: JobStatus) -> bool {
    matches!(status, JobStatus::Queued | JobStatus::Running | JobStatus::Validating)
}

pub(super) fn job_agent_state(status: JobStatus) -> AgentState {
    match status {
        JobStatus::Queued => AgentState::Waiting,
        JobStatus::Running => AgentState::Running,
        JobStatus::Validating => AgentState::Review,
        JobStatus::Completed | JobStatus::Canceled => AgentState::Done,
        JobStatus::Failed | JobStatus::Conflict => AgentState::Failed,
    }
}

impl DesktopApp {
    /// Draws the editor. Panels are added top, left, right, then the board last, so that the board
    /// gets what is left. A sidebar that would squeeze the board below 640 px floats over it.
    pub(super) fn draw_frame(&mut self, ctx: &Context) -> FrameLayout {
        // A runner model draft whose menu is no longer drawn, or the window's close, saves it (home.rs).
        self.settle_runner_drafts(ctx);
        let palette = theme::palette(ctx);
        let screen = ctx.content_rect();
        // A document opened in a window too narrow to dock the sidebar starts with it closed (narrow-800): it
        // would cover the board; the page picker stands in for it.
        if !self.view_ref().panels_placed && self.artifact.is_some() {
            let first = frame::editor_frame(screen, true, self.view_ref().left_width, true, self.view_ref().right_width);
            let view = self.view();
            view.panels_placed = true;
            if !first.left_docked {
                view.left_open = false;
            }
        }
        let view = self.view_ref();
        let geometry = frame::editor_frame(screen, view.left_open, view.left_width, view.right_open, view.right_width);
        self.draw_top_bar(ctx, &palette);
        let mut left_rect = None;
        if view.left_open && geometry.left_docked {
            let shown = egui::SidePanel::left("sidebar")
                .resizable(true)
                .show_separator_line(false)
                .width_range(theme::SIDEBAR_MIN..=theme::SIDEBAR_MAX)
                .default_width(view.left_width)
                .frame(Frame::new().fill(palette.surface))
                .show(ctx, |ui| self.draw_left_sidebar(ui, &palette));
            // Only the right edge has a line (tokens `layout.sidebar.border`).
            let rect = shown.response.rect;
            ctx.layer_painter(egui::LayerId::background()).vline(rect.right() - 0.5, rect.y_range(), Stroke::new(theme::hairline(ctx), palette.line));
            left_rect = Some(shown.response.rect);
            self.view().left_width = shown.response.rect.width();
        }
        let mut right_rect = None;
        if view.right_open {
            let max_width = (screen.width() - frame::BOARD_MIN_WITH_ONE).max(theme::PANEL_MIN);
            let shown = egui::SidePanel::right("properties")
                .resizable(true)
                .show_separator_line(false)
                .width_range(theme::PANEL_MIN..=max_width.min(theme::PANEL_MAX))
                .default_width(view.right_width.min(max_width))
                .frame(Frame::new().fill(palette.surface))
                .show(ctx, |ui| {
                    // The panel has no head row of its own: one as tall as the sidebar's tab row holds its close button.
                    let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), theme::SIDEBAR_TABS_HEIGHT), Sense::hover());
                    self.panel_close_button(ui, &palette, row, "panel-right", "nav.toggle_right");
                    self.draw_properties(ui, &palette)
                });
            // Only the left edge has a line (tokens `layout.panel.border`).
            let rect = shown.response.rect;
            ctx.layer_painter(egui::LayerId::background()).vline(rect.left() + 0.5, rect.y_range(), Stroke::new(theme::hairline(ctx), palette.line));
            right_rect = Some(shown.response.rect);
            self.view().right_width = shown.response.rect.width();
        }
        let center = egui::CentralPanel::default()
            .frame(Frame::NONE.fill(palette.board))
            .show(ctx, |ui| {
                // A selected chart panel puts its data sheet under the board (figure-board); the floating groups stay
                // on the board part.
                let (board, sheet) = self.split_for_sheet(ctx, ui.max_rect());
                let mut board_ui = ui.new_child(egui::UiBuilder::new().max_rect(board));
                self.draw_board(ctx, &mut board_ui);
                if let Some(sheet) = sheet {
                    let mut sheet_ui = ui.new_child(egui::UiBuilder::new().max_rect(sheet));
                    self.draw_data_sheet(&mut sheet_ui, &palette, sheet);
                }
                board
            })
            .inner;
        if view.left_open && !geometry.left_docked {
            // Over the board, not beside it: the board keeps its full width.
            let rect = geometry.left.unwrap_or(center);
            let rect = Rect::from_min_size(Pos2::new(rect.left(), rect.top()), rect.size());
            Area::new(egui::Id::new("sidebar-overlay"))
                .order(Order::Foreground)
                .fixed_pos(rect.min)
                .show(ctx, |ui| {
                    Frame::new()
                        .fill(palette.surface)
                        .stroke(Stroke::new(1.0, palette.line))
                        .corner_radius(CornerRadius::same(theme::RADIUS_FLOAT))
                        .shadow(theme::float_shadow()[1])
                        .show(ui, |ui| {
                            ui.set_min_size(rect.size());
                            ui.set_max_size(rect.size());
                            self.draw_left_sidebar(ui, &palette);
                        });
                });
            left_rect = Some(rect);
        }
        self.draw_floating_groups(ctx, &palette, center);
        FrameLayout { top: geometry.top, left: left_rect, right: right_rect, center }
    }

    fn draw_top_bar(&mut self, ctx: &Context, palette: &theme::Palette) {
        let editor = self.screen_is_editor();
        // tokens `layout.top-bar`: 44 high including its 1 px bottom line (y = 43); no other edge has a line.
        let padding = if editor { Margin::symmetric(8, 0) } else { Margin { left: 12, right: 8, top: 0, bottom: 0 } };
        let shown = TopBottomPanel::top("top-bar")
            .exact_height(theme::TOP_BAR_HEIGHT)
            .show_separator_line(false)
            .frame(Frame::new().fill(palette.surface).inner_margin(padding))
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = theme::GROUP_GAP;
                    if editor {
                        self.top_bar_editor(ui, palette);
                    } else {
                        self.top_bar_home(ui, palette);
                    }
                });
            });
        let rect = shown.response.rect;
        ctx.layer_painter(egui::LayerId::background()).hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(theme::hairline(ctx), palette.line));
    }

    fn top_bar_editor(&mut self, ui: &mut Ui, palette: &theme::Palette) {
        if self.icon(ui, palette, "home", "nav.home").clicked() {
            self.invoke_ui("nav.home", json!({}));
        }
        let title_response = self.title_button(ui, palette);
        if title_response.clicked() {
            self.recent_open = true;
        }
        egui::Popup::from_toggle_button_response(&title_response)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_width(280.0);
                let current = self.active.as_ref().map(|item| item.id.clone());
                let items: Vec<(String, String, ArtifactKind)> = self.artifacts.iter().map(|item| (item.id.clone(), item.title.clone(), item.kind)).collect();
                for (id, title, kind) in items {
                    let checked = current.as_deref() == Some(id.as_str());
                    if widgets::menu_item(ui, palette, Some(kind_icon(kind)), &title, "", checked).clicked() {
                        self.invoke_ui("nav.open_project", json!({ "artifact_id": id }));
                        ui.close();
                    }
                }
            });
        let ready = self.context.is_some();
        if self.icon(ui, palette, "undo", "edit.undo").clicked() && ready {
            self.invoke_ui("edit.undo", json!({}));
        }
        if self.icon(ui, palette, "redo", "edit.redo").clicked() && ready {
            self.invoke_ui("edit.redo", json!({}));
        }
        if let Some(status) = self.agent_status(std::time::Instant::now()) {
            ui.label(egui::RichText::new(status).color(palette.ink_3));
        }
        // The optional agent status must not change the action buttons' popup IDs.
        ui.scope_builder(egui::UiBuilder::new().id("editor-top-actions").layout(Layout::right_to_left(Align::Center)), |ui| {
            ui.spacing_mut().item_spacing.x = theme::GROUP_GAP;
            // The button keeps `surface-2` while its popover is open (`label-button.open`); its id is the next auto id.
            let open = egui::Popup::is_id_open(ui.ctx(), ui.next_auto_id().with("popup"));
            let export = widgets::labeled_button_with(ui, palette, Some("export"), "내보내기", false, open, false);
            if export.clicked() {
                self.export_open = true;
            }
            if self.smoke_open.as_deref() == Some("export") {
                self.smoke_open = None;
                egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&export));
            }
            // tokens `export-popover.anchor` (lead r11): 4 below the top bar, right edges aligned with the button.
            let gap = (theme::TOP_BAR_HEIGHT + 4.0 - export.rect.bottom()).max(4.0);
            egui::Popup::from_toggle_button_response(&export)
                .align(egui::RectAlign::BOTTOM_END)
                .gap(gap)
                // tokens `export-popover.padding` 8 (other menus keep 4).
                .frame(Frame::popup(&ui.ctx().style()).inner_margin(Margin::same(8)))
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| self.export_menu(ui, palette));
            if let Some((kind, dirty)) = self.active.as_ref().map(|item| (item.kind, item.dirty)) {
                if is_file_backed(kind) && widgets::labeled_button_with(ui, palette, Some("save"), "저장", false, false, dirty).clicked() {
                    self.invoke_ui("file.save", json!({}));
                }
            }
            // Agents sit 10 before the buttons, 4 apart (tokens `layout.top-bar`).
            ui.add_space(10.0 - theme::GROUP_GAP);
            // The agent is always shown (waiting when idle, §5.5); its state follows the running job.
            let state = self.job.as_ref().map_or(AgentState::Waiting, |job| job_agent_state(job.status));
            // The letter is the runtime's mark, not an account: the tooltip says what the press does (GUI_DESIGN §5.5).
            let runtime = self.active_runtime();
            let tooltip = format!("AI 패널 열기 · {} {}", super::home::agent_name(&runtime, None), agent_state_word(state));
            let response = widgets::avatar(ui, palette, agent_letter(&runtime), palette.agent(0), state, 24.0).on_hover_text(tooltip);
            if response.clicked() {
                self.left_tab_to(LeftTab::Ai);
            }
        });
    }

    fn top_bar_home(&mut self, ui: &mut Ui, palette: &theme::Palette) {
        let app_icon = egui::Rect::from_min_size(ui.cursor().min + Vec2::new(4.0, 10.0), Vec2::splat(24.0));
        icons::paint(ui, app_icon, "app-24", egui::Color32::WHITE);
        ui.add_space(30.0);
        ui.label(egui::RichText::new(self.workspace_name.clone()).font(theme::display_font(true, 14.0)).color(palette.ink));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if widgets::labeled_button(ui, palette, Some("open"), "열기", false).clicked() {
                self.invoke_ui("nav.open_file", json!({}));
            }
        });
    }

    /// An icon-only button for the top bar, labelled with the command's name and shortcut.
    pub(super) fn icon(&mut self, ui: &mut Ui, palette: &theme::Palette, icon: &str, command: &str) -> Response {
        let tooltip = tooltip_for(command);
        widgets::icon_button(ui, palette, icon, &tooltip, false)
    }

    /// The document title as a button: kind icon, title, file extension and a chevron. Clicking
    /// it opens the list of other documents.
    fn title_button(&mut self, ui: &mut Ui, palette: &theme::Palette) -> Response {
        let (title, kind) = self
            .active
            .as_ref()
            .map(|item| (item.title.clone(), item.kind))
            .unwrap_or_else(|| (String::new(), ArtifactKind::Canvas));
        // tokens `component.title-button`: margin 2 | 8 icon 16, 6, title 14 Bold (+2 + extension 14 Regular ink-3), 6,
        // chevron 16, 6 | margin 6; at most 360 wide.
        let extension = origin_extension(kind);
        let title_font = theme::font(theme::Type::Title);
        let extension_font = theme::display_font(false, theme::Type::Title.metrics().0);
        let measure = |text: &str, font: &egui::FontId| ui.painter().layout_no_wrap(text.to_owned(), font.clone(), palette.ink).size().x;
        let extension_width = if extension.is_empty() { 0.0 } else { 2.0 + measure(extension, &extension_font) };
        let chrome = 8.0 + 16.0 + 6.0 + 6.0 + 16.0 + 6.0;
        let title_room = (360.0 - chrome - extension_width).max(40.0);
        let title_width = measure(&title, &title_font).min(title_room);
        ui.add_space(2.0);
        let (rect, response) = ui.allocate_exact_size(Vec2::new(chrome + title_width + extension_width, theme::ICON_BUTTON), Sense::click());
        ui.add_space(6.0);
        if response.hovered() {
            ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_2);
        }
        let icon_rect = Rect::from_min_size(Pos2::new(rect.left() + 8.0, rect.center().y - 8.0), Vec2::splat(16.0));
        icons::paint(ui, icon_rect, kind_icon(kind), palette.ink_2);
        let mut x = icon_rect.right() + 6.0;
        let shown = truncate_font(ui, &title, title_room, &title_font);
        ui.painter().text(Pos2::new(x, rect.center().y), egui::Align2::LEFT_CENTER, shown, title_font, palette.ink);
        x += title_width;
        if !extension.is_empty() {
            ui.painter().text(Pos2::new(x + 2.0, rect.center().y), egui::Align2::LEFT_CENTER, extension, extension_font, palette.ink_3);
        }
        let chevron = Rect::from_min_size(Pos2::new(rect.right() - 6.0 - 16.0, rect.center().y - 8.0), Vec2::splat(16.0));
        icons::paint(ui, chevron, "chev-down", palette.ink_2);
        response
    }

    /// The export menu: formats the kind can write, then the scope and the options (§12).
    /// tokens `component.export-popover`: 320 wide, the formats as menu items, then the scope, scale and
    /// background rows (label column 52; scale and background only for PNG), the loss list and the button.
    pub(super) fn export_menu(&mut self, ui: &mut Ui, palette: &theme::Palette) {
        // 320 outside: the popover frame adds its padding (8) and its 1 px line on each side.
        ui.set_width(theme::EXPORT_WIDTH - 2.0 * (8.0 + 1.0));
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        let kind = self.active.as_ref().map_or(ArtifactKind::Canvas, |item| item.kind);
        let formats = export_formats(kind);
        // Smoke captures only: `DESIGNLLM_SMOKE_EXPORT_FORMAT=png` shows the PNG rows (export-menu-png).
        if let Some(format) = self.smoke_output.as_ref().and_then(|_| std::env::var("DESIGNLLM_SMOKE_EXPORT_FORMAT").ok()) {
            if formats.iter().any(|(key, _, _)| *key == format) && !self.smoke_export_format_done {
                self.export_format = format;
                self.smoke_export_format_done = true;
            }
        }
        if !formats.iter().any(|(key, _, _)| *key == self.export_format) {
            self.export_format = formats[0].0.to_owned();
        }
        for (key, label, icon) in formats {
            let checked = self.export_format == *key;
            if widgets::menu_item(ui, palette, Some(icon), label, "", checked).clicked() {
                self.export_format = (*key).to_owned();
            }
        }
        widgets::menu_separator(ui, palette);
        let pages = self.page_sizes().len();
        let scopes = self.export_scopes();
        if !scopes.iter().any(|(key, _)| *key == self.export_scope) {
            self.export_scope = String::from("page");
        }
        if scopes.len() > 1 {
            let labels: Vec<&str> = scopes.iter().map(|(_, label)| *label).collect();
            let selected = scopes.iter().position(|(key, _)| *key == self.export_scope).unwrap_or(0);
            if let Some(index) = widgets::option_row(ui, palette, "범위", &labels, selected) {
                self.export_scope = scopes[index].0.to_owned();
            }
        }
        // `쪽 범위`: the page numbers typed here (kept for this document); a refused text is shown under the field and
        // nothing is sent until it is right.
        let mut pages_text = String::new();
        let mut range_error = None;
        if self.export_scope == "range" {
            let range_id = egui::Id::new(("export-page-range", self.active.as_ref().map_or("", |item| item.id.as_str())));
            let mut text = ui.data(|data| data.get_temp::<String>(range_id)).unwrap_or_default();
            let width = ui.available_width();
            ui.allocate_ui_with_layout(Vec2::new(width, 36.0), Layout::left_to_right(Align::Center), |ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                let (rect, _) = ui.allocate_exact_size(Vec2::new(52.0, 36.0), Sense::hover());
                ui.painter().text(Pos2::new(rect.left() + 8.0, rect.center().y), egui::Align2::LEFT_CENTER, "쪽", theme::font(theme::Type::Caption), palette.ink_2);
                widgets::text_field(ui, palette, &mut text, "예: 1, 3-5", width - 52.0);
            });
            ui.data_mut(|data| data.insert_temp(range_id, text.clone()));
            range_error = print_range::parse_page_range(&text, pages).err();
            if let Some(error) = &range_error {
                ui.label(egui::RichText::new(error.to_string()).font(theme::font(theme::Type::Caption)).color(palette.danger));
            }
            pages_text = text;
        }
        if self.export_format == "png" {
            if kind == ArtifactKind::Figure {
                let choices = [150, 300, 600];
                let current = choices.iter().position(|dpi| *dpi == self.view_ref().figure_export_dpi).unwrap_or(usize::MAX);
                if let Some(index) = widgets::option_row(ui, palette, "DPI", &["150", "300", "600"], current) { self.view().figure_export_dpi = choices[index]; }
                if current == usize::MAX { ui.label(format!("{} DPI", self.view_ref().figure_export_dpi)); }
                if let Some(Artifact { body: ArtifactBody::Figure(figure), .. }) = self.artifact.as_ref() {
                    if let Ok((width, height)) = designllm_contracts::figure_rules::figure_png_pixels(figure.size, self.view_ref().figure_export_dpi) {
                        ui.label(format!("예상 크기: {width} × {height} px"));
                    }
                }
            } else {
                let current = (self.export_scale as usize).saturating_sub(1).min(2);
                if let Some(index) = widgets::option_row(ui, palette, "배율", &["1×", "2×", "3×"], current) { self.export_scale = index as u32 + 1; }
            }
            if let Some(index) = widgets::option_row(ui, palette, "배경", &["페이지 색", "투명"], usize::from(self.export_transparent)) {
                self.export_transparent = index == 1;
            }
        }
        self.preview_export_losses(&pages_text);
        let key = self.export_key(&pages_text);
        let losses: Vec<designllm_contracts::Diagnostic> = self
            .export_preview
            .as_ref()
            .filter(|(seen, _)| *seen == key)
            .map(|(_, losses)| losses.clone())
            .unwrap_or_default();
        if !losses.is_empty() {
            ui.add_space(8.0);
            widgets::loss_list(ui, palette, &losses);
        }
        ui.add_space(8.0);
        let label = if losses.is_empty() { "내보내기…" } else { "손실 허용하고 내보내기" };
        let clicked = ui
            .add_enabled_ui(range_error.is_none(), |ui| widgets::wide_primary(ui, palette, Some("export"), label, !losses.is_empty()).clicked())
            .inner;
        if clicked {
            let args = self.popover_export_args(&pages_text);
            ui.close();
            self.invoke_ui("file.export", args);
        }
    }

    /// The left sidebar: a tab row (layers, AI) and the chosen tab.
    pub(super) fn draw_left_sidebar(&mut self, ui: &mut Ui, palette: &theme::Palette) {
        // tokens `layout.sidebar.tabs` and `component.sidebar-tab`: a 40 px row, padding 8, pill tabs 2 apart,
        // and a 1 px line under it. The first tab is the kind's own (layers, outline or files).
        // The toolbox tab exists for canvases and slides only (68g).
        let toolbox = self.toolbox_shown();
        let tab = self.shown_left_tab();
        let first = match self.active.as_ref().map(|item| item.kind) {
            Some(ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp) => "목차",
            Some(ArtifactKind::WebCode) => "파일",
            _ => "레이어",
        };
        let mut tabs = vec![(first, LeftTab::Layers)];
        if toolbox {
            tabs.push(("도구", LeftTab::Tools));
        }
        tabs.push(("AI", LeftTab::Ai));
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        let width = ui.available_width();
        let (row, _) = ui.allocate_exact_size(Vec2::new(width, theme::SIDEBAR_TABS_HEIGHT), Sense::hover());
        ui.painter().hline(row.x_range(), row.bottom() - 0.5, Stroke::new(theme::hairline(ui.ctx()), palette.line));
        let mut strip = ui.new_child(egui::UiBuilder::new().max_rect(row.shrink2(Vec2::new(8.0, 0.0))).layout(Layout::left_to_right(Align::Center)));
        strip.spacing_mut().item_spacing = Vec2::new(theme::GROUP_GAP, 0.0);
        for (label, value) in tabs {
            // An agent at work: a running job, or nodes an agent holds (reservations).
            let working = self.job_running() || !self.reservations.is_empty();
            let dot = (value == LeftTab::Ai && working && tab != LeftTab::Ai).then(|| palette.agent(0));
            if widgets::tab(&mut strip, palette, label, tab == value, dot).clicked() {
                self.left_tab_to(value);
            }
        }
        self.panel_close_button(ui, palette, row, "panel-left", "nav.toggle_left");
        match tab {
            LeftTab::Layers => {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    egui::Frame::new().inner_margin(Margin::same(theme::TREE_PADDING as i8)).show(ui, |ui| {
                        ui.spacing_mut().item_spacing = Vec2::ZERO;
                        self.draw_layers(ui, palette);
                    });
                });
            }
            LeftTab::Tools => {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    egui::Frame::new().inner_margin(super::toolbox::MARGIN).show(ui, |ui| {
                        self.draw_toolbox(ui, palette);
                    });
                });
            }
            LeftTab::Ai => {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.draw_ai_tab(ui, palette));
            }
        }
    }

    /// A panel's own close button at the right end of its head row `row`: the same icon (shown on) and command as the view
    /// group's button for that panel.
    fn panel_close_button(&mut self, ui: &mut Ui, palette: &theme::Palette, row: Rect, icon: &str, command: &str) {
        let close = Rect::from_center_size(Pos2::new(row.right() - 8.0 - theme::ICON_BUTTON / 2.0, row.center().y), Vec2::splat(theme::ICON_BUTTON));
        let mut close_ui = ui.new_child(egui::UiBuilder::new().max_rect(close).layout(Layout::left_to_right(Align::Center)));
        if widgets::icon_button(&mut close_ui, palette, icon, &tooltip_for(command), true).clicked() {
            self.invoke_ui(command, json!({}));
        }
    }

    pub(super) fn left_tab_to(&mut self, tab: LeftTab) {
        let view = self.view();
        view.left_tab = tab;
        view.left_open = true;
    }

    pub(super) fn job_running(&self) -> bool {
        self.job.as_ref().is_some_and(|job| job_status_running(job.status))
    }

    /// The floating groups on the board: view controls (top right), the page selector (top left,
    /// when the sidebar is closed), and the tool bar (bottom centre).
    fn draw_floating_groups(&mut self, ctx: &Context, palette: &theme::Palette, board_rect: Rect) {
        if self.artifact.is_none() || !self.screen_is_editor() {
            return;
        }
        let view = self.view_ref();
        let zoom_label = format!("{:.0}%", view.zoom * 100.0);
        // ▶ plays a canvas (its HTML in the preview window, D09); slides have no HTML export to play yet.
        let playable = commands::available("view.play") && self.active.as_ref().is_some_and(|item| item.kind == ArtifactKind::Canvas);
        let group = float_bar(ctx, palette, "view-group", board_rect, FloatPlace::TopRight, Bar::View, |ui| {
            if playable {
                // Without a preview engine the button stays, off, and says why (web_runtime.rs).
                let reason = self.web_runtime_unavailable.clone();
                let mut play = ui.add_enabled_ui(reason.is_none(), |ui| widgets::icon_button(ui, palette, "play", &tooltip_for("view.play"), false)).inner;
                if let Some(reason) = reason {
                    play = play.on_disabled_hover_text(reason);
                }
                if play.clicked() {
                    self.invoke_ui("view.play", json!({}));
                }
            }
            if widgets::icon_button(ui, palette, "panel-left", &tooltip_for("nav.toggle_left"), view.left_open).clicked() {
                self.invoke_ui("nav.toggle_left", json!({}));
            }
            if widgets::icon_button(ui, palette, "panel-right", &tooltip_for("nav.toggle_right"), view.right_open).clicked() {
                self.invoke_ui("nav.toggle_right", json!({}));
            }
            let open = egui::Popup::is_id_open(ui.ctx(), egui::Id::new("zoom-menu"));
            let zoom = widgets::zoom_button(ui, palette, &zoom_label, open);
            egui::Popup::from_toggle_button_response(&zoom)
                .id(egui::Id::new("zoom-menu"))
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| {
                    ui.set_width(220.0);
                    for (command, label, shortcut) in [
                        ("view.zoom_in", "확대", "Ctrl+="),
                        ("view.zoom_out", "축소", "Ctrl+-"),
                        ("view.zoom_100", "100%", "Ctrl+0"),
                        ("view.zoom_fit", "전체 맞춤", "Shift+1"),
                        ("view.zoom_selection", "선택 맞춤", "Shift+2"),
                    ] {
                        // The judgement the run (`invoke`) and the registry listing use, so a disabled zoom reads as disabled.
                        let enabled = self.command_enabled_for_current_kind(command);
                        let clicked = ui.add_enabled_ui(enabled, |ui| widgets::menu_item(ui, palette, None, label, shortcut, false).clicked()).inner;
                        if clicked {
                            self.invoke_ui(command, json!({}));
                            ui.close();
                        }
                    }
                });
        });
        self.view_group_rect = Some(group);
        if !view.left_open && matches!(self.active.as_ref().map(|item| item.kind), Some(ArtifactKind::Canvas | ArtifactKind::Slides)) {
            let title = self.page_title(view.page_index);
            float_bar(ctx, palette, "page-selector", board_rect, FloatPlace::TopLeft, Bar::View, |ui| {
                let open = egui::Popup::is_id_open(ui.ctx(), egui::Id::new("page-menu"));
                let button = widgets::zoom_button(ui, palette, &title, open);
                egui::Popup::from_toggle_button_response(&button)
                    .id(egui::Id::new("page-menu"))
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show(|ui| {
                        ui.set_width(240.0);
                        let slides = self.active.as_ref().is_some_and(|item| item.kind == ArtifactKind::Slides);
                        let count = self.page_sizes().len() as u32;
                        for index in 0..count {
                            let label = self.page_title(index);
                            let icon = if slides { "layer-slide" } else { "layer-page" };
                            if widgets::menu_item(ui, palette, Some(icon), &label, "", index == view.page_index).clicked() {
                                self.invoke_ui("nav.page", json!({ "page_index": index }));
                                ui.close();
                            }
                        }
                        widgets::menu_separator(ui, palette);
                        let (icon, label, command) = if slides { ("add-slide", "슬라이드 추가", "add.slide") } else { ("page-add", "페이지 추가", "add.page") };
                        if widgets::menu_item(ui, palette, Some(icon), label, "", false).clicked() {
                            self.invoke_ui(command, json!({}));
                            ui.close();
                        }
                    });
            });
        }
        self.draw_tool_group(ctx, palette, board_rect);
    }

}

/// The memory key of the bottom bar's rectangle, stored by `float_bar` with the pass that drew it.
const BOTTOM_BAR: &str = "bottom-tool-bar";
/// The gap between a callout and the bottom bar it stands above.
const CALLOUT_ABOVE_BAR: f32 = 8.0;

/// A failure the screen cannot show elsewhere, as one callout at the bottom centre of the window. While a bottom bar is drawn it
/// stands above that bar, so the two do not cover each other.
impl DesktopApp {
    pub(super) fn draw_error_callout(&mut self, ctx: &Context) {
        let notice = self.notices.current(std::time::Instant::now()).cloned();
        if notice.is_none() && self.agent_follow.notices.is_empty() {
            return;
        }
        let palette = theme::palette(ctx);
        let screen = ctx.content_rect();
        let pass = ctx.cumulative_pass_nr();
        let bottom = ctx
            .data(|data| data.get_temp::<(u64, Rect)>(egui::Id::new(BOTTOM_BAR)))
            .filter(|(drawn_in, _)| *drawn_in == pass)
            .map_or(screen.bottom() - 16.0, |(_, bar)| bar.top() - CALLOUT_ABOVE_BAR);
        let requires_acknowledgement = notice.as_ref().is_some_and(|notice| notice.requires_acknowledgement());
        let agent_notice = !self.agent_follow.notices.is_empty()
            && notice.as_ref().is_none_or(|notice| notice.text == super::messages::AGENT_FOLLOW_NOTICE || notice.text == super::messages::AGENT_FOLLOW_BUSY);
        let buttons = if requires_acknowledgement { &["확인"][..] } else if agent_notice { &["보기"][..] } else { &[] };
        let agent_text = self.agent_follow.notices.front().map(|id| {
            let title = self.artifacts.iter().find(|artifact| &artifact.id == id).map_or(id.as_str(), |artifact| artifact.title.as_str());
            super::messages::agent_follow_notice(title)
        });
        let text = if agent_notice && notice.as_ref().is_none_or(|notice| notice.text != super::messages::AGENT_FOLLOW_BUSY) {
            agent_text.as_deref().unwrap_or(super::messages::AGENT_FOLLOW_NOTICE)
        } else {
            notice.as_ref().map_or(super::messages::AGENT_FOLLOW_NOTICE, |notice| notice.text.as_str())
        };
        let warning = notice.as_ref().is_some_and(|notice| notice.severity == Severity::Warning);
        Area::new(egui::Id::new("error-callout"))
            .order(Order::Foreground)
            .pivot(egui::Align2::CENTER_BOTTOM)
            .fixed_pos(Pos2::new(screen.center().x, bottom))
            .show(ctx, |ui| {
                let clicked = widgets::callout(ui, &palette, text, buttons, warning);
                if requires_acknowledgement && clicked == Some(0) {
                    self.acknowledge_workspace_notice();
                } else if agent_notice && clicked == Some(0) {
                    self.open_agent_notice();
                }
            });
    }
}

/// Where a floating group sits on the board (tokens `layout.board`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FloatPlace {
    /// 12 from the right and top edges.
    TopRight,
    /// 12 from the left and top edges.
    TopLeft,
    /// Centred, 16 above the bottom edge.
    BottomCenter,
}

/// The two kinds of floating group: the view group (36 high, padding 1, radius 10) and the tool bar
/// (42 high, padding 4, radius 12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Bar {
    View,
    Tools,
}

/// A floating group: `surface`, 1 px `line`, both float shadow layers, contents in one row 2 apart.
/// It is placed by its own size (a pivot), so it keeps its distance to the board edges whatever it holds.
/// Returns its rectangle on the screen.
pub(super) fn float_bar(ctx: &Context, palette: &theme::Palette, id: &str, board: Rect, place: FloatPlace, bar: Bar, add: impl FnOnce(&mut Ui)) -> Rect {
    let (pivot, position) = match place {
        FloatPlace::TopRight => (egui::Align2::RIGHT_TOP, Pos2::new(board.right() - theme::FLOAT_INSET, board.top() + theme::FLOAT_INSET)),
        FloatPlace::TopLeft => (egui::Align2::LEFT_TOP, Pos2::new(board.left() + theme::FLOAT_INSET, board.top() + theme::FLOAT_INSET)),
        FloatPlace::BottomCenter => (egui::Align2::CENTER_BOTTOM, Pos2::new(board.center().x, board.bottom() - theme::TOOLS_BOTTOM)),
    };
    let (padding, radius) = match bar {
        Bar::View => (theme::VIEW_GROUP_PADDING, theme::RADIUS_FLOAT),
        Bar::Tools => (theme::TOOLS_PADDING, theme::RADIUS_TOOLBAR),
    };
    let corner = CornerRadius::same(radius);
    let shown = Area::new(egui::Id::new(id))
        .order(Order::Foreground)
        .pivot(pivot)
        .fixed_pos(position)
        .constrain(false)
        .show(ctx, |ui| {
            Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.line))
                .corner_radius(corner)
                .inner_margin(Margin::same(padding as i8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(theme::GROUP_GAP, 0.0);
                        add(ui);
                    });
                })
                .response
                .rect
        });
    let rect = shown.inner;
    if place == FloatPlace::BottomCenter {
        let pass = ctx.cumulative_pass_nr();
        ctx.data_mut(|data| data.insert_temp(egui::Id::new(BOTTOM_BAR), (pass, rect)));
    }
    // A frame takes one shadow; both layers go under it on the layer below.
    let painter = ctx.layer_painter(egui::LayerId::new(Order::Middle, egui::Id::new(id).with("shadow")));
    theme::paint_shadow(&painter, rect, corner, theme::Elevation::Float);
    rect
}

/// Cuts `text` with an ellipsis so that it fits `width` in `font`.
pub(super) fn truncate_font(ui: &Ui, text: &str, width: f32, font: &egui::FontId) -> String {
    let measure = |candidate: &str| ui.painter().layout_no_wrap(candidate.to_owned(), font.clone(), egui::Color32::WHITE).size().x;
    if measure(text) <= width {
        return text.to_owned();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let candidate = chars.iter().collect::<String>() + "…";
        if measure(&candidate) <= width {
            return candidate;
        }
    }
    String::new()
}

/// The hover text of a command: its name and its shortcut (§7 L2).
pub(super) fn tooltip_for(command: &str) -> String {
    match commands::find(command) {
        Some(spec) if spec.shortcut.is_empty() => spec.label.to_owned(),
        Some(spec) => format!("{}  {}", spec.label, spec.shortcut),
        None => command.to_owned(),
    }
}

/// The formats a document kind writes, in menu order (key, label, icon). JPEG is not listed: no engine writes it yet.
pub(super) fn export_formats(kind: ArtifactKind) -> &'static [(&'static str, &'static str, &'static str)] {
    match kind {
        ArtifactKind::Slides => &[("pptx", "PPTX", "kind-slides"), ("pdf", "PDF", "fmt-document"), ("png", "PNG", "fmt-raster"), ("svg", "SVG", "fmt-vector")],
        ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp => &[("pdf", "PDF", "fmt-document")],
        ArtifactKind::WebCode => &[("html", "HTML", "fmt-web"), ("zip", "ZIP", "fmt-web")],
        ArtifactKind::Figure => &[("svg", "SVG", "fmt-vector"), ("png", "PNG", "fmt-raster"), ("pdf", "PDF", "fmt-document"), ("pptx", "PPTX", "kind-slides")],
        _ => &[("png", "PNG", "fmt-raster"), ("svg", "SVG", "fmt-vector"), ("pdf", "PDF", "fmt-document"), ("html", "HTML", "fmt-web")],
    }
}

impl DesktopApp {
    /// 범위 of the export popover: this page · the selection (PNG and SVG of the selected nodes) · every page · the named pages
    /// (formats with pages).
    pub(super) fn export_scopes(&self) -> Vec<(&'static str, &'static str)> {
        let kind = self.active.as_ref().map_or(ArtifactKind::Canvas, |item| item.kind);
        let pages = self.page_sizes().len();
        let single_page = matches!(self.export_format.as_str(), "png" | "svg");
        let mut scopes: Vec<(&'static str, &'static str)> = vec![("page", "이 페이지")];
        // Only the canvas engine writes some nodes of a page (`EngineAdapter::exports_nodes`); a deck writes whole pages.
        if single_page && !self.selected_nodes.is_empty() && kind == ArtifactKind::Canvas {
            scopes.push(("selection", "선택"));
        }
        if pages > 1 && !single_page {
            scopes.push(("all", "모든 페이지"));
            if print_range::range_supported(kind, &self.export_format) {
                scopes.push(("range", "쪽 범위"));
            }
        }
        scopes
    }

    pub(super) fn export_format_supported(&self, format: &str) -> bool {
        let kind = self.active.as_ref().map_or(ArtifactKind::Canvas, |item| item.kind);
        let format = format.trim().to_ascii_lowercase();
        export_formats(kind).iter().any(|(key, _, _)| *key == format)
    }
}

impl DesktopApp {
    /// The runtime the agent avatar speaks for: the running job's runtime, else the configured one.
    pub(super) fn active_runtime(&self) -> String {
        match self.job.as_ref().map(|job| job.runtime.as_str()) {
            Some(runtime) if !runtime.is_empty() => runtime.to_owned(),
            _ => self.ai_runtime.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use eframe::egui::{Id, Rect};

    use crate::app::test_support::{canvas_with_page, screen, Host, Window};

    /// The rectangles of the callout and of the bottom bar `bar`, as the last frame drew them.
    fn callout_and_bar(window: &Window, bar: &str) -> (Rect, Rect) {
        let (callout, bar) = window.ctx.memory(|memory| (memory.area_rect(Id::new("error-callout")), memory.area_rect(Id::new(bar))));
        (callout.expect("the callout is drawn"), bar.expect("the bottom bar is drawn"))
    }

    /// A callout on a canvas and on a figure stands above the bottom bar: the two rectangles do not meet, so the bar's buttons
    /// stay pressable while the callout shows.
    #[test]
    fn the_callout_stands_above_the_bottom_bar_of_a_canvas_and_of_a_figure() {
        let mut host = Host::start();
        let canvas = canvas_with_page(&mut host, "알림");
        let figure = host.create_figure();
        for (artifact, bar) in [(canvas, "tool-group"), (figure, "figure-tool-group")] {
            let mut window = Window::open_with_previews(&mut host, &artifact);
            window.draw(&mut host, 1.0);
            window.app.notices.warning("이 패널의 그림은 개체 단위로 편집할 수 없습니다.", Instant::now());
            // `App::update` draws the callout after the frame; the test runs both, twice, so the callout's area is known.
            let app = &mut window.app;
            for _ in 0..2 {
                let _ = window.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
                    app.draw_frame(ctx);
                    app.draw_error_callout(ctx);
                });
            }
            let (callout, bar_rect) = callout_and_bar(&window, bar);
            let shared = callout.intersect(bar_rect);
            assert!(shared.width() <= 0.0 || shared.height() <= 0.0, "{bar}: the callout {callout:?} covers the bar {bar_rect:?}");
            assert!(callout.bottom() <= bar_rect.top(), "{bar}: the callout {callout:?} ends above the bar {bar_rect:?}");
        }
    }
}
