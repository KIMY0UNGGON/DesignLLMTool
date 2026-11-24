//! The UI channel (docs/UI_CHANNEL.md): this window publishes what it shows and its command
//! registry, runs the commands an agent queued, and reports each result. A queued run goes through
//! the same `invoke` as a click; the channel adds no second path into the document. Its ChangeSets
//! carry the run's actor and allowed intents (`RunContext`).

use std::{
    collections::{BTreeSet, VecDeque},
    path::PathBuf,
    time::{Duration, Instant},
};

use designllm_contracts::{
    new_id, ArtifactKind, AssetRef, DltError, ErrorCode, Feature, Intent, JobStatus, Selection, Support, UiCommand, UiCompleteRequest, UiPanel,
    UiPermission, UiPublishRequest, UiPublishResult, UiRun, UiState, CAPABILITY_EXTENSION_COMPONENTS, UI_RUN_GUARD_VERSION, UI_SCREENSHOT_COMMAND,
};
use eframe::egui::{self, ColorImage, Context, Event};
use serde_json::{json, Value};

use super::{
    board::Tool,
    save_screenshot, DesktopApp, PendingAction,
};
use crate::{board as board_geometry, commands};

/// The window republishes at least this often (a view lives for 10 s by default).
const REPUBLISH: Duration = Duration::from_secs(3);

/// How many commits an agent's run started that move the selection are remembered (`UiChannel::agent_commits`). An answer that never
/// comes leaves its ticket behind, so the list stays short.
const AGENT_COMMIT_LIMIT: usize = 16;

/// Commands whose runs change the selected nodes and refuse an empty selection: the UI channel lists them enabled only with a
/// selection (backlog desktop 29).
const SELECTED_NODES_COMMANDS: &[&str] = &[
    "edit.lock", "edit.unlock", "edit.hide", "edit.show", "edit.rename", "canvas.token.bind", "canvas.token.unbind",
    "props.fill", "props.fill_remove", "props.stroke", "props.stroke_width", "props.stroke_remove", "props.corner_radius",
    "props.opacity", "props.shadow", "props.shadow_remove", "props.text_style", "props.text_align",
    "props.line_height", "props.letter_spacing", "props.vertical_align", "props.stroke_style", "props.gradient", "props.gradient_remove",
];

/// Commands whose runs change the one selected node on its page and refuse without it (dispatch.rs `selected_with_page`).
/// `props.position`, `props.size` and `props.align` take a multi-selection another way, so they are not here.
const SELECTED_ON_PAGE_COMMANDS: &[&str] = &[
    "edit.order_forward", "edit.order_backward", "edit.order_front", "edit.order_back", "edit.nudge", "props.layout_mode",
    "props.layout_direction", "props.layout_gap", "props.layout_padding", "props.layout_align", "props.layout_justify",
];

/// Why an agent run is refused a command `ui_run_supported` does not offer, in the gate's own terms (docs/UI_CHANNEL.md: a
/// command whose result the run cannot confirm safely, such as one that opens a dialog or starts work, is off for agent runs).
pub(super) const UI_RUN_UNCONFIRMED: &str =
    "대화상자를 열거나 작업을 시작하는 명령처럼 실행 결과를 확인할 수 없는 명령은 agent run에서 실행할 수 없습니다";

/// Reads and single change sets use the common dispatch path. File and job commands wait for
/// their own replies. Figure preparations carry the run through to its document commit.
/// GUI-only dialogs and commands without a confirmable outcome stay disabled.
fn ui_run_supported(id: &str) -> bool {
    if matches!(
        id,
        "add.image" | "file.save_as"
            | "file.dismiss_loss" | "view.play"
            | "home.attach" | "home.star" | "home.runner"
    ) {
        return false;
    }
    if tracked_command(id) || matches!(id, "edit.undo" | "edit.redo" | "ai.undo_change" | "ai.cancel" | "file.save" | "file.export") {
        return true;
    }
    if matches!(id, "canvas.design_system.publish" | "canvas.design_system.set") {
        return true;
    }
    let Some(spec) = commands::find(id) else { return false };
    if commands::ui_permission(spec) == UiPermission::Read {
        return true;
    }
    commands::ui_permission(spec) == UiPermission::Change && commands::one_tracked_change(id)
}

fn tracked_command(id: &str) -> bool {
    matches!(id, "home.create" | "home.use_template" | "nav.open_file" | "file.export_selection" | "web.preview"
        | "add.figure_image" | "add.figure_link" | "figure.update_link" | "add.chart"
        | "data.edit_cell" | "data.add_row" | "data.add_column" | "data.remove_row" | "data.remove_column")
}

/// The argument of an agent run that names a file. `ui_run_refusal` checks it, and the core request the run sends gets
/// the agent path rules (`mark_agent_path`).
fn agent_path_arg(id: &str) -> Option<&'static str> {
    match id {
        "nav.open_file" | "add.figure_image" | "add.chart" => Some("path"),
        "file.save" | "file.export" | "file.export_selection" => Some("target_path"),
        _ => None,
    }
}

/// Marks the core file request of an agent run that names a file, so the core keeps the agent path rules (local drive,
/// no UNC, no link or reparse point) although the window's connection is the user's. The user's own commands send no mark.
pub(super) fn mark_agent_path(run: Option<&RunContext>, method: &str, params: &mut Value) {
    if run.is_some_and(|run| agent_path_arg(&run.command_id).is_some())
        && designllm_contracts::local_path::AGENT_PATH_METHODS.contains(&method)
    {
        if let Some(params) = params.as_object_mut() {
            params.insert(designllm_contracts::local_path::AGENT_PATH_PARAM.into(), Value::Bool(true));
        }
    }
}

fn ui_run_awaits_reply(id: &str) -> bool {
    matches!(
        id,
        "edit.undo" | "edit.redo" | "ai.undo_change" | "ai.cancel" | "file.save" | "file.export"
            | "select.next" | "select.previous" | "layer.find.next" | "layer.find.previous" | "canvas.design_system.list" | "canvas.design_system.publish" | "canvas.design_system.set"
    ) || tracked_command(id) || commands::one_tracked_change(id)
}

/// The channel's own state.
pub(super) struct UiChannel {
    /// This window's view id (a new one per process).
    pub view_id: String,
    /// The token of the last publish; polls and completions must carry it.
    pub token: Option<String>,
    /// Sequence and guard version returned by the latest accepted state publish.
    pub state_seq: Option<u64>,
    pub run_guard_version: Option<u32>,
    last_state: Option<String>,
    pub last_publish: Option<Instant>,
    /// Whether the registry has been sent for the current artifact kind.
    pub commands_sent: bool,
    /// The artifact context the last published registry enables.
    commands_kind: Option<ArtifactKind>,
    commands_enabled: Option<Vec<bool>>,
    /// The items the user chose on the open document, as (view id, artifact id, node ids in the order chosen). Only the user's own
    /// selection sets it, never a reload that re-applies the window's selection or an agent's run; an AI request names these items.
    pub user_selection: Option<(String, String, Vec<String>)>,
    /// Set while the window applies a selection that is not the user's choice (`restore_selection`): a reload that re-applies the
    /// window's own selection, or the selection a commit an agent started brings. `set_selection` then leaves `user_selection`.
    pub restoring_selection: bool,
    /// Tickets of the commits an agent's run started that move the selection (`canvas.table.add`, `canvas.open_url`), oldest first.
    agent_commits: VecDeque<u64>,
    /// Screenshot runs waiting for the captured frame, oldest first.
    pub screenshot_runs: VecDeque<String>,
    /// Set when a screenshot run needs the window captured this frame.
    pub screenshot_wanted: bool,
    pub(super) pending_home_create: Option<(UiRun, Instant)>,
}

impl Default for UiChannel {
    fn default() -> Self {
        Self {
            view_id: new_id(),
            token: None,
            state_seq: None,
            run_guard_version: None,
            last_state: None,
            last_publish: None,
            commands_sent: false,
            commands_kind: None,
            commands_enabled: None,
            user_selection: None,
            restoring_selection: false,
            agent_commits: VecDeque::new(),
            screenshot_runs: VecDeque::new(),
            screenshot_wanted: false,
            pending_home_create: None,
        }
    }
}

impl UiChannel {
    /// Keeps the ticket of a commit an agent's run started, until its answer is applied.
    pub(super) fn remember_agent_commit(&mut self, ticket: u64) {
        self.agent_commits.push_back(ticket);
        if self.agent_commits.len() > AGENT_COMMIT_LIMIT {
            self.agent_commits.pop_front();
        }
    }

    /// Whether the commit `ticket` was started by an agent's run. The ticket is forgotten either way.
    pub(super) fn take_agent_commit(&mut self, ticket: u64) -> bool {
        let before = self.agent_commits.len();
        self.agent_commits.retain(|kept| *kept != ticket);
        self.agent_commits.len() != before
    }
}

/// Who a running command acts for: its ChangeSets carry this actor and these allowed intents.
#[derive(Clone)]
pub(super) struct RunContext {
    pub run_id: String,
    pub deadline: Instant,
    pub command_id: String,
    pub actor: String,
    pub allowed_intents: BTreeSet<Intent>,
    pub await_rpc_reply: bool,
    pub mutation_attempted: bool,
    pub rpc_ticket: Option<u64>,
    pub enqueue_error: Option<DltError>,
    pub expected: Option<designllm_contracts::UiRunExpected>,
    pub prepared_asset: Option<String>,
}

impl RunContext {
    pub(super) fn prepare_error(&self, mut error: DltError) -> DltError {
        if let Some(hash) = &self.prepared_asset {
            error.message.push_str(&format!(" (작업공간에 준비된 그림 자산: {hash})"));
            error.affected_ids.push(hash.clone());
        }
        error
    }
}

#[derive(Clone, Copy)]
enum CommandScope { Always, Home, Editor, Canvas, CanvasFigure, CanvasSlides, CanvasSlidesFigure, TextSetSupported, TextStyleSupported, Web, Slides, Flow, Figure }

fn command_scope(id: &str) -> Option<CommandScope> {
    match id {
        "nav.home" | "nav.new" | "nav.open_project" | "nav.open_file" | "nav.focus_request" | "home.runner"
            | "nav.toggle_left" | "nav.toggle_right" | "nav.toggle_panels" => Some(CommandScope::Always),
        "nav.ai" | "nav.tab" | "nav.page"
            | "view.zoom_in" | "view.zoom_out" | "view.zoom_100" | "view.zoom_fit" | "view.zoom_selection"
            | "view.zoom_set" | "view.zoom_page" | "view.toggle_guides" | "view.toggle_snap"
            | "select.node" | "select.many" | "select.clear" | "select.next" | "select.previous" | "layer.find"
            | "layer.find.next" | "layer.find.previous" | "edit.undo" | "edit.redo" | "edit.delete" | "edit.rename"
            | "add.arrow" | "add.pen" | "add.pencil" | "add.memo" | "add.image" | "add.figure_link"
            | "ai.request" | "ai.cancel" | "ai.retry"
            | "ai.undo_change" | "ai.toggle_intent" | "ai.deliver_as" | "proposal.select" | "proposal.apply"
            | "proposal.discard" | "file.save" | "file.save_as" | "file.export" | "file.overwrite"
            | "file.dismiss_loss" | "figure.update_link" | "figure.open_source" | "canvas.design_system.list" => Some(CommandScope::Editor),
        "edit.copy" | "edit.paste" | "edit.duplicate" | "edit.order_forward" | "edit.order_backward"
            | "edit.order_front" | "edit.order_back" | "edit.nudge" | "add.text" | "add.rect" | "add.oval"
            | "add.line" | "add.frame" | "add.page" | "add.slide" | "add.icon" | "props.position" | "props.size" | "props.align"
            | "props.distribute" | "layer.move_to" | "fix.grow_to_min" | "fix.darken_text" | "fix.grow_box"
            | "fix.shrink_text" | "fix.grow_parent" | "fix.move_into_page" | "page.remove" | "page.reorder"
            | "page.notes" | "layer.move" | "props.page_size" | "props.rename_page"
            | "props.shadow" | "props.shadow_remove" | "design_theme.apply" => Some(CommandScope::CanvasSlides),
        "edit.copy_properties" | "edit.paste_properties" | "props.fill" | "props.fill_remove" | "props.stroke"
            | "props.stroke_width" | "props.stroke_remove" | "props.text_align" => Some(CommandScope::CanvasSlidesFigure),
        "props.opacity" | "props.corner_radius" | "props.line_height" | "props.letter_spacing"
            | "props.vertical_align" | "props.stroke_style" => Some(CommandScope::CanvasFigure),
        "text.set" => Some(CommandScope::TextSetSupported),
        "props.text_style" => Some(CommandScope::TextStyleSupported),
        "edit.group" | "edit.ungroup" | "edit.stack" | "edit.lock" | "edit.unlock" | "edit.hide" | "edit.show"
            | "canvas.table.add" | "canvas.table.add_row" | "canvas.table.remove_row" | "canvas.table.add_column"
            | "canvas.table.remove_column" | "canvas.table.set_header" | "canvas.table.set_column_widths"
            | "table.merge_cells" | "table.unmerge_cell" | "add.path" | "props.size_mode"
            | "props.gradient" | "props.gradient_remove"
            | "props.image_fit" | "props.image_alt" | "props.path" | "canvas.token.put" | "canvas.token.remove"
            | "canvas.token.mode" | "canvas.token.bind" | "canvas.token.unbind"
            | "canvas.design_system.publish" | "canvas.design_system.set" | "props.layout_mode"
            | "props.layout_direction" | "props.layout_justify" | "props.layout_align" | "props.layout_gap"
            | "props.layout_padding" | "props.grid" | "props.grid_span" | "props.padding_sides"
            | "props.align_self" | "props.variant_style" | "canvas.open_url" | "canvas.interaction.set"
            | "canvas.interaction.remove" | "canvas.image.replace" | "canvas.image.crop" | "canvas.board_memos.set"
            | "canvas.page_sections.set" | "canvas.guides.set" | "canvas.guides.clear" | "page.component"
            | "component.instantiate" | "component.detach" | "repeat.create" | "repeat.bind" | "repeat.clear"
            | "repeat.condition_set" | "repeat.condition_clear" | "text.list" | "file.export_selection" | "view.play" => Some(CommandScope::Canvas),
        "add.table" | "add.paragraph" | "doc.paragraph_format" | "doc.page_setup" | "doc.named_style"
            | "doc.table_columns" | "text.replace" | "text.style_range" => Some(CommandScope::Flow),
        "data.edit_cell" | "add.figure_image" | "data.add_column" | "data.add_row" | "data.remove_row"
            | "data.remove_column" | "add.figure_panel" | "add.diagram_node" | "figure.add_equation"
            | "add.connector" | "add.evidence" | "add.csv" | "add.chart" | "figure.arrange_panels"
            | "figure.remove_panel" | "figure.reorder_panels" | "figure.caption" | "figure.panel_label"
            | "figure.plot" | "figure.plot_display" | "figure.style" | "figure.toggle_sheet" | "figure.axis_label"
            | "figure.axis_ticks" | "figure.put_dataset" | "figure.put_evidence" | "figure.diagram_provenance"
            | "figure.common_legend" | "figure.output_profile" | "figure.visual_review" | "figure.image_transform"
            | "figure.image_calibration" => Some(CommandScope::Figure),
        "slides.theme" => Some(CommandScope::Slides),
        "web.replace_source" | "web.mapped_edit" | "web.open_file" | "web.refresh" | "web.preview" => Some(CommandScope::Web),
        "home.create" | "home.attach" | "home.view" | "home.starred_only" | "home.star" | "home.mode"
            | "home.design_system" | "home.tile" | "home.tab" | "home.preset" | "home.search" | "home.use_template"
            | "home.workspace" => Some(CommandScope::Home),
        _ => None,
    }
}

