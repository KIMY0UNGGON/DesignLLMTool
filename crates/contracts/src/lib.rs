//! Shared, versioned data and engine boundary. No host or format implementation lives here.
//!
//! v1.2 (docs/P0_CONTRACT_V1_2.md) only adds: every new field is `Option`/empty and skipped when
//! unset, so v1.1 JSON, receipt hashes, digests and snapshot bytes are unchanged.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use unicode_segmentation::UnicodeSegmentation;

mod svg_path;
pub use svg_path::{PATH_MAX_COMMANDS, PATH_MAX_COORDINATE};
mod pdf_diag;
pub use pdf_diag::{PDF_INTERNAL_PANIC, PDF_MEMORY_LIMIT, PDF_REQUEST_BUDGET, PDF_STACK_LIMIT, PDF_TOO_LARGE};
pub use pdf_diag::{PDF_EDIT_APPROXIMATE, PDF_EDIT_FAILED, PDF_EDIT_NO_TEXT, PDF_EDIT_WORD_REQUIRED};
pub mod pdf_coverage;
mod engine_rules;
pub use engine_rules::{CONTRAST_MIN, MAX_BREAKPOINTS, MAX_GRID_COLUMNS, TOUCH_MIN_PX};
pub mod layout_guides;
pub mod figure_rules;
pub mod figure_math;
pub mod design_policy;
pub mod design_themes;
pub mod design_quality;
pub mod shape_library;
pub mod base64;
pub mod native_envelope;
pub mod page_losses;
pub mod media_header;
pub mod interaction_url;
pub mod local_path;
pub mod repeat_groups;
pub mod diag_codes;
mod ui;
pub use ui::*;
mod constructors;
mod operation_name;

pub const SCHEMA_VERSION: u32 = 1;
pub const PROTOCOL_VERSION: u32 = 1;
pub const PROJECT_STORE_DIR: &str = ".designllm";
pub const PROJECT_ARTIFACTS_DIR: &str = "artifacts";
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const EMU_PER_INCH: i64 = 914_400;
pub const EMU_PER_MM: i64 = 36_000;
pub const EMU_PER_PT: i64 = 12_700;
pub const EMU_PER_TWIP: i64 = 635;
pub const EMU_PER_HWPUNIT: i64 = 127;
/// One CSS pixel (1/96 in), the unit of screen frame presets.
pub const EMU_PER_CSS_PX: i64 = 9_525;
pub type Id = String;
pub type Revision = u64;
pub type DltResult<T> = Result<T, DltError>;

