use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{mpsc::{self, Receiver}, Arc, Mutex},
    time::{Duration, Instant},
};

use designllm_bridge::home::HomeRequest;
use designllm_contracts::{
    new_id, sha256, AssetRef, Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, CanvasBoardMemo, ChangeSet,
    ContextSnapshot, Dataset, DesignSystemListResult, Diagnostic, DltError,
    DltResult, Editability, ErrorCode, ExportDryRun, ExportFileRequest, ExportRequest, Feature,
    FigureRevisionRef, FlowBlock, HistoryRequest, Intent, Job, JobStartRequest, JobStatus, Node,
    NodeKind, Operation, Page, Paragraph, ParagraphFormat, RenderPage, RenderProfile,
    RenderSnapshot, ReservationRequest, SaveRequest,
    Selection, SelectionRequest, Size, Style, StylePatch, Support, TextRange, TextRun,
    ViewTransform, EMU_PER_MM, EMU_PER_PT,
};
use eframe::egui::{
    self, Context, Id, Pos2, Rect, Sense,
    Ui, Vec2, ViewportCommand,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde::Deserialize;
use serde_json::{json, Value};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    grapheme::{GraphemeAdapter, TextInputEdit},
    notice::{Notices, Severity},
    rpc::{RpcPayload, RpcReply, RpcWorker},
};

mod board;
mod board_memos;
mod canvas_find;
#[cfg(test)] mod canvas_find_tests;
#[cfg(test)] mod undo_selection_tests;
mod page_sections;
mod print_range;
mod page_guides;
mod chrome;
mod data_sheet;
mod dispatch;
mod figure_tools;
mod figure_math;
mod fix_commands;
mod home;
mod kind_commands;
mod lint;
mod links;
mod multi;
mod object_menu;
mod properties;
mod repeat_groups;
mod selection;
mod settings;
mod sidebar;
mod tools;
mod toolbox;
#[cfg(test)] mod toolbox_tests;
mod video_insert;
mod thumbs;
mod ui_channel;
mod ui_test_input;
mod perf;
#[cfg(test)] mod ui_test_input_tests;
pub(crate) use ui_test_input::{ui_test_mode_from_env, ui_test_scale_from_env, UiTestMode};
/// The UI-test mode, window visibility and dialog refusal of a launch (`smoke`: a smoke capture).
pub(crate) fn ui_test_launch_mode(smoke: bool) -> (Option<UiTestMode>, bool, bool) {
    ui_test_input::launch_mode(smoke, std::env::var(ui_test_input::UI_TEST_INPUT_ENV).ok().as_deref())
}
mod runner_models;
mod figure_ai_attach;
mod figure_close;
mod figure_r07;
mod drafts;
mod apply;
mod close_guard;
mod doc_tree;
mod drag;
mod edit_fields;
mod events;
mod held_edit;
mod host_link;
pub(crate) use host_link::HostStarter;
mod image_edit;
mod inline_cell_nav;
mod inline_text;
mod nudge_keys;
mod jobs;
mod messages;
mod operation_commands;
mod payload;
mod pdf_coverage;
mod proposals;
mod replies;
mod save_export;
mod smoke;
mod state;
mod table_merge;
mod view_fetch;
mod web_preview;
mod web_mapping;
mod web_runtime;
mod workspace_info;
#[cfg(test)] mod web_mapping_tests;
#[cfg(test)] mod repeat_groups_tests;
#[cfg(test)]
mod drafts_tests;
#[cfg(test)]
mod figure_ai_attach_tests;
#[cfg(test)]
mod figure_close_tests;
#[cfg(test)]
mod figure_diagnostic_tests;
#[cfg(test)]
mod figure_plot_display_tests;
#[cfg(test)]
mod figure_r07_tests;
#[cfg(test)]
mod figure_math_tests;
#[cfg(test)]
mod figure_flow_tests;
#[cfg(test)]
mod group_flow_tests;
#[cfg(test)]
mod link_flow_tests;
#[cfg(test)]
mod command_calls_tests;
#[cfg(test)]
mod page_guides_tests;
#[cfg(test)]
mod held_edit_tests;
#[cfg(test)]
mod inline_table_cell_integrity_tests;
#[cfg(test)]
mod inline_table_cell_gui_tests;
#[cfg(test)]
mod inline_hit_test_integrity_tests;
#[cfg(test)]
mod image_replacement_tests;
#[cfg(test)]
mod image_crop_tests;
#[cfg(test)]
mod video_insert_tests;
#[cfg(test)]
mod pixel_preview_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod layout_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod parity_tests;
#[cfg(test)]
mod dpi_tests;
#[cfg(test)]
mod proposals_tests;
#[cfg(test)]
mod gui_check_64_tests;
#[cfg(test)]
mod panel_close_tests;

use figure_ai_attach::{FigureAiAttachments, FigureAiRequest};
use state::{AgentBatch, BoardMemoEditor, BoardMemoGesture, BoardMemoMutation, CoveredTableTextSelection, CsvImportDraft, DatasetCellEdit, DragState, ExportConfirmation, FigureForm, FlowFindCursor, FlowFindLookup, FlowFindPageRequest, FlowFindSearch, HeldEdit, HeldPdfEvidence, ImageCropEditor, ImageCropStage, ImageReplacementStage, InlineCellReadiness, InlineTextEdit, JobReservation, NodeClipboard, PdfEvidenceTarget, PendingAction, PendingImageReplacement, PendingPageReorderSelection, PendingRequest, PendingSaveNotice, PendingVideoInsert, Screen, SelectionRestoreGuard, TextTarget, UserReservation, VideoInsertStage, VideoPosterWorkerResult};
pub(super) use messages::{startup_recovery_warning, user_error};
pub(super) use video_insert::remove_orphaned_video_poster_temp;