impl DesktopApp {
    pub(super) fn command_enabled_for_current_kind(&self, id: &str) -> bool {
        let editor = self.screen_is_editor();
        let kind = self.active.as_ref().map(|active| active.kind);
        let in_scope = match command_scope(id) {
            Some(CommandScope::Always) => true,
            // Existing GUI callers may prepare or create from home while an editor is open.
            Some(CommandScope::Home) => true,
            Some(CommandScope::Editor) => editor,
            Some(CommandScope::Canvas) => editor && kind == Some(ArtifactKind::Canvas),
            Some(CommandScope::CanvasFigure) => editor && matches!(kind, Some(ArtifactKind::Canvas | ArtifactKind::Figure)),
            Some(CommandScope::CanvasSlides) => editor && matches!(kind, Some(ArtifactKind::Canvas | ArtifactKind::Slides)),
            Some(CommandScope::CanvasSlidesFigure) => editor && matches!(kind, Some(ArtifactKind::Canvas | ArtifactKind::Slides | ArtifactKind::Figure)),
            Some(CommandScope::TextSetSupported) => editor && matches!(kind, Some(ArtifactKind::Canvas | ArtifactKind::Slides | ArtifactKind::Figure | ArtifactKind::Docx | ArtifactKind::Hwpx)),
            Some(CommandScope::TextStyleSupported) => editor && matches!(kind, Some(ArtifactKind::Canvas | ArtifactKind::Slides | ArtifactKind::Figure | ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp)),
            Some(CommandScope::Web) => editor && kind == Some(ArtifactKind::WebCode),
            Some(CommandScope::Slides) => editor && kind == Some(ArtifactKind::Slides),
            Some(CommandScope::Flow) => editor && matches!(kind, Some(ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp)),
            Some(CommandScope::Figure) => editor && kind == Some(ArtifactKind::Figure),
            None => false,
        };
        let direct_gui_figure_openers = matches!(id, "add.figure_image" | "add.evidence" | "add.csv" | "add.chart");
        if !in_scope && id != "canvas.design_system.set" && id != "web.preview" && !direct_gui_figure_openers { return false; }
        let canvas_editor = self.screen_is_editor() && self.active.as_ref().is_some_and(|active| active.kind == ArtifactKind::Canvas);
        let web_editor = self.screen_is_editor() && self.active.as_ref().is_some_and(|active| active.kind == ArtifactKind::WebCode);
        let flow_editor = self.screen_is_editor()
            && self.active.as_ref().is_some_and(|active| matches!(active.kind, ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp));
        let table_context_current = self.canvas_table_context_current();
        let table_writeable = table_context_current && self.feature_write_supported(Feature::Tables);
        let table_editable = table_context_current && self.feature_edit_supported(Feature::Tables);
        let selected_table = table_editable && self.selected_canvas_table_node().is_some();
        match id {
            "edit.lock" | "edit.unlock" | "edit.hide" | "edit.show" | "canvas.open_url" | "canvas.interaction.set" | "canvas.interaction.remove" => canvas_editor,
            "canvas.guides.set" | "canvas.guides.clear" => canvas_editor,
            "slides.theme" => self.screen_is_editor() && self.active.as_ref().is_some_and(|active| active.kind == ArtifactKind::Slides),
            "page.component" | "component.instantiate" | "component.detach" => {
                canvas_editor && self.context.as_ref()
                    .and_then(|context| context.capability_extensions.get(CAPABILITY_EXTENSION_COMPONENTS))
                    .is_some_and(|support| support.read == Support::Supported && support.edit == Support::Supported)
            }
            // The run finds the nodes its arguments name; what the change may do (the table, the bindings, the condition) is the
            // engine's answer. The selection does not enable these commands, so the published registry does not follow it.
            "repeat.create" | "repeat.bind" | "repeat.clear" | "repeat.condition_set" | "repeat.condition_clear" => canvas_editor,
            "add.table" => flow_editor,
            "canvas.table.add" => canvas_editor && table_writeable,
            "canvas.table.add_row" | "canvas.table.remove_row" | "canvas.table.add_column" | "canvas.table.remove_column" | "canvas.table.set_header" | "canvas.table.set_column_widths" => {
                canvas_editor && table_editable && selected_table
            }
            // A merge is made from selected cells, so the table node itself is not the selection here.
            "table.merge_cells" | "table.unmerge_cell" => canvas_editor && table_editable,
            "select.next" | "select.previous" => self.screen_is_editor() && self.has_selectable_layers(),
            "layer.find.next" | "layer.find.previous" => flow_editor || canvas_editor,
            id if super::web_runtime::WEB_RUNTIME_COMMANDS.contains(&id) => self.web_runtime_unavailable.is_none(),
            "text.list" => canvas_editor && self.canvas_list_support().is_some_and(|(read, edit)| {
                matches!(read, Support::Supported | Support::Approximate | Support::PreserveOnly)
                    && matches!(edit, Support::Supported | Support::Approximate)
            }),
            "canvas.token.put" | "canvas.token.remove" | "canvas.token.mode" => {
                canvas_editor
                    && self.canvas_token_support().is_some_and(|(_, edit)| matches!(edit, Support::Supported | Support::Approximate))
                    && !self.canvas_token_form_stale()
            }
            "canvas.token.bind" | "canvas.token.unbind" | "canvas.design_system.publish" => {
                canvas_editor
                    && self.canvas_token_support().is_some_and(|(_, edit)| matches!(edit, Support::Supported | Support::Approximate))
                    && self.canvas_token_snapshot_current()
            }
            // A WebCode project links too. Its engine declares the design-system feature, not the Canvas token extension.
            // Word and HWPX documents link their named styles (task 49d, with direct_formatting); HWP does not.
            "canvas.design_system.set" => {
                let named_style_editor = flow_editor && self.active.as_ref().is_some_and(|active| matches!(active.kind, ArtifactKind::Docx | ArtifactKind::Hwpx));
                (canvas_editor
                    && self.canvas_token_support().is_some_and(|(_, edit)| matches!(edit, Support::Supported | Support::Approximate))
                    && self.canvas_token_snapshot_current()
                    && !self.artifact.as_ref().is_some_and(|artifact| artifact.publishes_design_system))
                    || ((web_editor || named_style_editor) && self.canvas_token_snapshot_current())
            }
            // A Canvas or Slides editor with a selection: the one judgement the menu, the run and the registry listing share.
            "view.zoom_selection" => {
                self.screen_is_editor()
                    && self.active.as_ref().is_some_and(|active| matches!(active.kind, ArtifactKind::Canvas | ArtifactKind::Slides))
                    && !self.selected_nodes.is_empty()
            }
            _ => in_scope || direct_gui_figure_openers,
        }
    }

    fn ui_command_enabled_for_current_kind(&self, id: &str) -> bool {
        // Let the desktop return the runtime's reason. The core rejects disabled entries before delivery.
        if id == "web.preview" {
            return self.screen_is_editor() && self.active.as_ref().is_some_and(|active| active.kind == ArtifactKind::WebCode);
        }
        self.command_enabled_for_current_kind(id)
            && match id {
                id if matches!(command_scope(id), Some(CommandScope::Home)) => !self.screen_is_editor(),
                "add.chart" | "add.csv" | "add.evidence" | "add.figure_image" | "data.edit_cell" | "data.add_row" | "data.add_column" | "data.remove_row" | "data.remove_column" =>
                    self.screen_is_editor() && self.active.as_ref().is_some_and(|active| active.kind == ArtifactKind::Figure),
                "add.figure_link" => self.screen_is_editor() && self.feature_edit_supported(Feature::FigureLinks)
                    && self.artifacts.iter().any(|item| item.kind == ArtifactKind::Figure),
                "figure.update_link" => self.screen_is_editor() && self.feature_edit_supported(Feature::FigureLinks),
                "figure.open_source" => self.artifact.as_ref().is_some_and(|artifact| !artifact.figure_links.is_empty()),
                "canvas.image.replace" => self.image_replacement_check().is_ok(),
                "file.export_selection" => self.screen_is_editor() && !self.selected_nodes.is_empty()
                    && self.active.as_ref().is_some_and(|active| active.kind == ArtifactKind::Canvas),
                "file.save" => self.screen_is_editor() && self.feature_write_supported(Feature::NativeRoundtrip),
                "file.export" => self.screen_is_editor() && self.active.as_ref().is_some_and(|active| !super::chrome::export_formats(active.kind).is_empty()),
                "ai.cancel" => self.can_cancel_current_job(),
                // The same targets the commands' runs take (dispatch.rs), so the listing and the run agree.
                "props.image_fit" => self.image_fit_target().is_ok(),
                "props.image_alt" => self.image_alt_target().is_ok(),
                "edit.delete" => self.delete_targets().is_ok(),
                // The runs take the selected nodes and refuse an empty selection (dispatch.rs `selected_ids`, `style_change`).
                id if SELECTED_NODES_COMMANDS.contains(&id) => !self.selected_nodes.is_empty(),
                // The runs take the one selected node on its page and refuse without it (dispatch.rs `selected_with_page`).
                id if SELECTED_ON_PAGE_COMMANDS.contains(&id) => self.selected_on_page().is_some(),
                _ => command_scope(id).is_some(),
            }
    }

    fn can_cancel_current_job(&self) -> bool {
        self.screen_is_editor()
            && self.active.as_ref().zip(self.job.as_ref()).is_some_and(|(active, job)| {
                active.id == job.artifact_id
                    && matches!(job.status, JobStatus::Queued | JobStatus::Running | JobStatus::Validating | JobStatus::Failed | JobStatus::Conflict)
            })
    }

    fn artifact_write_has_pending_edits(&self, artifact_id: &str) -> bool {
        self.pending.values().any(|pending| {
            pending.artifact_id.as_deref() == Some(artifact_id)
                && (pending.apply.is_some()
                    || matches!(pending.method.as_str(), "changes.apply" | "history.undo" | "history.redo" | "history.undo_change" | "figure.dataset.edit" | "proposal.apply")
                    || matches!(
                        &pending.action,
                        PendingAction::ImageAsset { .. }
                            | PendingAction::FigureImageAsset { .. }
                            | PendingAction::PdfAssetImport { .. }
                            | PendingAction::PdfEvidenceImport
                    ))
        }) || self.held_edits.iter().any(|held| held.apply.artifact_id == artifact_id)
    }

    fn visible_pages(&self) -> Vec<u32> {
        let Some(active) = self.active.as_ref() else {
            return Vec::new();
        };
        if !self.screen_is_editor() || active.kind == designllm_contracts::ArtifactKind::WebCode {
            return Vec::new();
        }
        let Some(board_rect) = self.board_rect else {
            return Vec::new();
        };
        let view = self.view_ref();
        let origin = board_rect.min + view.pan;
        let mut pages: Vec<u32> = board_geometry::arrange(active.kind, &self.page_sizes(), view.zoom)
            .into_iter()
            .filter(|slot| board_geometry::visible(board_geometry::to_screen(slot.world, origin, view.zoom), board_rect))
            .map(|slot| slot.page_index)
            .collect();
        pages.sort_unstable();
        pages
    }

