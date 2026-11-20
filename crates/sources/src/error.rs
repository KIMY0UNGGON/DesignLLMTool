//! Internal PDF error type. Converted to `DltError` at the public boundary so each failure carries a stable diagnostic code.

use designllm_contracts::{Diagnostic, DltError, ErrorCode};

pub(crate) type PResult<T> = Result<T, PdfErr>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PdfErr {
    /// Malformed or truncated input, or an object that does not have the expected type.
    Parse(String),
    /// Valid PDF feature that this extractor does not implement. Never guessed around.
    Unsupported { code: &'static str, message: String },
    /// Invalid request or invalid value inside the document (e.g. a negative UserUnit).
    Invalid { code: &'static str, message: String },
}

impl PdfErr {
    pub(crate) fn unsupported(code: &'static str, message: impl Into<String>) -> Self {
        Self::Unsupported { code, message: message.into() }
    }
    pub(crate) fn invalid(code: &'static str, message: impl Into<String>) -> Self {
        Self::Invalid { code, message: message.into() }
    }
    pub(crate) fn message(&self) -> String {
        match self {
            PdfErr::Parse(m) => format!("PDF parse error: {m}"),
            PdfErr::Unsupported { message, .. } | PdfErr::Invalid { message, .. } => message.clone(),
        }
    }
    pub(crate) fn code(&self) -> &'static str {
        match self {
            PdfErr::Parse(_) => "PDF_PARSE_ERROR",
            PdfErr::Unsupported { code, .. } | PdfErr::Invalid { code, .. } => code,
        }
    }
    pub(crate) fn diagnostic(&self) -> Diagnostic {
        Diagnostic { code: self.code().to_string(), message: self.message(), node_id: None, blocking: true, params: Default::default() }
    }
}

impl From<lopdf::Error> for PdfErr {
    fn from(error: lopdf::Error) -> Self {
        match error {
            lopdf::Error::Decryption(_) | lopdf::Error::AlreadyEncrypted | lopdf::Error::InvalidPassword => {
                PdfErr::unsupported("PDF_ENCRYPTED", format!("encrypted PDF is not supported: {error}"))
            }
            other => PdfErr::Parse(other.to_string()),
        }
    }
}

impl From<PdfErr> for DltError {
    fn from(error: PdfErr) -> Self {
        let code = match &error {
            PdfErr::Unsupported { .. } => ErrorCode::UnsupportedOperation,
            PdfErr::Parse(_) | PdfErr::Invalid { .. } => ErrorCode::InvalidArgument,
        };
        let mut out = DltError::new(code, error.message());
        out.diagnostics.push(error.diagnostic());
        out
    }
}
