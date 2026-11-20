//! Diagnostic codes that more than one crate names: a code several engines report, or a code the desktop words or
//! compares (md RUST 3). The engine that reports a code uses the constant here (its `diag` module re-exports it), and
//! the desktop's words table matches on it. A value is a stable protocol string: changing one is a contract change.

/// A requested font is not available, so the text is drawn with another face. Informational. Reported by flow (and the
/// DOCX, HWPX and HWP engines that lay out through it), slides, the PPTX export and canvas.
pub const FONT_SUBSTITUTED: &str = "FONT_SUBSTITUTED";
/// A requested value is not on the format's native grid; it was rounded and the message names the requested and the
/// stored value. Informational. Reported by the DOCX, HWPX, HWP and PPTX engines and slides.
pub const NATIVE_UNIT_ROUNDING: &str = "NATIVE_UNIT_ROUNDING";
/// Read-only old value: what flow reported as [`FONT_SUBSTITUTED`] before the codes were upper case (task 49f). No
/// engine reports it; a reader maps a stored diagnostic with it as the new code.
pub const LEGACY_FONT_SUBSTITUTED: &str = "font_substituted";
/// Read-only old value: what DOCX, HWPX and HWP reported as [`NATIVE_UNIT_ROUNDING`] before task 49f. No engine reports
/// it; a reader maps a stored diagnostic with it as the new code.
pub const LEGACY_NATIVE_UNIT_ROUNDING: &str = "native_unit_rounding";
/// A theme edit left a source shape as it was (not blocking): its text runs no longer line up with the source, and a
/// run's colour or font cannot be told to come from the theme. The shape keeps its values, saved as set values.
/// Reported by slides (task 49a); the desktop words it.
pub const SLIDES_THEME_KEPT_AS_SET: &str = "SLIDES_THEME_KEPT_AS_SET";
/// A v1.2 field, variant, node kind or value that the canvas or figure engine refuses (blocking, named in the message).
pub const UNSUPPORTED_FIELD: &str = "UNSUPPORTED_FIELD";

// Figure engine (`designllm-figures` `diag::code`).
pub const PANEL_SIZE_INVALID: &str = "PANEL_SIZE_INVALID";
pub const PANEL_OUTSIDE_CANVAS: &str = "PANEL_OUTSIDE_CANVAS";
pub const PANEL_OVERLAP: &str = "PANEL_OVERLAP";
pub const PANEL_TOO_SMALL: &str = "PANEL_TOO_SMALL";
pub const DATASET_NOT_FOUND: &str = "DATASET_NOT_FOUND";
pub const DATASET_REF_STALE: &str = "DATASET_REF_STALE";
pub const COLUMN_TYPE_INVALID: &str = "COLUMN_TYPE_INVALID";
pub const COLUMN_REQUIRED: &str = "COLUMN_REQUIRED";
pub const DATA_AMBIGUOUS: &str = "DATA_AMBIGUOUS";
pub const DATA_MALFORMED: &str = "DATA_MALFORMED";
pub const MISSING_VALUE: &str = "MISSING_VALUE";
pub const NO_PLOTTABLE_DATA: &str = "NO_PLOTTABLE_DATA";
pub const UNIT_REQUIRED: &str = "UNIT_REQUIRED";
pub const NODE_KIND_UNSUPPORTED: &str = "NODE_KIND_UNSUPPORTED";
pub const NODE_SIZE_INVALID: &str = "NODE_SIZE_INVALID";
pub const NODE_OUTSIDE_PANEL: &str = "NODE_OUTSIDE_PANEL";
pub const EDGE_ZERO_LENGTH: &str = "EDGE_ZERO_LENGTH";
pub const SELF_LOOP_UNSUPPORTED: &str = "SELF_LOOP_UNSUPPORTED";
pub const CONNECTOR_CROSSES_NODE: &str = "CONNECTOR_CROSSES_NODE";
pub const CONNECTOR_CROSSES_LABEL: &str = "CONNECTOR_CROSSES_LABEL";
pub const LABEL_COLLISION: &str = "LABEL_COLLISION";
pub const LABEL_OVERFLOW: &str = "LABEL_OVERFLOW";
pub const LABEL_OUTSIDE_PANEL: &str = "LABEL_OUTSIDE_PANEL";
pub const LEGEND_OVERFLOW: &str = "LEGEND_OVERFLOW";
/// The common legend (R07.6) has no items, an item without a label, or a colour that is not #RRGGBB or #RRGGBBAA.
pub const COMMON_LEGEND_INVALID: &str = "COMMON_LEGEND_INVALID";
/// The common legend does not fit in the margin outside the panels, inside the canvas.
pub const COMMON_LEGEND_OVERFLOW: &str = "COMMON_LEGEND_OVERFLOW";
/// The figure has a visual review that is not `approved`. Info at validation; blocking at export (figures engine.rs).
pub const VISUAL_REVIEW_NOT_APPROVED: &str = "VISUAL_REVIEW_NOT_APPROVED";
/// The visual review is `approved`, but the figure's content, data, or design changed after the approval (or the approval has no
/// recorded digest). Treated like a review that is not approved: info at validation, blocking at export.
pub const VISUAL_REVIEW_STALE: &str = "VISUAL_REVIEW_STALE";
pub const EVIDENCE_REVIEW_STATE_INVALID: &str = "EVIDENCE_REVIEW_STATE_INVALID";
pub const EVIDENCE_KIND_INVALID: &str = "EVIDENCE_KIND_INVALID";
pub const EVIDENCE_TEXT_EMPTY: &str = "EVIDENCE_TEXT_EMPTY";
pub const EVIDENCE_HASH_MISMATCH: &str = "EVIDENCE_HASH_MISMATCH";
pub const EVIDENCE_GEOMETRY_INVALID: &str = "EVIDENCE_GEOMETRY_INVALID";
pub const EVIDENCE_REGION_OUTSIDE_CROP: &str = "EVIDENCE_REGION_OUTSIDE_CROP";
pub const EVIDENCE_PAGE_NOT_IN_SCOPE: &str = "EVIDENCE_PAGE_NOT_IN_SCOPE";
pub const GENERATED_IMAGE_CALIBRATION: &str = "GENERATED_IMAGE_CALIBRATION";
pub const IMAGE_ROTATION_INVALID: &str = "IMAGE_ROTATION_INVALID";
pub const CALIBRATION_INVALID: &str = "CALIBRATION_INVALID";
pub const RASTER_IMAGE_PANEL: &str = "RASTER_IMAGE_PANEL";