    /// The state the window shows, as the channel publishes it.
    pub(super) fn ui_state(&self) -> UiState {
        let view = self.view_ref();
        let screen = if self.screen_is_editor() { "editor" } else { "home" };
        let tab = super::toolbox::left_tab_name(self.shown_left_tab());
        let selection = self.local_ui_selection();
        let mut state = UiState {
            screen: screen.into(),
            artifact_id: self.active.as_ref().map(|item| item.id.clone()),
            revision: self.active.as_ref().map(|item| item.revision),
            page_index: self.active.as_ref().map(|_| view.page_index),
            selection,
            zoom_milli: self.active.as_ref().map(|_| (view.zoom * 1000.0).round() as u32),
            viewport: None,
            panels: vec![
                UiPanel {
                    id: "sidebar".into(),
                    open: view.left_open,
                    tab: Some(tab.into()),
                    width_px: Some(view.left_width.round() as u32),
                },
                UiPanel {
                    id: "properties".into(),
                    open: view.right_open,
                    tab: None,
                    width_px: Some(view.right_width.round() as u32),
                },
            ],
            tool: Some(
                match view.tool {
                    Tool::Select => "select",
                    Tool::Hand => "hand",
                    Tool::Memo => "memo",
                }
                .into(),
            ),
            detail: json!({
                "runtime": self.ai_runtime,
                "job_status": self.job.as_ref().map(|job| format!("{:?}", job.status)),
                "job_id": self.job.as_ref().map(|job| job.id.as_str()),
                "job_attempt_id": self.job.as_ref().map(|job| job.attempt_id.as_str()),
                "visible_pages": self.visible_pages(),
            }),
        };
        if self.ui_test.is_some() {
            // The state tells an agent that this window takes synthetic input (task 69).
            state.detail["ui_test_input"] = json!(true);
        }
        state
    }

    fn local_ui_selection(&self) -> Selection {
        let mut node_ids = self.local_selection.as_ref().map_or_else(|| self.selected_nodes.clone(), |selection| selection.node_ids.clone());
        let local_text_range = self.local_selection.as_ref().and_then(|selection| selection.text_range.clone());
        let context_text_range = self.context.as_ref().filter(|context| {
            self.active.as_ref().is_some_and(|active| {
                active.id == context.artifact_id
                    && active.revision == context.revision
                    && self.artifact.as_ref().is_some_and(|artifact| {
                        artifact.id == context.artifact_id && artifact.revision == context.revision
                    })
            })
        }).and_then(|context| context.selection.text_range.clone());
        let text_range = if self.local_selection.is_some() { local_text_range } else { context_text_range };

        if self.local_selection.as_ref().is_none_or(|selection| !selection.node_ids.is_empty()) {
            if let Some(primary) = self.selected_node.as_ref() {
                if let Some(index) = node_ids.iter().position(|id| id == primary) {
                    let primary = node_ids.remove(index);
                    node_ids.insert(0, primary);
                } else {
                    node_ids.insert(0, primary.clone());
                }
            }
        }
        Selection { node_ids, text_range }
    }

    fn selection_context_pending(&self) -> bool {
        self.pending.values().any(|pending| matches!(pending.method.as_str(), "selection.set" | "context.read"))
            || self.reselect.as_ref().is_some_and(|selection| selection.text_range.is_some())
    }

    fn selection_matches_context(&self) -> bool {
        !self.selection_context_pending()
            && self.context.as_ref().is_some_and(|context| same_selection(&self.local_ui_selection(), &context.selection))
    }

    /// The whole command registry. `enabled` reflects both wiring and the current artifact kind.
    pub(super) fn ui_commands(&self) -> Vec<UiCommand> {
        let kind = self.active.as_ref().map_or(ArtifactKind::Canvas, |active| active.kind);
        let mut registry: Vec<UiCommand> = commands::COMMANDS
            .iter()
            .map(|spec| UiCommand {
                id: spec.id.into(),
                label: spec.label.into(),
                args_schema: serde_json::from_str(spec.args).unwrap_or_else(|_| json!({ "type": "object" })),
                intents: commands::ui_intents_for(spec, kind),
                permission: commands::ui_permission(spec),
                enabled: commands::available(spec.id) && self.ui_command_enabled_for_current_kind(spec.id) && ui_run_supported(spec.id),
                shortcut: (!spec.shortcut.is_empty()).then(|| spec.shortcut.to_owned()),
            })
            .collect();
        registry.extend(self.ui_test_commands());
        registry
    }

    /// Publishes the state when it changed, and at least every few seconds so that the view stays
    /// alive. The command registry follows the active kind and every command availability change.
    pub(super) fn publish_ui_if_due(&mut self) {
        let state = self.ui_state();
        let fingerprint = serde_json::to_string(&state).unwrap_or_default();
        let command_kind = if self.screen_is_editor() { self.active.as_ref().map(|active| active.kind) } else { None };
        let commands_enabled: Vec<bool> = commands::COMMANDS.iter().map(|spec| self.ui_command_enabled_for_current_kind(spec.id)).collect();
        let registry_changed = !self.ui.commands_sent
            || self.ui.commands_kind != command_kind
            || self.ui.commands_enabled.as_ref() != Some(&commands_enabled);
        let due = self.ui.last_publish.is_none_or(|at| at.elapsed() >= REPUBLISH);
        if !due && !registry_changed && self.ui.last_state.as_deref() == Some(fingerprint.as_str()) {
            return;
        }
        let commands = registry_changed.then(|| self.ui_commands());
        let request = UiPublishRequest {
            view_id: self.ui.view_id.clone(),
            view_token: self.ui.token.clone(),
            state,
            commands,
            run_guard_version: Some(UI_RUN_GUARD_VERSION),
            ttl_ms: None,
        };
        self.ui.last_state = Some(fingerprint);
        self.ui.last_publish = Some(Instant::now());
        self.ui.commands_sent = true;
        self.ui.commands_kind = command_kind;
        self.ui.commands_enabled = Some(commands_enabled);
        if let Ok(params) = serde_json::to_value(request) {
            self.call("ui.state.publish", params, PendingAction::UiPublish);
        }
    }

    /// Runs the commands that the core handed over in an `events.poll` answer.
    pub(super) fn take_ui_runs(&mut self, batch: &Value) {
        let Some(runs) = batch.get("ui_runs").cloned() else {
            return;
        };
        let Ok(runs) = serde_json::from_value::<Vec<UiRun>>(runs) else {
            return;
        };
        for run in runs {
            self.execute_ui_run(run);
        }
    }