pub struct DesktopApp {
    rpc: RpcWorker,
    next_ticket: u64,
    pending: HashMap<u64, PendingRequest>,
    artifacts: Vec<ArtifactSummary>,
    active: Option<ArtifactSummary>,
    artifact: Option<Artifact>,
    context: Option<ContextSnapshot>,
    render: Option<RenderSnapshot>,
    /// Rendered pages on the board, by page index.
    page_textures: HashMap<u32, board::PageTexture>,
    /// Per page: the part on screen at the shown resolution, and the last region asked for (rect, dpi, revision).
    region_textures: HashMap<u32, board::RegionTexture>,
    requested_regions: HashMap<u32, (designllm_contracts::Rect, u32, u64)>,
    /// The view transform last sent to the core for each page (hit tests use it).
    page_transforms: HashMap<u32, ViewTransform>,
    /// The id of the page transform the core holds for this view (it keeps one, for one page; board.rs `hit_test_page`).
    core_transform_id: Option<String>,
    /// A page transform the core refused: not sent again (`send_page_transform`) until the placement changes and makes a new one.
    failed_transform_id: Option<String>,
    /// (dpi, revision) of the last render request per page, so that one is not sent twice.
    requested_pages: HashMap<u32, (u32, u64)>,
    /// Page sizes the core reported, for pages the board has not drawn yet.
    known_sizes: HashMap<u32, Size>,
    /// Per-document view (zoom, pan, panels, tab), kept while the app runs.
    editor_views: HashMap<String, board::EditorView>,
    screen: Screen,
    home: home::HomeState,
    workspace_name: String,
    board_rect: Option<Rect>,
    board_dpi: u32,
    /// Set when a node region was clicked this frame, so that a click on the empty board clears.
    region_clicked: bool,
    /// The explanation of the last refused edit, shown next to the selection.
    refusal: Option<String>,
    refusal_ids: Vec<String>,
    held_refusal_id: Option<String>,
    ui: ui_channel::UiChannel,
    run_context: Option<ui_channel::RunContext>,
    /// The synthetic input of a window started for UI tests (task 69); None in every other window.
    ui_test: Option<ui_test_input::UiTestInput>,
    layer_rename: Option<sidebar::LayerRename>,
    node_clipboard: Option<NodeClipboard>,
    style_clipboard: Option<StylePatch>,
    layer_find_open: bool,
    layer_find_query: String,
    layer_find_focus: bool,
    /// The Canvas search match still to bring into view once its page is drawn (`canvas_find.rs`).
    canvas_find_reveal: Option<String>,
    /// Why no WebView2 preview can open, or None (`web_runtime.rs`).
    web_runtime_unavailable: Option<String>,
    flow_find: Option<FlowFindSearch>,
    flow_find_generation: u64,
    flow_find_lookup: Option<FlowFindLookup>,
    flow_find_request: Option<FlowFindPageRequest>,
    flow_find_cursor: Option<FlowFindCursor>,
    flow_find_cached_page: Option<(String, u64, RenderPage)>,
    recent_open: bool,
    export_open: bool,
    export_scale: u32,
    /// `page` (the current page) or `all`.
    export_scope: String,
    /// PNG only: a transparent background instead of the page colour.
    export_transparent: bool,
    /// The losses of the popover's current settings (`export_key`), from a dry run.
    export_preview: Option<(String, Vec<designllm_contracts::Diagnostic>)>,
    export_preview_pending: Option<String>,
    page_index: u32,
    pending_page_reorder_selection: Option<PendingPageReorderSelection>,
    total_pages: u32,
    view_id: String,
    open_path: String,
    save_path: String,
    export_path: String,
    export_format: String,
    pending_export: Option<ExportFileRequest>,
    pending_image_replacement: Option<PendingImageReplacement>,
    image_crop_editor: Option<ImageCropEditor>,
    video_insert: Option<PendingVideoInsert>,
    video_worker_tx: mpsc::Sender<VideoPosterWorkerResult>,
    video_worker_rx: Receiver<VideoPosterWorkerResult>,
    export_confirmation: Option<ExportConfirmation>,
    agent_batches: Vec<AgentBatch>,
    reservations: Vec<JobReservation>,
    user_reservations: Vec<UserReservation>,
    reservations_for: Option<String>,
    /// The engine's style list of a text document: (artifact id, revision, list). The list is None while it is
    /// asked for, or when the engine has none (the panel then falls back to the model's styles).
    flow_styles: Option<(String, u64, Option<Vec<designllm_contracts::FlowStyleInfo>>)>,
    /// The fix buttons of the refusal on screen (tokens `text.lint-rules`), at most two.
    refusal_fixes: Vec<selection::RefusalFix>,
    /// The design-check warnings of the open document (tokens `component.lint-warning`): non-blocking diagnostics
    /// with a `node_id`, from change results and validation. Replaced as a whole on each report; the selection
    /// overlay shows those of the one selected node.
    lint_warnings: Vec<designllm_contracts::Diagnostic>,
    /// User edits refused by a reservation; each retains its own original ChangeSet context.
    held_edits: Vec<HeldEdit>,
    close_confirmation_open: bool,
    permit_close_once: bool,
    close_after_inline_cleanup: bool,
    close_inline_session_id: Option<u64>,
    close_release_failed: bool,
    close_release_pending: bool,
    close_reservation_id: Option<String>,
    close_outcome_uncertain: bool,
    selected_node: Option<String>,
    selected_nodes: Vec<String>,
    covered_table_text_selection: Option<CoveredTableTextSelection>,
    cell_nav: inline_cell_nav::CellNav,
    table_merge: table_merge::TableMergeState,
    selected_board_memo: Option<String>,
    board_memo_editor: Option<BoardMemoEditor>,
    board_memo_mutation: Option<BoardMemoMutation>,
    board_memo_gesture: Option<BoardMemoGesture>,
    page_section_editor: Option<page_sections::SectionEditor>,
    page_section_mutation: Option<page_sections::SectionMutation>,
    /// The selection shown by the window, including the exact local Flow text range while a
    /// `selection.set` request is waiting for the host to confirm it.
    local_selection: Option<Selection>,
    selection_generation: u64,
    marquee: Option<(eframe::egui::Pos2, eframe::egui::Pos2)>,
    text_target: Option<TextTarget>,
    text_draft: String,
    text_editor_focused: bool,
    /// The arrow keys' presses not sent yet and the locked selection they told about (`nudge_keys`).
    nudge: nudge_keys::NudgeKeys,
    inline_text_edit: Option<InlineTextEdit>,
    next_text_session_id: u64,
    text_editor_id: Id,
    text_adapter: GraphemeAdapter,
    style_fill: String,
    style_color: String,
    style_font_size_pt: f32,
    token_panel: properties::TokenPanelState,
    geometry_mm: [f64; 4],
    paragraph_align: String,
    paragraph_space_before_pt: f32,
    paragraph_space_after_pt: f32,
    paragraph_line_spacing_pt: f32,
    paragraph_indent_mm: f32,
    ai_prompt: String,
    /// `nav.focus_request` in the editor: the AI tab's request box takes the keyboard when it is next drawn.
    ai_focus_request: bool,
    host_link: host_link::HostLink,
    /// The id `nav.open_project` did not find, for which the work list was read again (dispatch.rs).
    list_reread_for: Option<String>,
    ai_figure_refs: Vec<FigureRevisionRef>,
    /// Evidence files the open Figure's AI request carries, bound to the Figure they were attached for. Not the home list
    /// (`home.attachments`).
    figure_ai_attachments: Option<FigureAiAttachments>,
    /// The Figure AI request whose evidence is being imported or whose job is being started.
    figure_ai_request: Option<FigureAiRequest>,
    /// A document the window was asked to switch to while figure work is held; it waits for the answer (`figure_close`).
    pending_switch: Option<ArtifactSummary>,
    ai_runtime: String,
    ai_model: String,
    ai_effort: String,
    /// The runner menu's model lists, asked from each CLI once (`runner_models.rs`).
    runner_models: runner_models::RunnerModels,
    job: Option<Job>,
    /// Figure AI jobs the window does not show: those of other documents, kept aside while those documents are not open (their
    /// status and warnings stay right, but they are neither shown nor cancelled from the open document), and a running job of
    /// the open document that a newer start replaced as the shown one (`figure_close.rs`).
    parked_jobs: Vec<Job>,
    /// The job starts and evidence imports the window waits for, by ticket, with when each was sent (`expire_awaited_starts`).
    awaited_starts: BTreeMap<u64, Instant>,
    /// The job starts the window sent and has no answer for yet, by ticket (`SentStart`): a repeat of an unanswered start keeps its id.
    job_starts: BTreeMap<u64, figure_close::SentStart>,
    /// The request each job this window started was sent with, by job id, for the AI tab's job label. A job another client
    /// started has none here and gets the general label. Kept while the job is known (`place_jobs`).
    job_prompts: BTreeMap<String, String>,
    /// The window's egui context, kept from the first frame for the app's exit (`on_exit` has none of its own).
    exit_ctx: Option<Context>,
    /// The job starts the window no longer waits for (its document was left, or its wait timed out), by ticket, with their document.
    /// The host may still run their jobs, so their answers are recorded (`late_job_start`), and a new request for the document waits.
    late_starts: BTreeMap<u64, String>,
    /// The starts whose answer left their outcome unknown (the connection was lost), several per document, at most
    /// `UNKNOWN_STARTS_MAX` in all and in memory only. A repeat of the same request reuses its id (`with_job_request_id`).
    unknown_starts: figure_close::UnknownStarts,
    last_job_poll: Instant,
    last_event_seq: u64,
    last_event_poll: Instant,
    smoke_started: Instant,
    smoke_frames: u32,
    workspace_loaded: bool,
    notices: Notices,
    acknowledged_workspace_notices: BTreeSet<String>,
    pending_save_notices: BTreeMap<String, PendingSaveNotice>,
    last_resync: Option<Instant>,
    figure_form: Option<FigureForm>,
    connect_from: Option<String>,
    connect_to: Option<String>,
    /// Set when a popover or form opens, so its path field takes the keyboard cursor once.
    focus_popup_field: bool,
    /// The selection of the previous revision, to ask for again once the new document is loaded.
    reselect: Option<Selection>,
    /// A picture the window just put into the document: selected as soon as the document that
    /// holds it is loaded.
    select_after_commit: Option<Vec<String>>,
    /// The Apply ticket that owns the post-commit selection, when a selection is tied to a successful reply.
    select_after_commit_ticket: Option<u64>,
    /// A new table is selected only if the view still has the same selection and page.
    select_after_commit_guard: Option<SelectionRestoreGuard>,
    drag: Option<DragState>,
    next_drag_token: u64,
    dataset_edit: Option<DatasetCellEdit>,
    csv_path: String,
    pdf_path: String,
    pdf_page: String,
    /// The figure the PDF import in flight was started for; None when no PDF import is on its way (`figure_tools`).
    pdf_evidence_target: Option<PdfEvidenceTarget>,
    /// PDF evidence not in its figure yet, until it is retried, copied or discarded (`figure_tools`).
    held_pdf_evidence: Vec<HeldPdfEvidence>,
    pdf_coverage: Option<pdf_coverage::PdfCoverage>,
    csv_draft: Option<CsvImportDraft>,
    page_rect: Option<Rect>,
    allow_content: bool,
    allow_design: bool,
    allow_data: bool,
    active_web_path: Option<String>,
    parent_hwnd: Option<isize>,
    web_preview_child: Option<Child>,
    web_preview_run: Option<web_preview::PreviewRun>,
    #[cfg(test)]
    fake_preview_command: Option<Command>,
    smoke_output: Option<PathBuf>,
    smoke_result: Option<Arc<Mutex<Option<String>>>>,
    smoke_failure: Option<String>,
    smoke_pdf_codes: Vec<String>,
    smoke_requested: bool,
    smoke_scene_done: bool,
    smoke_scene_frame: u32,
    smoke_export_format_done: bool,
    /// A popup to open on the next frame: the export popover from a title row's export button, or for smoke
    /// captures (`DESIGNLLM_SMOKE_OPEN=export`).
    smoke_open: Option<String>,
    /// Where the view group was drawn last frame (the selection toolbar keeps clear of it).
    view_group_rect: Option<Rect>,
    /// The GPU's largest texture side (egui input), read each frame; textures are rendered to fit it.
    max_texture_side: usize,
    /// The workspace's GUI settings file (`settings::settings_path`); none in tests.
    settings_path: Option<PathBuf>,
    /// The frame preset the home size chip shows for the chosen tile (empty: the tile's default).
    home_preset: String,
    /// Unsent requests and editor drafts, written to the workspace's `.gui/drafts.json` (`drafts.rs`).
    drafts: drafts::DraftBook,
    /// The proposal rows of the window and the AI request's delivery choice (`proposals.rs`).
    proposals: proposals::ProposalBook,
}

