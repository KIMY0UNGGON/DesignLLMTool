use super::*;

#[derive(Debug, Deserialize)]
pub(super) struct WorkspaceListing {
    pub(super) workspace_id: String,
    pub(super) artifacts: Vec<ArtifactSummary>,
    #[serde(default)]
    pub(super) workspace_notices: Option<Vec<Diagnostic>>,
}

pub(super) struct PendingRequest {
    pub(super) action: PendingAction,
    pub(super) method: String,
    pub(super) artifact_id: Option<String>,
    pub(super) require_active: bool,
    pub(super) apply: Option<ApplyContext>,
    pub(super) ui_run: Option<Box<ui_channel::RunContext>>,
}

#[derive(Clone)]
pub(super) struct ApplyContext {
    pub(super) request_id: String,
    pub(super) workspace_id: String,
    pub(super) artifact_id: String,
    pub(super) base_revision: u64,
    pub(super) operations: Vec<Operation>,
    pub(super) allowed_intents: BTreeSet<Intent>,
    pub(super) actor: String,
    pub(super) user_edit: bool,
    pub(super) retryable: bool,
    pub(super) held_edit_id: Option<String>,
    pub(super) after_action: PendingAction,
}

pub(super) struct SelectionRestoreGuard {
    pub(super) ticket: u64,
    pub(super) expected_selection: Selection,
    pub(super) expected_page: u32,
    pub(super) expected_generation: u64,
    pub(super) committed_revision: u64,
}

pub(super) fn ui_run_success_result(run: &ui_channel::RunContext, payload: &RpcPayload) -> Value {
    if matches!(run.command_id.as_str(), "ai.cancel" | "file.export" | "file.export_selection" | "nav.open_file" | "home.create" | "home.use_template") {
        if let RpcPayload::Json(result) = payload {
            return result.clone();
        }
    }
    if matches!(
        run.command_id.as_str(),
        "add.figure_image" | "canvas.image.replace" | "add.figure_link" | "figure.update_link"
            | "data.edit_cell" | "data.add_row" | "data.add_column" | "data.remove_row" | "data.remove_column"
            | "canvas.token.put" | "canvas.token.remove" | "canvas.token.mode" | "canvas.token.bind" | "canvas.token.unbind"
            | "canvas.design_system.list" | "canvas.design_system.publish" | "canvas.design_system.set"
            | "canvas.table.add" | "canvas.table.add_row" | "canvas.table.remove_row" | "canvas.table.add_column" | "canvas.table.remove_column" | "canvas.table.set_header" | "canvas.table.set_column_widths" | "table.merge_cells" | "table.unmerge_cell"
    ) {
        if let RpcPayload::Json(result) = payload {
            let mut reply = json!({ "command": run.command_id, "accepted": true, "core_result": result });
            if let Some(hash) = &run.prepared_asset {
                reply["prepared_asset_sha256"] = json!(hash);
            }
            return reply;
        }
    }
    json!({ "command": run.command_id, "accepted": true })
}

#[derive(Clone, Debug)]
pub(crate) struct FlowFindMatch {
    pub paragraph_id: String,
    pub text: String,
    pub range: TextRange,
    pub page_index: Option<u32>,
}

#[derive(Clone, Debug)]
pub(crate) struct FlowFindSearch {
    pub artifact_id: String,
    pub revision: u64,
    pub query: String,
    pub generation: u64,
    pub matches: Vec<FlowFindMatch>,
}