    /// Why a queued run may not run, or None. The checks follow docs/UI_CHANNEL.md: the command
    /// exists and is built, the run's permission covers its class, the arguments match the schema,
    /// and the run allows the intents the command changes.
    pub(super) fn ui_run_refusal(&self, run: &UiRun) -> Option<DltError> {
        let state_relative_read = matches!(run.command_id.as_str(), "select.next" | "select.previous");
        let Some(spec) = commands::find(&run.command_id) else {
            return Some(DltError::new(ErrorCode::NotFound, format!("알 수 없는 명령: {}", run.command_id)));
        };
        if !commands::available(spec.id) {
            return Some(DltError::new(ErrorCode::UnsupportedOperation, format!("{}: 아직 연결되지 않은 명령입니다", spec.id)));
        }
        if state_relative_read {
            if let Some(error) = self.ui_run_state_refusal(run) {
                return Some(error);
            }
            if !self.selection_matches_context() {
                return Some(DltError::new(
                    ErrorCode::RevisionConflict,
                    "선택 상태가 최신 내용과 동기화되지 않아 실행하지 않았습니다. 선택 상태가 갱신된 뒤 다시 요청해 주세요",
                ));
            }
        }
        if spec.id == "web.preview" {
            if let Some(reason) = self.web_runtime_refusal(spec.id) {
                return Some(DltError::new(ErrorCode::Unavailable, reason));
            }
        }
        if !self.ui_command_enabled_for_current_kind(spec.id) {
            return Some(DltError::new(ErrorCode::UnsupportedOperation, "현재 작업물에서는 사용할 수 없는 명령입니다"));
        }
        if !ui_run_supported(spec.id) {
            return Some(DltError::new(ErrorCode::UnsupportedOperation, UI_RUN_UNCONFIRMED));
        }
        if !run.permission.allows(commands::ui_permission(spec)) {
            return Some(DltError::new(ErrorCode::InvalidArgument, format!("{}: 실행 권한이 부족합니다", spec.id)));
        }
        if let Err(message) = commands::validate(spec, &run.args) {
            return Some(DltError::new(ErrorCode::InvalidArgument, message));
        }
        if spec.id == "home.create" && ["attachments", "source_refs", "figure_refs"].iter().any(|key| run.args.get(*key).is_some()) {
            return Some(DltError::new(ErrorCode::InvalidArgument, "첨부나 그림 참조가 있는 작업은 job.start로 요청해 주세요"));
        }
        if let Some(key) = agent_path_arg(spec.id) {
            let raw = run.args[key].as_str().filter(|raw| !raw.trim().is_empty());
            // file.save and file.export refuse a missing target_path below, with their own reason.
            // add.chart without a path opens its dialog (no file is named).
            let missing_reported_later = raw.is_none() && matches!(spec.id, "file.save" | "file.export" | "add.chart");
            if !missing_reported_later && !raw.is_some_and(|raw| {
                !raw.starts_with(r"\\?\") && designllm_contracts::local_path::is_local_file_path(raw)
            }) {
                return Some(DltError::new(ErrorCode::InvalidArgument, format!("로컬 파일의 절대 경로를 {key}로 지정해 주세요")));
            }
        }
        if let Some(error) = self.ui_run_review_refusal(run) {
            return Some(error);
        }
        let kind = self.active.as_ref().map_or(ArtifactKind::Canvas, |active| active.kind);
        if !commands::ui_intents_for(spec, kind).is_subset(&run.allowed_intents) {
            return Some(DltError::new(ErrorCode::ContentLocked, format!("{}: 허용되지 않은 변경 종류입니다", spec.id)));
        }
        // A cell's text edited in place is not stored yet: the merge would leave it out (the window's own merge waits for it).
        if matches!(spec.id, "table.merge_cells" | "table.unmerge_cell") && self.inline_text_edit.is_some() {
            return Some(DltError::new(ErrorCode::TargetBusy, super::table_merge::EDITING_REFUSAL));
        }
        if commands::ui_permission(spec) != UiPermission::Read && !state_relative_read {
            if let Some(error) = self.ui_run_state_refusal(&run) {
                return Some(error);
            }
            if self.ui_run_uses_context_selection(&run) && !self.selection_matches_context() {
                return Some(DltError::new(
                    ErrorCode::RevisionConflict,
                    "글 선택이 아직 최신 상태와 동기화되지 않아 실행하지 않았습니다. 선택 상태가 갱신된 뒤 다시 요청해 주세요",
                ));
            }
        }
        if matches!(spec.id, "nav.open_file" | "home.create" | "home.use_template") {
            if self.pending_switch.is_some() || self.inline_text_edit.is_some()
                || self.video_insert.as_ref().is_some_and(|pending| pending.worker_cancel.is_some())
            {
                return Some(DltError::new(ErrorCode::TargetBusy, "현재 편집이나 전환 확인을 마친 뒤 다시 요청해 주세요"));
            }
            if let Some(reason) = self.figure_switch_warning() {
                return Some(DltError::new(ErrorCode::TargetBusy, reason));
            }
        }
        if spec.id == "file.save" {
            if !run.args.get("target_path").and_then(Value::as_str).is_some_and(|path| !path.trim().is_empty()) {
                return Some(DltError::new(ErrorCode::InvalidArgument, "저장할 target_path를 지정해 주세요"));
            }
            let Some(artifact_id) = run.expected.as_ref().and_then(|expected| expected.artifact_id.as_deref()) else {
                return Some(DltError::new(ErrorCode::RevisionConflict, "저장할 작업물 상태를 확인할 수 없습니다"));
            };
            if self.artifact_write_has_pending_edits(artifact_id) {
                return Some(DltError::new(ErrorCode::RevisionConflict, "작업물의 편집이 처리 중입니다. 최신 상태에서 다시 저장해 주세요"));
            }
        }
        if matches!(spec.id, "file.export" | "file.export_selection") {
            if !run.args.get("target_path").and_then(Value::as_str).is_some_and(|path| !path.trim().is_empty()) {
                return Some(DltError::new(ErrorCode::InvalidArgument, "내보낼 target_path를 지정해 주세요"));
            }
            let Some(format) = run.args.get("format").and_then(Value::as_str) else {
                return Some(DltError::new(ErrorCode::InvalidArgument, "내보낼 format을 지정해 주세요"));
            };
            if !self.export_format_supported(format) {
                return Some(DltError::new(ErrorCode::UnsupportedOperation, "현재 작업물에서 지원하지 않는 내보내기 형식입니다"));
            }
            if run.args.get("allowed_losses").is_some_and(|losses| {
                !losses.is_null() && !losses.as_array().is_some_and(|losses| losses.iter().all(Value::is_string))
            }) {
                return Some(DltError::new(ErrorCode::InvalidArgument, "allowed_losses는 진단 코드 문자열의 배열이어야 합니다"));
            }
            if run.args.get("scope").and_then(Value::as_str) == Some("selection") && self.selected_nodes.is_empty() {
                return Some(DltError::new(ErrorCode::InvalidArgument, "선택된 항목이 없어 선택 범위를 내보낼 수 없습니다"));
            }
            let Some(artifact_id) = run.expected.as_ref().and_then(|expected| expected.artifact_id.as_deref()) else {
                return Some(DltError::new(ErrorCode::RevisionConflict, "내보낼 작업물 상태를 확인할 수 없습니다"));
            };
            if self.artifact_write_has_pending_edits(artifact_id) {
                return Some(DltError::new(ErrorCode::RevisionConflict, "작업물의 편집이 처리 중입니다. 최신 상태에서 다시 내보내 주세요"));
            }
        }
        if spec.id == "ai.cancel" {
            let Some(job_id) = run.args.get("job_id").and_then(Value::as_str) else {
                return Some(DltError::new(ErrorCode::InvalidArgument, "취소할 job_id를 지정해 주세요"));
            };
            let Some(attempt_id) = run.args.get("attempt_id").and_then(Value::as_str) else {
                return Some(DltError::new(ErrorCode::InvalidArgument, "취소할 attempt_id를 지정해 주세요"));
            };
            let Some(job) = self.job.as_ref() else {
                return Some(DltError::new(ErrorCode::RevisionConflict, "현재 화면의 작업이 바뀌어 취소하지 않았습니다"));
            };
            if job.id != job_id || job.attempt_id != attempt_id || self.active.as_ref().is_none_or(|active| active.id != job.artifact_id) {
                return Some(DltError::new(ErrorCode::RevisionConflict, "취소할 작업이 현재 화면의 작업과 달라 실행하지 않았습니다. 최신 상태를 다시 읽어 주세요"));
            }
        }
        None
    }

    /// 49w: an agent's run may not open a review gate; the user's own click may (`designllm_bridge::review_gate`). The run is
    /// judged by the operation its command builds, read by the command's own code (`r07_review_change`, `evidence_operation`),
    /// so a key or a type the contract would not take is read as the command reads it. Arguments the command cannot read are
    /// refused here as it would refuse them: the gate never lets a run through that it could not judge.
    fn ui_run_review_refusal(&self, run: &UiRun) -> Option<DltError> {
        let operation = match run.command_id.as_str() {
            "figure.visual_review" => self.r07_review_change(&run.args),
            "figure.put_evidence" => super::kind_commands::evidence_operation(&run.args).map(Some),
            _ => return None,
        };
        match operation {
            Ok(operation) => designllm_bridge::review_gate::operations_refusal(operation.as_ref()),
            Err(message) => Some(DltError::new(ErrorCode::InvalidArgument, message)),
        }
    }

    fn ui_run_state_refusal(&self, run: &UiRun) -> Option<DltError> {
        self.ui_expected_refusal(run.expected.as_ref())
    }

    /// The window's state now, in the terms of a run's expected state.
    fn ui_run_expected_now(&self) -> Option<designllm_contracts::UiRunExpected> {
        let state = self.ui_state();
        Some(designllm_contracts::UiRunExpected {
            state_seq: self.ui.state_seq?,
            screen: state.screen,
            artifact_id: state.artifact_id,
            revision: state.revision,
            selection: state.selection,
            page_index: state.page_index,
        })
    }

    pub(super) fn ui_expected_refusal(&self, expected: Option<&designllm_contracts::UiRunExpected>) -> Option<DltError> {
        let stale = || {
            DltError::new(
                ErrorCode::RevisionConflict,
                "화면 상태가 바뀌었거나 확인할 수 없어 대기 중인 명령을 실행하지 않았습니다. 최신 상태에서 다시 요청해 주세요",
            )
        };
        if self.ui.run_guard_version != Some(UI_RUN_GUARD_VERSION) {
            return Some(stale());
        }
        let Some(expected) = expected else {
            return Some(stale());
        };
        if self.ui.state_seq != Some(expected.state_seq) {
            return Some(stale());
        }
        let current = self.ui_state();
        if expected.screen != current.screen
            || expected.artifact_id != current.artifact_id
            || expected.revision != current.revision
            || expected.page_index != current.page_index
            || !same_selection(&expected.selection, &current.selection)
        {
            return Some(stale());
        }
        if let Some(artifact_id) = expected.artifact_id.as_deref() {
            let Some(revision) = expected.revision else { return Some(stale()) };
            let snapshots_match = self.active.as_ref().is_some_and(|active| active.id == artifact_id && active.revision == revision)
                && self.artifact.as_ref().is_some_and(|artifact| artifact.id == artifact_id && artifact.revision == revision)
                && self.context.as_ref().is_some_and(|context| context.artifact_id == artifact_id && context.revision == revision);
            if !snapshots_match {
                return Some(stale());
            }
        } else if expected.revision.is_some() {
            return Some(stale());
        }
        None
    }

    fn ui_run_uses_context_selection(&self, run: &UiRun) -> bool {
        let has_arg = |key: &str| run.args.get(key).and_then(Value::as_str).is_some();
        match run.command_id.as_str() {
            "text.replace" | "text.style_range" | "doc.paragraph_format" => !has_arg("paragraph_id"),
            "canvas.token.bind" | "canvas.token.unbind" | "canvas.image.replace" | "file.export_selection" => true,
            "doc.page_setup" => {
                let section_id = run.args.get("section_id").and_then(Value::as_str);
                !section_id.is_some_and(|section_id| {
                    self.artifact.as_ref().is_some_and(|artifact| match &artifact.body {
                        designllm_contracts::ArtifactBody::Flow(document) => document.sections.iter().any(|section| section.id == section_id),
                        _ => false,
                    })
                })
            }
            "add.table" | "add.figure_link" | "add.image" => true,
            _ => false,
        }
    }

    /// Runs one queued command (or starts a screenshot) and completes it with the outcome.
    fn execute_ui_run(&mut self, mut run: UiRun) {
        if run.command_id == UI_SCREENSHOT_COMMAND {
            self.ui.screenshot_runs.push_back(run.run_id);
            self.ui.screenshot_wanted = true;
            return;
        }
        if super::ui_test_input::is_ui_test_command(&run.command_id) {
            self.execute_ui_test_run(run);
            return;
        }
        if let Some(error) = self.ui_run_refusal(&run) {
            self.complete_ui_run(&run.run_id, None, Some(error));
            return;
        }
        // Leave time for delivery and the RPC that reports the outcome.
        let deadline = Instant::now() + Duration::from_millis(run.timeout_ms - (run.timeout_ms / 2).min(2_000));
        if run.command_id == "home.create" && run.args["prompt"].as_str().is_some_and(|p| !p.trim().is_empty()) {
            if self.ui.pending_home_create.is_some() {
                self.complete_ui_run(&run.run_id, None, Some(DltError::new(ErrorCode::TargetBusy, "다른 모델 실행 요청이 확인을 기다리고 있습니다")));
                return;
            }
            for (key, fallback) in [("runtime", &self.ai_runtime), ("model", &self.ai_model), ("effort", &self.ai_effort)] {
                if run.args[key].is_null() {
                    run.args[key] = json!(fallback);
                }
            }
            if run.args["model"].as_str().unwrap_or_default().trim().is_empty() {
                self.complete_ui_run(&run.run_id, None, Some(DltError::invalid("모델 ID를 입력해 주세요")));
                return;
            }
            self.ui.pending_home_create = Some((run, deadline));
            return;
        }
        self.invoke_ui_run(run, deadline);
    }

    pub(super) fn has_home_confirmation(&self) -> bool {
        self.ui.pending_home_create.is_some()
    }

    pub(super) fn draw_home_confirmation(&mut self, ctx: &Context) {
        let Some((run, deadline)) = &self.ui.pending_home_create else { return };
        let error = if Instant::now() >= *deadline {
            Some(DltError::new(ErrorCode::Canceled, "확인 시간이 지나 모델 실행 요청을 취소했습니다"))
        } else {
            self.ui_run_refusal(run)
        };
        if let Some(error) = error {
            let (run, _) = self.ui.pending_home_create.take().unwrap();
            self.complete_ui_run(&run.run_id, None, Some(error));
            return;
        }
        let arg = |key: &str| run.args[key].as_str().unwrap_or_default();
        use crate::widgets::{DialogButton, DialogChoice};
        let palette = crate::theme::palette(ctx);
        // The buttons stay under the request, in view however long it is: no one approves a request they could not scroll to.
        let shown = crate::widgets::dialog_with_choices(ctx, &palette, egui::Id::new("agent-home-create"), "에이전트의 모델 실행 요청", crate::widgets::DialogPlace::Modal, |ui| {
            ui.label(format!("{} · {} · {}", arg("runtime"), arg("model"), arg("effort")));
            let tile = super::home::TILES.iter().find(|item| item.0 == arg("tile")).map_or(arg("tile"), |item| item.1);
            ui.label(format!("새 작업물 · {tile}"));
            // A long request scrolls in the dialog's body (`widgets::dialog_with_choices`); a scroll area of its own inside it
            // would size itself to the body's last height.
            ui.label(arg("prompt"));
            ((), vec![
                DialogChoice::new("거절", DialogButton::Plain).human_only(),
                DialogChoice::new("모델 실행", DialogButton::Primary).human_only(),
            ])
        });
        let pressed = &shown.inner.1;
        let pressed = if pressed[0] { Some(false) } else if pressed[1] { Some(true) } else { None };
        // Enter is `모델 실행`; Esc and a press on the backdrop are `거절`, the safe side. A synthetic key reaches neither: the run
        // ends with HUMAN_ONLY while this approval is open (`ui_test_approval_open`).
        let answer = pressed.or(shown.enter.then_some(true)).or(shown.dismissed.then_some(false));
        if let Some(accepted) = answer {
            let (run, deadline) = self.ui.pending_home_create.take().unwrap();
            if accepted && Instant::now() < deadline {
                self.invoke_ui_run(run, deadline);
            } else {
                self.complete_ui_run(&run.run_id, None, Some(DltError::new(ErrorCode::Canceled, "모델 실행을 승인하지 않아 요청을 취소했습니다")));
            }
        }
    }

    fn invoke_ui_run(&mut self, run: UiRun, deadline: Instant) {
        self.run_context = Some(RunContext {
            run_id: run.run_id.clone(),
            deadline,
            command_id: run.command_id.clone(),
            actor: run.actor.clone(),
            allowed_intents: run.allowed_intents.clone(),
            await_rpc_reply: ui_run_awaits_reply(&run.command_id),
            mutation_attempted: false,
            rpc_ticket: None,
            enqueue_error: None,
            expected: match run.command_id.as_str() {
                "add.figure_image" | "add.figure_link" | "figure.update_link" => run.expected,
                // A read command carries no state the core recorded: the chart table's reads are checked against the window's
                // state when the run starts.
                "add.chart" => run.expected.or_else(|| self.ui_run_expected_now()),
                _ => None,
            },
            prepared_asset: None,
        });
        // `add.chart` without a path opens the chart dialog, as the window's own command does: nothing to wait for.
        let chart_dialog = run.command_id == "add.chart" && run.args.get("path").is_none();
        let outcome = self.invoke(&run.command_id, run.args);
        let run_context = self.run_context.take();
        if outcome.is_ok() && run.command_id == "nav.home" {
            self.cancel_pending_agent_follow();
            self.agent_follow.home_in_poll = true;
        }
        match outcome {
            Ok(()) if run_context.as_ref().is_some_and(|context| context.await_rpc_reply && context.rpc_ticket.is_some()) => {}
            Ok(()) if chart_dialog => self.complete_ui_run(&run.run_id, Some(json!({ "command": run.command_id, "accepted": true })), None),
            Ok(()) if tracked_command(&run.command_id) || run_context.as_ref().is_some_and(|context| context.await_rpc_reply && context.mutation_attempted) => {
                let error = run_context
                    .and_then(|context| context.enqueue_error)
                    .unwrap_or_else(|| DltError::new(ErrorCode::Unavailable, "변경 요청을 전송하지 못했습니다"));
                self.complete_ui_run(&run.run_id, None, Some(error));
            }
            Ok(()) => self.complete_ui_run(&run.run_id, Some(json!({ "command": run.command_id, "accepted": true })), None),
            Err(message) => self.complete_ui_run(&run.run_id, None, Some(DltError::new(ErrorCode::InvalidArgument, message))),
        }
    }

    /// Reports a run's outcome with the view token.
    pub(super) fn complete_ui_run(&mut self, run_id: &str, result: Option<Value>, error: Option<DltError>) {
        let Some(token) = self.ui.token.clone() else {
            return;
        };
        let request = UiCompleteRequest {
            view_id: self.ui.view_id.clone(),
            view_token: token,
            run_id: run_id.into(),
            result,
            error,
        };
        if let Ok(params) = serde_json::to_value(request) {
            self.call("ui.command.complete", params, PendingAction::UiComplete);
        }
    }

    /// Requests the window capture for a screenshot run and handles the capture when it arrives.
    /// Screenshots of the smoke capture are left alone.
    pub(super) fn ui_screenshot_events(&mut self, ctx: &Context) {
        if std::mem::take(&mut self.ui.screenshot_wanted) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let images: Vec<std::sync::Arc<ColorImage>> = ctx.input(|input| {
            input
                .events
                .iter()
                .filter_map(|event| match event {
                    Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
                .collect()
        });
        if self.smoke_output.is_some() {
            return;
        }
        for image in images {
            self.finish_screenshot(&image);
        }
    }

    /// Saves a captured frame for the oldest screenshot run and imports it as an asset. The run
    /// completes when the asset is in the workspace (see `PendingAction::UiAsset`).
    pub(super) fn finish_screenshot(&mut self, image: &ColorImage) {
        let Some(run_id) = self.ui.screenshot_runs.pop_front() else {
            return;
        };
        let [width, height] = image.size;
        let path: PathBuf = std::env::temp_dir().join(format!("designllm-ui-{run_id}.png"));
        if save_screenshot(&path, image).is_err() {
            self.complete_ui_run(&run_id, None, Some(DltError::new(ErrorCode::Io, "화면을 PNG로 저장하지 못했습니다")));
            return;
        }
        self.call(
            "asset.import",
            json!({ "path": path.to_string_lossy(), "mime": "image/png" }),
            PendingAction::UiAsset {
                run_id,
                width: width as u32,
                height: height as u32,
                path,
            },
        );
    }

    /// The reply of `asset.import` for a screenshot: the run completes with the asset.
    pub(super) fn finish_ui_asset(&mut self, run_id: String, width: u32, height: u32, path: PathBuf, value: Value) {
        let _ = std::fs::remove_file(&path);
        match serde_json::from_value::<AssetRef>(value) {
            Ok(asset) => self.complete_ui_run(
                &run_id,
                Some(json!({ "asset": asset, "width_px": width, "height_px": height })),
                None,
            ),
            Err(_) => self.complete_ui_run(&run_id, None, Some(DltError::new(ErrorCode::IntegrityError, "화면 자산을 읽지 못했습니다"))),
        }
    }

    /// The publish answer: the view token for the polls and completions that follow.
    pub(super) fn accept_ui_publish(&mut self, value: Value) {
        if let Ok(result) = serde_json::from_value::<UiPublishResult>(value) {
            if result.view_id == self.ui.view_id {
                self.ui.token = Some(result.view_token);
                self.ui.state_seq = Some(result.state_seq);
                self.ui.run_guard_version = result.run_guard_version;
            } else {
                self.ui.state_seq = None;
                self.ui.run_guard_version = None;
            }
        }
    }
}

fn same_selection(left: &Selection, right: &Selection) -> bool {
    left.node_ids == right.node_ids
        && match (&left.text_range, &right.text_range) {
            (None, None) => true,
            (Some(left), Some(right)) => {
                left.paragraph_id == right.paragraph_id && left.start == right.start && left.end == right.end
            }
            _ => false,
        }
}
#[cfg(test)]
mod tests {
    use designllm_contracts::{new_id, ArtifactBody, Intent, Node, Operation, PanelBody};
    use serde_json::json;

    use crate::app::{
        link_flow_tests::{canvas_with_three_boxes, drawn_figure},
        test_support::{Host, Window},
    };

    #[test]
    fn every_registered_command_has_an_explicit_scope() {
        for spec in crate::commands::COMMANDS {
            assert!(super::command_scope(spec.id).is_some(), "unclassified command: {}", spec.id);
        }
        assert!(super::command_scope("figure.output_profile").is_some());
        assert!(super::command_scope("unknown.command").is_none());
    }

    #[test]
    fn registry_follows_home_and_all_artifact_kinds() {
        use designllm_contracts::ArtifactKind;
        let mut host = Host::start();
        let canvas = host.create("canvas", "scope");
        let mut window = Window::open(&mut host, &canvas);
        let commands = ["add.icon", "web.replace_source", "slides.theme", "doc.paragraph_format", "figure.add_equation"];
        window.app.screen = crate::app::Screen::Home;
        for id in commands { assert!(!listed(&window, id), "home: {id}"); }
        let home_registry = window.app.ui_commands();
        for spec in crate::commands::COMMANDS {
            if matches!(super::command_scope(spec.id), Some(super::CommandScope::Canvas | super::CommandScope::CanvasFigure | super::CommandScope::CanvasSlides | super::CommandScope::CanvasSlidesFigure | super::CommandScope::TextSetSupported | super::CommandScope::TextStyleSupported | super::CommandScope::Web | super::CommandScope::Slides | super::CommandScope::Flow | super::CommandScope::Figure)) {
                assert!(!home_registry.iter().find(|entry| entry.id == spec.id).unwrap().enabled, "home: {}", spec.id);
            }
        }
        assert!(listed(&window, "home.view"));
        for (kind, expected) in [
            (ArtifactKind::Canvas, 0), (ArtifactKind::WebCode, 1), (ArtifactKind::Slides, 2),
            (ArtifactKind::Docx, 3), (ArtifactKind::Hwpx, 3), (ArtifactKind::Hwp, 3), (ArtifactKind::Figure, 4),
        ] {
            window.app.screen = crate::app::Screen::Editor;
            window.app.active.as_mut().unwrap().kind = kind;
            for (index, id) in commands.iter().enumerate() {
                assert_eq!(listed(&window, id), index == expected || (kind == ArtifactKind::Slides && *id == "add.icon"), "{kind:?}: {id}");
            }
            let registry = window.app.ui_commands();
            for spec in crate::commands::COMMANDS {
                let allowed = match super::command_scope(spec.id) {
                    Some(super::CommandScope::Canvas) => kind == ArtifactKind::Canvas || spec.id == "canvas.design_system.set",
                    Some(super::CommandScope::CanvasFigure) => matches!(kind, ArtifactKind::Canvas | ArtifactKind::Figure),
                    Some(super::CommandScope::CanvasSlides) => matches!(kind, ArtifactKind::Canvas | ArtifactKind::Slides),
                    Some(super::CommandScope::CanvasSlidesFigure) => matches!(kind, ArtifactKind::Canvas | ArtifactKind::Slides | ArtifactKind::Figure),
                    Some(super::CommandScope::TextSetSupported) => matches!(kind, ArtifactKind::Canvas | ArtifactKind::Slides | ArtifactKind::Figure | ArtifactKind::Docx | ArtifactKind::Hwpx),
                    Some(super::CommandScope::TextStyleSupported) => kind != ArtifactKind::WebCode,
                    Some(super::CommandScope::Web) => kind == ArtifactKind::WebCode,
                    Some(super::CommandScope::Slides) => kind == ArtifactKind::Slides,
                    Some(super::CommandScope::Flow) => matches!(kind, ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp),
                    Some(super::CommandScope::Figure) => kind == ArtifactKind::Figure,
                    _ => true,
                };
                if !allowed { assert!(!registry.iter().find(|entry| entry.id == spec.id).unwrap().enabled, "{kind:?}: {}", spec.id); }
            }
            assert!(!listed(&window, "home.view"), "{kind:?}");
            assert!(window.app.command_enabled_for_current_kind("home.runner"));
            assert!(window.app.command_enabled_for_current_kind("home.tile"));
        }
    }

    #[test]
    fn registry_matches_independent_kind_and_selection_expectations() {
        use designllm_contracts::ArtifactKind;
        let mut host = Host::start();
        let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
        let mut window = Window::open(&mut host, &canvas);
        let kinds = [ArtifactKind::Canvas, ArtifactKind::Slides, ArtifactKind::Figure, ArtifactKind::Docx,
            ArtifactKind::Hwpx, ArtifactKind::Hwp, ArtifactKind::WebCode];
        let cases = [
            ("add.icon",             [true, true,  false, false, false, false, false]),
            ("text.set",             [true, true,  true,  true,  true,  false, false]),
            ("props.fill",           [true, true,  true,  false, false, false, false]),
            ("props.fill_remove",    [true, true,  true,  false, false, false, false]),
            ("props.stroke",         [true, true,  true,  false, false, false, false]),
            ("props.stroke_width",   [true, true,  true,  false, false, false, false]),
            ("props.stroke_remove",  [true, true,  true,  false, false, false, false]),
            ("props.opacity",        [true, false, true,  false, false, false, false]),
            ("props.corner_radius",  [true, false, true,  false, false, false, false]),
            ("props.shadow",         [true, true,  false, false, false, false, false]),
            ("props.shadow_remove",  [true, true,  false, false, false, false, false]),
            ("props.text_style",     [true, true,  true,  true,  true,  true,  false]),
            ("props.text_align",     [true, true,  true,  false, false, false, false]),
            ("props.line_height",    [true, false, true,  false, false, false, false]),
            ("props.letter_spacing", [true, false, true,  false, false, false, false]),
            ("props.vertical_align", [true, false, true,  false, false, false, false]),
            ("props.stroke_style",   [true, false, true,  false, false, false, false]),
            ("props.gradient",       [true, false, false, false, false, false, false]),
            ("props.gradient_remove",[true, false, false, false, false, false, false]),
        ];
        for (index, kind) in kinds.into_iter().enumerate() {
            window.app.active.as_mut().unwrap().kind = kind;
            window.app.selected_nodes = vec!["a".into()];
            for (id, expected) in cases {
                assert_eq!(listed(&window, id), expected[index], "{kind:?}: {id}");
            }
            window.app.selected_nodes.clear();
            for (id, expected) in cases {
                let without_selection = if id.starts_with("props.") { false } else { expected[index] };
                assert_eq!(listed(&window, id), without_selection, "{kind:?} without selection: {id}");
            }
        }
    }

    #[test]
    fn registry_is_republished_when_home_or_loaded_artifact_changes() {
        let mut host = Host::start();
        let figure = drawn_figure(&mut host, "figure", 1);
        let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
        let mut window = Window::open(&mut host, &canvas);
        window.step(&mut host, "add.figure_link", json!({ "figure_id": figure })).unwrap();
        let enabled = |window: &mut Window, host: &mut Host, id: &str| {
            window.app.publish_ui_if_due();
            window.settle(host);
            let listing = host.call("ui.command.list", json!({ "view_id": window.app.ui.view_id })).unwrap();
            listing["commands"].as_array().unwrap().iter().find(|entry| entry["id"] == id).unwrap()["enabled"].as_bool().unwrap()
        };
        assert!(enabled(&mut window, &mut host, "figure.open_source"));
        let artifact = window.app.artifact.take();
        assert!(!enabled(&mut window, &mut host, "figure.open_source"));
        window.app.artifact = artifact;
        assert!(enabled(&mut window, &mut host, "figure.open_source"));
        window.step(&mut host, "nav.home", json!({})).unwrap();
        assert!(enabled(&mut window, &mut host, "home.view"));
        assert!(!enabled(&mut window, &mut host, "figure.open_source"));
    }

    /// An agent's run as the core hands it over: change permission, content intent, no expected view state.
    fn agent_run(command_id: &str, args: serde_json::Value) -> designllm_contracts::UiRun {
        designllm_contracts::UiRun {
            run_id: new_id(), view_id: new_id(), expected: None, command_id: command_id.into(), args, actor: "agent".into(),
            permission: designllm_contracts::UiPermission::Change, allowed_intents: [Intent::Content].into_iter().collect(),
            status: designllm_contracts::UiRunStatus::Queued, timeout_ms: 10_000, result: None, error: None,
        }
    }

    /// Whether the refusal is the review gate's: CONTENT_LOCKED with its one diagnostic.
    fn review_refused(error: Option<designllm_contracts::DltError>) -> bool {
        error.is_some_and(|error| {
            error.code == designllm_contracts::ErrorCode::ContentLocked
                && error.diagnostics.len() == 1
                && error.diagnostics[0].code == designllm_bridge::review_gate::REVIEW_USER_ONLY
        })
    }

    fn statement(review_state: &str) -> serde_json::Value {
        json!({ "evidence": {
            "id": "ev-1", "source_hash": designllm_contracts::sha256("A causes B".as_bytes()), "origin": "user_statement",
            "physical_page_index": null, "region": null, "pdf_geometry": null, "text": "A causes B", "review_state": review_state,
        } })
    }

    /// 49w: an agent's UI channel run may not approve the visual review, clear it, or put reviewed evidence, and a refused run
    /// changes nothing. Pending, needs-revision and unreviewed runs pass the review gate. The user's own click still approves and
    /// clears.
    #[test]
    fn an_agent_run_cannot_open_a_review_gate_but_the_users_click_can() {
        let mut host = Host::start();
        let figure = host.create_figure();
        let mut window = Window::open(&mut host, &figure);
        let approve = json!({ "review": { "state": "approved" } });
        let clear = json!({ "review": null });
        for (id, args) in [("figure.visual_review", approve.clone()), ("figure.put_evidence", statement("reviewed"))] {
            let run = agent_run(id, args);
            assert!(review_refused(window.app.ui_run_refusal(&run)), "{id} is refused for an agent");
            window.app.execute_ui_run(run);
            window.settle(&mut host);
        }
        let (document, revision) = host.figure(&figure);
        assert!(document.visual_review.is_none() && document.evidence.is_empty() && revision == 0, "a refused run changes nothing");
        for (id, args) in [
            ("figure.visual_review", json!({ "review": { "state": "pending" } })),
            ("figure.visual_review", json!({ "review": { "state": "needs_revision", "note": "축 단위" } })),
            ("figure.visual_review", clear.clone()),
            ("figure.put_evidence", statement("unreviewed")),
            ("figure.put_evidence", json!({ "text": "세 번 반복 측정했다" })),
        ] {
            assert!(!review_refused(window.app.ui_run_refusal(&agent_run(id, args.clone()))), "{id} {args} passes the review gate");
        }

        window.step(&mut host, "figure.visual_review", approve).expect("the user approves");
        let approved = host.figure(&figure).0.visual_review.map(|review| review.state);
        assert_eq!(approved, Some(designllm_contracts::VisualReviewState::Approved), "the user's click approves");
        assert!(review_refused(window.app.ui_run_refusal(&agent_run("figure.visual_review", clear.clone()))), "an agent cannot clear it");
        window.step(&mut host, "figure.visual_review", clear).expect("the user clears the review");
        assert!(host.figure(&figure).0.visual_review.is_none(), "the user's click clears");
        window.step(&mut host, "figure.put_evidence", statement("reviewed")).expect("the user puts reviewed evidence");
        assert_eq!(host.figure(&figure).0.evidence[0].review_state, "reviewed");
    }

    /// 49w r3: the gate reads a run's arguments as the command does. A review with a key the contract does not know, or with
    /// an `approved_digest` of the wrong type, still approves when the user clicks, so an agent's run with it is refused; and
    /// arguments the command cannot read are refused too, not let through.
    #[test]
    fn an_agent_run_is_judged_by_the_arguments_the_command_reads() {
        let mut host = Host::start();
        let figure = host.create_figure();
        let mut window = Window::open(&mut host, &figure);
        let loose = [
            json!({ "review": { "state": "approved", "x": 1 } }),
            json!({ "review": { "state": "approved", "approved_digest": 5 } }),
        ];
        for args in &loose {
            let run = agent_run("figure.visual_review", args.clone());
            assert!(review_refused(window.app.ui_run_refusal(&run)), "{args} is refused for an agent");
            window.app.execute_ui_run(run);
            window.settle(&mut host);
        }
        assert!(host.figure(&figure).0.visual_review.is_none(), "a refused run changes nothing");
        // Refused as invalid before the view state is looked at (an agent run here has no expected state, which would be stale).
        let invalid = |error: Option<designllm_contracts::DltError>| error.is_some_and(|error| error.code == designllm_contracts::ErrorCode::InvalidArgument);
        for args in [json!({ "review": { "state": "bogus" } }), json!({ "review": 5 })] {
            assert!(invalid(window.app.ui_run_refusal(&agent_run("figure.visual_review", args.clone()))), "{args} is refused");
        }
        let mut reviewed = statement("reviewed");
        reviewed["evidence"]["x"] = json!(1);
        assert!(review_refused(window.app.ui_run_refusal(&agent_run("figure.put_evidence", reviewed))), "reviewed evidence with an extra key");
        let mistyped = json!({ "evidence": { "id": 5 } });
        assert!(invalid(window.app.ui_run_refusal(&agent_run("figure.put_evidence", mistyped))), "evidence the command cannot read");

        window.step(&mut host, "figure.visual_review", loose[0].clone()).expect("the user approves with the extra key");
        let approved = host.figure(&figure).0.visual_review.map(|review| review.state);
        assert_eq!(approved, Some(designllm_contracts::VisualReviewState::Approved), "the command reads the review loosely");
    }

    /// 49w r3: the user approves and then clears the visual review. An undo, redo or undo_change that an agent sends (on a
    /// connection without the gui role, or as a UI channel run) is judged by what it would restore, as the host does
    /// (`designllm-host` `conn.rs` `review_refusal`): the undo would bring the approval back, so it is refused and nothing
    /// changes. The user's own undo brings it back, and then an agent's redo, which would clear it again, is refused too.
    #[test]
    fn an_agent_cannot_undo_its_way_back_to_an_approval_the_user_cleared() {
        use designllm_bridge::review_gate::{history_refusal, ui_run_history_call};
        let mut host = Host::start();
        let figure = host.create_figure();
        let mut window = Window::open(&mut host, &figure);
        let approve_args = json!({ "review": { "state": "approved" } });
        window.step(&mut host, "figure.visual_review", approve_args).expect("the user approves");
        window.step(&mut host, "figure.visual_review", json!({ "review": null })).expect("the user clears the review");
        let revision = host.figure(&figure).1;

        let workspace_id = host.workspace_id.clone();
        let agent_history = |revision: u64| json!({
            "request_id": new_id(), "actor": "agent", "workspace_id": workspace_id, "artifact_id": figure, "expected_revision": revision,
            "allowed_intents": ["content"], "job_ref": null,
        });
        let undo = agent_history(revision);
        assert!(review_refused(history_refusal(host.workspace().history_preview("history.undo", &undo, None).expect("preview"))));
        let run = json!({ "command_id": "edit.undo", "actor": "agent", "allowed_intents": ["content"], "args": {} });
        let (method, params) = ui_run_history_call(&run, &window.app.ui_state(), &workspace_id).expect("the window's history request");
        assert_eq!(method, "history.undo");
        assert!(review_refused(history_refusal(host.workspace().history_preview(method, &params, None).expect("preview"))), "a UI channel undo");
        let (unchanged, unchanged_revision) = host.figure(&figure);
        assert!(unchanged.visual_review.is_none() && unchanged_revision == revision, "nothing changed");

        window.step(&mut host, "edit.undo", json!({})).expect("the user undoes");
        let (document, revision) = host.figure(&figure);
        assert_eq!(document.visual_review.map(|review| review.state), Some(designllm_contracts::VisualReviewState::Approved), "the user's undo");
        let redo = agent_history(revision);
        assert!(review_refused(history_refusal(host.workspace().history_preview("history.redo", &redo, None).expect("preview"))), "an agent's redo");
    }

    /// The review gate's values are the figure engine's (49w; to move to contracts::figure_rules once contracts can change).
    #[test]
    fn the_review_gate_names_the_figure_engine_values() {
        use designllm_bridge::review_gate::{REVIEWED, USER_PROVIDED, VISUAL_REVIEW_ID};
        assert!(designllm_figures::evidence::REVIEW_STATES.contains(&REVIEWED));
        assert!(designllm_figures::evidence::REVIEW_STATES.contains(&USER_PROVIDED));
        assert_eq!(VISUAL_REVIEW_ID, designllm_figures::layout::VISUAL_REVIEW_ID);
    }

    #[test]
    fn the_image_replacement_requires_a_selected_picture() {
        let mut host = Host::start();
        let canvas = host.create("canvas", "그림판");
        let window = Window::open(&mut host, &canvas);
        let listed = window.app.ui_commands();
        let replace = listed.iter().find(|command| command.id == "canvas.image.replace").expect("the command is listed");
        assert!(!replace.enabled);
        // A canvas command that needs no selection (`props.fill` now follows the selection, backlog desktop 29).
        let add = listed.iter().find(|command| command.id == "add.rect").expect("a canvas command is listed");
        assert!(add.enabled, "the other canvas commands stay enabled");
    }

    #[test]
    fn web_preview_reports_the_runtime_reason_through_the_channel() {
        let mut host = Host::start();
        let web = host.create("web_code", "웹");
        let mut window = Window::open(&mut host, &web);
        let reason = "이 기기에서 WebView2 런타임을 찾지 못했습니다";
        window.app.web_runtime_unavailable = Some(reason.into());
        assert!(listed(&window, "web.preview"));
        let mark = window.calls.len();
        let run = window.agent_run_granted(&mut host, "web.preview", json!({}), "read", &[]);
        assert_eq!(run["status"], "failed", "{run}");
        assert_eq!(run["error"]["message"], reason);
        assert!(!window.calls[mark..].iter().any(|(method, _, _)| method == "web.materialize"));
        window.step(&mut host, "nav.home", json!({})).unwrap();
        assert!(!listed(&window, "web.preview"));
    }

    /// A canvas with the text boxes `a`, `b` and `c`, and a picture, `picture`, that shows an imported PNG.
    fn canvas_with_picture(host: &mut Host) -> String {
        let canvas = canvas_with_three_boxes(host, [0, 1_000_000, 3_000_000]);
        let path = std::env::temp_dir().join(format!("designllm-ui-channel-{}.png", new_id()));
        image::RgbaImage::new(4, 4).save(&path).expect("the picture is written");
        let asset = host.call("asset.import", json!({ "path": path.to_string_lossy() })).expect("the picture imports");
        let _ = std::fs::remove_file(&path);
        let page_id = match &host.artifact(&canvas).body {
            ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
            _ => panic!("a canvas"),
        };
        let node: Node = serde_json::from_value(json!({
            "id": "picture",
            "kind": "image",
            "bounds": { "x": 100_000, "y": 3_000_000, "width": 1_200_000, "height": 800_000 },
            "asset": { "sha256": asset["sha256"], "mime": "image/png", "bytes": asset["bytes"] }
        }))
        .expect("a picture node");
        host.apply(&canvas, vec![Operation::AddNode { page_id, parent_id: None, node }], [Intent::Content].into_iter().collect())
            .expect("the picture is placed");
        canvas
    }

    /// Publishes the window's registry and reads it back from the host (`ui.command.list`). Returns, for `props.image_fit` and
    /// `props.image_alt`, whether the host lists the command as enabled and whether running it now passes its checks. The runs
    /// change the document, so they come after the listing of the same state.
    fn registry_and_runs(window: &mut Window, host: &mut Host) -> [(bool, bool); 2] {
        window.app.publish_ui_if_due();
        window.settle(host);
        let registry = host.call("ui.command.list", json!({})).expect("the window's registry is published");
        let enabled = |id: &str| {
            registry["commands"]
                .as_array()
                .and_then(|commands| commands.iter().find(|command| command["id"] == id))
                .and_then(|command| command["enabled"].as_bool())
                .expect("the command is listed")
        };
        let fit = (enabled("props.image_fit"), window.app.invoke("props.image_fit", json!({ "fit": "cover" })).is_ok());
        window.settle(host);
        let alt = (enabled("props.image_alt"), window.app.invoke("props.image_alt", json!({ "alt": "장면 설명" })).is_ok());
        window.settle(host);
        [fit, alt]
    }

    /// `props.image_fit` and `props.image_alt` are listed as enabled exactly when their run passes its checks, in the registry
    /// the host holds as well.
    #[test]
    fn the_image_commands_are_listed_as_enabled_exactly_when_their_run_passes() {
        let mut host = Host::start();
        let canvas = canvas_with_picture(&mut host);
        let mut window = Window::open(&mut host, &canvas);
        let refused = [(false, false), (false, false)];

        assert_eq!(registry_and_runs(&mut window, &mut host), refused, "nothing is selected");
        window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("a text box is selected");
        assert_eq!(registry_and_runs(&mut window, &mut host), refused, "a text box is no picture");
        window.step(&mut host, "select.many", json!({ "node_ids": ["picture", "a"] })).expect("two nodes are selected");
        assert_eq!(registry_and_runs(&mut window, &mut host), refused, "one node at a time");
        window.step(&mut host, "select.node", json!({ "node_id": "picture" })).expect("the picture is selected");
        assert_eq!(registry_and_runs(&mut window, &mut host), [(true, true), (true, true)], "the picture takes both");

        // A replacement of the picture on its way: neither is listed nor runs, and the registry published then says so.
        let png = std::env::temp_dir().join(format!("designllm-ui-channel-replace-{}.png", new_id()));
        image::RgbaImage::new(4, 4).save(&png).expect("the replacement picture is written");
        let intent = window.app.image_replacement_start().expect("the picture can start a replacement");
        window.app.start_canvas_image_replacement_import(intent, png.clone()).expect("the import starts");
        window.app.publish_ui_if_due();
        for (id, args) in [("props.image_fit", json!({ "fit": "cover" })), ("props.image_alt", json!({ "alt": "장면 설명" }))] {
            assert!(!listed(&window, id), "{id} waits for the replacement");
            assert!(window.app.invoke(id, args).is_err(), "{id} refuses while the replacement is on its way");
        }
        window.settle(&mut host);
        let registry = host.call("ui.command.list", json!({})).expect("the window's registry is published");
        for id in ["props.image_fit", "props.image_alt"] {
            let command = registry["commands"].as_array().and_then(|commands| commands.iter().find(|command| command["id"] == id));
            assert_eq!(command.and_then(|command| command["enabled"].as_bool()), Some(false), "{id} is listed as not enabled");
        }
        let _ = std::fs::remove_file(&png);
    }

    /// `canvas.design_system.set` is enabled in a WebCode editor, which links to a published Canvas, and `canvas.design_system.publish`
    /// is not: only a Canvas publishes.
    #[test]
    fn the_design_system_link_is_enabled_for_a_webcode_editor_and_publishing_stays_canvas_only() {
        let mut host = Host::start();
        let web = host.create("web_code", "웹 페이지");
        let window = Window::open(&mut host, &web);
        assert!(listed(&window, "canvas.design_system.set"), "a WebCode editor links to a shared design system");
        assert!(!listed(&window, "canvas.design_system.publish"), "a WebCode project cannot publish a design system");
    }

    /// The title paragraph style of `basic-preserve.docx` by its display name, and the style of `opaque-image-preserve.hwpx`
    /// that no paragraph uses.
    const DOCX_TITLE_STYLE: &str = "Luna Fixture Title";
    const HWPX_UNUSED_STYLE: &str = "본문";

    fn open_fixture(host: &mut Host, relative: &str) -> String {
        let path = crate::app::test_support::repository_root().join("fixtures").join(relative);
        let opened = host
            .call("artifact.open", json!({ "request_id": new_id(), "actor": "desktop", "path": path.to_string_lossy(), "kind": null }))
            .expect("the fixture opens");
        opened["id"].as_str().expect("an artifact id").to_owned()
    }

    /// A published Canvas whose tokens set the DOCX title style color and the HWPX body style color. Returns its id and
    /// revision.
    fn publish_named_style_colors(host: &mut Host, title_ink: &str, body_ink: &str) -> (String, u64) {
        let system = host.create("canvas", "프로젝트 규칙");
        host.apply(&system, named_style_tokens(title_ink, body_ink), [Intent::Design].into_iter().collect()).expect("the tokens");
        let revision = host.artifact(&system).revision;
        let published = host
            .call("design_system.publish", json!({
                "request_id": new_id(), "actor": "desktop", "workspace_id": host.workspace_id, "artifact_id": system,
                "expected_revision": revision, "publish": true, "allowed_intents": ["design"],
            }))
            .expect("publish");
        (system, published["revision"].as_u64().expect("the published revision"))
    }

    fn named_style_tokens(title_ink: &str, body_ink: &str) -> Vec<Operation> {
        let color = |light: &str| designllm_contracts::TokenValues::Color { light: light.into(), dark: "#FFFFFF".into() };
        vec![
            Operation::PutToken { token_id: "title-ink".into(), name: format!("{DOCX_TITLE_STYLE}.color"), values: color(title_ink) },
            Operation::PutToken { token_id: "body-ink".into(), name: format!("{HWPX_UNUSED_STYLE}.color"), values: color(body_ink) },
        ]
    }

    fn flow_body(artifact: &designllm_contracts::Artifact) -> &designllm_contracts::FlowDocument {
        match &artifact.body {
            ArtifactBody::Flow(document) => document,
            _ => panic!("a flow document"),
        }
    }

    /// The first run color of the first paragraph.
    fn title_run_color(artifact: &designllm_contracts::Artifact) -> String {
        match &flow_body(artifact).sections[0].blocks[0] {
            designllm_contracts::FlowBlock::Paragraph(paragraph) => paragraph.runs[0].style.color.clone(),
            other => panic!("block 0 is not a paragraph: {other:?}"),
        }
    }

    /// The serialised body without node ids: a DOCX open gives every node a new id, so two opens of one file differ only there.
    /// The keys of the `styles` map are style names, not node ids, and stay.
    fn body_without_ids(artifact: &designllm_contracts::Artifact) -> serde_json::Value {
        fn strip(value: &mut serde_json::Value, style_names: bool) {
            match value {
                serde_json::Value::Object(map) => {
                    if !style_names {
                        map.remove("id");
                    }
                    for (key, child) in map.iter_mut() {
                        strip(child, !style_names && key == "styles");
                    }
                }
                serde_json::Value::Array(items) => items.iter_mut().for_each(|item| strip(item, false)),
                _ => {}
            }
        }
        let mut body = serde_json::to_value(&artifact.body).expect("the body serialises");
        strip(&mut body, false);
        body
    }

    /// What a link leaves: the body without node ids, the style-name map DOCX keeps while linked, the reference and the
    /// revision.
    fn link_state(artifact: &designllm_contracts::Artifact) -> serde_json::Value {
        let style_map = artifact.native.as_ref().and_then(|native| native.preserved.get("design_system_styles")).cloned();
        json!({ "body": body_without_ids(artifact), "design_system_styles": style_map, "design_system": artifact.design_system, "revision": artifact.revision })
    }

    /// A result's diagnostics without what names nodes: code, blocking and params, and the message of those that name no node.
    fn diagnostics_of(result: &serde_json::Value) -> Vec<serde_json::Value> {
        result["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .map(|d| json!({ "code": d["code"], "blocking": d["blocking"], "params": d.get("params"), "message": d["node_id"].is_null().then(|| d["message"].clone()) }))
            .collect()
    }

    /// The artifact with its native state, which `artifact.inspect` leaves out: the current side of the core's undo preview
    /// (49w), so only for an artifact with a change to undo.
    fn full_artifact(host: &mut Host, artifact_id: &str) -> designllm_contracts::Artifact {
        let revision = host.artifact(artifact_id).revision;
        let params = json!({
            "request_id": new_id(), "actor": "desktop", "workspace_id": host.workspace_id, "artifact_id": artifact_id,
            "expected_revision": revision, "allowed_intents": ["design", "content", "data"], "job_ref": null,
        });
        let previews = host.workspace().history_preview("history.undo", &params, None).expect("the undo preview");
        previews.into_iter().map(|(current, _)| current).find(|current| current.id == artifact_id).expect("the artifact has a change to undo")
    }

    /// The `design_system.set` the window sent last and the host's answer (null when it refused).
    fn gui_link(host: &mut Host, artifact_id: &str, args: serde_json::Value) -> (designllm_contracts::Artifact, serde_json::Value) {
        let mut window = Window::open(host, artifact_id);
        window.step(host, "canvas.design_system.set", args).expect("the GUI command runs");
        let answer = window.calls.iter().rev().find(|(method, _, _)| method == "design_system.set").map(|(_, _, answer)| answer.clone());
        (host.artifact(artifact_id), answer.expect("the window sent design_system.set"))
    }

    /// One MCP tool call through the protocol, with its error result kept (the shared helper fails the test on one).
    fn mcp_tool(backend: std::sync::Arc<crate::app::test_support::HostBackend>, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        use designllm_bridge::{catalog::Scope, mcp::{McpServer, Routed}};
        let server = McpServer::new(backend, Scope::Workspace);
        let initialize = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": { "name": "desktop-parity", "version": "1" } }
        });
        assert!(matches!(server.route(initialize), Routed::Reply(_)));
        assert!(matches!(server.route(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })), Routed::Nothing));
        let request = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": name, "arguments": arguments } });
        let Routed::Call(pending) = server.route(request) else { panic!("the tool {name} is not called") };
        server.execute(&pending)
    }

