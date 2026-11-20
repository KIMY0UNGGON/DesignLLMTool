//! The home screen (GUI_DESIGN §3, §2): the request box, the template tiles, the tab row and the
//! project list. Its only instruction is the placeholder `무엇을 만들까요?` in the request box.

use designllm_bridge::home::HomeRequest;
use designllm_contracts::{new_id, ArtifactKind, ArtifactSummary, CreateArtifactRequest, CreateRequest, Size, EMU_PER_CSS_PX};
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Frame, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, Vec2};
use serde_json::{json, Value};

use super::{
    chrome::{is_file_backed, job_agent_state, kind_icon, origin_extension},
    drafts::{Body, Edit, Live, Outcome},
    thumbs::{connected_agents, ConnectedAgent, Fit, ThumbSource},
    DesktopApp, PendingAction, Screen,
};
use crate::{icons, theme, widgets::AgentState};

/// Width of the home column at the design width (§3).
const COLUMN: f32 = 840.0;
/// The ids of the runner menus' model drafts: the home row's and the editor AI tab's (`runner_popover`).
pub(super) const HOME_RUNNER_DRAFT: &str = "home-runner-model-draft";
pub(super) const AI_RUNNER_DRAFT: &str = "ai-runner-model-draft";
const RUNNER_DRAFTS: [&str; 2] = [HOME_RUNNER_DRAFT, AI_RUNNER_DRAFT];
/// The effort rows of the runner menu: the value sent to `home.runner` (`runner_models::RUNNER_EFFORTS`), and its label.
const EFFORT_ITEMS: [(&str, &str); 6] = [("minimal", "최소"), ("low", "낮음"), ("medium", "보통"), ("high", "높음"), ("xhigh", "매우 높음"), ("max", "최대")];
/// Grid view cells (§3): a 240 × 180 thumbnail with the title row under it.
const GRID_CELL: Vec2 = Vec2::new(240.0, 180.0);

/// The tiles of the home screen, in the order of the design (§3, D02).
pub(super) const TILES: &[(&str, &str, &str)] = &[
    ("blank", "빈 화면", "tpl-blank"),
    ("mobile", "모바일 앱", "tpl-mobile"),
    ("ui", "UI 목업", "tpl-ui"),
    ("wireframe", "와이어프레임", "tpl-wireframe"),
    ("slides", "슬라이드", "tpl-slides"),
    ("document", "문서", "tpl-document"),
    ("resume", "이력서", "tpl-resume"),
    ("poster", "포스터 · 전단", "tpl-poster"),
    ("social", "소셜 게시물", "tpl-social"),
    ("figure", "논문 그림", "tpl-figure"),
    ("diagram", "구조도", "tpl-diagram"),
    ("web", "웹 코드", "tpl-web"),
    ("animation", "애니메이션", "tpl-animation"),
];

/// Columns of the tile grid at the design width: 7 columns, 9.6 px apart, rows 104 high and 8 apart.
const TILE_COLUMNS: usize = 7;
const TILE_HEIGHT: f32 = 104.0;
const TILE_MIN_WIDTH: f32 = 96.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HomeTab {
    Projects,
    DesignSystems,
    Templates,
}

/// What the home screen remembers between frames.
pub(super) struct HomeState {
    pub prompt: String,
    pub tile: String,
    pub focus_request: bool,
    pub list_view: bool,
    pub starred_only: bool,
    pub code_mode: bool,
    pub attachments: Vec<String>,
    pub tab: HomeTab,
    pub search: String,
    pub design_system: String,
    pub starred: std::collections::BTreeSet<String>,
    /// Smoke captures: the requested home view was applied.
    pub smoke_view_done: bool,
    /// Smoke captures: a request-box popover to open once (`size`, `runner`, `design_system`).
    pub smoke_open: Option<String>,
    /// A create the template refused at the chosen size (`TEMPLATE_FIT`): the size it was asked at,
    /// shown as a callout under `만들기` while the same tile and size stay chosen.
    pub create_refusal: Option<String>,
    /// (tile, size choice) of the last create, to match a refusal to what is still chosen.
    pub create_attempt: (String, String),
    /// Where the size chip was drawn this frame (the refusal callout hangs under it).
    pub size_chip_rect: Option<Rect>,
    /// The size menu's `사용자 크기` fields: open, focus once, and the typed width and height.
    pub custom_open: bool,
    pub custom_focus: bool,
    pub custom_width: String,
    pub custom_height: String,
    /// Templates and connections read from the host (`templates.list`, `clients.list`).
    pub feeds: super::thumbs::HomeFeeds,
    /// The workspace information under the workspace name (migration backups and their warnings).
    pub workspace_info: super::workspace_info::WorkspaceInfo,
}

impl Default for HomeState {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            tile: "blank".into(),
            focus_request: false,
            list_view: true,
            starred_only: false,
            code_mode: false,
            attachments: Vec::new(),
            tab: HomeTab::Projects,
            search: String::new(),
            design_system: String::new(),
            starred: Default::default(),
            smoke_view_done: false,
            smoke_open: None,
            create_refusal: None,
            create_attempt: Default::default(),
            size_chip_rect: None,
            custom_open: false,
            custom_focus: false,
            custom_width: String::new(),
            custom_height: String::new(),
            feeds: Default::default(),
            workspace_info: Default::default(),
        }
    }
}

/// The document kind, size and template that a tile creates (§3 table).
pub(super) fn tile_recipe(tile: &str) -> (ArtifactKind, Option<Size>, Option<&'static str>) {
    match tile {
        // Template names are the canvas engine's ids.
        "mobile" => (ArtifactKind::Canvas, Some(designllm_contracts::frame_preset("phone").map_or(Size::a4(), |p| p.size)), Some("mobile_app")),
        "ui" => (ArtifactKind::Canvas, Some(designllm_contracts::frame_preset("desktop").map_or(Size::a4(), |p| p.size)), None),
        "slides" => (ArtifactKind::Slides, Some(Size::slide()), None),
        "document" => (ArtifactKind::Docx, Some(Size::a4()), None),
        "resume" => (ArtifactKind::Canvas, Some(Size::a4()), Some("resume")),
        "poster" => (ArtifactKind::Canvas, Some(Size { width: 297 * designllm_contracts::EMU_PER_MM, height: 420 * designllm_contracts::EMU_PER_MM }), Some("poster_a3")),
        "social" => (ArtifactKind::Canvas, Some(designllm_contracts::frame_preset("social_square").map_or(Size::a4(), |p| p.size)), Some("social_post")),
        "figure" | "diagram" => (ArtifactKind::Figure, None, None),
        "web" => (ArtifactKind::WebCode, None, None),
        "animation" => (ArtifactKind::WebCode, None, Some("webgl_animation")),
        _ => (ArtifactKind::Canvas, Some(designllm_contracts::frame_preset("desktop").map_or(Size::a4(), |p| p.size)), None),
    }
}

impl DesktopApp {
    #[cfg(test)]
    pub(super) fn create_from_home(&mut self, tile: &str, prompt: String) -> Result<(), String> {
        self.create_from_home_command(&json!({ "tile": tile, "prompt": prompt }))
    }

    pub(super) fn create_from_home_command(&mut self, args: &Value) -> Result<(), String> {
        let text = |key: &str| args.get(key).and_then(Value::as_str);
        let tile = text("tile").unwrap_or("blank");
        let prompt = text("prompt").unwrap_or_default().trim().to_owned();
        let model = text("model").unwrap_or(&self.ai_model).to_owned();
        if !prompt.is_empty() && model.trim().is_empty() {
            return Err("모델 ID를 입력해 주세요".into());
        }
        if let Some(preset) = text("preset").filter(|preset| !preset.is_empty()) {
            if tile == "resume" && preset != "a4" {
                return Err("이력서 템플릿은 A4 크기만 지원합니다".into());
            }
            if designllm_contracts::frame_preset(preset).is_none() {
                return Err("없는 크기입니다".into());
            }
        }
        let (kind, size, template) = tile_recipe(tile);
        // A size chosen in the size menu (`home.preset`: a preset, or `custom:<w>x<h>` in px or, for
        // print tiles, mm) replaces the tile's default size. A template is laid out at the chosen
        // size; without a choice it keeps its own page (size None). A size it cannot fit comes back
        // as `TEMPLATE_FIT`.
        let preset = text("preset").unwrap_or(&self.home_preset);
        let chosen = designllm_contracts::frame_preset(preset).map(|preset| preset.size).or_else(|| {
            custom_size(preset).map(|(width, height)| {
                let emu = if print_tile(tile) { designllm_contracts::EMU_PER_MM } else { designllm_contracts::EMU_PER_CSS_PX };
                Size { width: width * emu, height: height * emu }
            })
        });
        let size = if tile == "resume" { None } else if template.is_some() { chosen } else { chosen.or(size) };
        let title = if prompt.trim().is_empty() {
            TILES.iter().find(|item| item.0 == tile).map_or("새 작업물", |item| item.1).to_owned()
        } else {
            prompt.chars().take(24).collect()
        };
        self.home.create_refusal = None;
        self.home.create_attempt = (tile.to_owned(), preset.to_owned());
        let create = CreateRequest { kind, title, size, template: template.map(str::to_owned) };
        if prompt.trim().is_empty() {
            if self.run_context.is_none() && (!self.home.attachments.is_empty() || !self.ai_figure_refs.is_empty()) {
                return Err("첨부와 그림 참조는 요청과 함께 보냅니다. 요청을 적어 주세요".into());
            }
            let request = CreateArtifactRequest { request_id: new_id(), actor: "desktop".into(), options: create };
            let params = serde_json::to_value(request).map_err(|_| "작업물을 만들지 못했습니다".to_owned())?;
            if self.call("artifact.create", params, PendingAction::Create).is_none() {
                return Ok(());
            }
        } else if let Some(run) = &self.run_context {
            // Use only the supplied prompt. The user's composer and its draft belong to the GUI.
            let request = HomeRequest {
                actor: run.actor.clone(), create, prompt, allowed_intents: run.allowed_intents.intersection(&self.ai_intents()).copied().collect(),
                runtime: text("runtime").unwrap_or(&self.ai_runtime).to_owned(), model, effort: text("effort").unwrap_or(&self.ai_effort).to_owned(),
                attachments: Vec::new(), figure_refs: Vec::new(),
            };
            if self.call_home(request).is_none() {
                return Ok(()); // The channel returns the worker's enqueue error.
            }
        } else {
            // The box gives up its text and files for this request. They are written, marked with the request id, before the
            // request goes out, and they come back if it is refused (`drafts.rs`). Text typed while it is on its way stays in the box.
            self.sync_drafts();
            let request_id = new_id();
            let ticket = self.next_ticket;
            let now = std::time::Instant::now();
            // The usual request is the box's own text. A request whose text is another one (sent by a command) leaves the box's text
            // where it is, and its draft is the request's own.
            let attachments: Vec<String> = self.home.attachments.clone();
            if self.home.prompt.trim() == prompt.trim() {
                let Edit::Has(live) = self.home_edit() else {
                    return Err("요청 내용을 적어 주세요".into());
                };
                self.drafts.begin_send(ticket, "home", live, &request_id, false, now)?;
                self.home.prompt.clear();
            } else {
                let body = Body::Home { prompt: prompt.clone(), attachments: attachments.clone(), figure_refs: self.ai_figure_refs.clone() };
                let live = Live { artifact_id: None, revision: None, body };
                self.drafts.begin_send(ticket, "home-request", live, &request_id, false, now)?;
            }
            // One host call (application::create_from_home): the attachments, then the job that makes the artifact.
            let request = HomeRequest {
                actor: "desktop".into(),
                create,
                prompt: prompt.trim().into(),
                runtime: text("runtime").unwrap_or(&self.ai_runtime).to_owned(),
                model,
                effort: text("effort").unwrap_or(&self.ai_effort).to_owned(),
                allowed_intents: self.ai_intents(),
                attachments: attachments.into_iter().map(std::path::PathBuf::from).collect(),
                figure_refs: self.ai_figure_refs.clone(),
            };
            self.home.attachments.clear();
            match self.call_home(request) {
                Some(sent) => self.drafts.rebind(ticket, sent),
                None => {
                    // Not sent: the text and files come back to the box, and the home screen stays.
                    self.answer_sent_outcome(ticket, Outcome::Refused);
                    self.repeat_ui_test_refusal();
                    return Ok(());
                }
            }
        }
        self.screen = Screen::Editor;
        Ok(())
    }

    /// For the reply handler of `artifact.create` / `create_from_home` (app.rs): a template that
    /// cannot be laid out at the chosen size (`TEMPLATE_FIT`) is shown as a callout under `만들기`
    /// on the home screen instead of a status message. Returns true when it took the error.
    pub(super) fn home_template_refused(&mut self, error: &designllm_contracts::DltError) -> bool {
        if !error.diagnostics.iter().any(|diagnostic| diagnostic.code == "TEMPLATE_FIT") {
            return false;
        }
        // The message names the size as `<w> x <h> px`.
        let size = error
            .message
            .split(" fit ")
            .nth(1)
            .and_then(|rest| rest.split(':').next())
            .map(|size| size.trim().trim_end_matches(" px").replace(" x ", " × "))
            .unwrap_or_default();
        self.home.create_refusal = Some(size);
        self.screen = Screen::Home;
        true
    }