#[derive(Clone, Debug)]
pub(crate) struct FlowFindLookup {
    pub generation: u64,
    pub match_index: usize,
    pub range: TextRange,
    pub next_page: u32,
    pub total_pages: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct FlowFindPageRequest {
    pub generation: u64,
    pub match_index: usize,
    pub range: TextRange,
    pub page_index: u32,
    pub artifact_id: String,
    pub revision: u64,
}

impl PartialEq for FlowFindPageRequest {
    fn eq(&self, other: &Self) -> bool {
        self.generation == other.generation
            && self.match_index == other.match_index
            && self.range.paragraph_id == other.range.paragraph_id
            && self.range.start == other.range.start
            && self.range.end == other.range.end
            && self.page_index == other.page_index
            && self.artifact_id == other.artifact_id
            && self.revision == other.revision
    }
}

impl Eq for FlowFindPageRequest {}

#[derive(Clone, Debug)]
pub(crate) struct FlowFindCursor {
    pub generation: u64,
    pub match_index: usize,
    pub range: TextRange,
}

/// The figure a PDF evidence import was started for, as the window showed it then (`figure_tools`). The import's replies
/// and the commit of its records go to that figure only while the window still shows it at that revision. `asset_sha256`
/// is the hash of the PDF file the page is read from (set when the file is imported); the records must carry it. `awaiting`
/// is the ticket of the request the import waits for (`asset.import`, then `source.extract_pdf`); a reply with another
/// ticket answers an import that was replaced, and changes nothing.
#[derive(Clone, Debug)]
pub(super) struct PdfEvidenceTarget {
    pub(super) artifact_id: String,
    pub(super) revision: u64,
    pub(super) page_index: u32,
    pub(super) file_name: String,
    pub(super) asset_sha256: Option<String>,
    pub(super) awaiting: Option<u64>,
}

/// PDF evidence a page read produced that is not in its figure yet. The set keeps the figure, revision and page the read
/// was started for, so it goes into that figure only, and only while the figure is shown at that revision. `ticket` is set
/// while the commit of the set is on its way; `message` says why the set waits or what the last attempt met.
#[derive(Clone, Debug)]
pub(super) struct HeldPdfEvidence {
    pub(super) id: String,
    pub(super) target: PdfEvidenceTarget,
    pub(super) records: Vec<designllm_contracts::Evidence>,
    pub(super) message: String,
    pub(super) ticket: Option<u64>,
}

#[derive(Clone)]
pub(super) enum PendingAction {
    Hello,
    WorkspaceList,
    Create,
    Open,
    Context,
    Inspect,
    DesignSystemList,
    DesignSystemSource { consumer_id: String, source_id: String, source_revision: u64, generation: u64 },
    DesignSystemChange { refresh_systems: bool },
    Render { page_index: u32 },
    FlowFindPage { request: FlowFindPageRequest },
    PreviewAsset { snapshot: RenderSnapshot, page_index: u32 },
    Transform,
    /// `changes.apply` of `add.path`, with the document and the form's text: a refusal puts the text back in that document's
    /// form (tools.rs).
    PathAdd { artifact_id: String, d: String, view_box: String },
    Selection,
    Apply,
    PageReorder { token: String },
    TokenChange { baseline: Option<properties::TokenChangeBaseline> },
    ApplyWithSelectionIfUnchanged { node_ids: Vec<String>, expected_selection: Selection, expected_page: u32, expected_selection_generation: u64 },
    DatasetEdit,
    FigureDataset,
    FigureEvidence,
    CsvAssetImport,
    CsvDatasetImport,
    CsvDeclaredImport,
    PdfAssetImport { page_index: u32 },
    /// A picture imported for a new image panel of the open figure, with where it comes from.
    FigureImageAsset { origin: String, source_ref: Option<String> },
    /// `artifact.validate` for the design warnings of the open canvas or slides.
    LintWarnings { artifact_id: String },
    PdfEvidenceImport,
    History,
    Save { request_id: String, artifact_id: String },
    Export,
    UiExport,
    ExportDryRun,
    /// `create_from_home`, with its request (empty: only create, no job), for the label of the job it starts.
    HomeCreate { prompt: String },
    Reservations,
    HeldEditReservations { held_id: String },
    StartJob,
    /// `asset.import` of one evidence file of a Figure AI request (figure_ai_attach.rs).
    FigureAiImport { token: String },
    /// `job.start` of a Figure AI request once all its evidence files are imported.
    FigureAiJob { token: String },
    /// `job.get` of the shown job. A job the host does not know (not found) is dropped (`forget_job`), so its polls stop.
    GetJob { job_id: String },
    /// `job.get` of a figure job kept aside (`figure_close.rs`): its answer updates that job. A failed poll is asked again by
    /// the next poll rather than reported, but a job the host does not know (not found) is dropped (`forget_job`).
    GetParkedJob { job_id: String },
    CancelJob,
    StopForEdit,
    ReserveDrag { token: u64 },
    ReleaseDrag,
    PollEvents,
    HitTest { snapshot_id: String, page_index: u32, transform_id: String, additive: bool, edit_text: bool },
    InlineTextChange { session_id: u64 },
    InlineTextReserve { session_id: u64 },
    InlineTextRelease { session_id: u64, reservation_id: String },
    InlineTextCloseRelease { session_id: u64 },
    InlineTextCloseReservationCheck { session_id: u64, view_id: String, actor: String, node_id: String, artifact_id: String },
    MaterializeWeb,
    /// A figure's current revision and size, read to put it into (or refresh it in) a document.
    FigureInfo { request: links::LinkRequest },
    /// The figure at that revision rendered as a PNG; the document change follows.
    FigureRender { request: links::LinkRequest, figure: links::FigureFacts },
    /// The document change that inserts or refreshes a figure picture.
    LinkApply,
    BoardMemosChange { token: String },
    PageSectionsChange { token: String },
    /// The window's state publish (docs/UI_CHANNEL.md).
    UiPublish,
    UiComplete,
    /// The check that the host still answers over the pipe (`host_link.rs` `note_host_reply`).
    HostProbe,
    /// The asset import of a screenshot run; the run completes with it.
    UiAsset { run_id: String, width: u32, height: u32, path: PathBuf },
    /// The asset import of a picture the user chose; the picture becomes an image node.
    ImageAsset { page_id: String },
    VideoSourceImport { token: String },
    VideoPosterImport { token: String, temp_path: PathBuf },
    VideoInsertApply { token: String },
    /// A home thumbnail (render or asset read) for a document that need not be open.
    Thumb(thumbs::ThumbTicket),
    ImageReplacementImport { token: String },
    ImageReplacementApply { token: String },
    ImageCropLookup { token: String },
    ImageCropApply { token: String },
    /// The part of a page on screen, rendered at the resolution it is shown at (`RenderRequest.region`).
    RenderRegion { page_index: u32 },
    /// The PNG of a region render; it becomes the page's region texture.
    RegionAsset { snapshot: RenderSnapshot, page_index: u32 },
    /// `view.play`: what the HTML export of the canvas would lose, then the export itself into a scratch file.
    PlayDryRun { request: ExportFileRequest },
    PlayExport { path: PathBuf },
    /// A dry-run export for the popover's loss list, for the settings named by `key`.
    ExportPreview { key: String },
    /// `flow.styles` of a text document at one revision (the properties panel's style list).
    FlowStyles { artifact_id: String, revision: u64 },
}

#[derive(Clone)]
pub(super) struct PendingPageReorderSelection {
    pub(super) token: String,
    pub(super) artifact_id: String,
    pub(super) base_revision: u64,
    pub(super) base_page_ids: Vec<String>,
    pub(super) expected_page_ids: Vec<String>,
    pub(super) selected_page_id: Option<String>,
    pub(super) last_page_index: u32,
    pub(super) committed_revision: Option<u64>,
}

impl PendingAction {
    pub(super) fn prepares_figure_change(&self) -> bool {
        matches!(self, Self::FigureImageAsset { .. } | Self::FigureInfo { .. } | Self::FigureRender { .. } | Self::CsvAssetImport | Self::CsvDatasetImport)
    }