    /// `canvas.design_system.set` is enabled in Word and HWPX editors (their named styles take the project rules) and not in HWP.
    #[test]
    fn the_design_system_link_is_enabled_for_word_and_hwpx_editors() {
        let mut host = Host::start();
        for relative in ["office/basic-preserve.docx", "hwpx/opaque-image-preserve.hwpx"] {
            let id = open_fixture(&mut host, relative);
            let window = Window::open(&mut host, &id);
            assert!(listed(&window, "canvas.design_system.set"), "{relative} links its named styles");
            assert!(!listed(&window, "canvas.design_system.publish"), "{relative} cannot publish");
        }
    }

    /// BRIEF 17: the same link through the GUI command and through the MCP tool leaves the same body (ids aside), DOCX style-name
    /// map, reference, revision and diagnostics, for DOCX clear, DOCX keep and HWPX keep; a DOCX link without a policy is refused
    /// on both paths and changes nothing. Clear lets the published color replace the title run's direct color; keep leaves it.
    #[test]
    fn design_system_policy_gui_and_mcp_same_result() {
        use crate::app::test_support::HostBackend;
        let mut host = Host::start();
        let (system, system_revision) = publish_named_style_colors(&mut host, "#C00000", "#00A050");
        let workspace_id = host.workspace_id.clone();
        let backend = HostBackend::new(host);
        let mcp_args = |artifact_id: &str, policy: Option<&str>| {
            let mut args = json!({
                "request_id": new_id(), "actor": "mcp", "workspace_id": workspace_id, "artifact_id": artifact_id, "expected_revision": 0,
                "design_system": system, "expected_system_revision": system_revision, "allowed_intents": ["design"],
            });
            if let Some(policy) = policy {
                args["direct_formatting"] = json!(policy);
            }
            args
        };
        let gui_args = |policy: Option<&str>| {
            let mut args = json!({ "design_system": system, "expected_system_revision": system_revision });
            if let Some(policy) = policy {
                args["direct_formatting"] = json!(policy);
            }
            args
        };

        for (fixture, policy) in [("office/basic-preserve.docx", "clear"), ("office/basic-preserve.docx", "keep"), ("hwpx/opaque-image-preserve.hwpx", "keep")] {
            let (gui_doc, mcp_doc) = backend.with(|host| (open_fixture(host, fixture), open_fixture(host, fixture)));
            let before = backend.with(|host| host.artifact(&gui_doc));
            assert_eq!(link_state(&before), link_state(&backend.with(|host| host.artifact(&mcp_doc))), "two opens of {fixture}");

            let (_, gui_answer) = backend.with(|host| gui_link(host, &gui_doc, gui_args(Some(policy))));
            let mcp_answer = mcp_tool(backend.clone(), "design_system_set", mcp_args(&mcp_doc, Some(policy)));
            assert_ne!(mcp_answer["isError"], true, "{fixture} {policy}: {mcp_answer}");
            let gui = backend.with(|host| full_artifact(host, &gui_doc));
            let mcp_result = backend.with(|host| full_artifact(host, &mcp_doc));

            assert_eq!(link_state(&gui), link_state(&mcp_result), "{fixture} {policy}: the GUI command and the MCP tool differ");
            assert_ne!(link_state(&gui), link_state(&before), "{fixture} {policy}: the link changed nothing");
            assert_eq!(diagnostics_of(&gui_answer), diagnostics_of(&mcp_answer["structuredContent"]), "{fixture} {policy}: diagnostics");
            assert!(diagnostics_of(&gui_answer).iter().any(|d| d["code"] == "DESIGN_STYLE_ABSENT"), "the other format's style is reported");
            assert_eq!(gui.design_system.as_ref().map(|reference| reference.revision), Some(system_revision));
            if fixture.ends_with(".docx") {
                let expected = if policy == "clear" { "#C00000".to_owned() } else { title_run_color(&before) };
                assert_eq!(title_run_color(&gui), expected, "{policy}: the title run's direct color");
                assert!(gui.native.as_ref().unwrap().preserved.contains_key("design_system_styles"));
            }
        }

        let (gui_doc, mcp_doc) = backend.with(|host| (open_fixture(host, "office/basic-preserve.docx"), open_fixture(host, "office/basic-preserve.docx")));
        let (gui, gui_answer) = backend.with(|host| gui_link(host, &gui_doc, gui_args(None)));
        assert!(gui_answer.is_null(), "the host refused the GUI link without a policy");
        let mcp_answer = mcp_tool(backend.clone(), "design_system_set", mcp_args(&mcp_doc, None));
        assert_eq!(mcp_answer["isError"], true, "{mcp_answer}");
        assert!(mcp_answer.to_string().contains("direct_formatting"), "{mcp_answer}");
        let mcp_result = backend.with(|host| host.artifact(&mcp_doc));
        assert_eq!((gui.revision, gui.design_system.clone()), (0, None));
        assert_eq!(link_state(&gui), link_state(&mcp_result), "both refusals leave the document as it was");
    }