impl DesktopApp {
    pub fn new(
        rpc: RpcWorker,
        workspace_name: String,
        smoke_output: Option<PathBuf>,
        smoke_result: Option<Arc<Mutex<Option<String>>>>,
    ) -> Self {
        let (video_worker_tx, video_worker_rx) = mpsc::channel();
        let mut app = Self {
            rpc,
            video_worker_tx,
            video_worker_rx,
            next_ticket: 1,
            pending: HashMap::new(),
            artifacts: Vec::new(),
            active: None,
            artifact: None,
            context: None,
            render: None,
            page_textures: HashMap::new(),
            region_textures: HashMap::new(),
            requested_regions: HashMap::new(),
            page_transforms: HashMap::new(),
            core_transform_id: None,
            failed_transform_id: None,
            requested_pages: HashMap::new(),
            known_sizes: HashMap::new(),
            editor_views: HashMap::new(),
            screen: Screen::Home,
            home: home::HomeState::default(),
            workspace_name,
            board_rect: None,
            board_dpi: 96,
            region_clicked: false,
            refusal: None,
            refusal_ids: Vec::new(),
            held_refusal_id: None,
            ui: ui_channel::UiChannel::default(),
            run_context: None,
            ui_test: None,
            layer_rename: None,
            node_clipboard: None,
            style_clipboard: None,
            layer_find_open: false,
            layer_find_query: String::new(),
            layer_find_focus: false,
            canvas_find_reveal: None,
            web_runtime_unavailable: web_runtime::web_runtime_reason_at_start(),
            flow_find: None,
            flow_find_generation: 0,
            flow_find_lookup: None,
            flow_find_request: None,
            flow_find_cursor: None,
            flow_find_cached_page: None,
            recent_open: false,
            export_open: false,
            export_scale: 1,
            export_scope: String::from("page"),
            export_transparent: false,
            export_preview: None,
            export_preview_pending: None,
            page_index: 0,
            pending_page_reorder_selection: None,
            total_pages: 0,
            view_id: new_id(),
            open_path: String::new(),
            save_path: String::new(),
            export_path: String::new(),
            export_format: "pdf".into(),
            pending_export: None,
            pending_image_replacement: None,
            image_crop_editor: None,
            video_insert: None,
            export_confirmation: None,
            agent_batches: Vec::new(),
            reservations: Vec::new(),
            user_reservations: Vec::new(),
            reservations_for: None,
            flow_styles: None,
            refusal_fixes: Vec::new(),
            lint_warnings: Vec::new(),
            held_edits: Vec::new(),
            close_confirmation_open: false,
            permit_close_once: false,
            close_after_inline_cleanup: false,
            close_inline_session_id: None,
            close_release_failed: false,
            close_release_pending: false,
            close_reservation_id: None,
            close_outcome_uncertain: false,
            selected_node: None,
            selected_nodes: Vec::new(),
            covered_table_text_selection: None,
            cell_nav: Default::default(),
            table_merge: Default::default(),
            selected_board_memo: None,
            board_memo_editor: None,
            board_memo_mutation: None,
            board_memo_gesture: None,
            page_section_editor: None,
            page_section_mutation: None,
            local_selection: None,
            selection_generation: 0,
            marquee: None,
            text_target: None,
            text_draft: String::new(),
            text_editor_focused: false,
            nudge: nudge_keys::NudgeKeys::default(),
            inline_text_edit: None,
            next_text_session_id: 1,
            text_editor_id: Id::new("document-text-editor"),
            text_adapter: GraphemeAdapter::default(),
            style_fill: "#FFFFFF".into(),
            style_color: "#20252D".into(),
            style_font_size_pt: 12.0,
            token_panel: properties::TokenPanelState::default(),
            geometry_mm: [0.0; 4],
            paragraph_align: "left".into(),
            paragraph_space_before_pt: 0.0,
            paragraph_space_after_pt: 6.0,
            paragraph_line_spacing_pt: 0.0,
            paragraph_indent_mm: 0.0,
            ai_prompt: String::new(),
            ai_focus_request: false,
            host_link: Default::default(),
            list_reread_for: None,
            ai_figure_refs: Vec::new(),
            figure_ai_attachments: None,
            figure_ai_request: None,
            pending_switch: None,
            ai_runtime: "claude".into(),
            ai_model: String::new(),
            ai_effort: "high".into(),
            runner_models: Default::default(),
            job: None,
            parked_jobs: Vec::new(),
            awaited_starts: BTreeMap::new(),
            job_starts: BTreeMap::new(),
            job_prompts: BTreeMap::new(),
            exit_ctx: None,
            late_starts: BTreeMap::new(),
            unknown_starts: figure_close::UnknownStarts::default(),
            last_job_poll: Instant::now(),
            last_event_seq: 0,
            last_event_poll: Instant::now(),
            smoke_started: Instant::now(),
            smoke_frames: 0,
            workspace_loaded: false,
            notices: Notices::default(),
            acknowledged_workspace_notices: BTreeSet::new(),
            pending_save_notices: BTreeMap::new(),
            last_resync: None,
            figure_form: None,
            connect_from: None,
            connect_to: None,
            focus_popup_field: false,
            reselect: None,
            select_after_commit: None,
            select_after_commit_ticket: None,
            select_after_commit_guard: None,
            drag: None,
            next_drag_token: 1,
            dataset_edit: None,
            csv_path: String::new(),
            pdf_path: String::new(),
            pdf_page: "1".into(),
            pdf_evidence_target: None,
            held_pdf_evidence: Vec::new(),
            pdf_coverage: None,
            csv_draft: None,
            page_rect: None,
            allow_content: true,
            allow_design: true,
            allow_data: false,
            active_web_path: None,
            parent_hwnd: None,
            web_preview_child: None,
            web_preview_run: None,
            #[cfg(test)]
            fake_preview_command: None,
            smoke_output,
            smoke_result,
            smoke_failure: None,
            smoke_pdf_codes: Vec::new(),
            smoke_requested: false,
            smoke_scene_done: false,
            smoke_scene_frame: 0,
            smoke_export_format_done: false,
            smoke_open: None,
            view_group_rect: None,
            max_texture_side: 2048,
            settings_path: None,
            home_preset: String::new(),
            drafts: drafts::DraftBook::default(),
            proposals: proposals::ProposalBook::default(),
        };
        app.call("system.hello", json!({}), PendingAction::Hello);
        app.call("workspace.list", json!({}), PendingAction::WorkspaceList);
        app
    }