    /// The refusal callout (tokens `refusal-callout`) under the request box, right-aligned with
    /// `만들기`: the reason and one way out, back to the tile's own size.
    fn create_refusal_callout(&mut self, ctx: &egui::Context, palette: &theme::Palette, request_box: Rect) {
        let chosen = (self.home.tile.clone(), self.home_preset.clone());
        if self.home.create_refusal.is_some() && chosen != self.home.create_attempt {
            self.home.create_refusal = None;
        }
        let Some(size) = self.home.create_refusal.clone() else {
            return;
        };
        // tokens `text.lint-rules.TEMPLATE_FIT`: the message with the size, else its label.
        let text = if size.is_empty() { "템플릿 크기 맞지 않음".to_owned() } else { format!("{size}에는 이 템플릿이 맞지 않습니다") };
        // The button's icon is the preset icon of the template's own size.
        let default_icon = tile_presets(&self.home.tile).first().map_or("preset-custom", |id| super::tools::preset_icon(id));
        let width = 280.0;
        // Under the size chip that caused it (lead L4): its left edge, the request box bottom + 8.
        let left = self.home.size_chip_rect.map_or(request_box.right() - 14.0 - 1.0 - width, |chip| chip.left());
        // The box's last pixel row is its border (rect bottom − 1); the callout's border sits 8 below it.
        let at = Pos2::new(left, request_box.bottom() - 1.0 + 8.0);
        let mut back = false;
        egui::Area::new(egui::Id::new("home-create-refusal")).order(egui::Order::Foreground).fixed_pos(at).show(ctx, |ui| {
            let [_, drop] = theme::shadow(ctx, theme::Elevation::Float);
            Frame::new()
                .fill(palette.danger_soft)
                .stroke(Stroke::new(1.0, palette.derived(theme::Derived::DangerRing)))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(egui::Margin { left: 10, right: 10, top: 8, bottom: 10 })
                .shadow(drop)
                .show(ui, |ui| {
                    ui.set_width(width - 22.0);
                    // tokens `refusal-callout`: the message (icon 16, 6, caption 12/16), 8, the action.
                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 8.0);
                    let (line, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 16.0), Sense::hover());
                    icon16(ui, line.min, "state-error", palette.danger);
                    ui.painter().text(Pos2::new(line.left() + 22.0, line.center().y), Align2::LEFT_CENTER, &text, theme::font(theme::Type::Caption), palette.ink);
                    let label = "기본 크기로";
                    let font = theme::font(theme::Type::Caption);
                    let (rect, response) = ui.allocate_exact_size(Vec2::new(8.0 + 16.0 + 6.0 + text_width(ui, label, &font) + 10.0, 28.0), Sense::click());
                    ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_CONTROL), if response.hovered() { palette.surface_2 } else { palette.field });
                    ui.painter().rect_stroke(rect, CornerRadius::same(theme::RADIUS_CONTROL), Stroke::new(1.0, palette.derived(theme::Derived::DangerRing)), StrokeKind::Outside);
                    icon16(ui, Pos2::new(rect.left() + 8.0, rect.center().y - 8.0), default_icon, palette.ink_2);
                    ui.painter().text(Pos2::new(rect.left() + 30.0, rect.center().y), Align2::LEFT_CENTER, label, font, palette.ink);
                    back = response.clicked();
                });
        });
        if back {
            self.invoke_ui("home.preset", json!({ "preset": "" }));
        }
    }

    /// The home screen (central panel): top bar, request box, tiles, tabs and the list.
    pub(super) fn draw_home(&mut self, ctx: &egui::Context) {
        self.settle_runner_drafts(ctx);
        let palette = theme::palette(ctx);
        // Smoke captures only: `DESIGNLLM_SMOKE_HOME_VIEW=grid` shows the grid view, through its
        // command; `DESIGNLLM_SMOKE_HOME_TAB=templates` opens the template tab.
        if self.smoke_output.is_some() && !std::mem::replace(&mut self.home.smoke_view_done, true) {
            if std::env::var("DESIGNLLM_SMOKE_HOME_VIEW").as_deref() == Ok("grid") {
                self.invoke_ui("home.view", json!({ "view": "grid" }));
            }
            if std::env::var("DESIGNLLM_SMOKE_HOME_TAB").as_deref() == Ok("templates") {
                self.invoke_ui("home.tab", json!({ "tab": "templates" }));
            }
            // `DESIGNLLM_SMOKE_HOME_TILE=<tile>` chooses a tile through `home.tile`.
            if let Ok(tile) = std::env::var("DESIGNLLM_SMOKE_HOME_TILE") {
                self.invoke_ui("home.tile", json!({ "tile": tile }));
            }
            // `DESIGNLLM_SMOKE_HOME_OPEN=size|runner|design_system|workspace` opens that popover.
            self.home.smoke_open = std::env::var("DESIGNLLM_SMOKE_HOME_OPEN").ok();
        }
        // The template list and the connections (top bar agents) are read while the home is shown.
        self.poll_home_feeds();
        ctx.request_repaint_after(std::time::Duration::from_secs(3));
        egui::TopBottomPanel::top("home-top")
            .exact_height(theme::TOP_BAR_HEIGHT)
            .show_separator_line(false)
            .frame(Frame::new().fill(palette.surface).inner_margin(egui::Margin { left: 12, right: 8, top: 0, bottom: 0 }))
            .show(ctx, |ui| self.home_top_bar(ui, &palette));
        egui::CentralPanel::default().frame(Frame::new().fill(palette.home)).show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let available = ui.available_width();
                let width = (available - 48.0).clamp(560.0, COLUMN).min(available);
                let left = ((available - width) / 2.0).max(0.0);
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                ui.horizontal_top(|ui| {
                    ui.add_space(left);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing = Vec2::ZERO;
                        ui.set_width(width);
                        ui.add_space(64.0);
                        self.request_box(ui, &palette, width);
                        ui.add_space(16.0);
                        self.tile_grid(ui, &palette, width);
                        ui.add_space(36.0);
                        self.home_tabs(ui, &palette, width);
                        match self.home.tab {
                            HomeTab::Projects if self.home.list_view => {
                                ui.add_space(12.0);
                                self.project_list(ui, &palette, width);
                            }
                            HomeTab::Projects => {
                                ui.add_space(16.0);
                                self.project_grid(ui, &palette, width);
                            }
                            HomeTab::Templates => {
                                ui.add_space(16.0);
                                self.template_grid(ui, &palette, width);
                            }
                            HomeTab::DesignSystems => {}
                        }
                        ui.add_space(24.0);
                    });
                });
            });
        });
        self.draw_home_confirmation(ctx);
    }

    /// The home top bar (§3, mockup `.top.h`): the workspace button on the left; the agents and
    /// `열기` on the right.
    fn home_top_bar(&mut self, ui: &mut Ui, palette: &theme::Palette) {
        let bar = ui.max_rect();
        // The bar's 1 px `line` is its own last pixel row (mockup `border-bottom`, y = 43).
        let full = ui.clip_rect();
        ui.painter().hline(full.x_range(), full.bottom() - 0.5, Stroke::new(1.0, palette.line));
        // Centred in the 43 px above the line.
        let middle = bar.top() + (bar.height() - 1.0) / 2.0;
        let name_font = theme::display_font(true, 14.0);
        // The workspace menu (other / recent workspaces) waits for `home.workspace`; a menu with no
        // working item shows no ▾ (REVIEW_BUILD §5). The workspace information (68e: migration backups and their
        // warnings) is its one item so far: with it the name is a button with ▾ that opens it.
        let open_info = self.home.smoke_open.as_deref() == Some("workspace");
        if open_info {
            self.home.smoke_open = None;
        }
        if !self.home.workspace_info.is_empty() {
            let name_width = text_width(ui, &self.workspace_name, &name_font);
            let button = Rect::from_min_max(Pos2::new(bar.left(), middle - 16.0), Pos2::new(bar.left() + 36.0 + name_width + 4.0 + 16.0 + 8.0, middle + 16.0));
            if open_info {
                egui::Popup::open_id(ui.ctx(), ui.id().with("home-workspace").with("popup"));
            }
            let response = ui.interact(button, ui.id().with("home-workspace"), Sense::click()).on_hover_text("작업공간 정보");
            crate::ui_record::note(ui, "작업공간 정보", &response);
            if response.hovered() || popup_open(ui, "home-workspace") {
                ui.painter().rect_filled(button, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_2);
            }
            icon16(ui, Pos2::new(button.right() - 8.0 - 16.0, middle - 8.0), "chev-down", palette.ink_3);
            popover(&response, 360.0, |ui| super::workspace_info::show(ui, palette, &self.home.workspace_info));
        }
        icons::paint(ui, Rect::from_min_size(Pos2::new(bar.left() + 4.0, middle - 12.0), Vec2::splat(24.0)), "app-24", Color32::WHITE);
        ui.painter().text(Pos2::new(bar.left() + 36.0, middle), Align2::LEFT_CENTER, &self.workspace_name, name_font, palette.ink);

        let open_font = theme::display_font(false, 13.0);
        let open_width = 8.0 + 16.0 + 6.0 + text_width(ui, "열기", &open_font) + 10.0;
        let open = Rect::from_min_size(Pos2::new(bar.right() - open_width, middle - 16.0), Vec2::new(open_width, 32.0));
        let response = ui.interact(open, ui.id().with("home-open"), Sense::click()).on_hover_text("열기  Ctrl+O");
        crate::ui_record::note(ui, "열기", &response);
        if response.hovered() {
            ui.painter().rect_filled(open, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_2);
        }
        icon16(ui, Pos2::new(open.left() + 8.0, middle - 8.0), "open", palette.ink_2);
        ui.painter().text(Pos2::new(open.left() + 30.0, middle), Align2::LEFT_CENTER, "열기", open_font, palette.ink);
        if response.clicked() {
            self.invoke_ui("nav.open_file", json!({}));
        }

        // The agents (§5.5, tokens `top-bar.agents-*`): one per connected agent, oldest at the left,
        // 4 apart and 10 before `열기`. With none connected, the app's job (its runtime's mark) while one runs. With neither,
        // no mark: a waiting mark would press to nothing (GUI_DESIGN §5.5 marks only what can be opened).
        let mut agents = connected_agents(&self.home.feeds.clients);
        if let Some(job) = self.job.as_ref().filter(|job| agents.is_empty() && super::chrome::job_status_running(job.status)) {
            let runtime = self.active_runtime();
            agents.push(ConnectedAgent { name: agent_name(&runtime, None), runtime, job_id: Some(job.id.clone()), outside: false });
        }
        let mut right = open.left() - 2.0 - 10.0;
        let mut other_colours = 0;
        for agent in agents.iter().rev() {
            let slot = Rect::from_min_size(Pos2::new(right - 32.0, middle - 16.0), Vec2::splat(32.0));
            right = slot.left() - 4.0;
            let colour = match agent.runtime.as_str() {
                "claude" | "codex" => agent_color(palette, &agent.runtime),
                _ => {
                    other_colours += 1;
                    palette.agents[2 + (other_colours - 1) % 2]
                }
            };
            let (state, artifact_id) = if agent.outside { (AgentState::Done, None) } else { self.agent_job(agent.job_id.as_deref()) };
            // An app job shows its runtime's initial; an outside MCP client the plug (§5.5).
            let glyph = if agent.outside { "mcp".to_owned() } else { super::chrome::agent_letter(&agent.runtime).to_owned() };
            let response = ui.interact(slot, ui.id().with(("home-agent", &agent.name)), Sense::click()).on_hover_text(&agent.name);
            paint_agent(ui, palette, slot.center(), AgentMark::TOP_BAR, colour, &glyph, state);
            if response.clicked() {
                if let Some(artifact_id) = artifact_id {
                    self.invoke_ui("nav.open_project", json!({ "artifact_id": artifact_id }));
                }
            }
        }
    }

    /// The state and artifact of an app agent's job: from the listed artifacts' active jobs, else
    /// the window's own job. No job, or one not found: waiting.
    fn agent_job(&self, job_id: Option<&str>) -> (AgentState, Option<String>) {
        let Some(job_id) = job_id else {
            return (AgentState::Waiting, None);
        };
        for summary in &self.artifacts {
            if let Some(job) = summary.active_jobs.iter().find(|job| job.job_id == job_id) {
                return (job_agent_state(job.state), Some(summary.id.clone()));
            }
        }
        match self.job.as_ref().filter(|job| job.id == job_id) {
            Some(job) => (job_agent_state(job.status), Some(job.artifact_id.clone())),
            None => (AgentState::Waiting, None),
        }
    }

    /// The template tab (mockup `home-templates`): three columns of 240 × 180 previews, 60 apart
    /// across and 24 down, each with its name under it.
    fn template_grid(&mut self, ui: &mut Ui, palette: &theme::Palette, width: f32) {
        const GAP_Y: f32 = 24.0;
        // `templates.list` names a template once per size it can take; the tab shows each template once.
        let mut templates = self.home.feeds.templates.clone();
        let mut seen = std::collections::BTreeSet::new();
        templates.retain(|template| seen.insert((template.kind, template.id.clone())));
        // In the order and under the names of §12 (D02); templates not in that list follow.
        templates.sort_by_key(|template| template_order(&template.id));
        let columns = ((width + 24.0) / (GRID_CELL.x + 24.0)).floor().max(1.0) as usize;
        let gap_x = if columns > 1 { (width - columns as f32 * GRID_CELL.x) / (columns as f32 - 1.0) } else { 0.0 };
        let cell_height = GRID_CELL.y + 8.0 + 18.0;
        let lines = templates.len().div_ceil(columns);
        let (area, _) = ui.allocate_exact_size(Vec2::new(width, (lines as f32 * (cell_height + GAP_Y) - GAP_Y).max(0.0)), Sense::hover());
        for (index, template) in templates.iter().enumerate() {
            let (line, column) = (index / columns, index % columns);
            let cell = Rect::from_min_size(
                Pos2::new(area.left() + column as f32 * (GRID_CELL.x + gap_x), area.top() + line as f32 * (cell_height + GAP_Y)),
                Vec2::new(GRID_CELL.x, cell_height),
            );
            let thumb = Rect::from_min_size(cell.min, GRID_CELL);
            let response = ui.interact(cell, ui.id().with(("home-template", &template.id)), Sense::click());
            ui.painter().rect_filled(thumb, CornerRadius::same(theme::RADIUS_FLOAT), if response.hovered() { palette.surface_3 } else { palette.board });
            if template.kind == ArtifactKind::WebCode {
                // WebCode has no static RenderScene thumbnail. Its live animation is previewed in WebView2 after creation.
                ui.painter().text(thumb.center(), Align2::CENTER_CENTER, "WEBGL", theme::font(theme::Type::Body), palette.ink_3);
            } else if let Some(page) = self.paint_thumb(ui, ThumbSource::Template(template), thumb, Fit::Contain { margin: 12.0 }, CornerRadius::ZERO) {
                page_shadow(ui, page);
            }
            let name = template_name(template);
            ui.painter().text(Pos2::new(cell.left(), thumb.bottom() + 8.0 + 9.0), Align2::LEFT_CENTER, &name, theme::font(theme::Type::Body), palette.ink);
            if response.clicked() {
                self.invoke_ui("home.use_template", json!({ "template": template.id, "title": name, "kind": template.kind }));
            }
        }
    }

    fn request_box(&mut self, ui: &mut Ui, palette: &theme::Palette, width: f32) {
        // Two shadows (§5.3 `0 1 2 .05` + `0 8 28 .08`), painted under the box once its size is known.
        let shadow_slots = [ui.painter().add(Shape::Noop), ui.painter().add(Shape::Noop)];
        let corner = CornerRadius::same(theme::RADIUS_BOX);
        let inner_width = width - 16.0 - 14.0 - 2.0;
        let shown = Frame::new()
            .fill(palette.surface)
            .stroke(Stroke::new(1.0, palette.line))
            .corner_radius(corner)
            .inner_margin(egui::Margin { left: 16, right: 14, top: 14, bottom: 14 })
            .show(ui, |ui| {
                ui.set_width(inner_width);
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                let mut prompt = self.home.prompt.clone();
                let font = theme::display_font(false, 15.0);
                let edit = egui::TextEdit::multiline(&mut prompt)
                    .hint_text(egui::RichText::new("무엇을 만들까요?").font(font.clone()).color(palette.ink_3))
                    .font(font)
                    .text_color(palette.ink)
                    .frame(false)
                    // 2 px padding plus the half-leading of the 22 px line (egui rows are 18 high).
                    .margin(egui::Margin { left: 0, right: 0, top: 4, bottom: 0 })
                    .desired_rows(1)
                    .min_size(Vec2::new(inner_width, 56.0))
                    .desired_width(inner_width);
                let response = ui.add(edit);
                crate::ui_record::note(ui, "무엇을 만들까요?", &response);
                if std::mem::take(&mut self.home.focus_request) {
                    response.request_focus();
                }
                if response.changed() {
                    self.home.prompt = prompt;
                }
                self.draw_ai_figure_refs(ui);
                let enter_submits = ui.input(|input| input.key_pressed(egui::Key::Enter) && !input.modifiers.shift);
                if response.has_focus() && enter_submits && !self.has_home_confirmation() && !self.home.prompt.trim().is_empty() {
                    self.home_create_now();
                }
                ui.add_space(8.0);
                self.request_row(ui, palette, inner_width);
            });
        let rect = shown.response.rect;
        for (slot, shadow) in shadow_slots.into_iter().zip(theme::shadow(ui.ctx(), theme::Elevation::RequestBox)) {
            ui.painter().set(slot, shadow.as_shape(rect, corner));
        }
        let ctx = ui.ctx().clone();
        self.create_refusal_callout(&ctx, palette, rect);
    }

    /// The row under the request text (mockup `.rb`, 40 high, 8 apart): 첨부 · 디자인 시스템 · 디자인/코드,
    /// then at the right the size, the runner and 만들기.
    fn request_row(&mut self, ui: &mut Ui, palette: &theme::Palette, width: f32) {
        let (row, _) = ui.allocate_exact_size(Vec2::new(width, 40.0), Sense::hover());
        if let Some(name) = self.home.smoke_open.take() {
            let id = match name.as_str() {
                "runner" => "home-runner",
                "design_system" => "home-design-system",
                _ => "home-size",
            };
            egui::Popup::open_id(ui.ctx(), ui.id().with(id).with("popup"));
        }
        let body = theme::display_font(false, 13.0);
        let small = theme::display_font(false, 11.0);
        let mut x = row.left();

        let attach_width = 10.0 + 16.0 + 6.0 + text_width(ui, "첨부", &body) + 12.0;
        let attach = Rect::from_min_size(Pos2::new(x, row.top()), Vec2::new(attach_width, 40.0));
        let response = outlined(ui, palette, attach, "home-attach", false);
        icon16(ui, Pos2::new(attach.left() + 10.0, attach.center().y - 8.0), "attach", palette.ink_2);
        ui.painter().text(Pos2::new(attach.left() + 32.0, attach.center().y), Align2::LEFT_CENTER, "첨부", body.clone(), palette.ink);
        if response.clicked() {
            if let Some(path) = crate::dialogs::open_document(self.parent_hwnd) {
                self.invoke_ui("home.attach", json!({ "path": path.to_string_lossy() }));
            }
        }
        x = attach.right() + 8.0;

        if shows_design_system(&self.home.tile) {
            let system = if self.home.design_system.is_empty() { "없음".to_owned() } else { self.home.design_system.clone() };
            let chip_width = two_line_width(ui, "디자인 시스템", &system, &small, &body);
            let chip = Rect::from_min_size(Pos2::new(x, row.top()), Vec2::new(chip_width, 40.0));
            let open = popup_open(ui, "home-design-system");
            let response = outlined(ui, palette, chip, "home-design-system", open);
            paint_two_line(ui, palette, chip, "디자인 시스템", &system);
            // Mockup `home-templates` popover: `없음` and the workspace's design systems (none exist
            // before D11); `디자인 시스템 만들기` waits for its command.
            let mut chosen = None;
            popover(&response, 264.0, |ui| {
                if crate::widgets::menu_item(ui, palette, None, "없음", "", self.home.design_system.is_empty()).clicked() {
                    chosen = Some(String::new());
                    ui.close();
                }
            });
            if let Some(design_system) = chosen {
                self.invoke_ui("home.design_system", json!({ "design_system": design_system }));
            }
            x = chip.right() + 8.0;
        }

        if shows_mode(&self.home.tile) {
            let entries = [("kind-canvas", "디자인", false), ("code", "코드", true)];
            let widths: Vec<f32> = entries.iter().map(|(_, label, _)| 8.0 + 16.0 + 6.0 + text_width(ui, label, &body) + 10.0).collect();
            let tray = Rect::from_min_size(Pos2::new(x, row.top()), Vec2::new(2.0 + widths[0] + 2.0 + widths[1] + 2.0, 40.0));
            ui.painter().rect_filled(tray, CornerRadius::same(8), palette.surface_2);
            let mut left = tray.left() + 2.0;
            for ((icon, label, code), button_width) in entries.into_iter().zip(widths) {
                let button = Rect::from_min_size(Pos2::new(left, tray.top() + 2.0), Vec2::new(button_width, 36.0));
                left = button.right() + 2.0;
                let on = self.home.code_mode == code;
                let response = ui.interact(button, ui.id().with(("home-mode", code)), Sense::click());
                if on {
                    raised(ui, palette, button);
                }
                let ink = if on { palette.ink } else { palette.ink_2 };
                icon16(ui, Pos2::new(button.left() + 8.0, button.center().y - 8.0), icon, ink);
                ui.painter().text(Pos2::new(button.left() + 30.0, button.center().y), Align2::LEFT_CENTER, label, body.clone(), ink);
                if response.clicked() && !on {
                    self.invoke_ui("home.mode", json!({ "mode": if code { "code" } else { "design" } }));
                }
            }
        }

        // From the right edge.
        let bold = theme::display_font(true, 13.0);
        let go_width = 12.0 + 16.0 + 6.0 + text_width(ui, "만들기", &bold) + 14.0;
        let go = Rect::from_min_size(Pos2::new(row.right() - go_width, row.top()), Vec2::new(go_width, 40.0));
        let response = ui.interact(go, ui.id().with("home-go"), Sense::click()).on_hover_text("만들기  Enter");
        let fill = if response.hovered() || response.is_pointer_button_down_on() { palette.accent_ink } else { palette.accent };
        ui.painter().rect_filled(go, CornerRadius::same(8), fill);
        icon16(ui, Pos2::new(go.left() + 12.0, go.center().y - 8.0), "send", palette.on_accent);
        ui.painter().text(Pos2::new(go.left() + 34.0, go.center().y), Align2::LEFT_CENTER, "만들기", bold, palette.on_accent);
        if response.clicked() {
            self.home_create_now();
        }

        let (runtime, model) = self.runner_labels();
        let runner_width = two_line_width(ui, runtime, &model, &small, &body);
        let runner = Rect::from_min_size(Pos2::new(go.left() - 8.0 - runner_width, row.top()), Vec2::new(runner_width, 40.0));
        let open = popup_open(ui, "home-runner");
        let response = outlined(ui, palette, runner, "home-runner", open);
        paint_two_line(ui, palette, runner, runtime, &model);
        let model_draft_id = egui::Id::new(HOME_RUNNER_DRAFT);
        let changes = self.runner_popover(ui, palette, &response, model_draft_id);
        for change in changes {
            self.invoke_ui("home.runner", change);
        }

        let (_, size, _) = tile_recipe(&self.home.tile);
        let (value, icon) = self.size_chip_value();
        let size_width = 10.0 + 16.0 + 6.0 + text_width(ui, &value, &body) + 6.0 + 16.0 + 10.0;
        let chip = Rect::from_min_size(Pos2::new(runner.left() - 8.0 - size_width, row.top()), Vec2::new(size_width, 40.0));
        let open = popup_open(ui, "home-size");
        let response = outlined(ui, palette, chip, "home-size", open);
        self.home.size_chip_rect = Some(chip);
        // The size the template refused (§11, lead L3): the chip's 1 px border and a 1 px ring outside
        // it in `danger` while its callout shows.
        if self.home.create_refusal.is_some() {
            ui.painter().rect_stroke(chip, CornerRadius::same(8), Stroke::new(1.0, palette.danger), StrokeKind::Inside);
            ui.painter().rect_stroke(chip, CornerRadius::same(8), Stroke::new(1.0, palette.danger), StrokeKind::Outside);
        }
        icon16(ui, Pos2::new(chip.left() + 10.0, chip.center().y - 8.0), icon, palette.ink_2);
        ui.painter().text(Pos2::new(chip.left() + 32.0, chip.center().y), Align2::LEFT_CENTER, &value, body, palette.ink);
        icon16(ui, Pos2::new(chip.right() - 26.0, chip.center().y - 8.0), "chev-down", palette.ink_3);
        // The menu lists the tile's own sizes (§3 table) and, where a size can be typed, `사용자 크기`.
        let presets = tile_presets(&self.home.tile);
        let current = designllm_contracts::frame_preset(&self.home_preset)
            .map(|preset| preset.id)
            .or_else(|| presets.iter().copied().find(|id| designllm_contracts::frame_preset(id).map(|p| p.size) == size));
        let custom_allowed = custom_size_allowed(&self.home.tile);
        let custom_checked = self.home_preset.starts_with("custom:");
        let current = if custom_checked { None } else { current };
        let slides_tile = self.home.tile == "slides";
        let unit = if print_tile(&self.home.tile) { "mm" } else { "px" };
        if !open {
            self.home.custom_open = false;
        }
        let mut chosen = None;
        let mut custom = None;
        popover(&response, 280.0, |ui| {
            if let Some(id) = preset_menu(ui, palette, presets, current) {
                chosen = Some(id);
                ui.close();
            }
            // The slide tile also offers 4:3 (lead item 27), kept as a user size in CSS px.
            if slides_tile && crate::widgets::menu_item_value(ui, palette, Some("preset-slide"), "4:3", "960 × 720", custom_checked).clicked() {
                custom = Some((960.0, 720.0));
                ui.close();
            }
            if custom_allowed {
                if !presets.is_empty() {
                    crate::widgets::menu_separator(ui, palette);
                }
                if let Some(entered) = self.custom_size_row(ui, palette, unit, custom_checked) {
                    custom = Some(entered);
                    ui.close();
                }
            }
        });
        if let Some(id) = chosen {
            self.invoke_ui("home.preset", json!({ "preset": id }));
        }
        if let Some((width, height)) = custom {
            self.invoke_ui("home.preset", json!({ "width": width, "height": height }));
        }
    }

    /// The size chip's value and icon: a chosen preset, a user size (`custom:<w>x<h>`), or the tile's own size.
    fn size_chip_value(&self) -> (String, &'static str) {
        let (kind, size, _) = tile_recipe(&self.home.tile);
        if let Some(preset) = designllm_contracts::frame_preset(&self.home_preset) {
            return (preset_value(preset), super::tools::preset_icon(preset.id));
        }
        let slides = kind == ArtifactKind::Slides;
        if let Some((width, height)) = custom_size(&self.home_preset) {
            let unit = if print_tile(&self.home.tile) { " mm" } else { "" };
            return (format!("{width} × {height}{unit}"), if slides { "preset-slide" } else { "preset-custom" });
        }
        let icon = if slides { "preset-slide" } else { device_icon(size.as_ref()) };
        (size.map_or_else(|| kind_label_home(kind).to_owned(), |size| size_label(size, kind)), icon)
    }

    /// The `사용자 크기` row of the size menu (lead ruling 22): a menu row that opens, in its place,
    /// a width and a height field (28 high, 8 apart, the unit at the right); Enter sets the size.
    fn custom_size_row(&mut self, ui: &mut Ui, palette: &theme::Palette, unit: &str, checked: bool) -> Option<(f64, f64)> {
        if !self.home.custom_open {
            if crate::widgets::menu_item(ui, palette, Some("preset-custom"), "사용자 크기", "", checked).clicked() {
                self.home.custom_open = true;
                self.home.custom_focus = true;
                if let Some((width, height)) = custom_size(&self.home_preset) {
                    self.home.custom_width = width.to_string();
                    self.home.custom_height = height.to_string();
                }
            }
            return None;
        }
        let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::hover());
        let field_width = (row.width() - 16.0 - 8.0) / 2.0;
        let mut entered = false;
        let focus = std::mem::take(&mut self.home.custom_focus);
        for (index, (prefix, text)) in [("W", &mut self.home.custom_width), ("H", &mut self.home.custom_height)].into_iter().enumerate() {
            let rect = Rect::from_min_size(Pos2::new(row.left() + 8.0 + index as f32 * (field_width + 8.0), row.center().y - 14.0), Vec2::new(field_width, 28.0));
            ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS_CONTROL), palette.field);
            ui.painter().rect_stroke(rect, CornerRadius::same(theme::RADIUS_CONTROL), Stroke::new(1.0, palette.line_strong), StrokeKind::Inside);
            let small = theme::font(theme::Type::Caption);
            ui.painter().text(Pos2::new(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, prefix, small.clone(), palette.ink_3);
            ui.painter().text(Pos2::new(rect.right() - 8.0, rect.center().y), Align2::RIGHT_CENTER, unit, small, palette.ink_3);
            let inner = Rect::from_min_max(Pos2::new(rect.left() + 24.0, rect.center().y - 9.0), Pos2::new(rect.right() - 30.0, rect.center().y + 9.0));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner));
            let response = child.add(
                egui::TextEdit::singleline(text)
                    .frame(false)
                    .margin(egui::Margin::ZERO)
                    .font(theme::font(theme::Type::Body))
                    .text_color(palette.ink)
                    .desired_width(inner.width()),
            );
            if index == 0 && focus {
                response.request_focus();
            }
            if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                entered = true;
            }
        }
        if !entered {
            return None;
        }
        let parse = |text: &str| text.trim().replace(',', ".").parse::<f64>().ok().filter(|value| *value >= 1.0);
        match (parse(&self.home.custom_width), parse(&self.home.custom_height)) {
            (Some(width), Some(height)) => {
                self.home.custom_open = false;
                Some((width, height))
            }
            _ => None,
        }
    }

    fn home_create_now(&mut self) {
        let tile = self.home.tile.clone();
        let prompt = self.home.prompt.clone();
        self.invoke_ui("home.create", json!({ "tile": tile, "prompt": prompt }));
    }

    /// The runner as the home row and the editor's AI tab show it: the runtime, and the model with the effort.
    pub(super) fn runner_labels(&self) -> (&'static str, String) {
        let runtime = if self.ai_runtime == "codex" { "Codex" } else { "Claude" };
        (runtime, format!("{} · {}", self.ai_model_label(), self.effort_label()))
    }

    /// Saves a runner model draft (`runner_popover`) whose menu was not drawn in the last frame, as when a tab or a screen change
    /// takes the menu away while it is open, and every draft when the window is asked to close; then fits the runner to a model
    /// list that has just come in (`settle_runner_models`). Runs at the start of each frame, before either menu is drawn
    /// (`draw_home`, `draw_frame`).
    pub(super) fn settle_runner_drafts(&mut self, ctx: &egui::Context) {
        let closing = ctx.input(|input| input.viewport().close_requested());
        self.save_runner_drafts(ctx, closing);
        self.settle_runner_models(ctx);
    }

    /// Saves the runner model drafts: every one with `all`, else those whose menu was not drawn in the last frame. The app's
    /// exit saves them all (`on_exit`): a draft typed in the frame that closed the window comes after that frame's start.
    pub(super) fn save_runner_drafts(&mut self, ctx: &egui::Context, all: bool) {
        for name in RUNNER_DRAFTS {
            let draft_id = egui::Id::new(name);
            let drawn = ctx.data_mut(|data| data.remove_temp::<bool>(draft_id.with("drawn"))).unwrap_or(false);
            if drawn && !all {
                continue;
            }
            if let Some(model) = ctx.data_mut(|data| data.remove_temp::<String>(draft_id)) {
                if model.trim() != self.ai_model {
                    self.invoke_ui("home.runner", json!({ "model": model }));
                }
            }
        }
    }

    /// The runner menu (runtime, model ID, effort) under the control `response`, and the `home.runner` changes it makes, in
    /// the order the user made them. The typed model is a draft (kept under `model_draft_id`, one of `RUNNER_DRAFTS`) until the
    /// field loses focus (Enter included) or the menu closes: a keystroke does not save the settings. A menu that stops being
    /// drawn, and the window's close, save it at the next frame's start (`settle_runner_drafts`). The draft goes before the
    /// clicks, as a click closes the menu.
    pub(super) fn runner_popover(&self, ui: &mut Ui, palette: &theme::Palette, response: &Response, model_draft_id: egui::Id) -> Vec<Value> {
        let mut picked = Vec::new();
        let mut left_field = false;
        let typing_id = model_draft_id.with("typing");
        let kind = super::runner_models::runtime_kind(&self.ai_runtime);
        popover(response, 220.0, |ui| {
            for (id, label) in [("claude", "Claude"), ("codex", "Codex")] {
                if crate::widgets::menu_item(ui, palette, None, label, "", self.ai_runtime == id || (id == "claude" && self.ai_runtime != "codex")).clicked() {
                    picked.push(json!({ "runtime": id }));
                    ui.close();
                }
            }
            crate::widgets::menu_separator(ui, palette);
            // The first opening asks the CLI in the background; until the answer, the fixed list is shown.
            self.runner_models.request(kind, ui.ctx());
            let (list, awaiting) = self.runner_models.shown(kind);
            let current = self.ai_model.as_str();
            // A saved model the list does not name stays selected at the top.
            if !current.is_empty() && !list.models.iter().any(|model| model.id == current) && crate::widgets::menu_item(ui, palette, None, current, "", true).clicked() {
                ui.close();
            }
            for model in &list.models {
                if crate::widgets::menu_item(ui, palette, None, &model.display_name, "", model.id == current).clicked() {
                    picked.push(json!({ "model": model.id }));
                    ui.close();
                }
            }
            if awaiting {
                ui.label(egui::RichText::new("목록 불러오는 중").color(palette.ink_3));
            }
            let typing = ui.data(|data| data.get_temp::<bool>(typing_id)).unwrap_or(false);
            // The field takes the focus when it opens: in the click's frame and in the next one (the pending mark).
            let focus_id = model_draft_id.with("focus");
            let pending = ui.data_mut(|data| data.remove_temp::<bool>(focus_id)).unwrap_or(false);
            let mut focus = false;
            if crate::widgets::menu_item(ui, palette, None, "직접 입력…", "", typing).clicked() {
                ui.data_mut(|data| {
                    data.insert_temp(typing_id, true);
                    data.insert_temp(focus_id, true);
                });
                focus = true;
            }
            if typing || focus {
                let mut model = ui.data(|data| data.get_temp::<String>(model_draft_id)).unwrap_or_else(|| self.ai_model.clone());
                let width = ui.available_width();
                let field = crate::widgets::text_field(ui, palette, &mut model, "모델 ID", width);
                if focus || pending {
                    field.request_focus();
                }
                if field.changed() {
                    ui.data_mut(|data| data.insert_temp(model_draft_id, model.clone()));
                }
                left_field |= field.lost_focus();
            }
            crate::widgets::menu_separator(ui, palette);
            // The efforts the chosen model's list says it accepts; the default four when its list names none of them.
            let accepted: &[String] = list.models.iter().find(|model| model.id == current).map_or(&[], |model| model.efforts.as_slice());
            let any_accepted = EFFORT_ITEMS.iter().any(|(id, _)| accepted.iter().any(|level| level == id));
            for (id, label) in EFFORT_ITEMS {
                let shown = if any_accepted { accepted.iter().any(|level| level == id) } else { super::runner_models::DEFAULT_EFFORTS.contains(&id) };
                if shown && crate::widgets::menu_item(ui, palette, None, label, "", self.effort_label() == label).clicked() {
                    picked.push(json!({ "effort": id }));
                    ui.close();
                }
            }
        });
        let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(response));
        // The mark tells the next frame's start that this menu was drawn (`settle_runner_drafts`).
        ui.data_mut(|data| data.insert_temp(model_draft_id.with("drawn"), true));
        // The typed field opens with `직접 입력…` and closes with the menu.
        if !open {
            ui.data_mut(|data| data.remove_temp::<bool>(typing_id));
        }
        let mut changes = Vec::new();
        if left_field || !open {
            if let Some(model) = ui.data_mut(|data| data.remove_temp::<String>(model_draft_id)) {
                if model.trim() != self.ai_model {
                    changes.push(json!({ "model": model }));
                }
            }
        }
        changes.extend(picked);
        changes
    }

    fn ai_model_label(&self) -> String {
        if self.ai_model.trim().is_empty() { "모델 선택".into() } else { self.ai_model.clone() }
    }

    fn effort_label(&self) -> &'static str {
        match self.ai_effort.as_str() {
            "minimal" => "최소",
            "low" => "낮음",
            "medium" => "보통",
            "xhigh" => "매우 높음",
            "max" => "최대",
            _ => "높음",
        }
    }

    fn tile_grid(&mut self, ui: &mut Ui, palette: &theme::Palette, width: f32) {
        const GAP_X: f32 = 9.6;
        const GAP_Y: f32 = 8.0;
        let columns = ((width + GAP_X) / (TILE_MIN_WIDTH + GAP_X)).floor().clamp(1.0, TILE_COLUMNS as f32) as usize;
        let tile_width = (width - GAP_X * (columns as f32 - 1.0)) / columns as f32;
        let rows = TILES.len().div_ceil(columns);
        let (area, _) = ui.allocate_exact_size(Vec2::new(width, rows as f32 * (TILE_HEIGHT + GAP_Y) - GAP_Y), Sense::hover());
        let mut clicked = None;
        for (index, (id, label, icon)) in TILES.iter().enumerate() {
            let (row, column) = (index / columns, index % columns);
            let tile = Rect::from_min_size(
                Pos2::new(area.left() + column as f32 * (tile_width + GAP_X), area.top() + row as f32 * (TILE_HEIGHT + GAP_Y)),
                Vec2::new(tile_width, TILE_HEIGHT),
            );
            let response = ui.interact(tile, ui.id().with(("home-tile", *id)), Sense::click());
            let selected = self.home.tile == *id;
            let corner = CornerRadius::same(theme::RADIUS_FLOAT);
            if selected {
                ui.painter().rect_filled(tile, corner, palette.accent_soft);
                ui.painter().rect_stroke(tile, corner, Stroke::new(1.0, palette.derived(theme::Derived::TileOnBorder)), StrokeKind::Inside);
            } else if response.hovered() {
                ui.painter().rect_filled(tile, corner, palette.surface_2);
            }
            // Mockup `.tile`: 12 px padding, the 48 px icon, 8 px, then the 12/16 label.
            icons::paint(ui, Rect::from_min_size(Pos2::new(tile.center().x - 24.0, tile.top() + 12.0), Vec2::splat(48.0)), icon, palette.ink_2);
            let (font, ink) = if selected { (theme::display_font(true, 12.0), palette.accent_ink) } else { (theme::display_font(false, 12.0), palette.ink) };
            ui.painter().text(Pos2::new(tile.center().x, tile.top() + 12.0 + 48.0 + 8.0 + 8.0), Align2::CENTER_CENTER, *label, font, ink);
            if response.clicked() {
                clicked = Some(*id);
            }
        }
        if let Some(id) = clicked {
            self.invoke_ui("home.tile", json!({ "tile": id }));
        }
    }

    fn home_tabs(&mut self, ui: &mut Ui, palette: &theme::Palette, width: f32) {
        let (row, _) = ui.allocate_exact_size(Vec2::new(width, 32.0), Sense::hover());
        let mut x = row.left();
        for (label, tab, name) in [("프로젝트", HomeTab::Projects, "projects"), ("디자인 시스템", HomeTab::DesignSystems, "design_systems"), ("템플릿", HomeTab::Templates, "templates")] {
            let selected = self.home.tab == tab;
            let font = theme::display_font(selected, 13.0);
            let color = if selected { palette.ink } else { palette.ink_2 };
            let rect = Rect::from_min_size(Pos2::new(x, row.top()), Vec2::new(text_width(ui, label, &font) + 24.0, 32.0));
            x = rect.right() + 2.0;
            let response = ui.interact(rect, ui.id().with(("home-tab", label)), Sense::click());
            let corner = CornerRadius::same(7);
            if selected {
                ui.painter().rect_filled(rect, corner, palette.surface);
                ui.painter().rect_stroke(rect, corner, Stroke::new(1.0, palette.line), StrokeKind::Outside);
            } else if response.hovered() {
                ui.painter().rect_filled(rect, corner, palette.surface_2);
            }
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, label, font, color);
            if response.clicked() && !selected {
                self.invoke_ui("home.tab", json!({ "tab": name }));
            }
        }

        // From the right: list/grid (`.vseg`), 별표만, the search box; 8 apart.
        let tray = Rect::from_min_size(Pos2::new(row.right() - 66.0, row.top()), Vec2::new(66.0, 32.0));
        ui.painter().rect_filled(tray, CornerRadius::same(8), palette.surface_2);
        for (index, (icon, list, label)) in [("view-list", true, "목록"), ("layout-grid", false, "격자")].into_iter().enumerate() {
            let button = Rect::from_min_size(tray.min + Vec2::new(2.0 + index as f32 * 32.0, 2.0), Vec2::new(30.0, 28.0));
            let response = ui.interact(button, ui.id().with(("home-view", index)), Sense::click()).on_hover_text(label);
            // The template tab is always a grid (lead ruling 26).
            let shown_list = self.home.list_view && self.home.tab != HomeTab::Templates;
            let on = list == shown_list;
            if on {
                ui.painter().rect_filled(button, CornerRadius::same(6), palette.raised);
                ui.painter().rect_stroke(button, CornerRadius::same(6), Stroke::new(1.0, palette.line), StrokeKind::Outside);
            }
            icon16(ui, button.center() - Vec2::splat(8.0), icon, if on { palette.ink } else { palette.ink_2 });
            if response.clicked() && !on {
                self.invoke_ui("home.view", json!({ "view": if list { "list" } else { "grid" } }));
            }
        }

        let star = Rect::from_min_size(Pos2::new(tray.left() - 8.0 - 32.0, row.top()), Vec2::splat(32.0));
        let response = ui.interact(star, ui.id().with("home-starred-only"), Sense::click()).on_hover_text("별표만");
        let on = self.home.starred_only;
        if on {
            ui.painter().rect_filled(star, CornerRadius::same(theme::RADIUS_CONTROL), palette.accent_soft);
        } else if response.hovered() {
            ui.painter().rect_filled(star, CornerRadius::same(theme::RADIUS_CONTROL), palette.surface_2);
        }
        icon16(ui, star.center() - Vec2::splat(8.0), if on { "star-fill" } else { "star" }, if on { palette.accent } else { palette.ink_2 });
        if response.clicked() {
            self.invoke_ui("home.starred_only", json!({ "on": !on }));
        }

        // The search box (`.search`): 220 × 32, a magnifier and the field, no placeholder.
        let search = Rect::from_min_size(Pos2::new(star.left() - 8.0 - 220.0, row.top()), Vec2::new(220.0, 32.0));
        ui.painter().rect_filled(search, CornerRadius::same(8), palette.field);
        ui.painter().rect_stroke(search, CornerRadius::same(8), Stroke::new(1.0, palette.line_strong), StrokeKind::Inside);
        icon16(ui, Pos2::new(search.left() + 8.0, search.center().y - 8.0), "search", palette.ink_3);
        let field = Rect::from_min_max(Pos2::new(search.left() + 30.0, search.center().y - 9.0), Pos2::new(search.right() - 8.0, search.center().y + 9.0));
        // A child ui, so that the field does not move the column's cursor (`put` would).
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(field));
        let mut search_text = self.home.search.clone();
        let edited = child
            .add(
                egui::TextEdit::singleline(&mut search_text)
                    .frame(false)
                    .margin(egui::Margin::ZERO)
                    .font(theme::font(theme::Type::Body))
                    .text_color(palette.ink)
                    .desired_width(field.width()),
            )
            .changed();
        if edited {
            self.invoke_ui("home.search", json!({ "text": search_text }));
        }
    }

    /// The listed projects after the search text and the 별표만 filter.
    fn shown_projects(&self) -> Vec<ArtifactSummary> {
        let needle = self.home.search.trim().to_lowercase();
        let mut rows: Vec<ArtifactSummary> = self
            .artifacts
            .iter()
            .filter(|item| needle.is_empty() || item.title.to_lowercase().contains(&needle))
            .filter(|item| !self.home.starred_only || self.home.starred.contains(&item.id))
            .cloned()
            .collect();
        // The most recently changed first (mockup: `방금` on the first row); unknown times last.
        rows.sort_by(|a, b| b.modified_at_ms.cmp(&a.modified_at_ms));
        rows
    }

    /// The marks of a project at the right of a row or the top right of a cell, right to left
    /// from `right`: star, (in rows) the time, the unsaved dot, the agent. Returns the star's response.
    fn project_marks(&mut self, ui: &Ui, palette: &theme::Palette, summary: &ArtifactSummary, right: f32, middle: f32, with_time: bool) -> (Response, f32) {
        let star = Rect::from_min_size(Pos2::new(right - 28.0, middle - 14.0), Vec2::splat(28.0));
        let response = ui.interact(star, ui.id().with(("home-star", &summary.id)), Sense::click()).on_hover_text("별표");
        let starred = self.home.starred.contains(&summary.id);
        icon16(ui, star.center() - Vec2::splat(8.0), if starred { "star-fill" } else { "star" }, if starred { palette.star } else { palette.ink_3 });
        let mut cursor = star.left() - 12.0;
        if with_time {
            if let Some(modified) = summary.modified_at_ms {
                let label = modified_label(modified, now_ms(), local_offset_minutes());
                ui.painter().text(Pos2::new(cursor, middle), Align2::RIGHT_CENTER, label, theme::display_font(false, 12.0), palette.ink_3);
            }
            cursor -= 72.0 + 12.0;
        }
        // Only a document kept as its own file shows the unsaved dot (tokens `row-unsaved-dot`).
        if summary.dirty && is_file_backed(summary.kind) {
            ui.painter().circle_filled(Pos2::new(cursor - 3.0, middle), 3.0, palette.accent);
            cursor -= 6.0 + 12.0;
        }
        if let Some(job) = summary.active_jobs.first() {
            let center = Pos2::new(cursor - 12.0, middle);
            let hover = Rect::from_center_size(center, Vec2::splat(24.0));
            ui.interact(hover, ui.id().with(("home-agent-row", &summary.id)), Sense::hover())
                .on_hover_text(agent_name(&job.runtime, job.client_name.as_deref()));
            let glyph = agent_glyph(&job.runtime, job.client_name.as_deref());
            paint_agent(ui, palette, center, AgentMark::ROW, agent_color(palette, &job.runtime), &glyph, job_agent_state(job.state));
            cursor -= 24.0 + 12.0;
        }
        (response, cursor)
    }

    fn project_grid(&mut self, ui: &mut Ui, palette: &theme::Palette, width: f32) {
        const GAP_Y: f32 = 24.0;
        let rows = self.shown_projects();
        let columns = ((width + 24.0) / (GRID_CELL.x + 24.0)).floor().max(1.0) as usize;
        let gap_x = if columns > 1 { (width - columns as f32 * GRID_CELL.x) / (columns as f32 - 1.0) } else { 0.0 };
        let cell_height = GRID_CELL.y + 8.0 + 18.0;
        let lines = rows.len().div_ceil(columns);
        let (area, _) = ui.allocate_exact_size(Vec2::new(width, (lines as f32 * (cell_height + GAP_Y) - GAP_Y).max(0.0)), Sense::hover());
        for (index, summary) in rows.iter().enumerate() {
            let (line, column) = (index / columns, index % columns);
            let cell = Rect::from_min_size(
                Pos2::new(area.left() + column as f32 * (GRID_CELL.x + gap_x), area.top() + line as f32 * (cell_height + GAP_Y)),
                Vec2::new(GRID_CELL.x, cell_height),
            );
            let response = ui.interact(cell, ui.id().with(("home-cell", &summary.id)), Sense::click());
            crate::ui_record::note(ui, &summary.title, &response);
            let thumb = Rect::from_min_size(cell.min, GRID_CELL);
            ui.painter().rect_filled(thumb, CornerRadius::same(theme::RADIUS_FLOAT), palette.board);
            if let Some(page) = self.paint_thumb(ui, ThumbSource::Artifact(summary), thumb, Fit::Contain { margin: 12.0 }, CornerRadius::ZERO) {
                page_shadow(ui, page);
            }
            let (star, _) = self.project_marks(ui, palette, summary, thumb.right() - 6.0, thumb.top() + 20.0, false);
            let title_y = thumb.bottom() + 8.0 + 9.0;
            self.paint_title(ui, palette, summary, Pos2::new(cell.left(), title_y), cell.width() - 84.0);
            if let Some(modified) = summary.modified_at_ms {
                let label = modified_label(modified, now_ms(), local_offset_minutes());
                ui.painter().text(Pos2::new(cell.right(), title_y), Align2::RIGHT_CENTER, label, theme::display_font(false, 12.0), palette.ink_3);
            }
            if star.clicked() {
                self.invoke_ui("home.star", json!({ "artifact_id": summary.id }));
            } else if response.clicked() {
                self.invoke_ui("nav.open_project", json!({ "artifact_id": summary.id }));
            }
        }
    }

    /// The title (13 `ink`) and, for a document from a file, its extension (`ink-3`), 12 apart.
    fn paint_title(&self, ui: &Ui, palette: &theme::Palette, summary: &ArtifactSummary, left_center: Pos2, max_width: f32) {
        let font = theme::display_font(false, 13.0);
        let extension = origin_extension(summary.kind);
        let extension_width = if extension.is_empty() { 0.0 } else { 12.0 + text_width(ui, extension, &font) };
        let title = crate::widgets::truncate_to_width(ui, &summary.title, (max_width - extension_width).max(24.0), palette.ink);
        let title_width = text_width(ui, &title, &font);
        ui.painter().text(left_center, Align2::LEFT_CENTER, &title, font.clone(), palette.ink);
        if !extension.is_empty() {
            ui.painter().text(left_center + Vec2::new(title_width + 12.0, 0.0), Align2::LEFT_CENTER, extension, font, palette.ink_3);
        }
    }

    fn project_list(&mut self, ui: &mut Ui, palette: &theme::Palette, width: f32) {
        // Mockup `.list` / `.pr`: a 1 px line above, rows 56 high with a 1 px line under each;
        // 4 px side padding and 12 px between the parts.
        let (top, _) = ui.allocate_exact_size(Vec2::new(width, 1.0), Sense::hover());
        ui.painter().rect_filled(top, CornerRadius::ZERO, palette.line);
        for summary in self.shown_projects() {
            let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 56.0), Sense::click());
            crate::ui_record::note(ui, &summary.title, &response);
            let content = Rect::from_min_max(rect.min, Pos2::new(rect.right(), rect.bottom() - 1.0));
            if response.hovered() {
                ui.painter().rect_filled(content, CornerRadius::ZERO, palette.surface_2);
            }
            ui.painter().rect_filled(Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - 1.0), rect.max), CornerRadius::ZERO, palette.line);
            let middle = content.center().y;
            let thumb = Rect::from_min_size(Pos2::new(content.left() + 4.0, middle - 22.0), Vec2::new(64.0, 44.0));
            let corner = CornerRadius::same(5);
            ui.painter().rect_filled(thumb, corner, Color32::WHITE);
            self.paint_thumb(ui, ThumbSource::Artifact(&summary), thumb, Fit::Cover, corner);
            ui.painter().rect_stroke(thumb, corner, Stroke::new(1.0, palette.line), StrokeKind::Outside);
            icon16(ui, Pos2::new(thumb.right() + 12.0, middle - 8.0), kind_icon(summary.kind), palette.ink_2);
            let (star, title_end) = self.project_marks(ui, palette, &summary, content.right() - 4.0, middle, true);
            let title_left = thumb.right() + 12.0 + 16.0 + 12.0;
            self.paint_title(ui, palette, &summary, Pos2::new(title_left, middle), title_end - title_left);
            if star.clicked() {
                self.invoke_ui("home.star", json!({ "artifact_id": summary.id }));
            } else if response.clicked() {
                self.invoke_ui("nav.open_project", json!({ "artifact_id": summary.id }));
            }
        }
    }
}