    /// A publisher token update reaches the linked DOCX and HWPX consumers in its commit (keep), and one undo takes the whole
    /// group back.
    #[test]
    fn docx_consumer_follows_publisher_update_and_undo() {
        let mut host = Host::start();
        let (system, system_revision) = publish_named_style_colors(&mut host, "#C00000", "#00A050");
        let docx = open_fixture(&mut host, "office/basic-preserve.docx");
        let hwpx = open_fixture(&mut host, "hwpx/opaque-image-preserve.hwpx");
        let direct = title_run_color(&host.artifact(&docx));
        for consumer in [&docx, &hwpx] {
            let linked = host
                .call("design_system.set", json!({
                    "request_id": new_id(), "actor": "desktop", "workspace_id": host.workspace_id, "artifact_id": consumer,
                    "expected_revision": 0, "design_system": system, "expected_system_revision": system_revision,
                    "direct_formatting": "keep", "allowed_intents": ["design"],
                }))
                .expect("link with keep");
            let absent = linked["diagnostics"].as_array().expect("diagnostics").iter().any(|d| d["code"] == "DESIGN_STYLE_ABSENT");
            assert!(absent, "each document lacks the other format's style: {linked}");
        }
        let style_color = |artifact: &designllm_contracts::Artifact, style: &str| flow_body(artifact).styles[style].color.to_ascii_uppercase();
        assert_eq!(style_color(&host.artifact(&docx), "LunaTitle"), "#C00000");
        assert_eq!(style_color(&host.artifact(&hwpx), HWPX_UNUSED_STYLE), "#00A050");

        let revision = host.artifact(&system).revision;
        let update = designllm_contracts::ChangeSet {
            request_id: new_id(), actor: "desktop".into(), workspace_id: host.workspace_id.clone(), artifact_id: system.clone(),
            base_revision: revision, allowed_intents: [Intent::Design].into_iter().collect(), job_ref: None, read_dependencies: Vec::new(),
            operations: named_style_tokens("#2050D0", "#806000"),
        };
        let changed = host.call("changes.apply", serde_json::to_value(update).unwrap()).expect("the publisher update");
        let members = changed["group"]["members"].as_array().expect("a group result");
        assert_eq!(members.len(), 3, "the publisher and both consumers commit together: {changed}");
        let updated_docx = host.artifact(&docx);
        let updated_hwpx = host.artifact(&hwpx);
        assert_eq!(style_color(&updated_docx, "LunaTitle"), "#2050D0");
        assert_eq!(style_color(&updated_hwpx, HWPX_UNUSED_STYLE), "#806000");
        assert_eq!(title_run_color(&updated_docx), direct, "a publisher update keeps direct formatting");
        // Each consumer's report of the other format's style reaches the group result (naming the consumer) and its own event.
        for (consumer, absent_style) in [(&docx, HWPX_UNUSED_STYLE), (&hwpx, DOCX_TITLE_STYLE)] {
            let reported = changed["diagnostics"].as_array().expect("diagnostics").iter().any(|d| {
                d["code"] == "DESIGN_STYLE_ABSENT" && d["params"]["artifact_id"] == json!(consumer) && d["message"].as_str().is_some_and(|m| m.contains(absent_style))
            });
            assert!(reported, "the group result names {consumer}'s absent style: {changed}");
        }
        let events = host.call("events.poll", json!({ "after_seq": 0 })).expect("events");
        let group_id = changed["group"]["group_id"].clone();
        for consumer in [&docx, &hwpx] {
            let event = events["events"]
                .as_array()
                .expect("events")
                .iter()
                .find(|event| event["artifact_id"] == json!(consumer) && event["detail"]["group_id"] == group_id)
                .unwrap_or_else(|| panic!("the group event of {consumer}"));
            assert!(event["detail"]["diagnostics"].as_array().is_some_and(|d| d.iter().any(|d| d["code"] == "DESIGN_STYLE_ABSENT")), "{event}");
        }
        let pinned = changed["revision"].as_u64().unwrap();
        assert_eq!(updated_docx.design_system.as_ref().map(|reference| reference.revision), Some(pinned));
        assert_eq!(updated_hwpx.design_system.as_ref().map(|reference| reference.revision), Some(pinned));

        host.call("history.undo", json!({
            "request_id": new_id(), "actor": "desktop", "workspace_id": host.workspace_id, "artifact_id": docx,
            "expected_revision": updated_docx.revision, "allowed_intents": ["design"], "job_ref": null,
        }))
        .expect("undo from a consumer reverts the group");
        assert_eq!(style_color(&host.artifact(&docx), "LunaTitle"), "#C00000");
        assert_eq!(style_color(&host.artifact(&hwpx), HWPX_UNUSED_STYLE), "#00A050");
        assert_eq!(host.artifact(&hwpx).design_system.as_ref().map(|reference| reference.revision), Some(system_revision));

        // A later edit of the linked document re-applies the pinned tokens without changing the style or adding ids.
        let paragraph = match &flow_body(&host.artifact(&docx)).sections[0].blocks[0] {
            designllm_contracts::FlowBlock::Paragraph(paragraph) => paragraph.id.clone(),
            _ => unreachable!(),
        };
        let base_revision = host.artifact(&docx).revision;
        let edited = host
            .call("changes.apply", serde_json::to_value(designllm_contracts::ChangeSet {
                request_id: new_id(), actor: "desktop".into(), workspace_id: host.workspace_id.clone(), artifact_id: docx.clone(),
                base_revision, allowed_intents: [Intent::Content].into_iter().collect(), job_ref: None,
                read_dependencies: Vec::new(),
                operations: vec![Operation::ReplaceText {
                    range: designllm_contracts::TextRange { paragraph_id: paragraph.clone(), start: 0, end: 0 },
                    text: "새 ".into(),
                }],
            }).unwrap())
            .expect("an edit of a linked document");
        let changed_ids: Vec<&str> = edited["changed_ids"].as_array().unwrap().iter().filter_map(serde_json::Value::as_str).collect();
        assert!(!changed_ids.contains(&"LunaTitle"), "the re-applied tokens change nothing: {changed_ids:?}");
        assert_eq!(style_color(&host.artifact(&docx), "LunaTitle"), "#C00000");
    }