    fn call(&mut self, method: &str, mut params: Value, action: PendingAction) -> Option<u64> {
        // While the host is gone nothing is sent (host_link.rs): the board keeps its last pages, and no refusal is raised.
        if self.host_lost() {
            return None;
        }
        ui_channel::mark_agent_path(self.run_context.as_ref(), method, &mut params);
        // A UI-test window starts no model run but the mock runtime (task 69 r3), whoever asked for it.
        if let Some(reason) = self.ui_test_run_refusal(method, &params) {
            self.refuse_ui_test_run(reason);
            return None;
        }
        ui_test_input::pin_ui_test_request(self.ui_test.as_ref(), method, &mut params);
        let mut image_replacement_cancelled = false;
        let replacement = self.pending_image_replacement.as_ref()
            .filter(|pending| self.active.as_ref().is_some_and(|active| active.id == pending.intent.artifact_id))
            .map(|pending| (pending.intent.token.clone(), pending.intent.node_id.clone(), pending.stage));
        let competing_method = matches!(method,
            "changes.apply" | "history.undo" | "history.redo" | "history.undo_change"
                | "job.start" | "job.resume" | "design_system.publish" | "design_system.set" | "proposal.apply"
        );
        let call_artifact_id = params.get("artifact_id").and_then(Value::as_str)
            .or_else(|| self.active.as_ref().map(|active| active.id.as_str()));
        let video_applying = self.video_insert.as_ref()
            .filter(|pending| pending.stage == VideoInsertStage::Applying
                && call_artifact_id == Some(pending.intent.artifact_id.as_str()))
            .map(|pending| pending.intent.token.clone());
        if let Some(token) = video_applying.filter(|_| competing_method) {
            let own_video_apply = method == "changes.apply"
                && matches!(&action, PendingAction::VideoInsertApply { token: action_token } if action_token == &token);
            if !own_video_apply {
                let error = DltError::new(ErrorCode::TargetBusy, "a video insertion is being applied to this artifact");
                if let Some(run) = self.run_context.as_mut() {
                    run.mutation_attempted = true;
                    if run.await_rpc_reply { run.enqueue_error = Some(error); }
                }
                self.set_error("동영상 노드를 추가하는 동안 같은 작업물의 다른 변경을 잠시 보류합니다".into());
                return None;
            }
        }
        if let Some((token, _, stage)) = replacement.filter(|(_, target_node_id, _)| {
            competing_method || method == "targets.reserve" && params.get("node_ids").and_then(Value::as_array)
                .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(target_node_id.as_str())))
        }) {
            let own_replacement_apply = method == "changes.apply"
                && matches!(&action, PendingAction::ImageReplacementApply { token: action_token } if action_token == &token);
            match stage {
                ImageReplacementStage::Importing => {
                    self.pending_image_replacement = None;
                    image_replacement_cancelled = true;
                }
                ImageReplacementStage::Applying if !own_replacement_apply => {
                    if let Some(run) = self.run_context.as_mut() {
                        run.mutation_attempted = true;
                        if run.await_rpc_reply {
                            run.enqueue_error = Some(DltError::new(
                                ErrorCode::TargetBusy,
                                "an image replacement is being applied to this artifact",
                            ));
                        }
                    }
                    self.set_error("이미지 교체를 적용하는 동안 같은 작업물의 다른 변경을 잠시 보류합니다".into());
                    return None;
                }
                _ => {}
            }
        }
        let crop_applying = self.image_crop_editor.as_ref()
            .filter(|editor| editor.stage == ImageCropStage::Applying
                && self.active.as_ref().is_some_and(|active| active.id == editor.intent.artifact_id))
            .map(|editor| (editor.intent.token.clone(), editor.intent.node_id.clone()));
        if let Some((token, _)) = crop_applying.filter(|(_, target_node_id)| {
            competing_method || method == "targets.reserve" && params.get("node_ids").and_then(Value::as_array)
                .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(target_node_id.as_str())))
        }) {
            let own_crop_apply = method == "changes.apply"
                && matches!(&action, PendingAction::ImageCropApply { token: action_token } if action_token == &token);
            if !own_crop_apply {
                if let Some(run) = self.run_context.as_mut() {
                    run.mutation_attempted = true;
                    if run.await_rpc_reply {
                        run.enqueue_error = Some(DltError::new(ErrorCode::TargetBusy, "an image crop is being applied to this artifact"));
                    }
                }
                self.set_error("이미지 자르기를 적용하는 동안 같은 작업물의 다른 변경은 보내지 않습니다".into());
                return None;
            }
        }
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        let ui_run_rpc = self.run_context.as_ref().is_some_and(|run| {
            run.await_rpc_reply
                && (matches!(method, "changes.apply" | "history.undo" | "history.redo" | "history.undo_change" | "proposal.apply")
                    || (method == "selection.set"
                        && matches!(run.command_id.as_str(), "select.next" | "select.previous" | "layer.find.next" | "layer.find.previous"))
                    || (method == "artifact.save" && run.command_id == "file.save")
                    || (method == "artifact.export" && matches!(run.command_id.as_str(), "file.export" | "file.export_selection"))
                    || (method == "artifact.open" && run.command_id == "nav.open_file")
                    || (method == "web.materialize" && run.command_id == "web.preview")
                    || (method == "artifact.create" && matches!(run.command_id.as_str(), "home.create" | "home.use_template"))
                    || method == "figure.dataset.edit" || action.prepares_figure_change()
                    || (method == "job.cancel" && run.command_id == "ai.cancel")
                    || matches!(method, "design_system.list" | "design_system.publish" | "design_system.set"))
        });
        if ui_run_rpc {
            if let Some(run) = self.run_context.as_mut() {
                run.mutation_attempted = true;
                if matches!(method, "artifact.create" | "artifact.open" | "figure.dataset.edit") {
                    params["actor"] = json!(run.actor);
                }
                if method == "figure.dataset.edit" {
                    params["allowed_intents"] = json!(run.allowed_intents);
                }
            }
        }
        if action.user_initiated() {
            self.notices.clear_transient();
        }
        if image_replacement_cancelled {
            self.notices.warning("다른 작업을 시작해 이미지 교체 요청을 취소했습니다. 이미 가져온 자산은 사용되지 않은 채 남을 수 있습니다", Instant::now());
        }
        // The record of a job start is kept until its answer, so that an answer that leaves the outcome unknown can be repeated.
        let sent_start = (method == "job.start").then(|| figure_close::SentStart::of(&params)).flatten();
        // The flow search reads only the page's scene, not its picture: its render.get gets no raw pixels (pixels.rs).
        match self.rpc.call_with(ticket, method, params, !matches!(action, PendingAction::FlowFindPage { .. })) {
            Ok(()) => {
                let in_flight = self.pending.len();
                if let Some(test) = self.ui_test.as_mut() {
                    test.perf.sent(ticket, method, in_flight);
                }
                let scoped = matches!(
                    method,
                    "context.read"
                        | "artifact.inspect"
                        | "render.get"
                        | "render.hit_test"
                        | "view.transform.set"
                        | "selection.set"
                        | "changes.apply"
                        | "proposal.apply"
                        | "history.undo"
                        | "history.redo"
                        | "history.undo_change"
                        | "design_system.publish"
                        | "design_system.set"
                        | "artifact.save"
                        | "artifact.export"
                        | "job.start"
                        | "job.get"
                        | "job.cancel"
                        | "job.resume"
                        | "targets.reserve"
                        | "targets.list"
                        | "web.materialize"
                        | "figure.dataset.edit"
                        | "asset.import"
                        | "asset.lookup"
                        | "source.import_csv"
                        | "source.extract_pdf"
                );
                let external_source_read = matches!(&action, PendingAction::DesignSystemSource { .. });
                let job_scoped = matches!(method, "job.get" | "job.cancel" | "job.resume");
                let artifact_id = if external_source_read {
                    None
                } else if job_scoped {
                    self.job.as_ref().map(|job| job.artifact_id.clone())
                } else if scoped {
                    self.active.as_ref().map(|item| item.id.clone())
                } else {
                    None
                };
                let require_active = scoped && !job_scoped && !external_source_read && !matches!(&action, PendingAction::Save { .. });
                let ui_run = self.run_context.as_ref().filter(|_| ui_run_rpc).cloned().map(Box::new);
                if matches!(action, PendingAction::StartJob | PendingAction::FigureAiJob { .. } | PendingAction::FigureAiImport { .. }) {
                    self.awaited_starts.insert(ticket, Instant::now());
                }
                if let Some(sent) = sent_start {
                    self.job_starts.insert(ticket, sent);
                }
                self.pending.insert(ticket, PendingRequest {
                    action,
                    method: method.to_owned(),
                    artifact_id,
                    require_active,
                    apply: None,
                    ui_run,
                });
                if ui_run_rpc {
                    if let Some(run) = self.run_context.as_mut() {
                        run.rpc_ticket = Some(ticket);
                    }
                }
                Some(ticket)
            }
            Err(error) => {
                if ui_run_rpc {
                    if let Some(run) = self.run_context.as_mut().filter(|run| run.await_rpc_reply) {
                        run.enqueue_error = Some(error.clone());
                    }
                }
                self.set_error(user_error(&error));
                None
            }
        }
    }

    /// Starts a home request with a prompt as one host call (`application::create_from_home`): the
    /// attachments, then the job that creates the artifact, in one commit. The worker thread runs it, so
    /// the window keeps drawing. Returns false when the worker is gone.
    fn call_home(&mut self, request: HomeRequest) -> Option<u64> {
        if self.host_lost() {
            return None;
        }
        if self.ui_test.is_some() && !request.prompt.trim().is_empty() && request.runtime != "mock" {
            self.refuse_ui_test_run(ui_test_input::REAL_RUN_REFUSED);
            return None;
        }
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        self.notices.clear_transient();
        let prompt = request.prompt.clone();
        match self.rpc.create_from_home(ticket, request) {
            Ok(()) => {
                self.pending.insert(ticket, PendingRequest {
                    action: PendingAction::HomeCreate { prompt },
                    method: "create_from_home".to_owned(),
                    artifact_id: None,
                    require_active: false,
                    apply: None,
                    ui_run: self.run_context.clone().map(Box::new),
                });
                if let Some(run) = self.run_context.as_mut() {
                    run.rpc_ticket = Some(ticket);
                }
                Some(ticket)
            }
            Err(error) => {
                if let Some(run) = self.run_context.as_mut() {
                    run.enqueue_error = Some(error.clone());
                }
                self.set_error(user_error(&error));
                None
            }
        }
    }
}