fn text_width(ui: &Ui, text: &str, font: &FontId) -> f32 {
    ui.painter().layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE).size().x
}

fn icon16(ui: &Ui, min: Pos2, name: &str, color: Color32) {
    icons::paint(ui, Rect::from_min_size(min, Vec2::splat(16.0)), name, color);
}

/// A request-box control (mockup `.ob`): 40 high, white, 1 px `line-strong`, corners 8; hover `surface-2`.
fn outlined(ui: &Ui, palette: &theme::Palette, rect: Rect, id: &str, open: bool) -> Response {
    let response = ui.interact(rect, ui.id().with(id), Sense::click());
    let fill = if open || response.hovered() { palette.surface_2 } else { palette.field };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    ui.painter().rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, palette.line_strong), StrokeKind::Inside);
    response
}

/// Whether the popover of the control `id` (an `outlined` chip) is open: the chip then takes the
/// `open` fill (tokens `chip.open`).
fn popup_open(ui: &Ui, id: &str) -> bool {
    egui::Popup::is_id_open(ui.ctx(), ui.id().with(id).with("popup"))
}

/// A popover under a chip (tokens `component.menu`): its top 4 below the chip, its left at the
/// chip's left, `width` including the 1 px border, padding 4, corners 10, `shadow.menu`. Opens and
/// closes with clicks on the chip, closes on a click outside.
fn popover(response: &Response, width: f32, content: impl FnOnce(&mut Ui)) {
    let ctx = response.ctx.clone();
    let palette = theme::palette(&ctx);
    let [_, shadow] = theme::shadow(&ctx, theme::Elevation::Menu);
    let frame = Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.line))
        .corner_radius(CornerRadius::same(theme::RADIUS_FLOAT))
        .inner_margin(egui::Margin::same(4))
        .shadow(shadow);
    egui::Popup::from_toggle_button_response(response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .align(egui::RectAlign::BOTTOM_START)
        .gap(4.0)
        .frame(frame)
        .show(|ui| {
            ui.set_width(width - 2.0 * 4.0 - 2.0);
            // Menu rows sit flush (tokens `menu.item` 30 high, no gap).
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            content(ui);
        });
}