    /// Started by something the user did, as opposed to the window keeping itself in sync with
    /// the host. Only the first kind may raise an error message right away.
    pub(super) fn user_initiated(&self) -> bool {
        !matches!(
            self,
            Self::Hello
                | Self::WorkspaceList
                | Self::Context
                | Self::Inspect
                | Self::DesignSystemList
                | Self::DesignSystemSource { .. }
                | Self::LintWarnings { .. }
                | Self::Render { .. }
                | Self::FlowFindPage { .. }
                | Self::PreviewAsset { .. }
                | Self::Reservations
                | Self::Transform { .. }
                | Self::Selection
                | Self::GetJob { .. }
                | Self::GetParkedJob { .. }
                | Self::ReleaseDrag
                | Self::PollEvents
                | Self::HitTest { .. }
                | Self::UiPublish
                | Self::UiComplete
                | Self::UiAsset { .. }
                | Self::ExportPreview { .. }
                | Self::RenderRegion { .. }
                | Self::RegionAsset { .. }
                | Self::FlowStyles { .. }
        )
    }

}

#[derive(Clone, PartialEq, Eq)]
pub(super) enum TextTarget {
    Node { node_id: String, original: String, editable: bool },
    Paragraph { paragraph_id: String, original: String, editable: bool },
    WebFile { path: String, expected_hash: String, original: String, editable: bool },
    PanelLabel { panel_id: String, original: String, editable: bool },
}

#[derive(Clone)]
pub(super) struct InlineTextEdit {
    pub(super) id: u64,
    pub(super) view_id: String,
    pub(super) actor: String,
    pub(super) artifact_id: String,
    pub(super) node_id: String,
    pub(super) page_index: u32,
    pub(super) kind: ArtifactKind,
    pub(super) revision: u64,
    pub(super) committed_text: String,
    pub(super) queue: VecDeque<TextInputEdit>,
    pub(super) in_flight: Option<TextInputEdit>,
    pub(super) composing: bool,
    pub(super) ime_commit_pending: bool,
    pub(super) ime_cancelled_frame: bool,
    pub(super) ime_generation: u64,
    pub(super) reservation_id: Option<String>,
    pub(super) reservation_node_ids: Vec<String>,
    pub(super) reservation_pending: bool,
    pub(super) reservation_release_pending: bool,
    pub(super) reservation_release_failed: bool,
    pub(super) release_when_reserved: bool,
    pub(super) finish_requested: bool,
    pub(super) layout_pending: bool,
    pub(super) conflict: Option<String>,
    pub(super) retry_allowed: bool,
    pub(super) retry_expected: bool,
    pub(super) retry_same_request: bool,
    pub(super) reservation_expires: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InlineCellReadiness {
    Ready,
    WaitingForRender,
    RevisionDrift,
    FrameUnavailable,
    TargetChanged,
    SelectionChanged,
}

pub(super) fn inline_text_actor(view_id: &str) -> String {
    format!("desktop:inline:{view_id}")
}

pub(super) fn is_inline_text_actor(actor: &str) -> bool {
    let Some(view_id) = actor.strip_prefix("desktop:inline:") else { return false };
    let bytes = view_id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) { *byte == b'-' } else { byte.is_ascii_hexdigit() }
        })
}

