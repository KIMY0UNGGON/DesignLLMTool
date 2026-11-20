//! Codes of content that a flow engine (DOCX, HWPX, HWP) keeps in the native file but does not draw in visual export
//! (task 11d). docs/CONTRACTS.md lists them. An export reports each as a blocking loss unless the request allows its code.

/// Headers or footers (HWPX `hp:header`/`hp:footer`). DOCX and HWP report theirs with their own older codes.
pub const HEADER_FOOTER_NOT_RENDERED: &str = "header_footer_not_rendered";
/// An automatic page number placed on the page (HWPX `hp:pageNum`, HWP page number position).
pub const PAGE_NUMBER_NOT_RENDERED: &str = "page_number_not_rendered";
/// Footnotes or endnotes (HWPX `hp:footNote`/`hp:endNote`). HWP reports its notes with `hwp_note_not_rendered`.
pub const NOTE_NOT_RENDERED: &str = "note_not_rendered";
/// A visible page border (HWPX and HWP page border fill with a line, DOCX `w:pgBorders`).
pub const PAGE_BORDER_NOT_RENDERED: &str = "page_border_not_rendered";
/// A visible page background (HWPX and HWP page border fill with a fill, DOCX `w:background`).
pub const PAGE_BACKGROUND_NOT_RENDERED: &str = "page_background_not_rendered";
/// A master page (HWPX `hp:masterPage`, HWP section master pages).
pub const MASTER_PAGE_NOT_RENDERED: &str = "master_page_not_rendered";
/// An object that the visual output shows only as a placeholder (HWP drawing, form and unknown objects).
pub const OBJECT_NOT_RENDERED: &str = "object_not_rendered";

/// Losses that belong to every page, or to a page the layout does not know. A page selection keeps them all
/// (`whole_document_codes` of the flow page selection).
pub const WHOLE_DOCUMENT_LOSSES: &[&str] = &[
    HEADER_FOOTER_NOT_RENDERED,
    PAGE_NUMBER_NOT_RENDERED,
    NOTE_NOT_RENDERED,
    PAGE_BORDER_NOT_RENDERED,
    PAGE_BACKGROUND_NOT_RENDERED,
    MASTER_PAGE_NOT_RENDERED,
];

/// Export loss gate: a diagnostic that `is_loss` names stops being blocking when `allowed` (the request's
/// `allowed_losses`) holds its code. The other diagnostics are unchanged.
pub fn release_allowed(diagnostics: &mut [crate::Diagnostic], allowed: &[String], is_loss: impl Fn(&str) -> bool) {
    for diagnostic in diagnostics.iter_mut().filter(|d| d.blocking && is_loss(&d.code)) {
        if allowed.iter().any(|code| *code == diagnostic.code) {
            diagnostic.blocking = false;
        }
    }
}