/// The template list of §12 (D02), in its order: (engine template id, name).
const TEMPLATE_NAMES: &[(&str, &str)] = &[
    ("mobile_app", "모바일 앱 화면"),
    ("login", "로그인"),
    ("landing_page", "랜딩 페이지"),
    ("dashboard", "대시보드"),
    ("poster_a3", "포스터"),
    ("social_post", "소셜 게시물"),
    ("resume", "이력서"),
    ("invitation", "초대장"),
    ("banner", "배너"),
];

fn template_order(id: &str) -> usize {
    TEMPLATE_NAMES.iter().position(|(known, _)| *known == id).unwrap_or(TEMPLATE_NAMES.len())
}

/// The name the template tab shows: the §12 name, else the engine's title.
fn template_name(template: &designllm_contracts::TemplateInfo) -> String {
    TEMPLATE_NAMES.iter().find(|(id, _)| *id == template.id).map_or_else(|| template.title.clone(), |(_, name)| (*name).to_owned())
}

/// The sizes the size menu offers for a tile (§3 table), by frame preset id.
fn tile_presets(tile: &str) -> &'static [&'static str] {
    const ALL: &[&str] = &[
        "phone", "phone_large", "tablet", "desktop", "social_square", "social_portrait", "story", "a4", "letter", "a3", "invitation",
        "banner_medium", "leaderboard", "slide_16_9",
    ];
    match tile {
        "blank" => ALL,
        // Template tiles: the templates lay out at these sizes (and at a user size that passes the
        // template's lint, else `TEMPLATE_FIT`).
        "mobile" => &["phone", "phone_large"],
        "poster" => &["a3", "a4"],
        "social" => &["social_square", "social_portrait", "story"],
        "ui" => &["desktop", "tablet"],
        "wireframe" => &["phone", "phone_large", "tablet", "desktop"],
        "slides" => &["slide_16_9"],
        "document" => &["a4", "letter"],
        "resume" => &["a4"],
        "animation" => &["slide_16_9", "social_square"],
        _ => &[],
    }
}