pub(crate) struct RegionClick {
    pub position: Pos2,
    pub double_clicked: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FigureForm {
    Chart,
    Evidence,
}

/// The two screens of the window: the home screen, and the editor of one document (§2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Screen {
    Home,
    Editor,
}

/// The most recent agent change batches kept for taking one back.
pub(super) const AGENT_BATCH_LIMIT: usize = 20;

/// One change batch an agent made on the open document. Its request id names it for `ai.undo_change`.
#[derive(Clone)]
pub(super) struct AgentBatch {
    pub(super) request_id: String,
    pub(super) actor: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HeldEditState {
    WaitingForJobRelease,
    NeedsReview,
    Checking,
    Ready,
    Sending,
}

/// A refused direct user edit retained for explicit retry/cancel; this in-memory queue is not crash recovery.
#[derive(Clone)]
pub(super) struct HeldEdit {
    pub(super) id: String,
    pub(super) apply: ApplyContext,
    pub(super) nodes: Vec<String>,
    pub(super) state: HeldEditState,
    pub(super) message: String,
    pub(super) can_retry: bool,
    pub(super) retry_same_request: bool,
    pub(super) reservations_checked: bool,
    pub(super) checked_revision: Option<u64>,
}

/// A job's reservation of nodes on the open document: the nodes an AI job works on while it runs.
#[derive(Clone)]
pub(super) struct JobReservation {
    pub(super) reservation_id: String,
    pub(super) job_id: String,
    pub(super) owner: String,
    pub(super) node_ids: Vec<String>,
}

#[derive(Clone)]
pub(super) struct UserReservation {
    pub(super) node_ids: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SaveRpcOutcome {
    AwaitingReply,
    Succeeded,
    Unknown,
}

pub(super) struct PendingSaveNotice {
    pub(super) artifact_id: String,
    pub(super) outcome: SaveRpcOutcome,
    pub(super) diagnostics: Option<Vec<Diagnostic>>,
    pub(super) started: Instant,
}

#[derive(Clone)]
pub(super) struct ImageReplacementIntent {
    pub(super) token: String,
    pub(super) artifact_id: String,
    pub(super) expected_revision: u64,
    pub(super) page_index: u32,
    pub(super) page_id: String,
    pub(super) node_id: String,
    pub(super) expected_asset_sha256: String,
    pub(super) selection_generation: u64,
    pub(super) view_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ImageReplacementStage {
    Importing,
    Applying,
}

pub(super) struct PendingImageReplacement {
    pub(super) intent: ImageReplacementIntent,
    pub(super) stage: ImageReplacementStage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ImageCropStage {
    Loading,
    Editing,
    Applying,
}

#[derive(Clone)]
pub(super) struct ImageCropEditor {
    pub(super) intent: ImageReplacementIntent,
    pub(super) stage: ImageCropStage,
    pub(super) original: Option<designllm_contracts::PixelRect>,
    pub(super) dimensions: Option<(u32, u32)>,
    pub(super) draft: Option<[String; 4]>,
    pub(super) requested_crop: Option<Option<designllm_contracts::PixelRect>>,
    pub(super) outcome_uncertain: bool,
    pub(super) refresh_requested: bool,
    pub(super) refresh_failed: bool,
    pub(super) message: Option<String>,
}

impl ImageCropEditor {
    pub(super) fn has_close_work(&self) -> bool {
        self.stage == ImageCropStage::Applying
            || self.outcome_uncertain
            || self.requested_crop.is_some()
            || self.draft_is_dirty()
    }