    /// Whether the window lists the command as enabled now.
    fn listed(window: &Window, id: &str) -> bool {
        window.app.ui_commands().iter().find(|command| command.id == id).expect("the command is listed").enabled
    }

    /// Outside a Canvas neither command is listed or runs, even with a diagram node of a Figure selected.
    #[test]
    fn the_image_commands_are_not_listed_outside_a_canvas() {
        let mut host = Host::start();
        let figure = drawn_figure(&mut host, "그림", 1);
        let (document, _) = host.figure(&figure);
        let node = match &document.panels[0].body {
            PanelBody::Diagram(diagram) => diagram.nodes[0].id.clone(),
            _ => panic!("a diagram panel"),
        };
        let mut window = Window::open(&mut host, &figure);
        window.step(&mut host, "select.node", json!({ "node_id": node })).expect("the diagram node is selected");
        assert_eq!(registry_and_runs(&mut window, &mut host), [(false, false), (false, false)]);
    }

    /// The picture commands are not listed, and do not run, on the home screen or while the window has not read its document: the
    /// listing asks the same checks as the runs.
    #[test]
    fn the_image_commands_are_not_listed_on_the_home_screen_or_before_the_document_is_read() {
        let mut host = Host::start();
        let canvas = canvas_with_picture(&mut host);
        let mut window = Window::open(&mut host, &canvas);
        window.step(&mut host, "select.node", json!({ "node_id": "picture" })).expect("the picture is selected");
        assert_eq!(registry_and_runs(&mut window, &mut host), [(true, true), (true, true)], "the picture takes both");

        window.step(&mut host, "nav.home", json!({})).expect("the home screen opens");
        assert_eq!(registry_and_runs(&mut window, &mut host), [(false, false), (false, false)], "the home screen edits nothing");

        window.step(&mut host, "nav.open_project", json!({ "artifact_id": canvas })).expect("the document opens again");
        window.step(&mut host, "select.node", json!({ "node_id": "picture" })).expect("the picture is selected again");
        assert_eq!(registry_and_runs(&mut window, &mut host), [(true, true), (true, true)], "back in the editor");

        window.app.context = None;
        assert_eq!(registry_and_runs(&mut window, &mut host), [(false, false), (false, false)], "the document is not read");
    }