pub fn new_id() -> Id { uuid::Uuid::new_v4().to_string() }
pub fn sha256(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
/// Plain text only. Format adapters must additionally map protected inline atoms.
pub fn grapheme_to_byte(text: &str, index: u32) -> DltResult<usize> {
    let positions: Vec<usize> = text.grapheme_indices(true).map(|(offset, _)| offset).chain(std::iter::once(text.len())).collect();
    positions.get(index as usize).copied().ok_or_else(|| DltError::invalid("Grapheme offset is out of range"))
}
pub fn grapheme_to_scalar(text: &str, index: u32) -> DltResult<usize> { Ok(text[..grapheme_to_byte(text, index)?].chars().count()) }
pub fn json_hash<T: Serialize>(value: &T) -> DltResult<String> {
    let value = serde_json::to_value(value).map_err(|e| DltError::invalid(e.to_string()))?;
    Ok(sha256(canonical_json(&value)?.as_bytes()))
}
/// v1 canonical JSON: object keys sorted by UTF-8 bytes; array order preserved; no whitespace.
pub fn canonical_json(value: &Value) -> DltResult<String> {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort_unstable();
            let entries = keys.into_iter().map(|key| Ok(format!("{}:{}", serde_json::to_string(key).map_err(|e| DltError::invalid(e.to_string()))?, canonical_json(&map[key])?))).collect::<DltResult<Vec<_>>>()?;
            Ok(format!("{{{}}}", entries.join(",")))
        }
        Value::Array(items) => Ok(format!("[{}]", items.iter().map(canonical_json).collect::<DltResult<Vec<_>>>()?.join(","))),
        _ => serde_json::to_string(value).map_err(|e| DltError::invalid(e.to_string())),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Point { pub x: i64, pub y: i64 }
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Size { pub width: i64, pub height: i64 }
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Rect { pub x: i64, pub y: i64, pub width: i64, pub height: i64 }
/// Integer source-image pixels, with a top-left origin.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PixelRect { pub x: u32, pub y: u32, pub width: u32, pub height: u32 }
impl Size {
    pub fn a4() -> Self { Self { width: 210 * EMU_PER_MM, height: 297 * EMU_PER_MM } }
    pub fn slide() -> Self { Self { width: 12_192_000, height: 6_858_000 } }
    pub fn valid(self) -> bool { self.width > 0 && self.height > 0 && self.width <= 10_000 * EMU_PER_MM && self.height <= 10_000 * EMU_PER_MM }
}
impl Rect {
    pub fn size(self) -> Size { Size { width: self.width, height: self.height } }
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.x && p.y >= self.y && p.x <= self.x.saturating_add(self.width) && p.y <= self.y.saturating_add(self.height)
    }
}
pub fn pixels(emu: i64, dpi: u32) -> DltResult<u32> {
    if emu <= 0 || dpi == 0 || dpi > 9_600 { return Err(DltError::invalid("Positive length and DPI (at most 9600) required")); }
    let n = (i128::from(emu) * i128::from(dpi) + i128::from(EMU_PER_INCH / 2)) / i128::from(EMU_PER_INCH);
    u32::try_from(n.max(1)).map_err(|_| DltError::invalid("Raster dimension exceeds u32"))
}
/// Nearest representable native-format value, ties away from zero (e.g. DOCX twips).
pub fn quantize(emu: i64, step: i64) -> DltResult<i64> {
    if step <= 0 { return Err(DltError::invalid("Native unit step must be positive")); }
    let n = i128::from(emu);
    let unit = i128::from(step);
    let rounded = ((n.abs() + unit / 2) / unit) * unit * n.signum();
    i64::try_from(rounded).map_err(|_| DltError::invalid("Native unit rounding overflow"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidArgument, NotFound, AlreadyExists, RequestIdMismatch, TargetBusy,
    WorkspaceLocked, HostNotRunning, ProtocolMismatch, RenderPending, StaleRender,
    RevisionConflict, UnsupportedOperation, ContentLocked, DataAmbiguous,
    SourceChanged, UnsafeRoundtrip, ExportLossRequired, Canceled, Io, Internal,
    StorageFailure, JobNotActive, SchemaUnsupported, IntegrityError,
    /// P1: the counterpart is not there now (no live GUI view for a ui.* call, or its queue is full).
    Unavailable,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct DltError {
    pub code: ErrorCode, pub message: String,
    #[serde(default)] pub current_revision: Option<Revision>,
    #[serde(default)] pub affected_ids: Vec<Id>,
    #[serde(default)] pub retryable: bool,
    #[serde(default)] pub diagnostics: Vec<Diagnostic>,
}
impl DltError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self { Self { code, message: message.into(), current_revision: None, affected_ids: vec![], retryable: false, diagnostics: vec![] } }
    pub fn invalid(message: impl Into<String>) -> Self { Self::new(ErrorCode::InvalidArgument, message) }
    pub fn unsupported(message: impl Into<String>) -> Self { Self::new(ErrorCode::UnsupportedOperation, message) }
}
impl From<std::io::Error> for DltError {
    fn from(e: std::io::Error) -> Self { Self::new(ErrorCode::Io, e.to_string()) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind { Canvas, WebCode, Slides, Docx, Hwpx, Hwp, Figure }

pub fn artifact_native_extension(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Canvas | ArtifactKind::WebCode | ArtifactKind::Figure => "designllm",
        ArtifactKind::Slides => "pptx",
        ArtifactKind::Docx => "docx",
        ArtifactKind::Hwpx => "hwpx",
        ArtifactKind::Hwp => "hwp",
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Intent { Design, Content, Data }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Support { Supported, Approximate, PreserveOnly, Unsupported }
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    Text, TextStyle, NamedStyles, Geometry, PageSetup, ParagraphFormat, Tables, Images,
    Pages, Notes, Diagram, Plot, Dataset, Evidence, FigurePanels, FigureLinks,
    CanvasLayout, Interactions, WebSource, NativeRoundtrip, RenderSvg, RenderPdf,
    RenderPng, HitTest, TextSelection,
    /// v1.2: corner radius, opacity, shadows, gradients, text and stroke extras.
    Effects,
    /// v1.2: NodeKind::Path and icons.
    VectorPaths,
    /// v1.2: LayoutSpec mode "grid" and the new LayoutSpec fields.
    GridLayout,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FeatureSupport { pub read: Support, pub render: Support, pub edit: Support, pub preserve: Support, pub write: Support, pub reason: String }
pub type CapabilityReport = BTreeMap<Feature, FeatureSupport>;
/// String-keyed capabilities that can be added without extending the persisted [`Feature`] map.
pub type CapabilityExtensions = BTreeMap<String, FeatureSupport>;
/// Stable key in [`CapabilityExtensions`] for Canvas text lists.
pub const CAPABILITY_EXTENSION_LISTS: &str = "lists";
/// Stable key in [`CapabilityExtensions`] for local Canvas design tokens.
pub const CAPABILITY_EXTENSION_DESIGN_TOKENS: &str = "design_tokens";
/// Stable key in [`CapabilityExtensions`] for local Canvas component instances.
pub const CAPABILITY_EXTENSION_COMPONENTS: &str = "components";
/// Stable key in [`CapabilityExtensions`] for page-local Canvas board titles.
pub const CAPABILITY_EXTENSION_BOARD_TITLES: &str = "board_titles";
/// Stable key in [`CapabilityExtensions`] for Canvas page sections.
pub const CAPABILITY_EXTENSION_PAGE_SECTIONS: &str = "page_sections";
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Diagnostic {
    pub code: String, pub message: String, pub node_id: Option<Id>, pub blocking: bool,
    /// P1: the measured values behind the message (e.g. TOUCH_TARGET `{w, h, min}`, CONTRAST
    /// `{ratio, min}`, CLIPPED `{text, box}` in EMU, CHILD_OUTSIDE_PARENT `{node, parent}`), so a
    /// refusal can be explained with numbers. Empty and absent for every existing diagnostic.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")] pub params: BTreeMap<String, Value>,
}
impl Diagnostic {
    /// Adds one measured value: `blocking(..).with_param("w", 40).with_param("min", 44)`.
    pub fn with_param(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.params.insert(key.into(), value.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AssetRef { pub sha256: String, pub mime: String, pub bytes: u64 }
pub trait AssetStore: Send + Sync {
    fn put(&self, bytes: &[u8], mime: &str) -> DltResult<AssetRef>;
    fn read(&self, asset: &AssetRef) -> DltResult<Vec<u8>>;
    fn lookup(&self, hash: &str) -> DltResult<AssetRef>;
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NativeSource {
    pub asset: AssetRef,
    #[serde(default)] pub mappings: BTreeMap<Id, Value>,
    #[serde(default)] pub preserved: BTreeMap<String, Value>,
    #[serde(default)] pub preservation: PreservationManifest,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct PreservationManifest {
    /// ZIP part / OLE stream names; hashes are of decoded payloads, not archive bytes.
    pub parts: BTreeMap<String, PartFingerprint>,
    pub opaque_regions: Vec<OpaqueRegion>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PartFingerprint { pub sha256: String, pub content_type: Option<String>, pub immutable: bool }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OpaqueRegion { pub part: String, pub anchor: String, pub sha256: String }
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct PreservationVerification { pub checked_parts: u32, pub checked_regions: u32, pub diagnostics: Vec<Diagnostic> }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesignSystemRef { pub artifact_id: Id, pub revision: Revision }

fn bool_is_false(value: &bool) -> bool { !*value }

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Artifact {
    pub schema_version: u32, pub id: Id, pub kind: ArtifactKind, pub revision: Revision,
    pub title: String, pub engine_id: String, pub origin: String,
    pub body: ArtifactBody,
    #[serde(default)] pub native: Option<NativeSource>,
    #[serde(default)] pub capabilities: CapabilityReport,
    #[serde(default)] pub figure_links: Vec<FigureLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub design_system: Option<DesignSystemRef>,
    #[serde(default, skip_serializing_if = "bool_is_false")] pub publishes_design_system: bool,
    #[serde(default)] pub diagnostics: Vec<Diagnostic>,
}
impl Artifact {
    pub fn new(kind: ArtifactKind, title: impl Into<String>, engine_id: impl Into<String>, body: ArtifactBody) -> Self {
        Self { schema_version: SCHEMA_VERSION, id: new_id(), kind, revision: 0, title: title.into(), engine_id: engine_id.into(), origin: "created".into(), body, native: None, capabilities: BTreeMap::new(), figure_links: vec![], design_system: None, publishes_design_system: false, diagnostics: vec![] }
    }
    pub fn public_view(&self) -> Self { let mut value = self.clone(); value.native = None; value }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactSummary {
    pub id: Id, pub kind: ArtifactKind, pub title: String, pub revision: Revision, pub engine_id: String, pub dirty: bool,
    /// P1 (home list): when the current revision was committed, Unix milliseconds. Filled by
    /// `workspace.list`; None in create/open results and for revisions committed before P1.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub modified_at_ms: Option<u64>,
    /// P1 (home avatars): the artifact's queued, running or validating jobs, oldest first. Filled
    /// by `workspace.list`; empty in create/open results.
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub active_jobs: Vec<ActiveJob>,
}
/// One active job of an artifact (`ArtifactSummary.active_jobs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActiveJob {
    pub job_id: Id,
    pub runtime: String,
    /// MCP client of the current attempt, once its connection reported it (job.set_client).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub client_name: Option<String>,
    /// queued, running or validating.
    pub state: JobStatus,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", content = "document", rename_all = "snake_case")]
pub enum ArtifactBody { Canvas(CanvasDocument), WebCode(WebDocument), Slides(SlideDocument), Flow(FlowDocument), Figure(FigureDocument) }

/// A missing field takes its `Style::default()` value (v1.2: partial styles are accepted). The
/// v1.2 fields are unset by default and then not serialized.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Style {
    pub font_family: String, pub font_size: i64, pub color: String, pub fill: String,
    pub stroke: String, pub stroke_width: i64, pub bold: bool, pub italic: bool,
    pub align: String,
    /// EMU, all four corners. None = engine default (rounded shapes: 10% of min(w, h)).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub corner_radius: Option<i64>,
    /// 0..=1000, multiplies every paint (fill, stroke, text, image, shadow color).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub opacity_milli: Option<u32>,
    /// v1.2: at most one, outer shadows only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub shadows: Vec<Shadow>,
    /// Painted instead of `fill`. Not for Text, Line or Image.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub gradient: Option<Gradient>,
    /// Multiple of the font size × 1000. None = font metrics.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub line_height_milli: Option<u32>,
    /// EMU added after every grapheme (as in CSS).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub letter_spacing: Option<i64>,
    /// Inside the text box.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub vertical_align: Option<VerticalAlign>,
    /// 100..=900; when set it wins over `bold`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub font_weight: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub stroke_dash: Option<StrokeDash>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub stroke_cap: Option<StrokeCap>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub stroke_join: Option<StrokeJoin>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub underline: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub strikethrough: Option<bool>,
    /// Line marker at `from`. Unset = no start marker.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub stroke_start: Option<LineEnd>,
    /// Line marker at `to`. Unset = legacy `Primitive::Line.arrow` behavior, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub stroke_end: Option<LineEnd>,
}
impl Default for Style {
    fn default() -> Self {
        Self {
            font_family: "Pretendard".into(), font_size: 12 * EMU_PER_PT, color: "#20252D".into(), fill: "#FFFFFF".into(), stroke: "#566479".into(), stroke_width: EMU_PER_PT, bold: false, italic: false, align: "left".into(),
            corner_radius: None, opacity_milli: None, shadows: Vec::new(), gradient: None, line_height_milli: None, letter_spacing: None, vertical_align: None,
            font_weight: None, stroke_dash: None, stroke_cap: None, stroke_join: None, underline: None, strikethrough: None,
            stroke_start: None, stroke_end: None,
        }
    }
}
impl Style {
    /// The v1.2 fields that carry a value (engines that do not support them refuse these).
    pub fn non_default_v12_fields(&self) -> BTreeSet<StyleField> {
        use StyleField as F;
        [
            (F::CornerRadius, self.corner_radius.is_some()), (F::Opacity, self.opacity_milli.is_some()), (F::Shadows, !self.shadows.is_empty()),
            (F::Gradient, self.gradient.is_some()), (F::LineHeight, self.line_height_milli.is_some()), (F::LetterSpacing, self.letter_spacing.is_some()),
            (F::VerticalAlign, self.vertical_align.is_some()), (F::FontWeight, self.font_weight.is_some()), (F::StrokeDash, self.stroke_dash.is_some()),
            (F::StrokeCap, self.stroke_cap.is_some()), (F::StrokeJoin, self.stroke_join.is_some()), (F::Underline, self.underline.is_some()),
            (F::Strikethrough, self.strikethrough.is_some()), (F::StrokeStart, self.stroke_start.is_some()), (F::StrokeEnd, self.stroke_end.is_some()),
        ]
        .into_iter().filter(|(_, set)| *set).map(|(f, _)| f).collect()
    }
    /// Back to the `Style::default()` value (v1.2 fields: unset).
    fn clear_field(&mut self, field: StyleField) {
        let base = Style::default();
        match field {
            StyleField::FontFamily => self.font_family = base.font_family,
            StyleField::FontSize => self.font_size = base.font_size,
            StyleField::Color => self.color = base.color,
            StyleField::Fill => self.fill = base.fill,
            StyleField::Stroke => self.stroke = base.stroke,
            StyleField::StrokeWidth => self.stroke_width = base.stroke_width,
            StyleField::Bold => self.bold = base.bold,
            StyleField::Italic => self.italic = base.italic,
            StyleField::Align => self.align = base.align,
            StyleField::CornerRadius => self.corner_radius = None,
            StyleField::Opacity => self.opacity_milli = None,
            StyleField::Shadows => self.shadows.clear(),
            StyleField::Gradient => self.gradient = None,
            StyleField::LineHeight => self.line_height_milli = None,
            StyleField::LetterSpacing => self.letter_spacing = None,
            StyleField::VerticalAlign => self.vertical_align = None,
            StyleField::FontWeight => self.font_weight = None,
            StyleField::StrokeDash => self.stroke_dash = None,
            StyleField::StrokeCap => self.stroke_cap = None,
            StyleField::StrokeJoin => self.stroke_join = None,
            StyleField::Underline => self.underline = None,
            StyleField::Strikethrough => self.strikethrough = None,
            StyleField::StrokeStart => self.stroke_start = None,
            StyleField::StrokeEnd => self.stroke_end = None,
        }
    }
}
/// A null (or missing) field means "no change"; `clear` resets fields (applied before the sets).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StylePatch {
    pub font_family: Option<String>, pub font_size: Option<i64>, pub color: Option<String>,
    pub fill: Option<String>, pub stroke: Option<String>, pub stroke_width: Option<i64>,
    pub bold: Option<bool>, pub italic: Option<bool>, pub align: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub corner_radius: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub opacity_milli: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub shadows: Option<Vec<Shadow>>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub gradient: Option<Gradient>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub line_height_milli: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub letter_spacing: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub vertical_align: Option<VerticalAlign>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub font_weight: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub stroke_dash: Option<StrokeDash>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub stroke_cap: Option<StrokeCap>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub stroke_join: Option<StrokeJoin>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub underline: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub strikethrough: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub stroke_start: Option<LineEnd>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub stroke_end: Option<LineEnd>,
    /// Fields reset to their default before the sets. A field may not be set and cleared at once.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")] pub clear: BTreeSet<StyleField>,
}
impl StylePatch {
    /// Clears `clear`, then sets every non-null field. Refuse a patch with [`StylePatch::check`] first.
    pub fn apply(&self, style: &mut Style) {
        for field in &self.clear { style.clear_field(*field); }
        if let Some(v) = &self.font_family { style.font_family.clone_from(v); }
        if let Some(v) = self.font_size { style.font_size = v; }
        if let Some(v) = &self.color { style.color.clone_from(v); }
        if let Some(v) = &self.fill { style.fill.clone_from(v); }
        if let Some(v) = &self.stroke { style.stroke.clone_from(v); }
        if let Some(v) = self.stroke_width { style.stroke_width = v; }
        if let Some(v) = self.bold { style.bold = v; }
        if let Some(v) = self.italic { style.italic = v; }
        if let Some(v) = &self.align { style.align.clone_from(v); }
        if let Some(v) = self.corner_radius { style.corner_radius = Some(v); }
        if let Some(v) = self.opacity_milli { style.opacity_milli = Some(v); }
        if let Some(v) = &self.shadows { style.shadows.clone_from(v); }
        if let Some(v) = &self.gradient { style.gradient = Some(v.clone()); }
        if let Some(v) = self.line_height_milli { style.line_height_milli = Some(v); }
        if let Some(v) = self.letter_spacing { style.letter_spacing = Some(v); }
        if let Some(v) = self.vertical_align { style.vertical_align = Some(v); }
        if let Some(v) = self.font_weight { style.font_weight = Some(v); }
        if let Some(v) = self.stroke_dash { style.stroke_dash = Some(v); }
        if let Some(v) = self.stroke_cap { style.stroke_cap = Some(v); }
        if let Some(v) = self.stroke_join { style.stroke_join = Some(v); }
        if let Some(v) = self.underline { style.underline = Some(v); }
        if let Some(v) = self.strikethrough { style.strikethrough = Some(v); }
        if let Some(v) = self.stroke_start { style.stroke_start = Some(v); }
        if let Some(v) = self.stroke_end { style.stroke_end = Some(v); }
    }
    /// Fields the patch sets (non-null).
    pub fn set_fields(&self) -> BTreeSet<StyleField> {
        use StyleField as F;
        [
            (F::FontFamily, self.font_family.is_some()), (F::FontSize, self.font_size.is_some()), (F::Color, self.color.is_some()), (F::Fill, self.fill.is_some()),
            (F::Stroke, self.stroke.is_some()), (F::StrokeWidth, self.stroke_width.is_some()), (F::Bold, self.bold.is_some()), (F::Italic, self.italic.is_some()),
            (F::Align, self.align.is_some()), (F::CornerRadius, self.corner_radius.is_some()), (F::Opacity, self.opacity_milli.is_some()), (F::Shadows, self.shadows.is_some()),
            (F::Gradient, self.gradient.is_some()), (F::LineHeight, self.line_height_milli.is_some()), (F::LetterSpacing, self.letter_spacing.is_some()),
            (F::VerticalAlign, self.vertical_align.is_some()), (F::FontWeight, self.font_weight.is_some()), (F::StrokeDash, self.stroke_dash.is_some()),
            (F::StrokeCap, self.stroke_cap.is_some()), (F::StrokeJoin, self.stroke_join.is_some()), (F::Underline, self.underline.is_some()),
            (F::Strikethrough, self.strikethrough.is_some()), (F::StrokeStart, self.stroke_start.is_some()), (F::StrokeEnd, self.stroke_end.is_some()),
        ]
        .into_iter().filter(|(_, set)| *set).map(|(f, _)| f).collect()
    }
    /// Every field the patch touches: the ones it sets and the ones it clears.
    pub fn fields(&self) -> BTreeSet<StyleField> { self.set_fields().union(&self.clear).copied().collect() }
    /// INVALID_ARGUMENT when a field is both set and cleared. The message names fields by their
    /// JSON key (`fill`, `opacity_milli`), the name a client sends back.
    pub fn check(&self) -> DltResult<()> {
        let both: Vec<&str> = self.set_fields().intersection(&self.clear).map(|f| f.name()).collect();
        if both.is_empty() { Ok(()) } else { Err(DltError::invalid(format!("these style fields are both set and cleared: {}", both.join(", ")))) }
    }
}
/// The fields of [`Style`] by name (`StylePatch::clear`, guards and diagnostics). Each wire name
/// is exactly the Style/StylePatch key it stands for (`opacity_milli`, not `opacity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StyleField {
    FontFamily, FontSize, Color, Fill, Stroke, StrokeWidth, Bold, Italic, Align,
    CornerRadius,
    #[serde(rename = "opacity_milli")] Opacity,
    Shadows, Gradient,
    #[serde(rename = "line_height_milli")] LineHeight,
    LetterSpacing, VerticalAlign,
    FontWeight, StrokeDash, StrokeCap, StrokeJoin, Underline, Strikethrough, StrokeStart, StrokeEnd,
}
impl StyleField {
    /// Added in v1.2 (everything after `Align`).
    pub fn is_v12(self) -> bool { !matches!(self, Self::FontFamily | Self::FontSize | Self::Color | Self::Fill | Self::Stroke | Self::StrokeWidth | Self::Bold | Self::Italic | Self::Align) }
    /// The JSON name, equal to the Style/StylePatch key (`fill`, `opacity_milli`). Use this (or
    /// `Display`), not `Debug`, in messages: clients resend the name they read.
    pub fn name(self) -> &'static str {
        match self {
            Self::FontFamily => "font_family", Self::FontSize => "font_size", Self::Color => "color", Self::Fill => "fill", Self::Stroke => "stroke",
            Self::StrokeWidth => "stroke_width", Self::Bold => "bold", Self::Italic => "italic", Self::Align => "align", Self::CornerRadius => "corner_radius",
            Self::Opacity => "opacity_milli", Self::Shadows => "shadows", Self::Gradient => "gradient", Self::LineHeight => "line_height_milli",
            Self::LetterSpacing => "letter_spacing", Self::VerticalAlign => "vertical_align", Self::FontWeight => "font_weight", Self::StrokeDash => "stroke_dash",
            Self::StrokeCap => "stroke_cap", Self::StrokeJoin => "stroke_join", Self::Underline => "underline", Self::Strikethrough => "strikethrough",
            Self::StrokeStart => "stroke_start", Self::StrokeEnd => "stroke_end",
        }
    }
}
impl std::fmt::Display for StyleField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.name()) }
}
/// CSS outer box shadow. `blur` >= 0; `color` is #RRGGBB or #RRGGBBAA.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Shadow { pub x: i64, pub y: i64, pub blur: i64, pub spread: i64, pub color: String }
/// Linear gradient. `angle_millideg` follows CSS (0 = bottom to top, clockwise), in [0, 360000).
/// v1.2: exactly two opaque stops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Gradient { pub angle_millideg: i32, pub stops: Vec<GradientStop> }
/// `pos_milli` in 0..=1000.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GradientStop { pub pos_milli: u32, pub color: String }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerticalAlign { Top, Middle, Bottom }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StrokeDash { Solid, Dashed, Dotted }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StrokeCap { Butt, Round, Square }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StrokeJoin { Miter, Round, Bevel }
/// Marker drawn at a line endpoint. `Open` draws two unfilled arms; `Filled` draws a filled triangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LineEnd { Open, Filled }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Text, Rectangle, Ellipse, Line, Image, Group,
    /// Canvas video; the source is in `Node.asset` and its first-frame PNG is in `ext.video_poster_asset`.
    Video,
    /// v1.2: vector path in `ext.path`.
    Path,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Node {
    pub id: Id, pub kind: NodeKind, pub bounds: Rect,
    #[serde(default)] pub text: String,
    #[serde(default)] pub style: Style,
    #[serde(default)] pub asset: Option<AssetRef>,
    #[serde(default)] pub children: Vec<Node>,
    #[serde(default)] pub layout: Option<LayoutSpec>,
    #[serde(default)] pub interactions: Vec<Interaction>,
    #[serde(default)] pub variants: BTreeMap<String, StylePatch>,
    /// v1.2 fields, flattened into the node object and omitted while unset (last, so v1.1 bytes stay).
    #[serde(flatten, default)] pub ext: NodeExt,
}
/// Instance-specific values retained when a component page is refreshed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub style: Option<StylePatch>,
    /// `Some(true)` when a repeat row's condition hides this node (D21). The copy is hidden when the source or this is.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub hidden: Option<bool>,
}

/// Canvas component source and instance-specific overrides for one Group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstanceRef {
    pub component_page_id: Id,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")] pub overrides: BTreeMap<Id, NodeOverride>,
}

/// Canvas node extension fields. Each is omitted from JSON while unset; engines refuse values they do not support.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NodeExt {
    /// NodeKind::Path only.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub path: Option<PathSpec>,
    /// Where an icon Path came from.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub icon: Option<IconRef>,
    /// NodeKind::Image.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub image_fit: Option<ImageFit>,
    /// Non-destructive crop in source-image pixels; `None` shows the full image.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub image_crop_pixel_rect: Option<PixelRect>,
    /// NodeKind::Video only. Automatically extracted first-frame poster as a PNG asset.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub video_poster_asset: Option<AssetRef>,
    /// Alternative text.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub alt: Option<String>,
    /// Layer name.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub hidden: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub locked: Option<bool>,
    /// Canvas NodeKind::Text only. Each hard paragraph is one list item.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub list: Option<TextListKind>,
    /// Canvas Group instance metadata; derived child nodes remain ordinary materialized nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub instance: Option<InstanceRef>,
    /// Canvas table metadata on a Group. Its rows reference ordinary Rectangle cell children.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub table: Option<CanvasTable>,
    /// Canvas repeat group (D21): an empty Group expanded into one component instance per data row of a table.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub repeat: Option<RepeatSpec>,
    /// Canvas show condition (D21) on a Group inside a repeat component source, decided per data row.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub condition: Option<ShowCondition>,
    /// Figure Diagram NodeKind::Text only: `Node.text` is a math source in this syntax (`figure_math::parse`).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub math: Option<MathSyntax>,
}
/// Syntax of a math node's source text. `tex_subset`: the TeX subset that `figure_math::parse` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MathSyntax { TexSubset }
impl NodeExt {
    /// Names of the fields that carry a value (engines that do not support them refuse these).
    pub fn set_fields(&self) -> Vec<&'static str> {
        [
            ("path", self.path.is_some()), ("icon", self.icon.is_some()), ("image_fit", self.image_fit.is_some()),
            ("image_crop_pixel_rect", self.image_crop_pixel_rect.is_some()), ("video_poster_asset", self.video_poster_asset.is_some()),
            ("alt", self.alt.is_some()),
            ("name", self.name.is_some()), ("hidden", self.hidden.is_some()), ("locked", self.locked.is_some()), ("list", self.list.is_some()),
            ("instance", self.instance.is_some()),
            ("table", self.table.is_some()),
            ("repeat", self.repeat.is_some()),
            ("condition", self.condition.is_some()),
            ("math", self.math.is_some()),
        ]
        .into_iter().filter(|(_, set)| *set).map(|(name, _)| name).collect()
    }
}
/// The data table, the component page, and the text bindings of a repeat group (D21, limits in `repeat_groups`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepeatSpec {
    /// The Canvas table (`NodeExt.table`) whose data rows are repeated; its header row is not a data row.
    pub table_id: Id,
    /// The component page instantiated once per data row.
    pub component_page_id: Id,
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub bindings: Vec<RepeatBinding>,
}
/// A text node of the component that shows one column of the row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepeatBinding {
    pub source_node_id: Id,
    /// Column index, counted from 0.
    pub column: u32,
}
/// Shows a Group only for the rows whose column value passes `op`. Values are compared as plain strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShowCondition {
    /// Column index, counted from 0.
    pub column: u32,
    pub op: ConditionOp,
    /// The value `equals` and `not_equals` compare with.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub value: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConditionOp { Equals, NotEquals, IsEmpty, IsNotEmpty }
