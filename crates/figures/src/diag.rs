//! Diagnostic codes and helpers. Codes are stable identifiers for the GUI and LLM clients; messages
//! are for people. A blocking diagnostic refuses the change, the layout, or the output it concerns.

use std::collections::BTreeSet;

use designllm_contracts::{DltError, Diagnostic, ErrorCode, Id, figure_math};

/// Stable diagnostic codes used by the figure engine.
pub mod code {
    // Codes another crate names too (the desktop words them, or canvas reports the same refusal) live in contracts.
    pub use designllm_contracts::diag_codes::{
        PANEL_SIZE_INVALID, PANEL_OUTSIDE_CANVAS, PANEL_OVERLAP, PANEL_TOO_SMALL, DATASET_NOT_FOUND, DATASET_REF_STALE,
        COLUMN_TYPE_INVALID, COLUMN_REQUIRED, DATA_AMBIGUOUS, DATA_MALFORMED, MISSING_VALUE, NO_PLOTTABLE_DATA,
        UNIT_REQUIRED, NODE_KIND_UNSUPPORTED, NODE_SIZE_INVALID, NODE_OUTSIDE_PANEL, EDGE_ZERO_LENGTH,
        SELF_LOOP_UNSUPPORTED, CONNECTOR_CROSSES_NODE, CONNECTOR_CROSSES_LABEL, LABEL_COLLISION, LABEL_OVERFLOW,
        LABEL_OUTSIDE_PANEL, LEGEND_OVERFLOW, COMMON_LEGEND_INVALID, COMMON_LEGEND_OVERFLOW, VISUAL_REVIEW_NOT_APPROVED,
        VISUAL_REVIEW_STALE, EVIDENCE_REVIEW_STATE_INVALID, EVIDENCE_KIND_INVALID, EVIDENCE_TEXT_EMPTY,
        EVIDENCE_HASH_MISMATCH, EVIDENCE_GEOMETRY_INVALID, EVIDENCE_REGION_OUTSIDE_CROP, EVIDENCE_PAGE_NOT_IN_SCOPE,
        GENERATED_IMAGE_CALIBRATION, IMAGE_ROTATION_INVALID, CALIBRATION_INVALID, RASTER_IMAGE_PANEL, UNSUPPORTED_FIELD
    };
    pub const CANVAS_INVALID: &str = "CANVAS_INVALID";
    pub const ID_INVALID: &str = "ID_INVALID";
    pub const ID_DUPLICATE: &str = "ID_DUPLICATE";
    pub const NATIVE_VERIFY_FAILED: &str = "NATIVE_VERIFY_FAILED";
    pub const ELEMENT_NOT_FOUND: &str = "ELEMENT_NOT_FOUND";

    pub const DATASET_INVALID: &str = "DATASET_INVALID";
    pub const DATASET_ORIGIN_INVALID: &str = "DATASET_ORIGIN_INVALID";
    pub const DATASET_ROW_SHAPE: &str = "DATASET_ROW_SHAPE";
    pub const DATASET_ROWS_MISMATCH: &str = "DATASET_ROWS_MISMATCH";
    pub const DATASET_ROW_ORDER_UNRECORDED: &str = "DATASET_ROW_ORDER_UNRECORDED";
    pub const ERROR_KEY_NOT_DRAWN: &str = "ERROR_KEY_NOT_DRAWN";
    pub const DATASET_SOURCE_HASH_INVALID: &str = "DATASET_SOURCE_HASH_INVALID";
    pub const DATASET_SOURCE_MISMATCH: &str = "DATASET_SOURCE_MISMATCH";
    pub const DATASET_VERSION_MISMATCH: &str = "DATASET_VERSION_MISMATCH";
    pub const DATASET_LINEAGE_REQUIRED: &str = "DATASET_LINEAGE_REQUIRED";
    pub const DATASET_LINEAGE_INVALID: &str = "DATASET_LINEAGE_INVALID";
    pub const DATASET_HISTORY_REWRITTEN: &str = "DATASET_HISTORY_REWRITTEN";
    pub const DATASET_TRANSFORM_MISMATCH: &str = "DATASET_TRANSFORM_MISMATCH";
    /// A scaled value with no exact decimal of at most 18 digits (the service refuses the same scale, see dataset.rs).
    pub const UNIT_SCALE_NOT_EXACT: &str = "UNIT_SCALE_NOT_EXACT";
    pub const DATA_TYPE_INVALID: &str = "DATA_TYPE_INVALID";
    pub const CELL_STATE_MISMATCH: &str = "CELL_STATE_MISMATCH";
    pub const EDIT_MISMATCH: &str = "EDIT_MISMATCH";
    pub const SOURCE_ASSET_MISSING: &str = "SOURCE_ASSET_MISSING";
    pub const COLUMN_NOT_FOUND: &str = "COLUMN_NOT_FOUND";