/// Tiles whose size can be typed (`사용자 크기`): screens, print and code; slides and documents
/// keep their page formats.
fn custom_size_allowed(tile: &str) -> bool {
    !matches!(tile, "slides" | "document" | "resume")
}

/// Tiles measured in millimetres.
fn print_tile(tile: &str) -> bool {
    matches!(tile, "resume" | "poster" | "document" | "figure" | "diagram")
}

/// A user size kept by `home.preset {width, height}` as `custom:<w>x<h>`.
fn custom_size(preset: &str) -> Option<(i64, i64)> {
    let (width, height) = preset.strip_prefix("custom:")?.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

/// The size menu of the request box (mockup `home-presets`): the frame presets in groups, each
/// group under its caption; icon, name, size at the right, a check on the current one. Returns the
/// preset chosen this frame.
fn preset_menu(ui: &mut Ui, palette: &theme::Palette, ids: &[&str], current: Option<&str>) -> Option<&'static str> {
    use designllm_contracts::PresetFamily;
    let mut chosen = None;
    let mut last = None;
    for preset in designllm_contracts::FRAME_PRESETS.iter().filter(|preset| ids.contains(&preset.id)) {
        if last != Some(preset.family) {
            if last.is_some() {
                crate::widgets::menu_separator(ui, palette);
            }
            last = Some(preset.family);
            let caption = match preset.family {
                PresetFamily::Screen => "화면",
                PresetFamily::Social => "소셜",
                PresetFamily::Print => "인쇄",
                PresetFamily::Ad => "배너",
                PresetFamily::Slide => "슬라이드",
            };
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::hover());
            // tokens `menu.caption`: padding 6 above and 2 below a 16 px line.
            ui.painter().text(Pos2::new(rect.left() + 8.0, rect.top() + 6.0 + 8.0), Align2::LEFT_CENTER, caption, theme::font(theme::Type::Caption), palette.ink_3);
        }
        let name = match preset.id {
            "phone" => "휴대폰",
            "phone_large" => "큰 휴대폰",
            "tablet" => "태블릿",
            "desktop" => "데스크톱",
            "social_square" => "정사각형",
            "social_portrait" => "세로형",
            "story" => "스토리",
            "a4" => "A4",
            "letter" => "Letter",
            "a3" => "포스터 A3",
            "invitation" => "초대장",
            "banner_medium" => "사각 배너",
            "leaderboard" => "가로 배너",
            "slide_16_9" => "16:9",
            other => super::tools::preset_name(other),
        };
        let value = preset_value(preset);
        if crate::widgets::menu_item_value(ui, palette, Some(super::tools::preset_icon(preset.id)), name, &value, current == Some(preset.id)).clicked() {
            chosen = Some(preset.id);
        }
    }
    chosen
}