/// Vector geometry. `view_box` and every command coordinate are SVG user units × 1000 (integers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PathSpec { pub view_box: Rect, pub fill_rule: FillRule, pub commands: Vec<PathCommand> }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PathCommand {
    MoveTo { x: i64, y: i64 },
    LineTo { x: i64, y: i64 },
    CubicTo { x1: i64, y1: i64, x2: i64, y2: i64, x: i64, y: i64 },
    Close,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FillRule { #[default] Nonzero, Evenodd }
/// Source of an icon placed as a Path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IconRef { pub set: String, pub name: String, pub version: String }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImageFit { Fill, Contain, Cover }
/// List marker style for a Canvas text node. Each hard paragraph in the node becomes an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TextListKind { Bullet, Numbered }
/// Semantic Canvas table metadata. Cells remain ordinary rectangle nodes so text, borders, selection, and layout
/// continue to use the standard node operations. Missing column weights mean equal-width tracks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CanvasTable {
    pub rows: Vec<CanvasTableRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub header_row_id: Option<Id>,
    /// Positive relative EMU weights, one per column; omitted weights preserve legacy equal-width bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(inner(range(min = 1))) ]
    pub column_widths: Option<Vec<u32>>,
    /// Merged cell extents keyed by anchor cell ID; an omitted map means no merged cells.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cell_spans: BTreeMap<Id, CanvasTableSpan>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CanvasTableSpan {
    #[schemars(range(min = 1))] pub row_span: u32,
    #[schemars(range(min = 1))] pub column_span: u32,
}
/// A row's ordered cell node IDs. Every row in a CanvasTable has the same number of cells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CanvasTableRow { pub id: Id, pub cell_ids: Vec<Id> }

/// A page used as a Canvas component source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PageRole { Component }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Page {
    pub id: Id, pub size: Size, pub nodes: Vec<Node>, #[serde(default)] pub notes: String,
    /// v1.2: artboard title / page list name.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub name: Option<String>,
    /// P3: component pages are sources and are omitted from default exports.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub role: Option<PageRole>,
}
/// The active local Canvas token mode. Missing `CanvasDocument.token_mode` means `Light`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TokenMode { Light, Dark }

/// A local token's two concrete values. Tokens apply concrete values to the ordinary style or layout fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TokenValues {
    Color { light: String, dark: String },
    FontFamily { light: String, dark: String },
    FontSize { light: i64, dark: i64 },
    Spacing { light: i64, dark: i64 },
    Radius { light: i64, dark: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenDef { pub name: String, pub values: TokenValues }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TokenSource { Local, Shared }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenRef { pub source: TokenSource, pub token_id: String }

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaddingSide { Top, Right, Bottom, Left }

/// Typed paths prevent a binding from being attached to an unrelated style or layout field.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BindTarget {
    StyleColor,
    StyleFill,
    StyleStroke,
    StyleFontFamily,
    StyleFontSize,
    StyleCornerRadius,
    LayoutGap,
    LayoutRowGap,
    LayoutPadding,
    LayoutPaddingSide { side: PaddingSide },
    BreakpointGap { min_width_css_px: u32 },
    BreakpointPadding { min_width_css_px: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenBinding { pub node_id: Id, pub target: BindTarget, pub token: TokenRef }

/// The kind of a board-only annotation anchored to one Canvas page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CanvasBoardMemoKind {
    #[default]
    Memo,
    Title,
}

impl CanvasBoardMemoKind {
    fn is_memo(&self) -> bool { *self == Self::Memo }
}

/// A board-only memo or title anchored to one Canvas page. It is not rendered document content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CanvasBoardMemo {
    /// The default memo kind is omitted so existing memo JSON and content digests stay unchanged.
    #[serde(default, skip_serializing_if = "CanvasBoardMemoKind::is_memo")]
    pub kind: CanvasBoardMemoKind,
    pub id: Id,
    pub page_id: Id,
    /// Page-local EMU coordinates; x and y may be negative outside the artboard.
    pub bounds: Rect,
    pub text: String,
    /// Memo background color, or title text color when `kind` is Title.
    pub color: String,
}

/// A section marker on a normal Canvas page. Membership is derived from page order through the
/// page immediately before the next section marker. CanvasDocument stores sections in ID order;
/// their visible order follows their start pages in `pages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CanvasPageSection {
    pub id: Id,
    pub name: String,
    pub start_page_id: Id,
}

/// Saved page-local layout guides. The page ID is the key in `CanvasDocument.layout_guides`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CanvasPageGuides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<CanvasAxisGuides>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<CanvasAxisGuides>,
}

/// One page-local axis layout; every length is stored in EMU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CanvasAxisGuides {
    #[schemars(range(min = 1, max = 24))]
    pub count: u32,
    pub spacing_emu: i64,
    pub leading_margin_emu: i64,
    pub trailing_margin_emu: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CanvasDocument {
    pub pages: Vec<Page>,
    /// Canvas board annotations are excluded from RenderScene and document exports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub board_memos: Vec<CanvasBoardMemo>,
    /// Page grouping markers; membership and display order follow `pages`, while this stored list is ID-sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub page_sections: Vec<CanvasPageSection>,
    /// Page-local editor guides. Empty documents omit the field to preserve existing snapshot bytes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")] pub layout_guides: BTreeMap<Id, CanvasPageGuides>,
    #[serde(default)] pub styles: BTreeMap<String, Style>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")] pub tokens: BTreeMap<String, TokenDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub bindings: Vec<TokenBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub token_mode: Option<TokenMode>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SlideDocument {
    pub pages: Vec<Page>, #[serde(default)] pub styles: BTreeMap<String, Style>,
    /// Theme values set by `SetTheme`, over the theme of the deck's package. A deck without theme edits omits the field.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub theme: Option<ThemePatch>,
}
/// Colour slots of a slide theme (`a:clrScheme`), in schema order.
pub const THEME_COLOR_SLOTS: [&str; 12] =
    ["dk1", "lt1", "dk2", "lt2", "accent1", "accent2", "accent3", "accent4", "accent5", "accent6", "hlink", "folHlink"];
