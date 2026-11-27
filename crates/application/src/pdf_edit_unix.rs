//! Opening a PDF as an editable DOCX copy drives Microsoft Word through COM automation, which exists on Windows only.
//! On macOS and Linux the request is refused with the same diagnostic a Windows PC without Word gets.

use designllm_contracts::{Diagnostic, DltError, DltResult, ErrorCode, PDF_EDIT_WORD_REQUIRED};

pub(crate) fn convert(_pdf: &[u8], _cancel: &designllm_core::PdfCancel) -> DltResult<Vec<u8>> {
    let message = "editing PDFs requires Microsoft Word on Windows; this platform has no Word automation";
    let mut error = DltError::new(ErrorCode::UnsupportedOperation, message);
    error.diagnostics.push(Diagnostic {
        code: PDF_EDIT_WORD_REQUIRED.into(),
        message: message.into(),
        node_id: None,
        blocking: true,
        params: Default::default(),
    });
    Err(error)
}
