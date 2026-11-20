//! Diagnostic codes and constructors for the DOCX engine.
//!
//! Codes are stable strings. Non-blocking codes describe preserved or approximated content;
//! blocking codes mark a save or export that must not be published.

use designllm_contracts::{Diagnostic, Id};

pub use designllm_contracts::diag_codes::NATIVE_UNIT_ROUNDING;
pub use designllm_contracts::diag_codes::DOCX_PROTECTED_INLINE as PROTECTED_INLINE;
pub use designllm_contracts::diag_codes::DOCX_PROTECTED_BLOCK as PROTECTED_BLOCK;
/// A table whose grid or cell spans do not form a regular rectangle; kept as an opaque block.
pub const IRREGULAR_TABLE: &str = "docx.irregular_table";
pub use designllm_contracts::diag_codes::DOCX_HEADER_FOOTER_NOT_RENDERED as HEADER_FOOTER_NOT_RENDERED;
pub use designllm_contracts::diag_codes::DOCX_FLOATING_OBJECT_NOT_RENDERED as FLOATING_OBJECT_NOT_RENDERED;
/// Non-zero coordinates on an inserted inline picture are ignored because text flow positions it.
pub const INLINE_PICTURE_POSITION_IGNORED: &str = "docx.inline_picture_position_ignored";
pub use designllm_contracts::diag_codes::DOCX_INLINE_IMAGE_PLACEHOLDER as INLINE_IMAGE_PLACEHOLDER;
/// Tracked insertion or deletion is preserved; the accepted view is shown.
pub const TRACKED_CHANGE: &str = "docx.tracked_change_preserved";
/// The package holds XML comments or processing instructions. They are kept on an unchanged save; an edit
/// that would drop them is refused with UNSAFE_ROUNDTRIP. Informational.
pub const XML_COMMENT_EDIT_REFUSED: &str = "docx.xml_comment_edit_refused";
/// Refused before any change: a v1.2 field, operation, or variant that this engine does not support yet. The
/// message and this diagnostic name it (docs/P0_CONTRACT_V1_2.md §4).
pub const UNSUPPORTED_FIELD: &str = "docx.unsupported_field";
/// Informational: a style whose `w:type` is not one of the four ST_StyleType values is left out of the style list. The
/// message names the style id and the value.
pub const UNKNOWN_STYLE_TYPE: &str = "docx.unknown_style_type";
pub use designllm_contracts::diag_codes::DOCX_DIGITAL_SIGNATURE_UNSUPPORTED as DIGITAL_SIGNATURE_UNSUPPORTED;
pub use designllm_contracts::diag_codes::DOCX_COMPOUND_CONTAINER_UNSUPPORTED as COMPOUND_CONTAINER_UNSUPPORTED;
/// Refused: an existing picture's model alt text differs from its source description.
pub const PICTURE_ALT_MISMATCH: &str = "docx.picture_alt_mismatch";
/// Refused: the DOCX picture alt-source marker is unknown or malformed.
pub const PICTURE_ALT_SOURCE_UNSUPPORTED: &str = "docx.picture_alt_source_unsupported";
/// Informational: a legacy workspace does not model an imported picture's alt text; its source description stays intact.
pub const LEGACY_PICTURE_ALT_PRESERVED: &str = "docx.legacy_picture_alt_preserved";

/// Blocking: a saved package does not match the model (the byte-level or semantic check failed).
pub const VERIFY_MISMATCH: &str = "docx.verify_mismatch";
/// Blocking: an immutable package part changed.
pub const IMMUTABLE_PART_CHANGED: &str = "docx.immutable_part_changed";
/// Blocking: a protected subtree changed or is missing.
pub const PROTECTED_CHANGED: &str = "docx.protected_subtree_changed";
/// Blocking: a relationship, content type, or package structure is inconsistent.
pub const PACKAGE_STRUCTURE: &str = "docx.package_structure";
/// Blocking: the encoder could not represent part of the model.
pub const ENCODE_BLOCKED: &str = "docx.encode_blocked";

pub fn info(code: &str, message: impl Into<String>, node_id: Option<&Id>) -> Diagnostic {
    Diagnostic { code: code.to_string(), message: message.into(), node_id: node_id.cloned(), blocking: false, params: Default::default() }
}

pub fn blocking(code: &str, message: impl Into<String>, node_id: Option<&Id>) -> Diagnostic {
    Diagnostic { code: code.to_string(), message: message.into(), node_id: node_id.cloned(), blocking: true, params: Default::default() }
}

/// Records a native rounding: the requested value and the value actually stored.
pub fn rounding(field: &str, requested_emu: i64, applied_emu: i64, unit: &str, node_id: Option<&Id>) -> Diagnostic {
    info(
        NATIVE_UNIT_ROUNDING,
        format!("{field}: requested {requested_emu} EMU, applied {applied_emu} EMU (native unit {unit})"),
        node_id,
    )
}