impl eframe::App for DesktopApp {
    /// tokens `layout.window.clear-color`: no cell may show eframe's default clear colour.
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        let palette = if visuals.dark_mode { crate::theme::DARK } else { crate::theme::LIGHT };
        let color = if self.screen_is_editor() { palette.board } else { palette.home };
        egui::Rgba::from(color).to_array()
    }

    fn raw_input_hook(&mut self, ctx: &Context, input: &mut egui::RawInput) {
        // A UI-test run's events join the frame's input before anything reads it, as a person's would (task 69).
        self.inject_ui_test_input(ctx, input);
        let board_selection_click = self.board_rect.is_some_and(|board_rect| input.events.iter().any(|event| matches!(event,
            egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, .. } if board_rect.contains(*pos)
        )));
        if board_selection_click && self.pending_image_replacement.as_ref().is_some_and(|pending| pending.stage == ImageReplacementStage::Importing) {
            self.pending_image_replacement = None;
        }
        if self.close_after_inline_cleanup || self.close_release_failed {
            input.events.retain(|event| !matches!(event,
                egui::Event::Text(_) | egui::Event::Paste(_) | egui::Event::Ime(_) | egui::Event::Cut
                    | egui::Event::Key { pressed: true, .. }
            ));
            return;
        }
        // Keys typed while a table cell's edit hands over to what follows it are held for that (inline_cell_nav.rs).
        self.route_cell_follow_input(input);
        if self.inline_text_edit.is_some() {
            let cell_readiness = self.inline_text_edit.as_ref().filter(|session| session.reservation_node_ids.len() > 1)
                .map(|session| (session.id, self.inline_table_cell_readiness(session)));
            if let Some((session_id, readiness)) = cell_readiness {
                // While the cell waits for the render of its own commit the editor stays on the shown frame and keeps
                // taking input: it is queued and sent once the render is in (`start_next_inline_text_change`).
                if !self.apply_inline_cell_readiness(session_id, readiness) && readiness != InlineCellReadiness::WaitingForRender {
                    let composing = self.inline_text_edit.as_ref().is_some_and(|session| session.composing)
                        || self.text_adapter.is_ime_active();
                    if composing {
                        ctx.memory_mut(|memory| memory.request_focus(self.text_editor_id));
                        self.text_editor_focused = true;
                        input.events.retain(|event| matches!(event,
                            egui::Event::Ime(_) | egui::Event::Text(_)
                                | egui::Event::Key { key: egui::Key::Escape, pressed: true, .. }
                        ));
                        self.process_inline_text_adapter_input(ctx, input);
                        return;
                    }
                    let escape = input.events.iter().any(|event| matches!(event,
                        egui::Event::Key { key: egui::Key::Escape, pressed: true, .. }
                    ));
                    if readiness == InlineCellReadiness::WaitingForRender && escape
                        && !self.text_adapter.is_ime_active()
                        && self.inline_text_edit.as_ref().is_some_and(|session| !session.composing)
                    {
                        self.finish_inline_text_edit();
                    }
                    input.events.retain(|event| !matches!(event,
                        egui::Event::Text(_) | egui::Event::Paste(_) | egui::Event::Ime(_) | egui::Event::Cut
                            | egui::Event::Key { pressed: true, .. }
                    ));
                    return;
                }
            }
            let composing = self.inline_text_edit.as_ref().is_some_and(|session| session.composing);
            if self.inline_text_edit.as_ref().is_some_and(|session| session.conflict.is_some()) && !composing {
                // Escape does nothing while the conflict notice shows (its buttons act): left in the input, the board's `Esc`
                // shortcut would clear the selection under the kept draft.
                input.events.retain(|event| !matches!(event, egui::Event::Key { key: egui::Key::Escape, pressed: true, .. }));
                return;
            }
            let mut blocked = false;
            input.events.retain(|event| {
                let stop = matches!(event,
                    egui::Event::Key { key: egui::Key::Enter | egui::Key::Z | egui::Key::Y | egui::Key::X, modifiers, pressed: true, .. }
                        if modifiers.ctrl || modifiers.mac_cmd
                );
                blocked |= stop;
                !stop
            });
            if blocked {
                self.set_error("텍스트 편집을 끝낸 뒤 이 단축키를 사용하세요".into());
            }
            if let Some(session) = self.inline_text_edit.as_mut() {
                session.ime_cancelled_frame = false;
            }
            if self.text_target.as_ref().is_none_or(|target| !target.editable()) {
                return;
            }
            self.process_inline_text_adapter_input(ctx, input);
            return;
        }
        // A board memo or title editor owns the keyboard: the document text adapter must not take its IME.
        if self.board_memo_editor_shown() || matches!(self.text_target.as_ref(), Some(TextTarget::Node { .. })) {
            return;
        }
        if self.text_target.as_ref().is_none_or(|target| !target.editable()) {
            return;
        }
        let Some(range) = self.text_adapter.raw_input_hook(
            ctx,
            input,
            self.text_editor_id,
            &mut self.text_draft,
        ) else {
            return;
        };
        let Some(TextTarget::Paragraph { paragraph_id, .. }) = self.text_target.as_ref() else {
            return;
        };
        let desired = Selection {
            node_ids: Vec::new(),
            text_range: Some(TextRange {
                paragraph_id: paragraph_id.clone(),
                start: range.start,
                end: range.end,
            }),
        };
        let already_sent = self.context.as_ref().is_some_and(|context| {
            context.selection.text_range.as_ref().is_some_and(|current| {
                current.paragraph_id == paragraph_id.as_str()
                    && current.start == range.start
                    && current.end == range.end
            })
        });
        if !already_sent {
            self.set_selection(desired);
        }
    }

    fn update(&mut self, ctx: &Context, frame: &mut eframe::Frame) {
        if self.exit_ctx.is_none() {
            self.exit_ctx = Some(ctx.clone());
            self.rpc.set_waker(ctx);
        }
        self.parent_hwnd = window_hwnd(frame);
        // A UI-test window times its frames (68f, perf.rs).
        let frame_started = self.ui_test.is_some().then(Instant::now);
        self.process_replies(ctx);
        let replies_took = frame_started.map(|at| at.elapsed());
        self.expire_awaited_starts();
        self.sync_drafts();
        for notice in self.drafts.take_notices() {
            self.notices.warning(notice, Instant::now());
        }
        self.poll_video_poster_workers();
        self.cancel_stale_video_worker();
        self.guard_held_edit_close(ctx);
        self.advance_inline_text_close_cleanup(ctx);
        self.poll_events();
        self.apply_pending_page_reorder_selection();
        self.run_span_after_edit();
        if self.inline_text_edit.is_some() {
            self.screen = Screen::Editor;
            self.renew_inline_text_target_if_needed();
        }
        // The job of the open document and the jobs kept aside are polled on the same interval; a kept-aside job that
        // finishes leaves the warnings when its answer comes back (`place_jobs`).
        if !self.host_lost() && self.last_job_poll.elapsed() >= Duration::from_secs(2) {
            if self.job.as_ref().is_some_and(figure_close::job_running) {
                self.request_job_status();
            }
            if self.parked_jobs.iter().any(figure_close::job_running) {
                self.request_parked_job_status();
            }
            self.last_job_poll = Instant::now();
        }
        self.poll_web_preview_child();

        if !self.host_lost() {
            self.handle_board_memo_shortcut(ctx);
        }
        if !self.has_home_confirmation() {
            self.handle_shortcuts(ctx);
        }
        match self.screen {
            Screen::Home => self.draw_home(ctx),
            Screen::Editor => {
                self.draw_frame(ctx);
                self.draw_home_confirmation(ctx);
            }
        }
        // An icon waiting for its pick is dropped once its toolbox tab was not drawn (68g).
        self.forget_hidden_toolbox_icon(ctx);
        // A video dropped on another page opens its dialog once the page change is answered (68p).
        self.run_waiting_toolbox_video(ctx);
        self.observe_board_memo_gesture(ctx);
        self.finish_board_memo_gesture_if_released(ctx);
        self.draw_board_memo_editor(ctx);
        self.draw_board_memo_mutation(ctx);
        self.draw_page_section_editor(ctx);
        self.draw_page_section_mutation(ctx);
        self.draw_export_loss_dialog(ctx);
        self.draw_figure_form(ctx);
        self.draw_held_edit_close_dialog(ctx);
        self.draw_figure_switch_dialog(ctx);
        self.draw_draft_recovery(ctx);
        self.draw_error_callout(ctx);
        self.draw_host_link(ctx);
        if !self.host_lost() {
            self.publish_ui_if_due();
        }
        self.ui_screenshot_events(ctx);
        if let (Some(test), Some(started), Some(replies)) = (self.ui_test.as_mut(), frame_started, replies_took) {
            test.perf.frame(started, replies, started.elapsed());
        }
        self.ui_test_frame_end(ctx);
        ctx.request_repaint_after(Duration::from_secs(1));
        self.smoke_step(ctx);

        if !self.pending.is_empty() || self.web_preview_run.is_some() || self.smoke_output.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if self.has_video_poster_worker() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        // Repaint when the visible message times out, so it does not stay until the next input.
        if let Some(wait) = self.notices.expires_in(Instant::now()) {
            ctx.request_repaint_after(wait);
        }
    }

    /// The runner model drafts are saved at exit, whether or not their menu was drawn last (`save_runner_drafts`).
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(ctx) = self.exit_ctx.take() {
            self.save_runner_drafts(&ctx, true);
        }
    }
}