/// A preset's size in its natural unit, as the size chip and menu show it: `390 × 844`,
/// `210 × 297 mm`, `8.5 × 11 in`; a slide in CSS px like its panel, `1280 × 720` (lead item 27).
fn preset_value(preset: &designllm_contracts::FramePreset) -> String {
    use designllm_contracts::{NaturalUnit, EMU_PER_INCH, EMU_PER_MM};
    let number = |value: f64| {
        let text = format!("{value:.1}");
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    };
    let Size { width, height } = preset.size;
    match preset.natural_unit {
        NaturalUnit::Px => format!("{} × {}", width / EMU_PER_CSS_PX, height / EMU_PER_CSS_PX),
        NaturalUnit::Mm => format!("{} × {} mm", number(width as f64 / EMU_PER_MM as f64), number(height as f64 / EMU_PER_MM as f64)),
        NaturalUnit::In => format!("{} × {} in", number(width as f64 / EMU_PER_INCH as f64), number(height as f64 / EMU_PER_INCH as f64)),
        // On purpose CSS px, not pt: a slide's size panel shows CSS px, so its preset reads 1280 × 720 here too (lead item 27,
        // the test `value("slide_16_9")`). Its natural unit pt (12,700 EMU) is what the MCP manual names (bridge manual.rs).
        NaturalUnit::Pt => format!("{} × {}", width / EMU_PER_CSS_PX, height / EMU_PER_CSS_PX),
    }
}

/// The chosen button of a segmented tray: white with a 1 px `line` ring and a faint drop.
fn raised(ui: &Ui, palette: &theme::Palette, rect: Rect) {
    let corner = CornerRadius::same(6);
    // tokens `shadow.raised`: its drop equals the first layer of `float`; its ring is the `line` below.
    let [drop, _] = theme::shadow(ui.ctx(), theme::Elevation::Float);
    ui.painter().add(drop.as_shape(rect, corner));
    ui.painter().rect_filled(rect, corner, palette.raised);
    ui.painter().rect_stroke(rect, corner, Stroke::new(1.0, palette.line), StrokeKind::Outside);
}

/// Width of a two-line chip (mockup `.ob` + `.two`): 12 left, the wider line, 6, the chevron, 10.
fn two_line_width(ui: &Ui, caption: &str, value: &str, small: &FontId, body: &FontId) -> f32 {
    12.0 + text_width(ui, caption, small).max(text_width(ui, value, body)) + 6.0 + 16.0 + 10.0
}

/// The caption (11/14 `ink-3`) over the value (13/17 `ink`), centred as a 31 px block, and the chevron.
fn paint_two_line(ui: &Ui, palette: &theme::Palette, rect: Rect, caption: &str, value: &str) {
    let top = rect.center().y - 15.5;
    ui.painter().text(Pos2::new(rect.left() + 12.0, top + 7.0), Align2::LEFT_CENTER, caption, theme::display_font(false, 11.0), palette.ink_3);
    ui.painter().text(Pos2::new(rect.left() + 12.0, top + 14.0 + 8.5), Align2::LEFT_CENTER, value, theme::display_font(false, 13.0), palette.ink);
    icon16(ui, Pos2::new(rect.right() - 26.0, rect.center().y - 8.0), "chev-down", palette.ink_3);
}

/// Tiles whose artifact can take a design system (§3: canvas, slides and web tiles).
fn shows_design_system(tile: &str) -> bool {
    !matches!(tile, "document" | "figure" | "diagram")
}

/// Tiles that can be made as a design or as code (§3).
fn shows_mode(tile: &str) -> bool {
    matches!(tile, "blank" | "mobile" | "ui" | "wireframe" | "poster" | "social")
}

/// Sizes of an agent mark (§5.5): the top bar's 24 px disc in a 32 px slot with a 2 px ring at
/// 14.5, and the home row's 20 px disc with a 1.5 px ring at 11.
#[derive(Clone, Copy)]
struct AgentMark {
    disc: f32,
    ring_radius: f32,
    ring_width: f32,
    glyph_type: theme::Type,
    mcp_icon: f32,
    /// The arc's share of the circumference (tokens: 26 of 92, 18 of 70).
    arc: f32,
    /// The faint full ring under a running arc (top bar only).
    track: bool,
}

impl AgentMark {
    const TOP_BAR: Self = Self { disc: 24.0, ring_radius: 14.5, ring_width: 2.0, glyph_type: theme::Type::Avatar, mcp_icon: 16.0, arc: 26.0 / 92.0, track: true };
    const ROW: Self = Self { disc: 20.0, ring_radius: 11.0, ring_width: 1.5, glyph_type: theme::Type::AvatarSmall, mcp_icon: 12.0, arc: 18.0 / 70.0, track: false };
}

/// A page picture on the `board` tray (grid cells, template previews): tokens `shadow.artboard`,
/// a 1 px ring and a soft drop.
fn page_shadow(ui: &Ui, page: Rect) {
    let [ring, drop] = theme::shadow(ui.ctx(), theme::Elevation::Artboard);
    ui.painter().add(drop.as_shape(page, CornerRadius::ZERO));
    ui.painter().rect_stroke(page, CornerRadius::ZERO, Stroke::new(1.0, ring.color), StrokeKind::Outside);
}

/// Claude is the first agent colour (purple), Codex the second (green).
fn agent_color(palette: &theme::Palette, runtime: &str) -> Color32 {
    if runtime == "codex" { palette.agents[1] } else { palette.agents[0] }
}

/// The disc's glyph: the runtime's initial for the app's own jobs (§5.5 C / X), the plug for an
/// external MCP client.
fn agent_glyph(runtime: &str, client: Option<&str>) -> String {
    match client {
        Some(name) if !name.is_empty() && !name.to_lowercase().contains(runtime) && !runtime.is_empty() => "mcp".into(),
        _ if runtime.is_empty() => "mcp".into(),
        _ => super::chrome::agent_letter(runtime).into(),
    }
}

pub(super) fn agent_name(runtime: &str, client: Option<&str>) -> String {
    match (client.filter(|name| !name.is_empty()), runtime) {
        (Some(name), _) => name.to_owned(),
        (None, "codex") => "Codex".into(),
        (None, _) => "Claude".into(),
    }
}

fn paint_agent(ui: &Ui, palette: &theme::Palette, center: Pos2, mark: AgentMark, color: Color32, glyph: &str, state: AgentState) {
    let painter = ui.painter();
    painter.circle_filled(center, mark.disc / 2.0, color);
    if glyph == "mcp" {
        icons::paint(ui, Rect::from_center_size(center, Vec2::splat(mark.mcp_icon)), "mcp", palette.on_agent);
    } else {
        painter.text(center, Align2::CENTER_CENTER, glyph, theme::font(mark.glyph_type), palette.on_agent);
    }
    let tau = std::f32::consts::TAU;
    let ring = Stroke::new(mark.ring_width, color);
    let arc = |start: f32, sweep: f32| -> Shape {
        let points = (0..=32).map(|step| center + Vec2::angled(start + sweep * step as f32 / 32.0) * mark.ring_radius).collect();
        Shape::line(points, ring)
    };
    // The mockup's arc: 26 of 91 (top bar) or 18 of 69 (row) units of the circumference, from -80°.
    let sweep = tau * mark.arc;
    let start = -80f32.to_radians();
    match state {
        AgentState::Waiting => {
            for index in 0..8 {
                painter.add(arc(start + index as f32 / 8.0 * tau, tau / 16.0));
            }
        }
        AgentState::Running => {
            if mark.track {
                painter.circle_stroke(center, mark.ring_radius, Stroke::new(mark.ring_width, palette.agent_alpha(color, theme::AgentAlpha::RingTrack)));
            }
            let time = ui.input(|input| input.time) as f32;
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
            painter.add(arc(start + time * tau, sweep));
        }
        AgentState::Review => {
            if mark.track {
                painter.circle_stroke(center, mark.ring_radius, Stroke::new(mark.ring_width, palette.agent_alpha(color, theme::AgentAlpha::RingTrack)));
            }
            painter.add(arc(start, sweep));
        }
        AgentState::Done => {}
        AgentState::Failed => {
            let dot = center + Vec2::splat(mark.disc * 0.35);
            painter.circle_filled(dot, 4.0, palette.danger);
            painter.circle_stroke(dot, 4.0, Stroke::new(1.0, palette.surface));
        }
    }
}

/// The device icon of a size: a phone for a narrow portrait box, a tablet for other portrait boxes, else a desktop.
fn device_icon(size: Option<&Size>) -> &'static str {
    match size {
        Some(size) if size.height > size.width && size.width * 10 <= size.height * 6 => "preset-phone",
        Some(size) if size.height > size.width => "preset-tablet",
        _ => "preset-desktop",
    }
}

fn kind_label_home(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Figure => "논문 그림",
        ArtifactKind::WebCode => "웹 화면",
        _ => "",
    }
}

/// `1440 × 900` or `HWPX · A4`-style value for a size chip.
fn size_label(size: Size, kind: ArtifactKind) -> String {
    match kind {
        ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp => "A4".into(),
        _ => format!("{} × {}", size.width / EMU_PER_CSS_PX, size.height / EMU_PER_CSS_PX),
    }
}

/// The current time in Unix milliseconds.
pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// The local time zone's offset from UTC in minutes: Windows' bias, with daylight time when it is in effect.
pub(super) fn local_offset_minutes() -> i64 {
    use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    let mut info = TIME_ZONE_INFORMATION::default();
    // SAFETY: the call only writes the time zone structure it is given.
    let state = unsafe { GetTimeZoneInformation(&mut info) };
    let daylight = if state == 2 { info.DaylightBias } else { 0 };
    -(i64::from(info.Bias) + i64::from(daylight))
}