    /// The page guide commands are listed as enabled, and run, only in the Canvas editor: not on the home screen and not in a
    /// figure. A refused run stores nothing.
    #[test]
    fn the_guide_commands_are_listed_and_run_only_in_the_canvas_editor() {
        const GUIDE_COMMANDS: [&str; 2] = ["canvas.guides.set", "canvas.guides.clear"];
        let mut host = Host::start();
        let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
        let figure = drawn_figure(&mut host, "그림", 1);
        let set = json!({ "axis": "columns", "count": 2, "spacing_emu": 0, "leading_margin_emu": 0, "trailing_margin_emu": 0 });

        let mut window = Window::open(&mut host, &canvas);
        assert!(GUIDE_COMMANDS.iter().all(|id| listed(&window, id)), "both are listed in the Canvas editor");
        window.step(&mut host, "nav.home", json!({})).expect("the home screen opens");
        assert!(GUIDE_COMMANDS.iter().all(|id| !listed(&window, id)), "neither is listed on the home screen");
        assert!(window.app.invoke("canvas.guides.set", set.clone()).is_err(), "the home screen runs no guide change");
        window.settle(&mut host);
        let stored = match host.artifact(&canvas).body {
            ArtifactBody::Canvas(document) => document.layout_guides.is_empty(),
            _ => false,
        };
        assert!(stored, "the refused run stored no guides");
        drop(window);

        let mut window = Window::open(&mut host, &figure);
        assert!(GUIDE_COMMANDS.iter().all(|id| !listed(&window, id)), "neither is listed in a figure");
        assert!(window.app.invoke("canvas.guides.set", set).is_err(), "a figure runs no guide change");
    }

    /// `view.zoom_selection` is listed as enabled, and runs, only in a Canvas or Slides editor with something selected.
    #[test]
    fn the_zoom_to_selection_is_listed_as_enabled_only_with_a_selection() {
        let mut host = Host::start();
        let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
        let mut window = Window::open(&mut host, &canvas);
        assert!(!listed(&window, "view.zoom_selection"), "nothing is selected");
        assert!(window.app.invoke("view.zoom_selection", json!({})).is_err(), "a zoom to nothing runs nothing");
        window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("a box is selected");
        assert!(listed(&window, "view.zoom_selection"), "a selection can be zoomed to");
    }

    /// `view.zoom_selection` is listed as enabled, and runs, only in a Canvas or Slides editor with something selected: a Figure or a
    /// WebCode refuses it, as neither has node frames to fit.
    #[test]
    fn the_zoom_to_selection_is_listed_only_for_canvas_and_slides() {
        let mut host = Host::start();
        let figure = drawn_figure(&mut host, "그림", 1);
        let (document, _) = host.figure(&figure);
        let node = match &document.panels[0].body {
            PanelBody::Diagram(diagram) => diagram.nodes[0].id.clone(),
            _ => panic!("a diagram panel"),
        };
        let mut window = Window::open(&mut host, &figure);
        window.step(&mut host, "select.node", json!({ "node_id": node })).expect("the diagram node is selected");
        assert!(!listed(&window, "view.zoom_selection"), "a Figure is not zoomed to its selection");
        assert!(window.app.invoke("view.zoom_selection", json!({})).is_err(), "a Figure refuses the zoom");

        let web = host.create("web_code", "웹 페이지");
        let mut window = Window::open(&mut host, &web);
        window.app.selected_nodes = vec![String::from("html")];
        assert!(!listed(&window, "view.zoom_selection"), "a WebCode is not zoomed to its selection");
        assert!(window.app.invoke("view.zoom_selection", json!({})).is_err(), "a WebCode refuses the zoom");

        let slides = host.create("slides", "발표");
        let mut window = Window::open(&mut host, &slides);
        window.app.selected_nodes = vec![String::from("slide")];
        assert!(listed(&window, "view.zoom_selection"), "Slides zoom to their selection");
    }

    /// `edit.delete` is listed as enabled by its run's own target check: with nothing selected the listing says no, and the run
    /// refuses with the same check.
    #[test]
    fn delete_is_listed_by_its_runs_target_check() {
        let mut host = Host::start();
        let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
        let mut window = Window::open(&mut host, &canvas);
        assert!(window.app.selected_nodes.is_empty(), "nothing is selected");
        assert!(!listed(&window, "edit.delete"), "nothing to delete");
        assert_eq!(window.app.invoke("edit.delete", json!({})), Err("지울 대상을 고르세요".into()));
        assert!(!listed(&window, "add.figure_link"), "the workspace has no figure");
        window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("a box is selected");
        assert!(listed(&window, "edit.delete"), "the selected box can be deleted");
    }

    /// Regression (backlog desktop 29): the commands whose runs take the selection were listed as enabled with nothing selected
    /// (`_ => true`), and every run was refused. They follow their run's selection check now, and the published registry
    /// follows the selection.
    #[test]
    fn the_commands_that_take_the_selection_are_listed_by_it() {
        let mut host = Host::start();
        let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
        let mut window = Window::open(&mut host, &canvas);
        assert!(window.app.selected_nodes.is_empty(), "nothing is selected");
        for id in super::SELECTED_NODES_COMMANDS.iter().chain(super::SELECTED_ON_PAGE_COMMANDS) {
            assert!(!listed(&window, id), "{id} is not offered with nothing selected");
        }
        assert_eq!(window.app.invoke("edit.lock", json!({})), Err("대상을 고르세요".into()), "the run refuses the same way");
        assert_eq!(window.app.invoke("edit.nudge", json!({ "x": 1, "y": 0 })), Err("옮길 대상을 고르세요".into()));
        assert_eq!(window.app.invoke("canvas.token.bind", json!({ "token_id": "t", "target": "style_fill" })), Err("토큰을 연결할 캔버스 요소를 선택하세요".into()));
        let enabled_on_host = |window: &mut Window, host: &mut Host, id: &str| {
            window.app.publish_ui_if_due();
            window.settle(host);
            let listing = host.call("ui.command.list", json!({ "view_id": window.app.ui.view_id.clone() })).expect("the registry");
            listing["commands"].as_array().expect("commands").iter().find(|item| item["id"] == id).and_then(|item| item["enabled"].as_bool())
        };
        assert_eq!(enabled_on_host(&mut window, &mut host, "edit.lock"), Some(false), "the published registry says no");

        window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("a box is selected");
        for id in ["edit.lock", "edit.rename", "props.fill", "props.opacity", "edit.nudge", "edit.order_front", "props.layout_gap"] {
            assert!(listed(&window, id), "{id} is offered for the selected box");
        }
        assert_eq!(enabled_on_host(&mut window, &mut host, "edit.lock"), Some(true), "the registry is published again with the selection");
    }

    /// A link with a Unicode format character (here U+202E, right-to-left override) is refused by the window before anything is
    /// sent, with the same words as any other address it does not take, as the core's rule for new links refuses it.
    #[test]
    fn a_link_with_a_format_character_is_refused_before_it_is_sent() {
        let mut host = Host::start();
        let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
        let mut window = Window::open(&mut host, &canvas);
        window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("a box is selected");
        let revision = host.artifact(&canvas).revision;
        let refused = Err("HTTPS, mailto 또는 tel 주소를 입력하세요".to_owned());
        assert_eq!(window.app.invoke("canvas.open_url", json!({ "url": "https://example.test/\u{202E}a" })), refused);
        assert_eq!(window.app.invoke("canvas.interaction.set", json!({ "index": 0, "action": "open_url", "url": "https://example.test/\u{200B}" })), refused);
        window.settle(&mut host);
        assert_eq!(host.artifact(&canvas).revision, revision, "nothing was sent");
    }

    /// A refused commit that an agent's run started leaves no ticket behind: only an applied answer brings a selection.
    #[test]
    fn a_refused_agent_commit_forgets_its_ticket() {
        use crate::rpc::{RpcReply, RpcWorker};
        let (rpc, replies, _work) = RpcWorker::detached();
        let mut app = crate::app::DesktopApp::new(rpc, "작업공간".into(), None, None);
        let ticket = app.call("changes.apply", json!({}), crate::app::PendingAction::WorkspaceList).expect("the call is sent");
        app.ui.remember_agent_commit(ticket);
        let refused = designllm_contracts::DltError::new(designllm_contracts::ErrorCode::InvalidArgument, "refused");
        replies.send(RpcReply { ticket, result: Err(refused) }).expect("the window listens");
        app.process_replies(&eframe::egui::Context::default());
        assert!(!app.ui.take_agent_commit(ticket), "the refused commit's ticket is gone");
    }
}