    pub const X_VALUE_MISSING: &str = "X_VALUE_MISSING";
    pub const CATEGORY_MISSING: &str = "CATEGORY_MISSING";
    pub const DUPLICATE_SERIES_ROW: &str = "DUPLICATE_SERIES_ROW";
    pub const NUMERIC_RANGE: &str = "NUMERIC_RANGE";

    pub const PLOT_KIND_UNSUPPORTED: &str = "PLOT_KIND_UNSUPPORTED";
    pub const AXIS_SCALE_INVALID: &str = "AXIS_SCALE_INVALID";
    pub const AXIS_NOT_APPLICABLE: &str = "AXIS_NOT_APPLICABLE";
    pub const AXIS_RANGE_PARTIAL: &str = "AXIS_RANGE_PARTIAL";
    pub const AXIS_RANGE_INVALID: &str = "AXIS_RANGE_INVALID";
    pub const TICK_STEP_INVALID: &str = "TICK_STEP_INVALID";
    pub const TICK_STEP_NOT_APPLICABLE: &str = "TICK_STEP_NOT_APPLICABLE";
    pub const TICK_LIMIT: &str = "TICK_LIMIT";
    pub const LOG_AXIS_MIN_NOT_POSITIVE: &str = "LOG_AXIS_MIN_NOT_POSITIVE";
    pub const LOG_DOMAIN_VALUE: &str = "LOG_DOMAIN_VALUE";
    pub const LOG_DOMAIN_ERROR_BOUND: &str = "LOG_DOMAIN_ERROR_BOUND";
    pub const VALUE_OUTSIDE_AXIS_RANGE: &str = "VALUE_OUTSIDE_AXIS_RANGE";
    pub const ERROR_SERIES_AMBIGUOUS: &str = "ERROR_SERIES_AMBIGUOUS";
    pub const ERROR_KIND_VALUES_MISMATCH: &str = "ERROR_KIND_VALUES_MISMATCH";
    pub const ERROR_MISSING: &str = "ERROR_MISSING";
    pub const NEGATIVE_ERROR: &str = "NEGATIVE_ERROR";
    pub const BOUNDS_ORDER: &str = "BOUNDS_ORDER";
    pub const CENTER_OUTSIDE_BOUNDS: &str = "CENTER_OUTSIDE_BOUNDS";
    pub const CI_LEVEL_REQUIRED: &str = "CI_LEVEL_REQUIRED";
    pub const CI_LEVEL_INVALID: &str = "CI_LEVEL_INVALID";
    pub const CI_LEVEL_NOT_APPLICABLE: &str = "CI_LEVEL_NOT_APPLICABLE";
    pub const N_REQUIRED: &str = "N_REQUIRED";
    pub const N_INVALID: &str = "N_INVALID";
    pub const N_NOT_PROVIDED: &str = "N_NOT_PROVIDED";

    pub const EDGE_ENDPOINT_NOT_FOUND: &str = "EDGE_ENDPOINT_NOT_FOUND";
    pub const ORIGIN_INVALID: &str = "ORIGIN_INVALID";
    pub const EVIDENCE_NOT_FOUND: &str = "EVIDENCE_NOT_FOUND";
    pub const EVIDENCE_REQUIRED: &str = "EVIDENCE_REQUIRED";
    pub const EVIDENCE_MAP_KEY_UNKNOWN: &str = "EVIDENCE_MAP_KEY_UNKNOWN";
    pub const UNSOURCED_PROPOSAL: &str = "UNSOURCED_PROPOSAL";
    pub const EVIDENCE_TABLE_REF_INVALID: &str = "EVIDENCE_TABLE_REF_INVALID";

    pub const IMAGE_ASSET_MISSING: &str = "IMAGE_ASSET_MISSING";
    pub const IMAGE_HASH_MISMATCH: &str = "IMAGE_HASH_MISMATCH";
    pub const IMAGE_FORMAT_UNSUPPORTED: &str = "IMAGE_FORMAT_UNSUPPORTED";
    pub const IMAGE_SIZE_MISMATCH: &str = "IMAGE_SIZE_MISMATCH";
    pub const IMAGE_ORIGIN_INVALID: &str = "IMAGE_ORIGIN_INVALID";
    pub const IMAGE_SOURCE_REF_MISSING: &str = "IMAGE_SOURCE_REF_MISSING";
    pub const IMAGE_CROP_INVALID: &str = "IMAGE_CROP_INVALID";
    pub const IMAGE_CROP_NOT_APPLIED: &str = "IMAGE_CROP_NOT_APPLIED";
    pub const IMAGE_ROTATION_NOT_APPLIED: &str = "IMAGE_ROTATION_NOT_APPLIED";
    pub const PLOT_EXPORTED_AS_OBJECTS: &str = "PLOT_EXPORTED_AS_OBJECTS";
    /// Export info: the figure's caption is kept in the figure (document, digest, scene metadata) but not drawn in the
    /// exported file, as journals take the legend in the manuscript text. Non-blocking (`05-decisions/figures.md`).
    pub const FIGURE_CAPTION_NOT_RENDERED: &str = "FIGURE_CAPTION_NOT_RENDERED";
    pub const CONTENT_LOCKED: &str = "CONTENT_LOCKED";