    pub(super) fn draft_is_dirty(&self) -> bool {
        let (Some((width, height)), Some(draft)) = (self.dimensions, self.draft.as_ref()) else { return false };
        let initial = self.original.unwrap_or(designllm_contracts::PixelRect { x: 0, y: 0, width, height });
        draft.as_slice() != image_crop_fields(initial).as_slice()
    }

}

#[derive(Clone)]
pub(super) struct VideoInsertIntent {
    pub(super) token: String,
    pub(super) artifact_id: String,
    pub(super) expected_revision: u64,
    pub(super) page_index: u32,
    pub(super) page_id: String,
    pub(super) view_id: String,
    pub(super) expected_selection: Selection,
    pub(super) selection_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VideoInsertStage {
    InspectingSource,
    ExtractingPoster,
    ImportingSource,
    ImportingPoster,
    Applying,
    OutcomeUncertain,
}

pub(super) enum VideoPosterWorkerResult {
    Inspected { token: String, result: DltResult<designllm_web_preview::VideoSourceInfo> },
    Extracted { token: String, result: DltResult<designllm_web_preview::VideoPoster> },
}

pub(super) struct PendingVideoInsert {
    pub(super) intent: VideoInsertIntent,
    pub(super) source_path: PathBuf,
    pub(super) stage: VideoInsertStage,
    pub(super) source_info: Option<designllm_web_preview::VideoSourceInfo>,
    pub(super) source_asset: Option<AssetRef>,
    pub(super) poster: Option<designllm_web_preview::VideoPoster>,
    pub(super) poster_asset: Option<AssetRef>,
    pub(super) poster_temp_path: Option<PathBuf>,
    pub(super) worker_cancel: Option<designllm_web_preview::VideoPosterCancellation>,
    pub(super) cancel_requested: bool,
    pub(super) node_id: String,
    pub(super) apply: Option<ApplyContext>,
    pub(super) resolution_retry_sent: bool,
    pub(super) message: Option<String>,
}

impl PendingVideoInsert {
    pub(super) fn close_work(&self) -> bool { true }

    pub(super) fn close_wait_required(&self) -> bool {
        matches!(
            self.stage,
            VideoInsertStage::ImportingSource
                | VideoInsertStage::ImportingPoster
                | VideoInsertStage::Applying
        ) || matches!(self.stage, VideoInsertStage::InspectingSource | VideoInsertStage::ExtractingPoster)
            && self.worker_cancel.is_some()
    }