/// Days since 1970-01-01 to (year, month, day), in the proleptic Gregorian calendar.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = (if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 }) as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The local day number (days since 1970-01-01) of a time, and its hour and minute.
fn local_day(ms: u64, offset_minutes: i64) -> (i64, u32, u32) {
    let minutes = (ms as i64).div_euclid(60_000) + offset_minutes;
    let minute_of_day = minutes.rem_euclid(1440);
    (minutes.div_euclid(1440), (minute_of_day / 60) as u32, (minute_of_day % 60) as u32)
}

/// The label of a modified time, as the home list shows it (`docs/GUI_DESIGN.md` §3): 방금, the
/// clock time today, 어제, the month and day this year, or the full date.
pub(super) fn modified_label(modified_ms: u64, now_ms: u64, offset_minutes: i64) -> String {
    if now_ms.saturating_sub(modified_ms) < 60_000 {
        return "방금".into();
    }
    let (day, hour, minute) = local_day(modified_ms, offset_minutes);
    let (today, _, _) = local_day(now_ms, offset_minutes);
    if day == today {
        return format!("{hour:02}:{minute:02}");
    }
    if day + 1 == today {
        return "어제".into();
    }
    let (year, month, day_of_month) = civil_from_days(day);
    let (this_year, _, _) = civil_from_days(today);
    if year == this_year {
        format!("{month}월 {day_of_month}일")
    } else {
        format!("{year}. {month}. {day_of_month}.")
    }
}

#[cfg(test)]
mod template_refusal_tests {
    use designllm_contracts::{Diagnostic, DltError, ErrorCode};

    use super::*;
    use crate::rpc::RpcWorker;
    use eframe::egui::{epaint::Shape, Context, Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, Vec2, ViewportEvent, ViewportId};

    fn app() -> DesktopApp {
        let (rpc, _replies, _work) = RpcWorker::detached();
        DesktopApp::new(rpc, "작업공간".into(), None, None)
    }

    fn template_fit() -> DltError {
        let message = "template mobile_app does not fit 300 x 400 px: TOUCH_TARGET";
        let mut error = DltError::new(ErrorCode::InvalidArgument, message);
        error.diagnostics.push(Diagnostic { code: "TEMPLATE_FIT".into(), message: message.into(), node_id: None, blocking: true, params: Default::default() });
        error
    }