/// Theme values to set. A missing value is unchanged. `colors` maps a slot of `THEME_COLOR_SLOTS` to `#RRGGBB`; the fonts
/// are the Latin (`a:latin`) and East Asian (`a:ea`) faces of the major (headings) and minor (body) fonts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThemePatch {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")] pub colors: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub major_latin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub minor_latin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub major_ea: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub minor_ea: Option<String>,
}
impl ThemePatch {
    /// True when the patch sets nothing.
    pub fn is_empty(&self) -> bool {
        self.colors.is_empty() && self.major_latin.is_none() && self.minor_latin.is_none() && self.major_ea.is_none() && self.minor_ea.is_none()
    }
    /// `self` with the values of `later` written over it.
    pub fn merged(&self, later: &ThemePatch) -> ThemePatch {
        let mut out = self.clone();
        out.colors.extend(later.colors.iter().map(|(slot, value)| (slot.clone(), value.clone())));
        for (target, value) in [
            (&mut out.major_latin, &later.major_latin), (&mut out.minor_latin, &later.minor_latin),
            (&mut out.major_ea, &later.major_ea), (&mut out.minor_ea, &later.minor_ea),
        ] {
            if value.is_some() { target.clone_from(value); }
        }
        out
    }
}
/// A missing field takes its `LayoutSpec::default()` value (v1.2). `mode` "grid" is a v1.2 value
/// (engines validate it). The v1.2 fields are omitted while unset.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct LayoutSpec {
    pub mode: String, pub width_mode: String, pub height_mode: String,
    pub gap: i64, pub padding: i64, pub align: String,
    #[serde(default)] pub min_width: Option<i64>, #[serde(default)] pub max_width: Option<i64>,
    #[serde(default)] pub min_height: Option<i64>, #[serde(default)] pub max_height: Option<i64>,
    #[serde(default)] pub breakpoints: Vec<Breakpoint>,
    /// Main-axis distribution.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub justify: Option<Justify>,
    /// top, right, bottom, left (EMU). Used instead of `padding` when set.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub padding_sides: Option<[i64; 4]>,
    /// mode "grid": number of equal columns.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub grid_columns: Option<u32>,
    /// None = `gap`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub row_gap: Option<i64>,
    /// On a child: its own cross-axis alignment.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub align_self: Option<AlignSelf>,
    /// On a child of a grid: columns it spans.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub grid_span: Option<u32>,
}
impl Default for LayoutSpec {
    fn default() -> Self {
        Self {
            mode: "absolute".into(), width_mode: "fixed".into(), height_mode: "fixed".into(), gap: 0, padding: 0, align: "start".into(), min_width: None, max_width: None, min_height: None, max_height: None, breakpoints: vec![],
            justify: None, padding_sides: None, grid_columns: None, row_gap: None, align_self: None, grid_span: None,
        }
    }
}
impl LayoutSpec {
    /// Names of the v1.2 fields that carry a value, plus "mode=grid" (engines that do not support them refuse these).
    pub fn v12_fields(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = [
            ("justify", self.justify.is_some()), ("padding_sides", self.padding_sides.is_some()), ("grid_columns", self.grid_columns.is_some()),
            ("row_gap", self.row_gap.is_some()), ("align_self", self.align_self.is_some()), ("grid_span", self.grid_span.is_some()),
        ]
        .into_iter().filter(|(_, set)| *set).map(|(name, _)| name).collect();
        if self.mode == "grid" { out.push("mode=grid"); }
        for b in &self.breakpoints {
            if b.grid_columns.is_some() && !out.contains(&"breakpoints.grid_columns") { out.push("breakpoints.grid_columns"); }
            if b.justify.is_some() && !out.contains(&"breakpoints.justify") { out.push("breakpoints.justify"); }
            if b.mode.as_deref() == Some("grid") && !out.contains(&"breakpoints.mode=grid") { out.push("breakpoints.mode=grid"); }
        }
        out
    }
}
/// A missing field takes its default (v1.2). The v1.2 fields are omitted while unset.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Breakpoint {
    pub min_width_css_px: u32, pub mode: Option<String>, pub gap: Option<i64>, pub padding: Option<i64>, pub align: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub grid_columns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub justify: Option<Justify>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Justify { Start, Center, End, SpaceBetween, SpaceAround, SpaceEvenly }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlignSelf { Auto, Start, Center, End, Stretch }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Interaction {
    NavigatePage { page_id: Id }, ToggleVisibility { node_id: Id }, SetVariant { node_id: Id, variant: String },
    /// v1.2. Only https:, mailto: and tel: (checked when the value is read, see [`check_interaction_url`]).
    OpenUrl {
        #[serde(deserialize_with = "deserialize_interaction_url")]
        #[schemars(with = "String")]
        url: String,
    },
    /// v1.2: back to the previously shown page.
    Back,
}
/// Largest accepted interaction URL, in bytes.
pub const MAX_INTERACTION_URL_BYTES: usize = 2048;
/// An `Interaction::OpenUrl` target: `https://host…`, `mailto:…` or `tel:…` (scheme in any case),
/// with no whitespace or control characters. INVALID_ARGUMENT otherwise.
pub fn check_interaction_url(url: &str) -> DltResult<()> {
    let bad = |why: &str| Err(DltError::invalid(format!("interaction url {url:?} {why}")));
    if url.is_empty() || url.len() > MAX_INTERACTION_URL_BYTES { return bad(&format!("must be 1..={MAX_INTERACTION_URL_BYTES} bytes")); }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) { return bad("must not contain whitespace or control characters"); }
    let Some((scheme, rest)) = url.split_once(':') else { return bad("has no scheme; use https:, mailto: or tel:") };
    match scheme.to_ascii_lowercase().as_str() {
        "https" => match rest.strip_prefix("//") {
            Some(host) if !host.is_empty() && !host.starts_with(['/', '?', '#']) => Ok(()),
            _ => bad("must be https://host/..."),
        },
        "mailto" | "tel" if !rest.is_empty() => Ok(()),
        "mailto" | "tel" => bad("has nothing after the scheme"),
        _ => bad("must use https:, mailto: or tel:"),
    }
}
fn deserialize_interaction_url<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let url = String::deserialize(deserializer)?;
    check_interaction_url(&url).map_err(|e| serde::de::Error::custom(e.message))?;
    Ok(url)
}
/// A required key whose value may be null: null clears the value, and a missing key is refused. serde's derive reads a
/// missing `Option` field as None; a field with `deserialize_with` reports the missing key instead. Pair it with
/// `#[schemars(schema_with = "nullable_schema::<T>")]` so the schema lists the key in `required`.
fn required_nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(deserializer: D) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(deserializer)
}
/// The schema of `Option<T>` (the value or null) for a field that `required_nullable` reads.
fn nullable_schema<T: JsonSchema>(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    generator.subschema_for::<Option<T>>()
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebFile { pub content: String, pub sha256: String }
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct WebDocument { pub entry: String, pub files: BTreeMap<String, WebFile> }

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PageSetup { pub size: Size, pub margin_left: i64, pub margin_top: i64, pub margin_right: i64, pub margin_bottom: i64 }
impl Default for PageSetup {
    fn default() -> Self { Self { size: Size::a4(), margin_left: 20 * EMU_PER_MM, margin_top: 20 * EMU_PER_MM, margin_right: 20 * EMU_PER_MM, margin_bottom: 20 * EMU_PER_MM } }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TextRun { pub id: Id, pub text: String, #[serde(default)] pub style: Style, #[serde(default)] pub protected: bool }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ParagraphFormat {
    pub align: String, pub space_before: i64, pub space_after: i64, pub line_spacing: i64, pub indent: i64,
    /// The paragraph starts a new page. `false` is left out of JSON, so existing documents keep their bytes and digests.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub page_break_before: bool,
    /// Proportional line spacing in percent of the largest font size (HWP and HWPX PERCENT). When set it decides the
    /// spacing and `line_spacing` holds the pitch derived from it. `None` is left out of JSON, so existing documents keep
    /// their bytes and digests: no proportional spacing, or 100 percent (single); a new paragraph or table cell without
    /// it takes the document's base proportional spacing. Edit requests take 1..=1000; DOCX and Slides refuse it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_spacing_percent: Option<u32>,
}
impl Default for ParagraphFormat {
    fn default() -> Self { Self { align: "left".into(), space_before: 0, space_after: 6 * EMU_PER_PT, line_spacing: 0, indent: 0, page_break_before: false, line_spacing_percent: None } }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Paragraph { pub id: Id, pub runs: Vec<TextRun>, #[serde(default)] pub style_ref: Option<String>, #[serde(default)] pub format: ParagraphFormat }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TableCell { pub id: Id, pub blocks: Vec<FlowBlock>, pub column_span: u32, pub row_span: u32 }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Table { pub id: Id, pub column_widths: Vec<i64>, pub rows: Vec<Vec<TableCell>>, #[serde(default)] pub style: Style }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum FlowBlock { Paragraph(Paragraph), Table(Table), Image(Node), Opaque { id: Id, label: String } }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Section { pub id: Id, pub setup: PageSetup, pub blocks: Vec<FlowBlock> }
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FlowDocument { pub sections: Vec<Section>, pub styles: BTreeMap<String, Style> }

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Evidence { pub id: Id, pub source_hash: String, pub origin: String, pub physical_page_index: Option<u32>, pub region: Option<Rect>, pub pdf_geometry: Option<PdfPageGeometry>, pub text: String, pub review_state: String }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PdfPageGeometry { pub media_box: PdfBox, pub crop_box: PdfBox, pub user_unit: String, pub rotation_degrees: u16, pub printed_page_label: Option<String> }
/// PDF default user-space, x-right/y-up, with UserUnit applied and converted to EMU.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PdfBox { pub x_min: i64, pub y_min: i64, pub x_max: i64, pub y_max: i64 }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SourceCoverage { pub source_hash: String, pub physical_page_indices: Vec<u32> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DataColumn { pub id: Id, pub name: String, pub data_type: String, pub unit: Option<String> }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CellState { Valid, Missing, Ambiguous, Malformed }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DataCell { pub original: String, pub parsed_decimal: Option<String>, pub state: CellState }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DataRow { pub id: Id, pub cells: BTreeMap<String, DataCell> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Dataset { pub id: Id, pub version_hash: String, pub original_asset_hash: String, pub origin: String, pub columns: Vec<DataColumn>, pub rows: Vec<DataRow>, #[serde(default)] pub derived_from: Option<DatasetRef>, #[serde(default)] pub transforms: Vec<DataTransform> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DataTransform {
    SelectRows { row_ids: Vec<Id> }, Sort { column_id: Id, descending: bool },
    UnitScale { column_id: Id, numerator: String, denominator: String, output_unit: String },
    EditCell { row_id: Id, column_id: Id, previous: String, replacement: String },
    /// P1 (data sheet): a new column after the last one; every existing row gets `fill` (None = a
    /// missing cell). The figures engine generates its id (`col{seq:04}`).
    AddColumn { name: String, data_type: String, #[serde(default)] unit: Option<String>, #[serde(default)] fill: Option<String> },
    /// P1: new rows after `after_row_id` (None = after the last row). Each row maps column id to
    /// its text; a column it leaves out is a missing cell. The figures engine generates the row ids
    /// (`row{seq:04}-{k:03}`).
    AddRows { #[serde(default)] after_row_id: Option<Id>, rows: Vec<BTreeMap<Id, String>> },
    /// P1: removes a row. `previous` must equal its current cell texts (column id -> original).
    RemoveRow { row_id: Id, previous: BTreeMap<Id, String> },
    /// P1: removes a column. `previous_name` must equal its current name; a plot that still uses
    /// the column makes the edit fail when the plots are rebound.
    RemoveColumn { column_id: Id, previous_name: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DatasetRef { pub dataset_id: Id, pub version_hash: String }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlotErrorKind { Sd, Sem, Ci, Range }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ErrorValues { Symmetric { column: Id }, Asymmetric { minus_column: Id, plus_column: Id }, Bounds { lower_column: Id, upper_column: Id } }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ErrorSpec { pub kind: PlotErrorKind, pub values: ErrorValues, pub sample_count_column: Option<String>, pub confidence_level: Option<String> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AxisSpec { pub label: String, pub unit: Option<String>, pub scale: String, pub minimum: Option<String>, pub maximum: Option<String>, #[serde(default)] pub tick_step: Option<String> }
/// Optional display mode of a plot. `None` (absent from every plot written before this field) draws no error key.
/// `ErrorKey` states the statistic the error bars show (SD, SEM, CI with its level, or an explicit range) under the axis
/// caption. A figure that uses a mode is saved with the persisted feature key `figure.plot_error_key.v1`. Only
/// `Operation::SetPlotDisplay` changes a mode; `Operation::SetPlot` must carry the mode the plot has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlotDisplayMode { ErrorKey }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PlotSpec { pub kind: String, pub dataset_ref: DatasetRef, pub x_column: String, pub y_columns: Vec<String>, pub x_axis: AxisSpec, pub y_axis: AxisSpec, pub error: Option<ErrorSpec>, #[serde(default, skip_serializing_if = "Option::is_none")] pub display: Option<PlotDisplayMode> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Edge { pub id: Id, pub from: Id, pub to: Id, pub label: String, pub evidence_ids: Vec<Id>, pub origin: String, #[serde(default)] pub style: Style }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Diagram { pub nodes: Vec<Node>, pub edges: Vec<Edge>, pub evidence_ids: BTreeMap<Id, Vec<Id>>, #[serde(default)] pub node_origins: BTreeMap<Id, String> }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Calibration { pub length: String, pub unit: String, pub pixel_span: String, pub evidence_id: Option<Id> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FigureImage { pub asset: AssetRef, pub origin: String, pub source_ref: Option<Id>, pub pixel_width: u32, pub pixel_height: u32, pub crop_pixel_rect: Option<PixelRect>, pub rotation_millidegrees: i32, pub calibration: Option<Calibration> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum PanelBody { Diagram(Diagram), Plot(PlotSpec), Image(FigureImage) }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FigurePanel { pub id: Id, pub bounds: Rect, pub label: String, pub body: PanelBody, #[serde(default)] pub style: Style }
/// Where the common legend sits, outside every panel. `Bottom` is the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommonLegendPlacement { #[default] Bottom, Top, Right }
/// One entry of a common legend: its label and swatch colour (`#RRGGBB` or `#RRGGBBAA`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommonLegendItem { pub label: String, pub color: String }
/// A legend shared by the panels of a composite figure (R07.6). Labels, colours, title and placement are content
/// (`Intent::Content`) and count in the content digest. A figure without a legend keeps its old JSON and digests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommonLegend {
    pub items: Vec<CommonLegendItem>,
    #[serde(default)] pub placement: CommonLegendPlacement,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub title: Option<String>,
}
/// Visual review state of a figure (R07.8). It is not `Evidence.review_state`: it never sets or clears an evidence review.
/// `Pending` is a review that was requested and not yet decided.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VisualReviewState { #[default] Pending, Approved, NeedsRevision }
/// The figure's visual review (R07.8). It is outside the content and data digests, and it stays as set until
/// `Operation::SetFigureVisualReview` changes it. An approval is given for the figure as it is at approval time: the engine
/// records that figure's digest in `approved_digest`. A content, data, or design change afterwards makes the approval stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VisualReview {
    pub state: VisualReviewState,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub note: Option<String>,
    /// Digest of the figure's content, data, and design when it was approved. Set by the engine for `approved` only; a value sent by a
    /// caller is replaced. Absent for every other state, and for an approval written before this field existed (then stale).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub approved_digest: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FigureDocument {
    pub size: Size, pub panels: Vec<FigurePanel>, pub caption: String, pub datasets: Vec<Dataset>, pub evidence: Vec<Evidence>,
    #[serde(default)] pub source_scopes: Vec<SourceCoverage>,
    /// Shared legend of the panels (R07.6). Absent = no legend: the field is then left out of the JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub common_legend: Option<CommonLegend>,
    /// Visual review (R07.8). Absent = never requested: the field is then left out of the JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub visual_review: Option<VisualReview>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FigureLink { pub figure_id: Id, pub figure_revision: Revision, pub target_node_id: Id, pub representation: String, pub render_hash: String }

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TextRange { pub paragraph_id: Id, pub start: u32, pub end: u32 }
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Selection { pub node_ids: Vec<Id>, pub text_range: Option<TextRange> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DependencyRef { pub id: Id, pub revision: Option<Revision>, pub hash: Option<String> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobRef { pub job_id: Id, pub attempt_id: Id }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChangeSet {
    pub request_id: Id, pub actor: String, pub workspace_id: Id, pub artifact_id: Id,
    pub base_revision: Revision, pub allowed_intents: BTreeSet<Intent>,
    #[serde(default)] pub job_ref: Option<JobRef>,
    #[serde(default)] pub read_dependencies: Vec<DependencyRef>,
    pub operations: Vec<Operation>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    ReplaceText { range: TextRange, text: String },
    SetText { node_id: Id, text: String },
    SetStyle { node_ids: Vec<Id>, patch: StylePatch },
    SetTextStyleRange { range: TextRange, patch: StylePatch },
    SetNamedStyle { name: String, patch: StylePatch, clear_overrides: bool },
    SetGeometry { node_id: Id, bounds: Rect },
    AddNode { page_id: Id, parent_id: Option<Id>, node: Node },
    RemoveNode { node_id: Id },
    SetParagraphFormat { paragraph_id: Id, format: ParagraphFormat },
    SetPageSetup { section_id: Id, setup: PageSetup },
    SetTableDimensions { table_id: Id, column_widths: Vec<i64> },
    InsertFlowBlock { section_id: Id, index: u32, block: FlowBlock },
    InsertPage { index: u32, page: Page },
    RemovePage { page_id: Id },
    ReorderPages { page_ids: Vec<Id> },
    SetPageNotes { page_id: Id, notes: String },
    /// Replaces one page's complete board-memo list from an expected snapshot.
    SetCanvasBoardMemos { page_id: Id, expected: Vec<CanvasBoardMemo>, memos: Vec<CanvasBoardMemo> },
    /// Replaces the full Canvas page-section snapshot. Membership is derived from each start page.
    SetCanvasPageSections { expected: Vec<CanvasPageSection>, sections: Vec<CanvasPageSection> },
    /// Replaces or removes one page's complete guide settings from an expected snapshot.
    SetCanvasPageGuides { page_id: Id, expected: Option<CanvasPageGuides>, guides: Option<CanvasPageGuides> },
    ConnectDiagram { panel_id: Id, edge: Edge },
    AddDiagramNode { panel_id: Id, node: Node, evidence_ids: Vec<Id>, origin: String },
    AddPanel { index: u32, panel: FigurePanel },
    RemovePanel { panel_id: Id },
    ReorderPanels { panel_ids: Vec<Id> },
    SetCaption { caption: String },
    PutEvidence { evidence: Evidence },
    /// Replaces a plot's definition (data binding, columns, axes, error). `plot.display` must equal the plot's current
    /// mode; a different mode is refused (ContentLocked). Change the mode with `SetPlotDisplay`.
    SetPlot { panel_id: Id, plot: PlotSpec },
    /// Changes only a plot's display mode (PlotSpec.display). `expected_display` must equal the mode the plot has now;
    /// a stale value is refused (RevisionConflict) and nothing changes. Design intent: the mode is outside the content
    /// and data digests.
    SetPlotDisplay {
        panel_id: Id,
        #[serde(deserialize_with = "required_nullable")] #[schemars(schema_with = "nullable_schema::<PlotDisplayMode>")]
        expected_display: Option<PlotDisplayMode>,
        #[serde(deserialize_with = "required_nullable")] #[schemars(schema_with = "nullable_schema::<PlotDisplayMode>")]
        display: Option<PlotDisplayMode>,
    },
    /// Sets the provenance of one diagram element (a node or a connector `element_id` of diagram panel `panel_id`): its
    /// origin and evidence references. `expected_origin` and `expected_evidence_ids` must equal the current values, or the
    /// change is refused with RevisionConflict. A node may have no origin (`origin: None` clears it); a connector always
    /// has one. An empty `evidence_ids` clears the references. Intent: content.
    SetDiagramProvenance {
        panel_id: Id, element_id: Id,
        #[serde(deserialize_with = "required_nullable")] #[schemars(schema_with = "nullable_schema::<String>")]
        expected_origin: Option<String>,
        expected_evidence_ids: Vec<Id>,
        #[serde(deserialize_with = "required_nullable")] #[schemars(schema_with = "nullable_schema::<String>")]
        origin: Option<String>,
        evidence_ids: Vec<Id>,
    },
    /// Replaces or removes (`legend: None`) the common legend of a figure. `expected` must equal the current legend
    /// (RevisionConflict otherwise). Intent: content, for all of the legend (labels, colours, placement and title).
    SetCommonLegend {
        #[serde(deserialize_with = "required_nullable")] #[schemars(schema_with = "nullable_schema::<CommonLegend>")]
        expected: Option<CommonLegend>,
        #[serde(deserialize_with = "required_nullable")] #[schemars(schema_with = "nullable_schema::<CommonLegend>")]
        legend: Option<CommonLegend>,
    },
    /// Replaces or removes (`review: None`) the visual review of a figure. `expected` must equal the current review.
    /// Intent: content. The review is outside both digests, so the change alters neither the content nor the data.
    SetFigureVisualReview {
        #[serde(deserialize_with = "required_nullable")] #[schemars(schema_with = "nullable_schema::<VisualReview>")]
        expected: Option<VisualReview>,
        #[serde(deserialize_with = "required_nullable")] #[schemars(schema_with = "nullable_schema::<VisualReview>")]
        review: Option<VisualReview>,
    },
    /// Sets the crop window and the rotation of an image panel. The asset hash, the current crop and the current rotation
    /// must match, or the change is refused (RevisionConflict). Intent: design (outside both digests).
    SetFigureImageTransform {
        panel_id: Id, expected_asset_sha256: String,
        #[serde(deserialize_with = "required_nullable")] #[schemars(schema_with = "nullable_schema::<PixelRect>")]
        expected_crop_pixel_rect: Option<PixelRect>,
        expected_rotation_millidegrees: i32,
        #[serde(deserialize_with = "required_nullable")] #[schemars(schema_with = "nullable_schema::<PixelRect>")]
        crop_pixel_rect: Option<PixelRect>,
        rotation_millidegrees: i32,
    },
    /// Sets or removes (`calibration: None`) the measurement calibration of an image panel. The asset hash and the current
    /// calibration must match. Intent: data (the calibration is in the data digest).
    SetFigureImageCalibration {
        panel_id: Id, expected_asset_sha256: String,
        #[serde(deserialize_with = "required_nullable")] #[schemars(schema_with = "nullable_schema::<Calibration>")]
        expected_calibration: Option<Calibration>,
        #[serde(deserialize_with = "required_nullable")] #[schemars(schema_with = "nullable_schema::<Calibration>")]
        calibration: Option<Calibration>,
    },
    SetAxisLabel { panel_id: Id, axis: String, label: String },
    SetAxisTicks {
        panel_id: Id, axis: String,
        #[serde(deserialize_with = "required_nullable")] #[schemars(schema_with = "nullable_schema::<String>")]
        tick_step: Option<String>,
    },
    PutDataset { dataset: Dataset },
    ArrangePanels { bounds: BTreeMap<Id, Rect> },
    SetPanelLabel { panel_id: Id, label: String },
    UpdateFigureLink { link: FigureLink },
    SetLayout { node_id: Id, layout: LayoutSpec },
    SetInteraction { node_id: Id, interaction: Interaction },
    /// Replaces a node's complete ordered interaction list; an empty list clears all interactions.
    SetInteractions { node_id: Id, interactions: Vec<Interaction> },
    SetVariantStyle { node_id: Id, variant: String, patch: StylePatch },
    ReplaceWebSource { path: String, expected_hash: String, content: String },
    MappedWebEdit { path: String, expected_hash: String, mapping_id: String, property: String, value: String },
    /// v1.2: page (artboard) size.
    SetPageSize { page_id: Id, size: Size },
    /// v1.2: moves a node to `index` among the children of `parent_id` (None = the page's top level).
    MoveNode { node_id: Id, page_id: Id, parent_id: Option<Id>, index: u32 },
    /// v1.2: order among the node's current siblings. Intent content (P1 decision): sibling order is
    /// part of the content digest and the reading order of stacks.
    SetZOrder { node_id: Id, index: u32 },
    /// v1.2: NodeExt appearance fields; None = unchanged.
    SetNodeAppearance { node_ids: Vec<Id>, image_fit: Option<ImageFit>, locked: Option<bool> },
    /// Non-destructively sets or clears a Canvas image crop in source pixels; asset hash guards stale crop edits.
    SetCanvasImageCrop { node_id: Id, expected_asset_sha256: String, crop_pixel_rect: Option<PixelRect> },
    /// Replaces a Canvas Path node's path (commands and view box). The current path must equal `expected`, or the change is
    /// refused (RevisionConflict). Intent: content.
    SetNodePath { node_id: Id, expected: PathSpec, path: PathSpec },
    /// v1.2: NodeExt information fields; None = unchanged.
    SetNodeInfo { node_ids: Vec<Id>, alt: Option<String>, name: Option<String>, hidden: Option<bool> },
    /// Replace a Canvas image's asset only if its current content hash still matches.
    ReplaceImageAsset { node_id: Id, expected_asset_sha256: String, asset_sha256: String },
    /// Canvas text list marker; `None` clears the list formatting from this text node.
    SetNodeList { node_id: Id, list: Option<TextListKind> },
    /// Replaces Canvas table row/cell structure from an exact expected snapshot. Cell AddNode/RemoveNode
    /// operations in the same ChangeSet must make the ordinary Rectangle children match the new structure.
    SetCanvasTableStructure { table_id: Id, expected: CanvasTable, table: CanvasTable },
    /// Changes a table's relative column weights; None restores equal-width tracks.
    SetCanvasTableColumnWidths {
        table_id: Id,
        #[schemars(inner(range(min = 1))) ] expected: Option<Vec<u32>>,
        #[schemars(inner(range(min = 1))) ] column_widths: Option<Vec<u32>>,
    },
    /// v1.2: `name` None clears the page name.
    RenamePage { page_id: Id, name: Option<String> },
    /// Marks a Canvas page as a component source, or unmarks it when role is None.
    SetPageRole { page_id: Id, role: Option<PageRole> },
    /// Creates a materialized Group instance of a component page.
    InstantiateComponent { page_id: Id, parent_id: Option<Id>, instance_id: Id, component_page_id: Id, bounds: Rect },
    /// Detaches an instance while keeping its current materialized child nodes.
    DetachInstance { instance_id: Id },
    /// Adds or replaces a local Canvas token and updates every binding to that token.
    PutToken { token_id: String, name: String, values: TokenValues },
    /// Removes an unused local Canvas token.
    RemoveToken { token_id: String },
    /// Switches the document's active local token mode; absent mode means light.
    SetTokenMode { mode: TokenMode },
    /// Binds the listed nodes to one local token target and applies its active concrete value.
    BindToken { node_ids: Vec<Id>, target: BindTarget, token: TokenRef },
    /// Removes a binding while leaving its current concrete value in place.
    UnbindToken { node_ids: Vec<Id>, target: BindTarget },
    /// Sets or removes (`repeat: None`) the repeat spec of an empty Canvas Group (D21). `expected` must equal the
    /// current spec, or the change is refused (RevisionConflict). Intent: content.
    SetGroupRepeat { node_id: Id, expected: Option<RepeatSpec>, repeat: Option<RepeatSpec> },
    /// Sets or removes (`condition: None`) the show condition of a Group in a repeat component source (D21). `expected`
    /// must equal the current condition, or the change is refused (RevisionConflict). Intent: content.
    SetGroupCondition { node_id: Id, expected: Option<ShowCondition>, condition: Option<ShowCondition> },
    /// Sets theme colours and fonts of a slides deck (the theme of its slide master). Values not in `patch` keep their
    /// current value; an empty patch is refused. Slides only; other engines refuse it. Intent: design.
    SetTheme { patch: ThemePatch },
}
impl Operation {
    /// Every variant is listed (no wildcard): a new operation must choose its intent.
    pub fn intent(&self) -> Intent {
        match self {
            Self::SetStyle { .. } | Self::SetTextStyleRange { .. } | Self::SetNamedStyle { .. } | Self::SetGeometry { .. }
            | Self::SetParagraphFormat { .. } | Self::SetPageSetup { .. } | Self::SetTableDimensions { .. }
            | Self::ArrangePanels { .. } | Self::SetLayout { .. } | Self::SetAxisTicks { .. } | Self::SetVariantStyle { .. }
            | Self::SetPageSize { .. } | Self::SetNodeAppearance { .. } | Self::SetCanvasImageCrop { .. }
            | Self::SetCanvasTableColumnWidths { .. } | Self::SetPlotDisplay { .. } | Self::SetCanvasPageGuides { .. }
            | Self::SetFigureImageTransform { .. } | Self::SetTheme { .. } => Intent::Design,
            Self::PutToken { .. } | Self::RemoveToken { .. } | Self::SetTokenMode { .. }
            | Self::BindToken { .. } | Self::UnbindToken { .. } => Intent::Design,
            Self::SetPlot { .. } | Self::PutDataset { .. } | Self::SetFigureImageCalibration { .. } => Intent::Data,
            Self::ReplaceText { .. } | Self::SetText { .. } | Self::AddNode { .. } | Self::RemoveNode { .. } | Self::InsertFlowBlock { .. }
            | Self::InsertPage { .. } | Self::RemovePage { .. } | Self::ReorderPages { .. } | Self::SetPageNotes { .. }
            | Self::SetCanvasBoardMemos { .. }
            | Self::SetCanvasPageSections { .. }
            | Self::ConnectDiagram { .. } | Self::AddDiagramNode { .. } | Self::AddPanel { .. } | Self::RemovePanel { .. }
            | Self::ReorderPanels { .. } | Self::SetCaption { .. } | Self::PutEvidence { .. } | Self::SetAxisLabel { .. }
            | Self::SetPanelLabel { .. } | Self::UpdateFigureLink { .. } | Self::SetInteraction { .. } | Self::SetInteractions { .. } | Self::ReplaceWebSource { .. }
            | Self::MappedWebEdit { .. } | Self::MoveNode { .. } | Self::SetZOrder { .. } | Self::SetNodeInfo { .. } | Self::ReplaceImageAsset { .. } | Self::SetNodeList { .. }
            | Self::SetCanvasTableStructure { .. } | Self::RenamePage { .. } | Self::SetPageRole { .. }
            | Self::InstantiateComponent { .. } | Self::DetachInstance { .. }
            | Self::SetDiagramProvenance { .. } | Self::SetCommonLegend { .. } | Self::SetFigureVisualReview { .. }
            | Self::SetNodePath { .. } | Self::SetGroupRepeat { .. } | Self::SetGroupCondition { .. } => Intent::Content,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChangeGroupMember { pub artifact_id: Id, pub revision: Revision, pub changed_ids: Vec<Id> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChangeGroup { pub group_id: Id, pub members: Vec<ChangeGroupMember> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChangeResult {
    pub request_id: Id, pub artifact_id: Id, pub revision: Revision, pub changed_ids: Vec<Id>, pub diagnostics: Vec<Diagnostic>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub group: Option<ChangeGroup>,
}
/// Publishes or unpublishes the Canvas artifact as a workspace-shared design system.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DesignSystemPublishRequest {
    pub request_id: Id, pub actor: String, pub workspace_id: Id, pub artifact_id: Id,
    pub expected_revision: Revision, pub publish: bool, pub allowed_intents: BTreeSet<Intent>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DesignSystemSummary { pub artifact_id: Id, pub title: String, pub revision: Revision }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DesignSystemListResult { pub systems: Vec<DesignSystemSummary> }
/// Links or detaches a consumer from a published design system. `design_system: null` detaches.
/// `direct_formatting` says what a link does with the consumer's direct formatting; a consumer other
/// than Canvas or WebCode is refused without it, and a detach takes none.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DesignSystemSetRequest {
    pub request_id: Id, pub actor: String, pub workspace_id: Id, pub artifact_id: Id,
    pub expected_revision: Revision, #[serde(default)] pub design_system: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub expected_system_revision: Option<Revision>,
    pub allowed_intents: BTreeSet<Intent>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub direct_formatting: Option<design_policy::DirectFormattingPolicy>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HistoryRequest { pub request_id: Id, pub actor: String, pub workspace_id: Id, pub artifact_id: Id, pub expected_revision: Revision, pub allowed_intents: BTreeSet<Intent>, pub job_ref: Option<JobRef> }
/// P1 `history.undo_change`: takes back one committed change batch. `change_id` is that change's
/// `request_id` (ChangeResult.request_id, or `request_id` of its `artifact.changed` event). Refused
/// when a later change that is still in effect changed the same ids.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UndoChangeRequest { pub request_id: Id, pub actor: String, pub workspace_id: Id, pub artifact_id: Id, pub expected_revision: Revision, pub change_id: Id, pub allowed_intents: BTreeSet<Intent>, #[serde(default)] pub job_ref: Option<JobRef> }
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContentDigest { pub content: String, pub data: String }
#[derive(Debug, Clone)]
pub struct PreparedChange { pub artifact: Artifact, pub changed_ids: Vec<Id>, pub diagnostics: Vec<Diagnostic> }

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CreateRequest { pub kind: ArtifactKind, pub title: String, pub size: Option<Size>, pub template: Option<String> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CreateArtifactRequest { pub request_id: Id, pub actor: String, #[serde(flatten)] pub options: CreateRequest }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OpenInput { pub kind: ArtifactKind, pub title: String, pub asset: AssetRef }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OpenRequest { pub request_id: Id, pub actor: String, pub path: String, pub kind: Option<ArtifactKind> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SaveRequest {
    pub request_id: Id, pub artifact_id: Id, pub expected_revision: Revision,
    /// Absolute destination. Omit in a project to save in its artifacts folder.
    #[serde(default)] pub target_path: String,
    #[serde(default)] pub overwrite: bool, pub expected_target_hash: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SaveState { pub artifact_id: Id, pub workspace_revision: Revision, pub saved_revision: Option<Revision>, pub target_path: Option<String>, pub target_hash: Option<String>, pub dirty: bool, pub externally_modified: bool }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Editability { None, Vector, Objects, NativeData }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RenderProfile { pub dpi: u32, pub transparent: bool }
impl Default for RenderProfile { fn default() -> Self { Self { dpi: 144, transparent: false } } }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExportRequest {
    pub artifact_id: Id, pub expected_revision: Revision, pub format: String, pub required_editability: Editability, #[serde(default)] pub allowed_losses: Vec<String>, #[serde(default)] pub profile: RenderProfile, #[serde(default)] pub page_indices: Option<Vec<u32>>,
    /// P1 (export selection): only these nodes, the page cropped to their union bounds and every
    /// other node left out. The core passes it only to an engine whose `exports_nodes(format)` is
    /// true and refuses it (UNSUPPORTED_OPERATION, naming the format) otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub node_ids: Option<Vec<Id>>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExportFileRequest {
    pub request_id: Id, pub options: ExportRequest, pub target_path: String, #[serde(default)] pub overwrite: bool, pub expected_target_hash: Option<String>,
    /// P1: `Some(true)` encodes and reports editability and losses without writing a file, storing
    /// a receipt or an event (result: ExportDryRun). `target_path` may be empty; when given it is
    /// checked like a real export. None / Some(false): the normal export.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub dry_run: Option<bool>,
}
/// P1 result of `artifact.export` with `dry_run: true`. `losses` are the blocking diagnostics not
/// in `allowed_losses` plus `editability_below_required`; a real export with the same options
/// writes the file exactly when `losses` is empty.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExportDryRun {
    pub dry_run: bool, pub artifact_id: Id, pub revision: Revision, pub format: String, pub mime: String, pub bytes: u64,
    pub editability: Editability, pub losses: Vec<Diagnostic>, pub diagnostics: Vec<Diagnostic>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub target_path: Option<String>,
}
#[derive(Debug, Clone)]
pub struct EncodedOutput { pub bytes: Vec<u8>, pub mime: String, pub editability: Editability, pub diagnostics: Vec<Diagnostic> }
/// [`RenderProvider::raster_region`]: `width` by `height` pixels, row by row, 4 bytes each (RGBA, alpha premultiplied).
#[derive(Debug, Clone)]
pub struct RasterPixels { pub width: u32, pub height: u32, pub rgba_premultiplied: Vec<u8>, pub diagnostics: Vec<Diagnostic> }
/// `pixels.format` of a `render.pixels` answer: the bytes of [`RasterPixels::rgba_premultiplied`].
pub const RASTER_PIXEL_FORMAT: &str = "rgba8_premultiplied";
/// The most bytes of one `render.pixels` answer: the core refuses a larger render, and a client closes a connection that sends more.
pub const MAX_RASTER_PIXEL_BYTES: usize = 1 << 30;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SourceRegion { pub node_id: Id, pub bounds: Rect, pub logical_range: Option<TextRange>, #[serde(default)] pub grapheme_rects: Vec<Rect> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TextPosition { pub paragraph_id: Id, pub grapheme_index: u32, pub trailing: bool }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HitTestResult { pub node_id: Id, pub position: Option<TextPosition> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Primitive {
    /// One visual line; origin is the aligned baseline in page EMU, shaped glyphs are relative.
    Text { node_id: Id, bounds: Rect, origin: Point, text: String, style: Style, shaped: ShapedText },
    Shape { node_id: Id, bounds: Rect, shape: String, style: Style },
    Line { node_id: Id, from: Point, to: Point, style: Style, arrow: bool },
    Image {
        node_id: Id, bounds: Rect, asset: AssetRef,
        /// P1: how the picture fills `bounds` (None = stretched to bounds, the v1.1 drawing).
        #[serde(default, skip_serializing_if = "Option::is_none")] fit: Option<ImageFit>,
        /// Source-pixel window to draw before applying `fit`; None uses the complete source image.
        #[serde(default, skip_serializing_if = "Option::is_none")] image_crop_pixel_rect: Option<PixelRect>,
        /// P1: opacity, corner radius (as clip) and shadows of the picture; None = none.
        #[serde(default, skip_serializing_if = "Option::is_none")] style: Option<Style>,
    },
    /// v1.2: `path.view_box` is mapped onto `bounds`.
    Path { node_id: Id, bounds: Rect, path: PathSpec, style: Style },
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RenderPage { pub id: Id, pub page_index: u32, pub size: Size, pub primitives: Vec<Primitive>, pub source_map: Vec<SourceRegion> }
/// A clickable node region for formats that write link annotations; bounds use absolute page EMU.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SceneLink {
    pub page_id: Id,
    pub node_id: Id,
    pub bounds: Rect,
    #[serde(deserialize_with = "deserialize_interaction_url")]
    #[schemars(with = "String")]
    pub url: String,
}
impl SceneLink {
    /// Checks an in-memory link, including links built without deserializing JSON.
    pub fn check(&self) -> DltResult<()> {
        if self.node_id.is_empty() { return Err(DltError::invalid("scene link node_id must not be empty")); }
        check_interaction_url(&self.url)?;
        if self.bounds.width < 0 || self.bounds.height < 0 {
            return Err(DltError::invalid(format!("scene link node {} has a negative bounds size", self.node_id)));
        }
        self.bounds.x.checked_add(self.bounds.width).ok_or_else(|| DltError::invalid(format!("scene link node {} bounds x + width overflows", self.node_id)))?;
        self.bounds.y.checked_add(self.bounds.height).ok_or_else(|| DltError::invalid(format!("scene link node {} bounds y + height overflows", self.node_id)))?;
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RenderScene {
    pub pages: Vec<RenderPage>,
    pub font_fingerprint: String,
    pub diagnostics: Vec<Diagnostic>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<SceneLink>,
}
impl RenderScene {
    /// Checks links against the complete scene. An empty list does not add requirements to old scenes.
    pub fn check_links(&self) -> DltResult<()> {
        if self.links.is_empty() { return Ok(()); }
        let mut page_ids = BTreeSet::new();
        for page in &self.pages {
            if !page_ids.insert(page.id.as_str()) {
                return Err(DltError::invalid(format!("render scene has duplicate page id {}", page.id)));
            }
        }
        for link in &self.links {
            link.check()?;
            if !page_ids.contains(link.page_id.as_str()) {
                return Err(DltError::invalid(format!("render link node {} refers to missing page {}", link.node_id, link.page_id)));
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GlyphMetric { pub glyph_id: u32, pub font_key: String, pub position: Point, pub advance_x: i64, pub grapheme_start: u32, pub grapheme_end: u32 }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FontFaceRef { pub key: String, pub file_sha256: String, pub face_index: u32, pub postscript_name: String, pub family: String, pub variations: BTreeMap<String, String> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ShapedText { pub size: Size, pub baseline: i64, pub ascent: i64, pub descent: i64, pub line_gap: i64, pub cap_height: i64, pub glyphs: Vec<GlyphMetric>, pub caret_x: Vec<i64>, pub grapheme_rects: Vec<Rect>, pub fonts_used: Vec<FontFaceRef>, pub missing_glyphs: Vec<String> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RenderSnapshot { pub id: Id, pub artifact_id: Id, pub revision: Revision, pub profile: RenderProfile, pub scene: RenderScene, pub preview: Option<AssetRef>, pub preview_transform: Option<ViewTransform> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RenderRequest {
    pub artifact_id: Id, pub expected_revision: Revision, #[serde(default)] pub page_index: u32, #[serde(default)] pub profile: RenderProfile,
    /// P1: only this page-EMU rectangle, at `profile.dpi` (sharp board tiles, thumbnails). It must
    /// lie inside the page. The snapshot's `preview_transform.crop` is this rectangle, so hit tests
    /// on the region image map to page EMU. None = the whole page.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub region: Option<Rect>,
    /// P1 (GUI selection outlines): the snapshot's `scene.metadata["node_frames"]` holds the
    /// laid-out frame of every node (`{node_id: Rect}`, absolute page EMU) from an engine that
    /// knows them (`EngineAdapter::layout_with_node_frames`). false = the scene of `layout`.
    #[serde(default, skip_serializing_if = "is_false")] pub include_node_frames: bool,
}
fn is_false(value: &bool) -> bool { !*value }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ContextSnapshot {
    pub workspace_id: Id, pub artifact_id: Id, pub revision: Revision, pub view_id: Id, pub selection: Selection, pub visible_page: u32,
    pub selected_properties: Value, pub capabilities: CapabilityReport,
    /// Forward-compatible, string-keyed capabilities that old ContextSnapshot readers may ignore.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")] pub capability_extensions: CapabilityExtensions,
    pub render_snapshot: Option<RenderSnapshot>, pub view_transform: Option<ViewTransform>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
pub struct ScreenPoint { pub x_micropx: i64, pub y_micropx: i64 }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ViewTransform { pub id: Id, pub dpi: u32, pub zoom_milli: u32, pub offset_x_micropx: i64, pub offset_y_micropx: i64, pub crop: Option<Rect> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RenderQuery { pub artifact_id: Id, pub snapshot_id: Id, pub view_id: Option<Id>, pub transform_id: Id, pub page_index: u32, pub query: RenderQueryKind }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RenderQueryKind { HitTest { point: ScreenPoint }, Caret { position: TextPosition }, Selection { range: TextRange } }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SelectionRequest { pub artifact_id: Id, pub expected_revision: Revision, pub view_id: Id, pub selection: Selection, pub visible_page: u32 }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReservationRequest { pub artifact_id: Id, pub expected_revision: Revision, pub view_id: Id, pub owner: String, pub node_ids: Vec<Id> }
pub trait RenderProvider: Send + Sync {
    /// Shape a single visual line. Wrapping/pagination belong to canvas/flow/figure layout.
    fn shape_text(&self, text: &str, style: &Style) -> DltResult<ShapedText>;
    fn measure_text(&self, text: &str, style: &Style) -> DltResult<Size> { Ok(self.shape_text(text, style)?.size) }
    /// `RenderScene.links` are page-EMU click regions. PDF output uses URI-only link annotations for
    /// links on pages it emits; links for other pages are ignored. PNG/SVG may ignore links. A PDF
    /// region carrying links must be refused until the annotations can be clipped to that region.
    fn encode(&self, scene: &RenderScene, format: &str, profile: &RenderProfile, assets: &dyn AssetStore) -> DltResult<EncodedOutput>;
    /// P1 (`RenderRequest.region`): `None` is exactly `encode`. `Some(region)`: like `encode` for the
    /// single page of `scene`, but the output covers only `region` (page EMU, inside the page,
    /// checked by the core): `pixels(region.width, dpi)` by `pixels(region.height, dpi)` pixels,
    /// output pixel (px, py) showing page point (region.x + px * EMU_PER_INCH / dpi,
    /// region.y + py * EMU_PER_INCH / dpi), the whole-page mapping moved by the region origin. The
    /// default refuses a region, so a renderer without region support answers render.get with a
    /// region with UNSUPPORTED_OPERATION.
    fn encode_region(&self, scene: &RenderScene, format: &str, profile: &RenderProfile, region: Option<Rect>, assets: &dyn AssetStore) -> DltResult<EncodedOutput> {
        match region {
            None => self.encode(scene, format, profile, assets),
            Some(_) => Err(DltError::unsupported("this renderer does not render page regions")),
        }
    }
    /// The pixels `encode_region` would write to a PNG, before PNG encoding: for the desktop's previews, which go to a texture
    /// on the same machine (68i). The default refuses; the core then answers only PNG previews.
    fn raster_region(&self, scene: &RenderScene, profile: &RenderProfile, region: Option<Rect>, assets: &dyn AssetStore) -> DltResult<RasterPixels> {
        let _ = (scene, profile, region, assets);
        Err(DltError::unsupported("this renderer does not return raw pixels"))
    }
    /// Fingerprint of available font configuration, not a claim about fonts used in a scene.
    fn font_catalog_fingerprint(&self) -> String;
    fn available_fonts(&self) -> Vec<String>;
    fn hit_test(&self, page: &RenderPage, point: Point) -> DltResult<Option<HitTestResult>>;
    fn caret_rect(&self, page: &RenderPage, position: &TextPosition) -> DltResult<Rect>;
    fn selection_rects(&self, page: &RenderPage, range: &TextRange) -> DltResult<Vec<Rect>>;
}
pub struct EngineContext<'a> { pub assets: &'a dyn AssetStore, pub renderer: &'a dyn RenderProvider }
pub trait EngineAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn kinds(&self) -> Vec<ArtifactKind>;
    fn create(&self, request: &CreateRequest, context: &EngineContext<'_>) -> DltResult<Artifact>;
    fn open(&self, input: &OpenInput, context: &EngineContext<'_>) -> DltResult<Artifact>;
    fn capabilities(&self, artifact: &Artifact) -> CapabilityReport;
    /// Snapshot features this engine understands. Required rather than default-empty so an engine
    /// cannot accidentally advertise newer persisted fields as safe to an older core.
    fn supported_persisted_features(&self) -> BTreeSet<String>;
    /// Snapshot features used by this Artifact, including references stored at Artifact level.
    /// Return an error if this engine cannot classify a value; do not silently omit it.
    fn persisted_features(&self, artifact: &Artifact) -> DltResult<BTreeSet<String>>;
    /// Token definitions exposed by a published design system. Default: unsupported.
    fn design_tokens(&self, artifact: &Artifact) -> DltResult<BTreeMap<String, TokenDef>> {
        let _ = artifact;
        Err(DltError::unsupported("this engine does not publish design-system tokens"))
    }
    /// Read-only validation that a consumer snapshot's resolved values match its pinned shared tokens.
    /// Core uses this before creating a new synchronized candidate; default engines refuse.
    fn validate_shared_design_system_snapshot(
        &self,
        artifact: &Artifact,
        tokens: &BTreeMap<String, TokenDef>,
    ) -> DltResult<()> {
        let _ = (artifact, tokens);
        Err(DltError::unsupported("this engine does not validate shared design-system snapshots"))
    }
    /// Synchronizes shared token references in a Canvas artifact. The optional reference selects
    /// the exact publisher revision; None detaches shared bindings while retaining resolved values.
    /// Implementations return an independent candidate and must not mutate `artifact`. Default: unsupported.
    fn sync_design_system(
        &self,
        artifact: &Artifact,
        tokens: &BTreeMap<String, TokenDef>,
        design_system: Option<&DesignSystemRef>,
    ) -> DltResult<PreparedChange> {
        let _ = (artifact, tokens, design_system);
        Err(DltError::unsupported("this engine does not synchronize shared design-system tokens"))
    }
    /// `sync_design_system` with the direct-formatting policy stated; the same contract applies.
    /// `Keep` calls `sync_design_system`, so an engine without it stays unsupported. `Clear` is
    /// unsupported unless the engine overrides this method to remove direct formatting.
    fn sync_design_system_with_policy(
        &self,
        artifact: &Artifact,
        tokens: &BTreeMap<String, TokenDef>,
        design_system: Option<&DesignSystemRef>,
        direct_formatting: design_policy::DirectFormattingPolicy,
    ) -> DltResult<PreparedChange> {
        match direct_formatting {
            design_policy::DirectFormattingPolicy::Keep => self.sync_design_system(artifact, tokens, design_system),
            design_policy::DirectFormattingPolicy::Clear => {
                Err(DltError::unsupported("this engine does not clear direct formatting when synchronizing design-system tokens"))
            }
        }
    }
    /// `sync_design_system_with_policy` with the engine context, for engines whose sync reads the
    /// native source (DOCX, HWPX). Core calls this one; the default delegates without the context.
    fn sync_design_system_in_context(
        &self,
        artifact: &Artifact,
        tokens: &BTreeMap<String, TokenDef>,
        design_system: Option<&DesignSystemRef>,
        direct_formatting: design_policy::DirectFormattingPolicy,
        context: &EngineContext<'_>,
    ) -> DltResult<PreparedChange> {
        let _ = context;
        self.sync_design_system_with_policy(artifact, tokens, design_system, direct_formatting)
    }
    /// String-keyed runtime capabilities kept outside the persisted Feature map for forward-compatible discovery.
    /// Canvas-only extensions explicitly report Unsupported on other engines.
    fn capability_extensions(&self, _artifact: &Artifact) -> CapabilityExtensions {
        BTreeMap::from([(
            CAPABILITY_EXTENSION_LISTS.to_string(),
            FeatureSupport {
                read: Support::Unsupported,
                render: Support::Unsupported,
                edit: Support::Unsupported,
                preserve: Support::Unsupported,
                write: Support::Unsupported,
                reason: "Canvas text lists are not supported by this engine".into(),
            },
        ), (
            CAPABILITY_EXTENSION_DESIGN_TOKENS.to_string(),
            FeatureSupport {
                read: Support::Unsupported,
                render: Support::Unsupported,
                edit: Support::Unsupported,
                preserve: Support::Unsupported,
                write: Support::Unsupported,
                reason: "local Canvas design tokens are not supported by this engine".into(),
            },
        )])
    }
    fn inspect(&self, artifact: &Artifact, node_ids: &[Id]) -> DltResult<Value>;
    /// Refuse reads whose original native source is protected or otherwise unsafe to expose.
    /// Core calls this before returning a public snapshot or asking the engine to inspect nodes.
    fn validate_read_source(&self, _artifact: &Artifact, _context: &EngineContext<'_>) -> DltResult<()> { Ok(()) }
    fn prepare_change(&self, artifact: &Artifact, operations: &[Operation], context: &EngineContext<'_>) -> DltResult<PreparedChange>;
    fn validate(&self, artifact: &Artifact, context: &EngineContext<'_>) -> Vec<Diagnostic>;
    fn content_digest(&self, artifact: &Artifact) -> DltResult<ContentDigest>;
    fn layout(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<RenderScene>;
    /// P1 (GUI selection outlines, `RenderRequest.include_node_frames`): `layout` plus
    /// `metadata["node_frames"]` = `{node_id: Rect}` (absolute page EMU) of every laid-out node.
    /// Default: `layout`, without frames.
    fn layout_with_node_frames(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<RenderScene> { self.layout(artifact, context) }
    fn encode_native(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<EncodedOutput>;
    /// Reparse output and compare immutable parts, protected regions, and semantic content.
    fn verify_saved(&self, artifact: &Artifact, bytes: &[u8], context: &EngineContext<'_>) -> DltResult<PreservationVerification>;
    fn export(&self, artifact: &Artifact, request: &ExportRequest, context: &EngineContext<'_>) -> DltResult<EncodedOutput>;
    fn snapshot(&self, artifact: &Artifact) -> DltResult<Vec<u8>> { serde_json::to_vec(artifact).map_err(|e| DltError::invalid(e.to_string())) }
    fn restore(&self, bytes: &[u8]) -> DltResult<Artifact> { serde_json::from_slice(bytes).map_err(|e| DltError::invalid(e.to_string())) }
    /// P1: the templates `create` accepts in `CreateRequest.template` (home tiles,
    /// `templates.list`, `templates.preview`). `create` with one of them must be deterministic in
    /// everything that is drawn. Default: none.
    fn templates(&self) -> Vec<TemplateInfo> { Vec::new() }
    /// P1: true when `export` honors `ExportRequest.node_ids` for `format` (lower-case): only those
    /// nodes (and what they contain) are drawn, the page cropped to their union bounds. Default:
    /// false, so the core refuses node exports for the engine instead of exporting everything.
    fn exports_nodes(&self, format: &str) -> bool { let _ = format; false }
    /// P1 `flow.styles`: the named styles of a flow document (DOCX styles.xml, HWPX/HWP style
    /// tables), in document order. Read-only and not part of the artifact JSON, so `inspect` and
    /// snapshots stay as they are. Default: UNSUPPORTED_OPERATION.
    fn flow_styles(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<Vec<FlowStyleInfo>> {
        let _ = (artifact, context);
        Err(DltError::unsupported("this engine does not list flow styles"))
    }
}

/// P1 `flow.styles`: one named style of a flow document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FlowStyleInfo {
    /// The style id the document uses (DOCX `w:styleId`).
    pub id: String,
    pub kind: FlowStyleKind,
    /// The name people see (DOCX `w:name`), when the document has one.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub display_name: Option<String>,
    /// In the quick style gallery (DOCX `w:qFormat`).
    pub quick: bool,
    /// Hidden from style lists (DOCX `w:semiHidden` or `w:hidden`).
    pub hidden: bool,
    /// One of the application's built-in styles, not defined by the user (DOCX: no `w:customStyle="1"`).
    pub builtin: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlowStyleKind { Paragraph, Character, Table, Numbering }

/// P1: one template an engine creates artifacts from (`EngineAdapter::templates`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TemplateInfo {
    /// The `CreateRequest.template` value.
    pub id: String,
    pub kind: ArtifactKind,
    pub title: String,
    /// `FramePreset.id` of its first page, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub preset: Option<String>,
    /// `CreateRequest.size` to create it with; None = the engine's default.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub size: Option<Size>,
}

/// P1 `templates.preview` params. `kind` only when two kinds share the template id; `preset` (one
/// the template lists) or `size` picks the size it is created at, neither = the engine's default.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TemplatePreviewRequest {
    #[serde(default)] pub kind: Option<ArtifactKind>,
    pub template: String,
    /// 1..=9600.
    pub dpi: u32,
    #[serde(default)] pub preset: Option<String>,
    #[serde(default)] pub size: Option<Size>,
}
/// P1 `templates.preview` result: page 0 as a stored PNG asset.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TemplatePreview {
    pub kind: ArtifactKind, pub template: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub preset: Option<String>,
    pub dpi: u32, pub asset: AssetRef, pub page_size: Size,
}

/// P1 `clients.list`: one connection to the workspace host. A job connection sees only names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClientInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub connection_id: Option<String>,
    /// gui, bridge or cli.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub role: Option<String>,
    /// MCP `clientInfo.name` (or the name the host knows for the connection).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub client_version: Option<String>,
    /// A job-bound connection: its job and the job's runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub job_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub runtime: Option<String>,
    /// When the host reported the connection, Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub connected_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus { Queued, Running, Validating, Completed, Canceled, Failed, Conflict }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Job {
    pub id: Id, pub attempt_id: Id, pub artifact_id: Id, pub base_revision: Revision, pub status: JobStatus, pub allowed_intents: BTreeSet<Intent>, pub model: String, pub effort: String, pub error: Option<DltError>,
    /// P1: the runtime that runs the job (JobStartRequest.runtime, e.g. "claude", "codex").
    #[serde(default, skip_serializing_if = "String::is_empty")] pub runtime: String,
    /// P1: the nodes the job works on (JobStartRequest.target_ids); empty = the whole artifact.
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub target_ids: Vec<Id>,
    /// P1: MCP `clientInfo.name` of the current attempt's connection (`job.set_client`), once it connected.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub client_name: Option<String>,
    /// P1: how the job delivers its work (JobStartRequest.deliver_as); absent = apply.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub deliver_as: Option<DeliverAs>,
    /// The number of the current attempt (`attempt_id`): 1 for the job's first, one more for each `job.resume`. A client
    /// tells a late answer of an attempt a resume replaced by it, since attempt ids carry no order. Absent from a host that
    /// does not report it.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub attempt_seq: Option<u64>,
}
/// How an AI job delivers its work. `apply` (the default when absent): the job commits with `changes.apply`.
/// `proposal`: the job stores a `proposal.create` draft for the user to preview and apply; core refuses its `changes.apply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeliverAs { Apply, Proposal }
/// Exact Figure revision a user explicitly approved as a source for an AI job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FigureRevisionRef { pub figure_id: Id, pub figure_revision: Revision }
pub const MAX_JOB_FIGURE_REFS: usize = 128;
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobStartRequest {
    pub request_id: Id,
    /// With `create`: empty (the new artifact's id is in the resulting Job), base_revision 0.
    #[serde(default)] pub artifact_id: Id,
    #[serde(default)] pub base_revision: Revision,
    pub allowed_intents: BTreeSet<Intent>, pub runtime: String, pub model: String, pub effort: String, pub prompt: String, #[serde(default)] pub source_refs: Vec<AssetRef>,
    /// Exact Figure revisions approved by the job starter. Asset `source_refs` alone never grants Figure identity access.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 128))]
    pub figure_refs: Vec<FigureRevisionRef>,
    /// P1: nodes of the artifact the job works on (the request's target chips). Each must exist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub target_ids: Vec<Id>,
    /// P1: create the artifact and start the job in one commit (home "create + request"): both
    /// exist afterwards or neither does. `artifact_id` must be empty and `base_revision` 0.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub create: Option<JobCreate>,
    /// P1: `proposal` = deliver the work as a proposal the user previews and applies; absent = apply.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub deliver_as: Option<DeliverAs>,
}
/// The artifact `job.start` creates (the `artifact.create` params without `request_id`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobCreate { pub actor: String, #[serde(flatten)] pub options: CreateRequest }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Proposal { pub id: Id, pub change_set: ChangeSet, pub status: String, pub preview: Option<RenderSnapshot> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RpcRequest { pub protocol_version: u32, pub id: String, pub method: String, #[serde(default)] pub params: Value }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RpcResponse { pub protocol_version: u32, pub id: String, pub result: Option<Value>, pub error: Option<DltError> }
impl RpcResponse {
    pub fn success(id: String, result: Value) -> Self { Self { protocol_version: PROTOCOL_VERSION, id, result: Some(result), error: None } }
    pub fn failure(id: String, error: DltError) -> Self { Self { protocol_version: PROTOCOL_VERSION, id, result: None, error: Some(error) } }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoreEvent { pub protocol_version: u32, pub event_seq: u64, pub artifact_id: Option<Id>, pub revision: Option<Revision>, pub event: String, pub detail: Value }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IpcMessage { Request(RpcRequest), Response(RpcResponse), Event(CoreEvent) }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PresetFamily { Screen, Print, Social, Ad, Slide }
/// The unit a preset is naturally measured in (display only; sizes are EMU).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NaturalUnit { Px, Mm, In, Pt }
/// A frame (page) size preset. Display names belong to the GUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FramePreset { pub id: &'static str, pub family: PresetFamily, pub size: Size, pub natural_unit: NaturalUnit }
const fn css_px(width: i64, height: i64) -> Size { Size { width: width * EMU_PER_CSS_PX, height: height * EMU_PER_CSS_PX } }
const fn preset(id: &'static str, family: PresetFamily, size: Size, natural_unit: NaturalUnit) -> FramePreset { FramePreset { id, family, size, natural_unit } }
/// The one list of frame sizes (canvas/slides creation, page add).
pub const FRAME_PRESETS: &[FramePreset] = &[
    preset("phone", PresetFamily::Screen, css_px(390, 844), NaturalUnit::Px),
    preset("phone_large", PresetFamily::Screen, css_px(430, 932), NaturalUnit::Px),
    preset("tablet", PresetFamily::Screen, css_px(820, 1180), NaturalUnit::Px),
    preset("desktop", PresetFamily::Screen, css_px(1440, 900), NaturalUnit::Px),
    preset("social_square", PresetFamily::Social, css_px(1080, 1080), NaturalUnit::Px),
    preset("social_portrait", PresetFamily::Social, css_px(1080, 1350), NaturalUnit::Px),
    preset("story", PresetFamily::Social, css_px(1080, 1920), NaturalUnit::Px),
    preset("a4", PresetFamily::Print, Size { width: 210 * EMU_PER_MM, height: 297 * EMU_PER_MM }, NaturalUnit::Mm),
    preset("letter", PresetFamily::Print, Size { width: 17 * EMU_PER_INCH / 2, height: 11 * EMU_PER_INCH }, NaturalUnit::In),
    preset("a3", PresetFamily::Print, Size { width: 297 * EMU_PER_MM, height: 420 * EMU_PER_MM }, NaturalUnit::Mm),
    preset("invitation", PresetFamily::Print, Size { width: 5 * EMU_PER_INCH, height: 7 * EMU_PER_INCH }, NaturalUnit::In),
    preset("banner_medium", PresetFamily::Ad, css_px(300, 250), NaturalUnit::Px),
    preset("leaderboard", PresetFamily::Ad, css_px(728, 90), NaturalUnit::Px),
    preset("slide_16_9", PresetFamily::Slide, Size { width: 12_192_000, height: 6_858_000 }, NaturalUnit::Pt),
];
pub fn frame_preset(id: &str) -> Option<&'static FramePreset> { FRAME_PRESETS.iter().find(|p| p.id == id) }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paragraph_format_rejects_unknown_fields() {
        let value = serde_json::json!({"align": "center", "space_before": 1, "space_after": 2, "line_spacing": 3, "indent": 4, "page_break_before": true, "line_spacing_percent": 120});
        let format: ParagraphFormat = serde_json::from_value(value.clone()).unwrap();
        assert_eq!((format.align.as_str(), format.space_before, format.space_after, format.line_spacing, format.indent, format.page_break_before, format.line_spacing_percent), ("center", 1, 2, 3, 4, true, Some(120)));
        let mut invalid = value;
        invalid["unexpected"] = serde_json::json!(1);
        assert!(serde_json::from_value::<ParagraphFormat>(invalid).is_err());
    }
    #[test] fn unit_conversions_and_half_up_are_exact() {
        assert_eq!(pixels(EMU_PER_INCH, 300).unwrap(), 300);
        assert_eq!(pixels(EMU_PER_INCH / 2, 1).unwrap(), 1);
        assert_eq!(pixels(210 * EMU_PER_MM, 300).unwrap(), 2480);
        assert!(pixels(-1, 300).is_err());
        assert!(pixels(1, 0).is_err());
    }
    #[test] fn changes_use_typed_intent_and_unknown_operations_fail() {
        let op: Operation = serde_json::from_value(serde_json::json!({"op":"set_geometry","node_id":"n","bounds":{"x":0,"y":0,"width":36000,"height":36000}})).unwrap();
        assert_eq!(op.intent(), Intent::Design);
        assert!(serde_json::from_value::<Operation>(serde_json::json!({"op":"execute_code","source":"anything"})).is_err());
        assert!(serde_json::from_value::<Operation>(serde_json::json!({"op":"set_text","node_id":"n","text":"x","unexpected":true})).is_err());
        assert_eq!(Operation::PutDataset { dataset: Dataset { id: "d".into(), version_hash: "h".into(), original_asset_hash: "s".into(), origin: "user".into(), columns: vec![], rows: vec![], derived_from: None, transforms: vec![] } }.intent(), Intent::Data);
    }
    #[test] fn canvas_image_crop_is_a_design_operation_with_an_explicit_reset() {
        let crop = PixelRect { x: 4, y: 6, width: 20, height: 30 };
        let set = Operation::SetCanvasImageCrop {
            node_id: "image".into(), expected_asset_sha256: "asset-sha".into(), crop_pixel_rect: Some(crop),
        };
        let reset = Operation::SetCanvasImageCrop {
            node_id: "image".into(), expected_asset_sha256: "asset-sha".into(), crop_pixel_rect: None,
        };
        assert_eq!(set.intent(), Intent::Design);
        assert_eq!(reset.intent(), Intent::Design);
        assert_eq!(serde_json::to_value(&set).unwrap()["crop_pixel_rect"], serde_json::json!({"x":4,"y":6,"width":20,"height":30}));
        assert!(serde_json::to_value(&reset).unwrap()["crop_pixel_rect"].is_null(), "null resets to the full source image");

        let ext = NodeExt { image_crop_pixel_rect: Some(crop), ..NodeExt::default() };
        assert!(ext.set_fields().contains(&"image_crop_pixel_rect"));
        assert_eq!(serde_json::to_value(&NodeExt::default()).unwrap(), serde_json::json!({}), "unset crop adds no legacy node JSON");
        assert_eq!(serde_json::to_value(ext).unwrap()["image_crop_pixel_rect"], serde_json::to_value(crop).unwrap());
    }
    #[test] fn canvas_video_uses_a_distinct_kind_and_an_optional_png_poster_asset() {
        let poster = AssetRef { sha256: "b".repeat(64), mime: "image/png".into(), bytes: 32 };
        let node = Node {
            id: "video".into(), kind: NodeKind::Video, bounds: Rect { x: 0, y: 0, width: 640, height: 360 },
            text: String::new(), style: Style::default(),
            asset: Some(AssetRef { sha256: "a".repeat(64), mime: "video/mp4".into(), bytes: 64 }),
            children: Vec::new(), layout: None, interactions: Vec::new(), variants: BTreeMap::new(),
            ext: NodeExt { video_poster_asset: Some(poster.clone()), ..Default::default() },
        };
        let value = serde_json::to_value(&node).unwrap();
        assert_eq!(value["kind"], "video");
        assert_eq!(value["asset"]["mime"], "video/mp4");
        assert_eq!(value["video_poster_asset"], serde_json::to_value(poster).unwrap());
        let no_poster = NodeExt::default();
        assert!(serde_json::to_value(no_poster).unwrap().get("video_poster_asset").is_none(), "ordinary nodes keep their prior JSON shape");
    }
    #[test] fn numeric_source_text_roundtrips_without_float_conversion() {
        let cell = DataCell { original: "0.10".into(), parsed_decimal: Some("0.1".into()), state: CellState::Valid };
        let encoded = serde_json::to_string(&cell).unwrap();
        assert_eq!(serde_json::from_str::<DataCell>(&encoded).unwrap(), cell);
    }
    #[test] fn request_schema_is_materializable() {
        let schema = schemars::schema_for!(ChangeSet);
        let value = serde_json::to_value(schema).unwrap();
        assert!(value.to_string().contains("allowed_intents"));
        assert!(value.to_string().contains("base_revision"));
    }
    #[test] fn native_rounding_is_not_truncation_and_wire_kind_is_explicit() {
        assert_eq!(quantize(20 * EMU_PER_MM, EMU_PER_TWIP).unwrap(), 1134 * EMU_PER_TWIP);
        assert_eq!(quantize(-20 * EMU_PER_MM, EMU_PER_TWIP).unwrap(), -1134 * EMU_PER_TWIP);
        let msg = IpcMessage::Request(RpcRequest { protocol_version: PROTOCOL_VERSION, id: "r".into(), method: "context.read".into(), params: Value::Null });
        let value = serde_json::to_value(&msg).unwrap();
        assert_eq!(value["kind"], "request");
        assert_eq!(value["method"], "context.read");
        assert!(serde_json::from_value::<IpcMessage>(value).is_ok());
    }
    #[test] fn canonical_hash_does_not_depend_on_struct_declaration_order() {
        #[derive(Serialize)] struct A { z: u32, a: String }
        #[derive(Serialize)] struct B { a: String, z: u32 }
        assert_eq!(json_hash(&A { z: 7, a: "0.10".into() }).unwrap(), json_hash(&B { a: "0.10".into(), z: 7 }).unwrap());
        assert_eq!(canonical_json(&serde_json::json!({"z":1,"a":{"y":2,"b":3}})).unwrap(), "{\"a\":{\"b\":3,\"y\":2},\"z\":1}");
    }
    #[test] fn unicode_version_and_grapheme_scalar_mapping_are_fixed() {
        assert_eq!(unicode_segmentation::UNICODE_VERSION, (16, 0, 0));
        let text = "A한e\u{0301}B";
        assert_eq!(grapheme_to_scalar(text, 3).unwrap(), 4);
        assert_eq!(grapheme_to_byte(text, 4).unwrap(), text.len());
        assert!(grapheme_to_byte(text, 5).is_err());
    }
}