impl Drop for DesktopApp {
    fn drop(&mut self) {
        self.drafts.flush();
        let _ = self.stop_web_preview_child();
    }
}

fn window_hwnd(frame: &eframe::Frame) -> Option<isize> {
    #[cfg(windows)]
    {
        let handle = frame.window_handle().ok()?;
        match handle.as_raw() {
            RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
            _ => None,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = frame;
        None
    }
}

fn save_screenshot(path: &Path, screenshot: &egui::ColorImage) -> DltResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut rgba = Vec::with_capacity(screenshot.pixels.len() * 4);
    for pixel in &screenshot.pixels {
        rgba.extend_from_slice(&pixel.to_srgba_unmultiplied());
    }
    image::save_buffer_with_format(
        path,
        &rgba,
        screenshot.size[0] as u32,
        screenshot.size[1] as u32,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .map_err(|error| DltError::new(ErrorCode::Io, error.to_string()))
}

fn truncate(value: &str, max_graphemes: usize) -> String {
    let mut parts = value.graphemes(true);
    let prefix = parts.by_ref().take(max_graphemes).collect::<String>();
    if parts.next().is_some() { format!("{prefix}…") } else { prefix }
}

fn to_micropx(value: f32) -> i64 {
    let scaled = (f64::from(value) * 1_000_000.0).round();
    scaled.clamp(i64::MIN as f64, i64::MAX as f64) as i64
}

/// A point over the page inside the page's screen rectangle, in page EMU; None outside it.
fn page_point_to_emu(point: Pos2, page_rect: Rect, page_size: Size) -> Option<(i64, i64)> {
    if !page_rect.contains(point) {
        return None;
    }
    screen_to_page_emu(point, page_rect, page_size)
}

/// A screen point in page EMU, for a page drawn in `page_rect`, rounded and saturated to i64 (a point off the page gives
/// EMU off the page). None for an empty rectangle or an invalid page size.
fn screen_to_page_emu(point: Pos2, page_rect: Rect, page_size: Size) -> Option<(i64, i64)> {
    if page_rect.width() <= 0.0 || page_rect.height() <= 0.0 || !page_size.valid() {
        return None;
    }
    let x = f64::from(point.x - page_rect.left()) / f64::from(page_rect.width()) * page_size.width as f64;
    let y = f64::from(point.y - page_rect.top()) / f64::from(page_rect.height()) * page_size.height as f64;
    // `as` saturates at the i64 bounds.
    Some((x.round() as i64, y.round() as i64))
}

/// A page box in screen points, for a page drawn in `view`. Nothing for an invalid page or an empty view.
fn page_to_screen(bounds: designllm_contracts::Rect, page: Size, view: Rect) -> Rect {
    if !page.valid() || view.width() <= 0.0 || view.height() <= 0.0 {
        return Rect::NOTHING;
    }
    let sx = view.width() / page.width as f32;
    let sy = view.height() / page.height as f32;
    Rect::from_min_max(
        Pos2::new(view.left() + bounds.x as f32 * sx, view.top() + bounds.y as f32 * sy),
        Pos2::new(
            view.left() + bounds.x.saturating_add(bounds.width) as f32 * sx,
            view.top() + bounds.y.saturating_add(bounds.height) as f32 * sy,
        ),
    )
}

/// The node's own frame in page EMU, or the given box when the node is unknown. A text-bearing shape
/// reports its text lines as source regions with their own boxes, so a drag starts from the frame.
fn frame_bounds_in(node: Option<&Node>, region_bounds: designllm_contracts::Rect) -> designllm_contracts::Rect {
    node.map_or(region_bounds, |node| node.bounds)
}