// DOCX engine (`designllm-docx` `diag`).
/// Text or structure that the engine keeps byte-for-byte but does not edit.
pub const DOCX_PROTECTED_INLINE: &str = "docx.protected_inline";
/// A block-level construct kept as an opaque, non-editable block.
pub const DOCX_PROTECTED_BLOCK: &str = "docx.protected_block";
/// Headers and footers are preserved in the package but not laid out.
pub const DOCX_HEADER_FOOTER_NOT_RENDERED: &str = "docx.header_footer_not_rendered";
/// Floating (anchored) pictures are preserved in the package but not drawn.
pub const DOCX_FLOATING_OBJECT_NOT_RENDERED: &str = "docx.floating_object_not_rendered";
/// An inline picture inside a text paragraph is drawn as a placeholder.
pub const DOCX_INLINE_IMAGE_PLACEHOLDER: &str = "docx.inline_image_placeholder";
/// Refused: the DOCX contains OPC digital-signature markers that the engine cannot validate or renew.
pub const DOCX_DIGITAL_SIGNATURE_UNSUPPORTED: &str = "docx.digital_signature_unsupported";
/// Refused: the DOCX input is an OLE Compound File, not an OPC/ZIP package.
pub const DOCX_COMPOUND_CONTAINER_UNSUPPORTED: &str = "docx.compound_container_unsupported";

// HWPX engine (`designllm-hwpx` `diag`).
/// PrvText.txt was regenerated from the edited text.
pub const HWPX_PREVIEW_TEXT_REGENERATED: &str = "HWPX_PREVIEW_TEXT_REGENERATED";
/// Unmodeled content (floating objects, controls, unknown elements) was kept byte for byte.
pub const HWPX_UNMODELED_PRESERVED: &str = "HWPX_UNMODELED_PRESERVED";
/// Floating objects are not laid out by the engine and are omitted from rendered output.
pub const HWPX_FLOATING_OBJECT_NOT_RENDERED: &str = "HWPX_FLOATING_OBJECT_NOT_RENDERED";
/// Layout is the engine's own line breaking, not Hancom's; it can differ from Hancom.
pub const HWPX_LAYOUT_APPROXIMATE: &str = "HWPX_LAYOUT_APPROXIMATE";
