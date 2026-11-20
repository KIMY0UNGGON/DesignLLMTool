//! Diagnostic codes of the HWPX engine. Codes are stable protocol strings.
//!
//! `blocking` diagnostics stop a save (UNSAFE_ROUNDTRIP) or an export that lacks an allowed loss.
//! Informational diagnostics record a real limitation without stopping the operation.
use designllm_contracts::{Diagnostic, DltError, ErrorCode};

/// The package is not a valid HWPX container. Open is refused.
pub const PACKAGE_INVALID: &str = "HWPX_PACKAGE_INVALID";
/// The `mimetype` entry is not first, not stored, or does not read `application/hwp+zip`.
pub const MIMETYPE_INVALID: &str = "HWPX_MIMETYPE_INVALID";
/// Encrypted or digitally signed packages are preserved only by re-opening; they are not editable.
pub const ENCRYPTED_OR_SIGNED: &str = "HWPX_ENCRYPTED_OR_SIGNED";
/// An operation is outside the HWPX support table.
pub const UNSUPPORTED_OPERATION: &str = "HWPX_UNSUPPORTED_OPERATION";
/// A range edit overlaps a protected inline atom (picture, table, field or other object).
pub const PROTECTED_ATOM: &str = "HWPX_PROTECTED_ATOM";
pub use designllm_contracts::diag_codes::NATIVE_UNIT_ROUNDING as UNIT_ROUNDING;
/// Line-segment caches were removed for an edited paragraph and the paragraphs after it.
pub const LINESEG_INVALIDATED: &str = "HWPX_LINESEG_INVALIDATED";
pub use designllm_contracts::diag_codes::HWPX_PREVIEW_TEXT_REGENERATED as PREVIEW_TEXT_REGENERATED;
/// PrvImage.png was removed because it no longer describes the edited document.
pub const PREVIEW_IMAGE_REMOVED: &str = "HWPX_PREVIEW_IMAGE_REMOVED";
pub use designllm_contracts::diag_codes::HWPX_UNMODELED_PRESERVED as UNMODELED_PRESERVED;
pub use designllm_contracts::diag_codes::HWPX_FLOATING_OBJECT_NOT_RENDERED as FLOATING_NOT_RENDERED;
pub use designllm_contracts::diag_codes::HWPX_LAYOUT_APPROXIMATE as LAYOUT_APPROXIMATE;

/// Paragraphs whose AT_LEAST or BETWEEN_LINES line spacing is shown as an approximate exact pitch.
pub const LINE_SPACING_APPROXIMATED: &str = "HWPX_LINE_SPACING_APPROXIMATED";
/// A table grid could not be resolved, so the table is kept as an opaque block.
pub const TABLE_GRID_UNRESOLVED: &str = "HWPX_TABLE_GRID_UNRESOLVED";
/// A nested element that the model cannot represent is kept as an opaque block.
pub const OPAQUE_BLOCK: &str = "HWPX_OPAQUE_BLOCK";

/// Verification codes (blocking when a check fails).
pub const VERIFY_PART_MISSING: &str = "HWPX_VERIFY_PART_MISSING";
pub const VERIFY_PART_CHANGED: &str = "HWPX_VERIFY_PART_CHANGED";
pub const VERIFY_PART_UNEXPECTED: &str = "HWPX_VERIFY_PART_UNEXPECTED";
pub const VERIFY_REGION_MISSING: &str = "HWPX_VERIFY_REGION_MISSING";
pub const VERIFY_REGION_CHANGED: &str = "HWPX_VERIFY_REGION_CHANGED";
pub const VERIFY_REGION_COUNT: &str = "HWPX_VERIFY_REGION_COUNT";
pub const VERIFY_SEMANTIC: &str = "HWPX_VERIFY_SEMANTIC_MISMATCH";
pub const VERIFY_MANIFEST: &str = "HWPX_VERIFY_MANIFEST";
pub const VERIFY_HEADER_COUNT: &str = "HWPX_VERIFY_HEADER_COUNT";
pub const VERIFY_CACHE_POLICY: &str = "HWPX_VERIFY_LINESEG_POLICY";
pub const VERIFY_DERIVED: &str = "HWPX_VERIFY_DERIVED_PREVIEW";
pub const VERIFY_REPARSE: &str = "HWPX_VERIFY_REPARSE_FAILED";
pub const VERIFY_FOREIGN_ATTRIBUTE: &str = "HWPX_VERIFY_FOREIGN_ATTRIBUTE";
pub const VERIFY_FIGURE_LINK: &str = "HWPX_VERIFY_FIGURE_LINK";
/// A text or character-style change would rewrite runs that carry unmodeled markup (refused).
pub const UNSAFE_ROUNDTRIP: &str = "HWPX_UNSAFE_ROUNDTRIP";
/// A figure link update is refused: target, representation, or render asset does not qualify.
pub const FIGURE_LINK: &str = "HWPX_FIGURE_LINK";

/// Export would omit content that the caller did not allow to be lost.
pub const EXPORT_OMITS: &str = "HWPX_EXPORT_OMITS_CONTENT";

/// A non-blocking diagnostic on an optional node.
pub fn info(code: &str, message: impl Into<String>, node_id: Option<String>) -> Diagnostic {
    Diagnostic { code: code.to_string(), message: message.into(), node_id, blocking: false, params: Default::default() }
}

/// A blocking diagnostic on an optional node.
pub fn blocking(code: &str, message: impl Into<String>, node_id: Option<String>) -> Diagnostic {
    Diagnostic { code: code.to_string(), message: message.into(), node_id, blocking: true, params: Default::default() }
}

/// An error whose only diagnostic is the given blocking code.
pub fn error(code: ErrorCode, diagnostic: &str, message: impl Into<String>) -> DltError {
    let message = message.into();
    let mut err = DltError::new(code, message.clone());
    err.diagnostics.push(blocking(diagnostic, message, None));
    err
}

/// Refusal of a text or character-style change to a paragraph whose runs carry unmodeled markup.
pub fn locked_runs(paragraph_id: &str) -> DltError {
    error(
        ErrorCode::UnsafeRoundtrip,
        UNSAFE_ROUNDTRIP,
        format!(
            "paragraph {paragraph_id} has runs or text elements with attributes or namespace declarations that the model does not carry; changing its text or character style would drop them"
        ),
    )
}

/// Shorthand for an UNSUPPORTED_OPERATION error with a HWPX diagnostic.
pub fn unsupported(message: impl Into<String>) -> DltError {
    error(ErrorCode::UnsupportedOperation, UNSUPPORTED_OPERATION, message)
}

/// Shorthand for an INVALID_ARGUMENT error with a HWPX diagnostic.
pub fn invalid(diagnostic: &str, message: impl Into<String>) -> DltError {
    error(ErrorCode::InvalidArgument, diagnostic, message)
}

/// Converts any parse or internal string error into an INVALID_ARGUMENT error.
pub fn malformed(message: impl Into<String>) -> DltError {
    invalid(PACKAGE_INVALID, message)
}
