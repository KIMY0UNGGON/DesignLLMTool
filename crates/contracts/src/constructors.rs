//! `new` constructors for contract types. Each takes the fields a caller has to choose: identity, size, and the values
//! that make the object what it is (`TextRun.text`, `PlotSpec.y_columns`, even when empty). Every other field is set to
//! its empty value, `None`, or default, whether serde requires it or not (`Page.nodes`, `FigureDocument.caption`). No
//! `Default` is implemented here, and no field, serde attribute or schema changes.
use crate::{AssetRef, AxisSpec, DatasetRef, Editability, ExportRequest, FigureDocument, FigureImage, Id, Intent, JobStartRequest, Page, PlotSpec, RenderProfile, Revision, Size, Style, TextRun};
use std::collections::BTreeSet;

impl Page {
    /// An ordinary page: no nodes, notes, name or role.
    pub fn new(id: impl Into<Id>, size: Size) -> Self {
        Self { id: id.into(), size, nodes: Vec::new(), notes: String::new(), name: None, role: None }
    }
}

impl TextRun {
    /// Not protected, with the default style.
    pub fn new(id: impl Into<Id>, text: impl Into<String>) -> Self {
        Self { id: id.into(), text: text.into(), style: Style::default(), protected: false }
    }
}

impl FigureDocument {
    /// No panels, caption, datasets, evidence, source scopes, common legend or visual review.
    pub fn new(size: Size) -> Self {
        Self { size, panels: Vec::new(), caption: String::new(), datasets: Vec::new(), evidence: Vec::new(), source_scopes: Vec::new(), common_legend: None, visual_review: None }
    }
}

impl ExportRequest {
    /// No allowed losses, the default render profile, and no page or node selection.
    pub fn new(artifact_id: impl Into<Id>, expected_revision: Revision, format: impl Into<String>, required_editability: Editability) -> Self {
        Self { artifact_id: artifact_id.into(), expected_revision, format: format.into(), required_editability, allowed_losses: Vec::new(), profile: RenderProfile::default(), page_indices: None, node_ids: None }
    }
}

impl PlotSpec {
    /// No error bars and no display mode.
    pub fn new(kind: impl Into<String>, dataset_ref: DatasetRef, x_column: impl Into<String>, y_columns: Vec<String>, x_axis: AxisSpec, y_axis: AxisSpec) -> Self {
        Self { kind: kind.into(), dataset_ref, x_column: x_column.into(), y_columns, x_axis, y_axis, error: None, display: None }
    }
}

impl FigureImage {
    /// No source reference, crop, calibration or rotation.
    pub fn new(asset: AssetRef, origin: impl Into<String>, pixel_width: u32, pixel_height: u32) -> Self {
        Self { asset, origin: origin.into(), source_ref: None, pixel_width, pixel_height, crop_pixel_rect: None, rotation_millidegrees: 0, calibration: None }
    }
}

impl JobStartRequest {
    /// `artifact_id` and `base_revision` are arguments: an empty id with revision 0 means `create`, so neither can be
    /// defaulted. No source refs, figure refs, target ids or create block; delivered as apply.
    #[allow(clippy::too_many_arguments)]
    pub fn new(request_id: impl Into<Id>, artifact_id: impl Into<Id>, base_revision: Revision, allowed_intents: BTreeSet<Intent>, runtime: impl Into<String>, model: impl Into<String>, effort: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self { request_id: request_id.into(), artifact_id: artifact_id.into(), base_revision, allowed_intents, runtime: runtime.into(), model: model.into(), effort: effort.into(), prompt: prompt.into(), source_refs: Vec::new(), figure_refs: Vec::new(), target_ids: Vec::new(), create: None, deliver_as: None }
    }
}