    pub(super) fn status_text(&self) -> &'static str {
        match self.stage {
            VideoInsertStage::InspectingSource => "동영상 파일을 확인하고 있습니다",
            VideoInsertStage::ExtractingPoster => "첫 장면 이미지를 만들고 있습니다",
            VideoInsertStage::ImportingSource => "동영상 원본을 가져오고 있습니다",
            VideoInsertStage::ImportingPoster => "첫 장면 이미지를 작업물에 준비하고 있습니다",
            VideoInsertStage::Applying => "동영상 노드를 추가하고 있습니다",
            VideoInsertStage::OutcomeUncertain => "동영상 추가 결과를 확인하고 있습니다",
        }
    }
}

pub(super) fn image_crop_fields(rect: designllm_contracts::PixelRect) -> [String; 4] {
    [rect.x.to_string(), rect.y.to_string(), rect.width.to_string(), rect.height.to_string()]
}

pub(super) fn pixel_rect_within(rect: designllm_contracts::PixelRect, dimensions: (u32, u32)) -> bool {
    rect.width > 0 && rect.height > 0
        && rect.x.checked_add(rect.width).is_some_and(|right| right <= dimensions.0)
        && rect.y.checked_add(rect.height).is_some_and(|bottom| bottom <= dimensions.1)
}

#[derive(Clone)]
pub(super) struct BoardMemoEditor {
    pub(super) artifact_id: String,
    pub(super) revision: u64,
    pub(super) page_id: String,
    pub(super) page_index: u32,
    pub(super) expected: Vec<CanvasBoardMemo>,
    pub(super) memo: CanvasBoardMemo,
    pub(super) is_new: bool,
    pub(super) message: Option<String>,
}

pub(super) struct BoardMemoMutation {
    pub(super) token: String,
    pub(super) artifact_id: String,
    pub(super) revision: u64,
    pub(super) page_id: String,
    pub(super) page_index: u32,
    pub(super) expected: Vec<CanvasBoardMemo>,
    pub(super) desired: Vec<CanvasBoardMemo>,
    pub(super) selected_memo_id: Option<String>,
    pub(super) editor: Option<BoardMemoEditor>,
    pub(super) message: Option<String>,
    pub(super) pending: bool,
    pub(super) retry_allowed: bool,
    pub(super) outcome_uncertain: bool,
    pub(super) held_edit_id: Option<String>,
}

pub(super) struct BoardMemoGesture {
    pub(super) artifact_id: String,
    pub(super) revision: u64,
    pub(super) page_id: String,
    pub(super) page_index: u32,
    pub(super) start: Pos2,
    pub(super) page_rect: Rect,
    pub(super) page_size: Size,
    pub(super) tool_at_start: board::Tool,
    pub(super) space_down_at_start: bool,
    pub(super) zoom_at_start: f32,
    pub(super) pan_at_start: Vec2,
    pub(super) transform_id_at_start: Option<String>,
    pub(super) invalidated: bool,
    pub(super) memo_id: Option<String>,
    pub(super) original_bounds: Option<designllm_contracts::Rect>,
    pub(super) expected: Vec<CanvasBoardMemo>,
    pub(super) memo: CanvasBoardMemo,
}

/// The job reservations in a `targets.list` answer; user reservations are left out.
pub(super) fn parse_reservations(value: &Value) -> Vec<JobReservation> {
    let text = |item: &Value, key: &str| -> String { item.get(key).and_then(Value::as_str).unwrap_or_default().to_owned() };
    value
        .get("reservations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let job_id = item.get("job_id").and_then(Value::as_str)?.to_owned();
            let node_ids: Vec<String> = item
                .get("node_ids")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            Some(JobReservation { reservation_id: text(item, "reservation_id"), job_id, owner: text(item, "owner"), node_ids })
        })
        .collect()
}

pub(super) fn parse_user_reservations(value: &Value) -> Vec<UserReservation> {
    value
        .get("reservations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("job_id").and_then(Value::as_str).is_none())
        .map(|item| UserReservation {
            node_ids: item.get("node_ids").and_then(Value::as_array).into_iter().flatten()
                .filter_map(Value::as_str).map(str::to_owned).collect(),
        })
        .collect()
}

#[derive(Clone)]
pub(super) struct ExportConfirmation {
    pub(super) request: ExportFileRequest,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) allowed: BTreeSet<String>,
}

/// A table read from a CSV/TSV file, with the chart the user is describing for it. The file is
/// read twice: first with inferred column types, then with the types and units the user declares.
#[derive(Clone)]
pub(super) struct CsvImportDraft {
    pub(super) dataset: Dataset,
    /// One entry per header column, in file order: what the user declares for it.
    pub(super) columns: Vec<ColumnDeclaration>,
    pub(super) step: CsvStep,
    pub(super) x_column: String,
    pub(super) y_column: String,
    pub(super) plot_kind: String,
    pub(super) x_unit: String,
    pub(super) y_unit: String,
}

/// The type and unit being declared for one header column of an imported table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ColumnDeclaration {
    pub(super) name: String,
    pub(super) data_type: String,
    pub(super) unit: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CsvStep {
    /// The table was read with inferred types; the user states the real type and unit per column.
    Columns,
    /// The columns are declared; the user describes the chart.
    Chart,
}

#[derive(Clone)]
pub(super) struct DatasetCellEdit {
    pub(super) dataset_id: String,
    pub(super) version_hash: String,
    pub(super) row_id: String,
    pub(super) column_id: String,
    pub(super) original: String,
    pub(super) draft: String,
}

pub(super) struct DragState {
    pub(super) token: u64,
    pub(super) node_id: String,
    pub(super) bounds: designllm_contracts::Rect,
    pub(super) page_size: Size,
    pub(super) source_page_index: u32,
    pub(super) source_page_id: Option<String>,
    pub(super) source_page_rect: Rect,
    pub(super) source_page_frame: Option<designllm_contracts::Rect>,
    pub(super) start: Pos2,
    pub(super) delta: Vec2,
    pub(super) base_revision: u64,
    pub(super) reservation_id: Option<String>,
    pub(super) released: bool,
    pub(super) is_panel: bool,
    /// The other selected objects of the page that move with `node_id` (id, stored bounds when the drag started).
    pub(super) others: Vec<(String, designllm_contracts::Rect)>,
    /// The display scale (physical pixels per point) of the window when the drag started. The drop snaps within 8 physical
    /// pixels at that scale (`snap_free_move`).
    pub(super) pixels_per_point: f32,
    /// The board's zoom when the drag started. A drop on the same page measures its move on the page as drawn then, so its snap
    /// window is sized at this zoom too.
    pub(super) zoom: f32,
}

pub(super) struct CoveredTableTextSelection {
    pub(super) artifact_id: String,
    pub(super) covered_cell_id: String,
    pub(super) anchor_cell_id: String,
}

#[derive(Clone)]
pub(super) struct NodeClipboard {
    pub(super) artifact_id: String,
    pub(super) artifact_origin: String,
    pub(super) page_id: String,
    pub(super) kind: ArtifactKind,
    pub(super) nodes: Vec<Node>,
    pub(super) figure_links: Vec<designllm_contracts::FigureLink>,
}

/// The name a document made from a canvas template takes (the home template tab's names, §12).
pub(super) fn home_template_title(template: &str) -> &'static str {
    match template {
        "mobile_app" => "모바일 앱 화면",
        "login" => "로그인",
        "dashboard" => "대시보드",
        "resume" => "이력서",
        "landing_page" => "랜딩 페이지",
        "social_post" => "소셜 게시물",
        "poster_a3" => "포스터",
        "invitation" => "초대장",
        "banner" => "배너",
        "webgl_animation" => "WebGL 애니메이션",
        _ => "모바일 앱 화면",
    }
}

impl TextTarget {
    pub(super) fn editable(&self) -> bool {
        match self {
            Self::Node { editable, .. }
            | Self::Paragraph { editable, .. }
            | Self::WebFile { editable, .. }
            | Self::PanelLabel { editable, .. } => *editable,
        }
    }
}