    /// One frame of the offscreen home screen: a 1440 x 900 window at 1 pixel per point, with `events`.
    fn frame(events: Vec<Event>) -> RawInput {
        let mut input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1440.0, 900.0))), ..Default::default() };
        input.viewports.get_mut(&ViewportId::ROOT).expect("root viewport").native_pixels_per_point = Some(1.0);
        input.events = events;
        input
    }

    fn draw(ctx: &Context, app: &mut DesktopApp, input: RawInput) -> egui::FullOutput {
        ctx.run(input, |ctx| app.draw_home(ctx))
    }

    /// The press and release of the primary button at `at`.
    fn click(at: Pos2) -> Vec<Event> {
        vec![
            Event::PointerMoved(at),
            Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
            Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE },
        ]
    }

    /// The centre of the first text that reads `expected` in the frame.
    fn text_center(output: &egui::FullOutput, expected: &str) -> Pos2 {
        fn find(shape: &Shape, expected: &str) -> Option<Pos2> {
            match shape {
                Shape::Text(text) if text.galley.text() == expected => Some(text.pos + text.galley.size() / 2.0),
                Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, expected)),
                _ => None,
            }
        }
        output.shapes.iter().find_map(|clipped| find(&clipped.shape, expected))
            .unwrap_or_else(|| panic!("offscreen home screen did not draw {expected:?}"))
    }

    /// Opens the runner popover and types `typed` into its model field, which keeps the focus.
    fn type_model_draft(ctx: &Context, app: &mut DesktopApp, typed: &str) -> egui::FullOutput {
        let initial = draw(ctx, app, frame(Vec::new()));
        draw(ctx, app, frame(click(text_center(&initial, "모델 선택 · 높음"))));
        let opened = draw(ctx, app, frame(Vec::new()));
        draw(ctx, app, frame(click(text_center(&opened, "직접 입력…"))));
        draw(ctx, app, frame(Vec::new()));
        draw(ctx, app, frame(vec![Event::Text(typed.into())]))
    }

    fn saved_settings(root: &std::path::Path) -> serde_json::Value {
        let text = std::fs::read_to_string(root.join(".gui/settings.json")).expect("the window saves its settings");
        serde_json::from_str(&text).expect("settings JSON")
    }

    #[test]
    fn model_label_does_not_claim_a_model_when_none_is_selected() {
        let mut app = app();
        app.ai_model.clear();
        assert_eq!(app.ai_model_label(), "모델 선택");
        app.ai_model = "gpt-example".into();
        assert_eq!(app.ai_model_label(), "gpt-example");
    }

    #[test]
    fn typing_a_model_in_the_home_runner_saves_the_workspace_setting() {
        let root = std::env::temp_dir().join(format!("designllm-home-runner-{}", new_id()));
        let mut app = app().with_workspace(&root);
        let ctx = Context::default();
        theme::install(&ctx);
        type_model_draft(&ctx, &mut app, "gpt-example");
        // Typing is a draft: the window and the settings file take the model only when the field loses focus.
        assert_ne!(app.ai_model, "gpt-example", "a keystroke is not applied");
        assert!(!std::fs::read_to_string(root.join(".gui/settings.json")).unwrap_or_default().contains("gpt-example"), "a keystroke is not saved");

        let enter = Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        draw(&ctx, &mut app, frame(vec![enter]));
        draw(&ctx, &mut app, frame(Vec::new()));

        assert_eq!(app.ai_model, "gpt-example");
        assert_eq!(saved_settings(&root)["model"], "gpt-example", "runner choice is saved on Enter");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_runtime_switch_clears_a_model_the_new_runtime_does_not_list() {
        let root = std::env::temp_dir().join(format!("designllm-home-runner-runtime-{}", new_id()));
        let mut app = app().with_workspace(&root);
        let ctx = Context::default();
        theme::install(&ctx);
        let typed = type_model_draft(&ctx, &mut app, "gpt-example");
        draw(&ctx, &mut app, frame(click(text_center(&typed, "Codex"))));
        draw(&ctx, &mut app, frame(Vec::new()));
        // The draft is saved before the click; the switch to Codex then clears a model Codex does not list.
        assert_eq!(app.ai_runtime, "codex");
        assert_eq!(app.ai_model, "", "a model outside the new runtime's list is cleared");
        assert_eq!(saved_settings(&root)["model"], "");
        assert_eq!(saved_settings(&root)["runtime"], "codex");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_runtime_switch_keeps_a_model_the_new_runtime_lists() {
        let mut app = app();
        app.ai_model = "claude-haiku-5-5".into();
        assert_eq!(app.invoke("home.runner", json!({ "runtime": "codex" })), Ok(()));
        assert_eq!(app.ai_model, "", "claude-haiku-5-5 is not a Codex model");
        app.ai_model = "gpt-5.5".into();
        assert_eq!(app.invoke("home.runner", json!({ "runtime": "claude" })), Ok(()));
        assert_eq!(app.ai_model, "", "gpt-5.5 is not a Claude model");
        app.ai_model = "claude-sonnet-5-5".into();
        assert_eq!(app.invoke("home.runner", json!({ "runtime": "claude" })), Ok(()));
        assert_eq!(app.ai_model, "claude-sonnet-5-5", "no runtime change, no clearing");
        assert_eq!(app.invoke("home.runner", json!({ "runtime": "codex", "model": "gpt-5.5" })), Ok(()));
        assert_eq!(app.ai_model, "gpt-5.5", "a model named in the same call is kept");
    }

    #[test]
    fn a_menu_item_click_sets_the_model_once() {
        let root = std::env::temp_dir().join(format!("designllm-home-runner-item-{}", new_id()));
        let mut app = app().with_workspace(&root);
        let ctx = Context::default();
        theme::install(&ctx);
        let initial = draw(&ctx, &mut app, frame(Vec::new()));
        draw(&ctx, &mut app, frame(click(text_center(&initial, "모델 선택 · 높음"))));
        let opened = draw(&ctx, &mut app, frame(Vec::new()));
        draw(&ctx, &mut app, frame(click(text_center(&opened, "claude-sonnet-5-5"))));
        draw(&ctx, &mut app, frame(Vec::new()));
        assert_eq!(app.ai_model, "claude-sonnet-5-5");
        assert_eq!(saved_settings(&root)["model"], "claude-sonnet-5-5");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_saved_model_outside_the_list_is_shown_at_the_top_and_the_list_shows_display_names() {
        let mut app = app();
        app.ai_model = "gpt-example".into();
        app.runner_models.answer(
            designllm_runtimes::detect::RuntimeKind::Claude,
            designllm_runtimes::detect::ModelList {
                models: vec![designllm_runtimes::detect::ModelInfo { id: "claude-opus-5-5".into(), display_name: "Opus 5.5".into(), efforts: vec!["low".into(), "high".into()] }],
                source: designllm_runtimes::detect::ModelSource::Cli,
            },
        );
        let ctx = Context::default();
        theme::install(&ctx);
        let initial = draw(&ctx, &mut app, frame(Vec::new()));
        draw(&ctx, &mut app, frame(click(text_center(&initial, "gpt-example · 높음"))));
        let opened = draw(&ctx, &mut app, frame(Vec::new()));
        let texts = drawn_texts(&opened);
        let stored = texts.iter().position(|text| text == "gpt-example").expect("the saved model is listed");
        let listed = texts.iter().position(|text| text == "Opus 5.5").expect("the CLI's display name is listed");
        assert!(stored < listed, "the saved model comes first: {texts:?}");
        assert!(!texts.iter().any(|text| text == "claude-opus-5-5"), "the id is not the label: {texts:?}");
        draw(&ctx, &mut app, frame(click(text_center(&opened, "Opus 5.5"))));
        draw(&ctx, &mut app, frame(Vec::new()));
        assert_eq!(app.ai_model, "claude-opus-5-5");
    }

    #[test]
    fn the_effort_menu_offers_only_the_efforts_of_the_chosen_model() {
        let mut app = app();
        app.ai_model = "claude-opus-5-5".into();
        app.runner_models.answer(
            designllm_runtimes::detect::RuntimeKind::Claude,
            designllm_runtimes::detect::ModelList {
                models: vec![designllm_runtimes::detect::ModelInfo { id: "claude-opus-5-5".into(), display_name: "Opus 5.5".into(), efforts: vec!["low".into(), "high".into()] }],
                source: designllm_runtimes::detect::ModelSource::Cli,
            },
        );
        let ctx = Context::default();
        theme::install(&ctx);
        let initial = draw(&ctx, &mut app, frame(Vec::new()));
        draw(&ctx, &mut app, frame(click(text_center(&initial, "claude-opus-5-5 · 높음"))));
        let opened = draw(&ctx, &mut app, frame(Vec::new()));
        let texts = drawn_texts(&opened);
        assert!(texts.iter().any(|text| text == "낮음") && texts.iter().any(|text| text == "높음"), "{texts:?}");
        assert!(!texts.iter().any(|text| text == "보통" || text == "최대"), "{texts:?}");
    }

    #[test]
    fn minimal_and_xhigh_are_offered_only_where_the_model_lists_them() {
        use designllm_runtimes::detect::{ModelInfo, ModelList, ModelSource, RuntimeKind};
        fn opened_texts(app: &mut DesktopApp, label: &str) -> Vec<String> {
            let ctx = Context::default();
            theme::install(&ctx);
            let initial = draw(&ctx, app, frame(Vec::new()));
            draw(&ctx, app, frame(click(text_center(&initial, label))));
            drawn_texts(&draw(&ctx, app, frame(Vec::new())))
        }
        let levels = |values: &[&str]| values.iter().map(|value| value.to_string()).collect::<Vec<_>>();
        // Claude lists xhigh and max but no minimal.
        let mut app = app();
        app.ai_model = "claude-opus-5-5".into();
        let efforts = levels(&["low", "medium", "high", "xhigh", "max"]);
        app.runner_models.answer(RuntimeKind::Claude, ModelList { models: vec![ModelInfo { id: "claude-opus-5-5".into(), display_name: "Opus 5.5".into(), efforts }], source: ModelSource::Cli });
        let texts = opened_texts(&mut app, "claude-opus-5-5 · 높음");
        assert!(texts.iter().any(|text| text == "매우 높음") && texts.iter().any(|text| text == "최대"), "{texts:?}");
        assert!(!texts.iter().any(|text| text == "최소"), "{texts:?}");
        // The command takes xhigh (`commands.rs` schema).
        assert_eq!(app.invoke("home.runner", json!({ "effort": "xhigh" })), Ok(()));
        assert_eq!(app.ai_effort, "xhigh");
        // Codex lists minimal and xhigh but no max.
        let mut app = app_codex(levels(&["minimal", "low", "medium", "high", "xhigh"]));
        let texts = opened_texts(&mut app, "gpt-6.2-sol · 높음");
        assert!(texts.iter().any(|text| text == "최소") && texts.iter().any(|text| text == "매우 높음"), "{texts:?}");
        assert!(!texts.iter().any(|text| text == "최대"), "{texts:?}");
        assert_eq!(app.invoke("home.runner", json!({ "effort": "minimal" })), Ok(()));
        assert_eq!(app.ai_effort, "minimal");
        // A model whose list says nothing about efforts gets the default four.
        let mut app = app_codex(Vec::new());
        let texts = opened_texts(&mut app, "gpt-6.2-sol · 높음");
        assert!(["낮음", "보통", "높음", "최대"].iter().all(|label| texts.iter().any(|text| text == label)), "{texts:?}");
        assert!(!texts.iter().any(|text| text == "최소" || text == "매우 높음"), "{texts:?}");

        fn app_codex(efforts: Vec<String>) -> DesktopApp {
            let mut app = self::app();
            app.ai_runtime = "codex".into();
            app.ai_model = "gpt-6.2-sol".into();
            app.runner_models.answer(RuntimeKind::Codex, ModelList { models: vec![ModelInfo { id: "gpt-6.2-sol".into(), display_name: "GPT-6.2-Sol".into(), efforts }], source: ModelSource::Cli });
            app
        }
    }

    #[test]
    fn the_cli_list_is_asked_once_in_the_background_and_then_kept() {
        fn answer(kind: designllm_runtimes::detect::RuntimeKind) -> designllm_runtimes::detect::ModelList {
            let _ = kind;
            designllm_runtimes::detect::ModelList {
                models: vec![designllm_runtimes::detect::ModelInfo { id: "claude-fable-5-2".into(), display_name: "Fable 5.2".into(), efforts: Vec::new() }],
                source: designllm_runtimes::detect::ModelSource::Cli,
            }
        }
        let mut app = app();
        app.runner_models = super::super::runner_models::RunnerModels::with_lookup(answer);
        let ctx = Context::default();
        theme::install(&ctx);
        let initial = draw(&ctx, &mut app, frame(Vec::new()));
        draw(&ctx, &mut app, frame(click(text_center(&initial, "모델 선택 · 높음"))));
        let mut texts = Vec::new();
        for _ in 0..200 {
            let opened = draw(&ctx, &mut app, frame(Vec::new()));
            texts = drawn_texts(&opened);
            if texts.iter().any(|text| text == "Fable 5.2") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(texts.iter().any(|text| text == "Fable 5.2"), "the background answer reaches the open menu: {texts:?}");
        let (list, awaiting) = app.runner_models.shown(designllm_runtimes::detect::RuntimeKind::Claude);
        assert!(!awaiting);
        assert_eq!(list.source, designllm_runtimes::detect::ModelSource::Cli);
    }

    #[test]
    fn a_chosen_model_takes_the_nearest_effort_it_accepts() {
        use designllm_runtimes::detect::{ModelInfo, ModelList, ModelSource, RuntimeKind};
        let mut app = app();
        app.runner_models.answer(
            RuntimeKind::Claude,
            ModelList {
                models: vec![ModelInfo { id: "claude-sonnet-5-5".into(), display_name: "Sonnet 5.5".into(), efforts: vec!["low".into(), "medium".into(), "high".into()] }],
                source: ModelSource::Cli,
            },
        );
        app.ai_effort = "max".into();
        assert_eq!(app.invoke("home.runner", json!({ "model": "claude-sonnet-5-5" })), Ok(()));
        assert_eq!(app.ai_effort, "high", "max is not accepted: the nearest accepted effort");
        assert_eq!(app.invoke("home.runner", json!({ "model": "claude-sonnet-5-5", "effort": "max" })), Ok(()));
        assert_eq!(app.ai_effort, "max", "an effort named in the same call is kept as given");
    }

    #[test]
    fn a_runtime_switch_before_the_list_is_in_checks_the_model_against_the_answer() {
        use designllm_runtimes::detect::{ModelInfo, ModelList, ModelSource, RuntimeKind};
        fn answer(kind: RuntimeKind) -> ModelList {
            let id = if kind == RuntimeKind::Codex { "gpt-cli-only" } else { "claude-cli-only" };
            ModelList { models: vec![ModelInfo { id: id.into(), display_name: id.into(), efforts: vec!["low".into(), "medium".into()] }], source: ModelSource::Cli }
        }
        fn settle_until_in(ctx: &Context, app: &mut DesktopApp, kind: RuntimeKind) {
            for _ in 0..200 {
                if !app.runner_models.shown(kind).1 {
                    break;
                }
                draw(ctx, app, frame(Vec::new()));
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(!app.runner_models.shown(kind).1, "the answer came in");
            draw(ctx, app, frame(Vec::new()));
        }
        let root = std::env::temp_dir().join(format!("designllm-home-runner-recheck-{}", new_id()));
        let mut app = app().with_workspace(&root);
        app.runner_models = super::super::runner_models::RunnerModels::with_lookup(answer);
        let ctx = Context::default();
        theme::install(&ctx);
        // Only the Codex CLI names this model: the fixed list does not clear it while the answer is awaited.
        app.ai_model = "gpt-cli-only".into();
        assert_eq!(app.invoke("home.runner", json!({ "runtime": "codex" })), Ok(()));
        assert_eq!(app.ai_model, "gpt-cli-only");
        settle_until_in(&ctx, &mut app, RuntimeKind::Codex);
        assert_eq!(app.ai_model, "gpt-cli-only", "the answer names it");
        assert_eq!(app.ai_effort, "medium", "high is not accepted: the nearest accepted effort");
        assert_eq!(saved_settings(&root)["effort"], "medium");
        // The Claude answer does not name it: the waiting check clears it once the answer is in, and saves that.
        assert_eq!(app.invoke("home.runner", json!({ "runtime": "claude" })), Ok(()));
        assert_eq!(app.ai_model, "gpt-cli-only", "kept while the Claude answer is awaited");
        settle_until_in(&ctx, &mut app, RuntimeKind::Claude);
        assert_eq!(app.ai_model, "");
        assert_eq!(saved_settings(&root)["model"], "");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The text drawn in the frame, in order.
    fn drawn_texts(output: &egui::FullOutput) -> Vec<String> {
        fn collect(shape: &Shape, out: &mut Vec<String>) {
            match shape {
                Shape::Text(text) => out.push(text.galley.text().to_owned()),
                Shape::Vec(shapes) => shapes.iter().for_each(|shape| collect(shape, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        output.shapes.iter().for_each(|clipped| collect(&clipped.shape, &mut out));
        out
    }

    #[test]
    fn a_click_on_an_effort_keeps_the_typed_model() {
        let root = std::env::temp_dir().join(format!("designllm-home-runner-effort-{}", new_id()));
        let mut app = app().with_workspace(&root);
        let ctx = Context::default();
        theme::install(&ctx);
        let typed = type_model_draft(&ctx, &mut app, "gpt-example");
        draw(&ctx, &mut app, frame(click(text_center(&typed, "보통"))));
        draw(&ctx, &mut app, frame(Vec::new()));
        // Another control closes the popover; the draft is saved then, it is not dropped.
        assert_eq!(app.ai_model, "gpt-example");
        assert_eq!(app.ai_effort, "medium");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn asking_to_close_the_window_while_typing_saves_the_typed_model() {
        let root = std::env::temp_dir().join(format!("designllm-home-runner-close-{}", new_id()));
        let mut app = app().with_workspace(&root);
        let ctx = Context::default();
        theme::install(&ctx);
        type_model_draft(&ctx, &mut app, "gpt-example");
        let mut close = frame(Vec::new());
        close.viewports.get_mut(&ViewportId::ROOT).expect("root viewport").events.push(ViewportEvent::Close);
        draw(&ctx, &mut app, close);
        assert_eq!(app.ai_model, "gpt-example", "the close request saves the draft");
        assert_eq!(saved_settings(&root)["model"], "gpt-example");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_blank_draft_saves_an_empty_model_and_clears_the_saved_one() {
        let root = std::env::temp_dir().join(format!("designllm-home-runner-blank-{}", new_id()));
        let mut app = app().with_workspace(&root);
        app.ai_model = "gpt-example".into();
        let ctx = Context::default();
        theme::install(&ctx);
        let initial = draw(&ctx, &mut app, frame(Vec::new()));
        draw(&ctx, &mut app, frame(click(text_center(&initial, "gpt-example · 높음"))));
        let opened = draw(&ctx, &mut app, frame(Vec::new()));
        draw(&ctx, &mut app, frame(click(text_center(&opened, "직접 입력…"))));
        draw(&ctx, &mut app, frame(Vec::new()));
        let press = |key: Key| Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        draw(&ctx, &mut app, frame(vec![press(Key::End)]));
        draw(&ctx, &mut app, frame((0..11).map(|_| press(Key::Backspace)).collect()));
        draw(&ctx, &mut app, frame(vec![Event::Text("  ".into())]));
        draw(&ctx, &mut app, frame(vec![press(Key::Enter)]));
        draw(&ctx, &mut app, frame(Vec::new()));
        // A blank draft is committed like any other draft: it trims to an empty model, which clears the saved one.
        assert_eq!(app.ai_model, "");
        assert_eq!(app.ai_model_label(), "모델 선택");
        assert_eq!(saved_settings(&root)["model"], "");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_size_the_template_cannot_fit_becomes_the_callout_on_the_home_screen() {
        let mut app = app();
        app.home.tile = "mobile".into();
        app.home_preset = "custom:300x400".into();
        app.screen = Screen::Editor;
        app.home.create_attempt = (app.home.tile.clone(), app.home_preset.clone());
        assert!(app.home_template_refused(&template_fit()));
        assert_eq!(app.home.create_refusal.as_deref(), Some("300 × 400"));
        assert!(app.screen == Screen::Home, "the window goes back to the home screen");
    }

    #[test]
    fn other_create_errors_are_left_to_the_status_line() {
        let mut app = app();
        let error = DltError::new(ErrorCode::InvalidArgument, "anything else");
        assert!(!app.home_template_refused(&error));
        assert!(app.home.create_refusal.is_none());
    }
}

#[cfg(test)]
mod time_label_tests {
    use super::*;

    /// 2025-11-13 00:00 UTC (a day boundary).
    const DAY: u64 = 1_763_000_000_000 - (1_763_000_000_000 % 86_400_000);

    #[test]
    fn the_template_tab_uses_the_names_and_order_of_section_12() {
        let info = |id: &str, title: &str| designllm_contracts::TemplateInfo { id: id.into(), kind: ArtifactKind::Canvas, title: title.into(), preset: None, size: None };
        assert_eq!(template_name(&info("mobile_app", "모바일 앱")), "모바일 앱 화면");
        assert_eq!(template_name(&info("poster_a3", "포스터 · 전단")), "포스터");
        assert_eq!(template_name(&info("new_one", "새 템플릿")), "새 템플릿");
        let mut ids = ["social_post", "landing_page", "zzz", "mobile_app", "poster_a3"];
        ids.sort_by_key(|id| template_order(id));
        assert_eq!(ids, ["mobile_app", "landing_page", "poster_a3", "social_post", "zzz"]);
    }

    #[test]
    fn the_size_menu_shows_each_preset_in_its_own_unit() {
        let value = |id| preset_value(designllm_contracts::frame_preset(id).expect("a preset"));
        assert_eq!(value("phone"), "390 × 844");
        assert_eq!(value("a4"), "210 × 297 mm");
        assert_eq!(value("letter"), "8.5 × 11 in");
        assert_eq!(value("invitation"), "5 × 7 in");
        assert_eq!(value("slide_16_9"), "1280 × 720");
    }

    #[test]
    fn a_change_from_the_last_minute_is_just_now() {
        assert_eq!(modified_label(DAY + 3_600_000, DAY + 3_600_000 + 30_000, 0), "방금");
    }

    #[test]
    fn today_shows_the_clock_time_in_the_local_zone() {
        let fourteen_oh_two = DAY + (14 * 60 + 2) * 60_000;
        assert_eq!(modified_label(fourteen_oh_two, DAY + 20 * 3_600_000, 0), "14:02");
        // Seoul is UTC+9: 14:02 local is 05:02 UTC.
        assert_eq!(modified_label(DAY + (5 * 60 + 2) * 60_000, DAY + 10 * 3_600_000, 540), "14:02");
    }

    #[test]
    fn yesterday_says_so_and_older_days_show_the_date() {
        let noon = DAY + 12 * 3_600_000;
        assert_eq!(modified_label(noon, DAY + 26 * 3_600_000, 0), "어제");
        // 2026-11-10 is three days before 2026-11-13.
        assert_eq!(modified_label(DAY - 3 * 86_400_000 + 3_600_000, DAY + 12 * 3_600_000, 0), "11월 10일");
        // A different year shows the full date.
        assert_eq!(modified_label(DAY - 400 * 86_400_000, DAY, 0), "2024. 10. 9.");
    }
}

#[cfg(test)]
mod top_bar_agent_tests {
    use super::*;
    use crate::rpc::RpcWorker;
    use eframe::egui::{epaint::Shape, Context, Pos2, RawInput, Rect, Vec2, ViewportId};

    /// The top bar's agent marks: with no connected agent and no job there is no mark (a waiting mark would press to nothing,
    /// GUI_DESIGN §5.5). The letters `C`/`X` are the marks' only text, so none of them is drawn.
    #[test]
    fn the_top_bar_draws_no_agent_letter_without_a_connected_agent_or_a_job() {
        fn collect(shape: &Shape, out: &mut Vec<String>) {
            match shape {
                Shape::Text(text) => out.push(text.galley.text().to_owned()),
                Shape::Vec(shapes) => shapes.iter().for_each(|shape| collect(shape, out)),
                _ => {}
            }
        }
        let (rpc, _replies, _work) = RpcWorker::detached();
        let mut app = DesktopApp::new(rpc, "작업공간".into(), None, None);
        let ctx = Context::default();
        theme::install(&ctx);
        let mut input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1440.0, 900.0))), ..Default::default() };
        input.viewports.get_mut(&ViewportId::ROOT).expect("root viewport").native_pixels_per_point = Some(1.0);
        let output = ctx.run(input, |ctx| app.draw_home(ctx));
        let mut texts = Vec::new();
        output.shapes.iter().for_each(|clipped| collect(&clipped.shape, &mut texts));
        assert!(texts.iter().any(|text| text == "열기"), "the top bar is drawn: {texts:?}");
        assert!(!texts.iter().any(|text| text == "C" || text == "X"), "no agent letter without an agent: {texts:?}");
    }
}