    /// A v1.2 operation that the figure engine refuses (see guard.rs).
    pub const UNSUPPORTED_OPERATION: &str = "UNSUPPORTED_OPERATION";
    /// A v1.1 value that a stored figure keeps but does not render or export. Info only (see guard.rs).
    pub const PRESERVED_NOT_RENDERED: &str = "PRESERVED_NOT_RENDERED";
}

/// A blocking diagnostic. It refuses the change, layout, or output it concerns.
pub fn blocking(code: &str, node: Option<&str>, message: impl Into<String>) -> Diagnostic {
    Diagnostic { code: code.to_owned(), message: message.into(), node_id: node.map(str::to_owned), blocking: true, params: Default::default() }
}

/// A non-blocking diagnostic. Reported to the caller, does not refuse the output.
pub fn info(code: &str, node: Option<&str>, message: impl Into<String>) -> Diagnostic {
    Diagnostic { code: code.to_owned(), message: message.into(), node_id: node.map(str::to_owned), blocking: false, params: Default::default() }
}

pub fn has_blocking(diags: &[Diagnostic]) -> bool {
    diags.iter().any(|d| d.blocking)
}

/// Maps a diagnostic code to the closed contract error set. Detailed codes stay in `diagnostics`.
pub fn error_code_for(code: &str) -> ErrorCode {
    match code {
        "DATA_AMBIGUOUS" | "DATA_MALFORMED" => ErrorCode::DataAmbiguous,
        "PLOT_KIND_UNSUPPORTED"
        | "ERROR_SERIES_AMBIGUOUS"
        | "NODE_KIND_UNSUPPORTED"
        | "IMAGE_FORMAT_UNSUPPORTED"
        | "IMAGE_CROP_NOT_APPLIED"
        | "IMAGE_ROTATION_NOT_APPLIED"
        | "TICK_STEP_NOT_APPLICABLE"
        | "AXIS_NOT_APPLICABLE"
        | "CI_LEVEL_NOT_APPLICABLE"
        | "UNSUPPORTED_FIELD"
        | "UNSUPPORTED_OPERATION"
        | figure_math::FIGURE_MATH_UNSUPPORTED => ErrorCode::UnsupportedOperation,
        "SOURCE_ASSET_MISSING"
        | "IMAGE_ASSET_MISSING"
        | "DATASET_NOT_FOUND"
        | "EVIDENCE_NOT_FOUND"
        | "EDGE_ENDPOINT_NOT_FOUND"
        | "COLUMN_NOT_FOUND"
        | "ELEMENT_NOT_FOUND" => ErrorCode::NotFound,
        "DATASET_REF_STALE" => ErrorCode::RevisionConflict,
        "DATASET_SOURCE_MISMATCH"
        | "DATASET_VERSION_MISMATCH"
        | "CELL_STATE_MISMATCH"
        | "DATASET_ROWS_MISMATCH"
        | "DATASET_TRANSFORM_MISMATCH"
        | "EDIT_MISMATCH"
        | "DATASET_HISTORY_REWRITTEN"
        | "IMAGE_HASH_MISMATCH"
        | "EVIDENCE_HASH_MISMATCH" => ErrorCode::IntegrityError,
        "CONTENT_LOCKED" => ErrorCode::ContentLocked,
        // A request the dataset cannot take: a scale that is not exact, or a new id that does not name its parent.
        "UNIT_SCALE_NOT_EXACT" | "DATASET_LINEAGE_REQUIRED" => ErrorCode::InvalidArgument,
        _ => ErrorCode::InvalidArgument,
    }
}

/// Converts blocking diagnostics into one contract error. `diagnostics` carries every diagnostic.
pub fn to_error(diags: &[Diagnostic], message: &str) -> DltError {
    let first = diags.iter().find(|d| d.blocking);
    let code = first.map(|d| error_code_for(&d.code)).unwrap_or(ErrorCode::InvalidArgument);
    let affected: BTreeSet<Id> = diags.iter().filter(|d| d.blocking).filter_map(|d| d.node_id.clone()).collect();
    let detail = first.map(|d| format!("{}: {}", d.code, d.message)).unwrap_or_default();
    DltError {
        code,
        message: format!("{message}: {detail}"),
        current_revision: None,
        affected_ids: affected.into_iter().collect(),
        retryable: false,
        diagnostics: diags.to_vec(),
    }
}
