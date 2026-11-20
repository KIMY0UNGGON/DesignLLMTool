//! DltError construction rules used by the core: retryable defaults and which rejections are
//! deterministic (stored as the request's permanent receipt).
use designllm_contracts::{DltError, Diagnostic, ErrorCode, Id, Revision};

pub fn err(code: ErrorCode, message: impl Into<String>) -> DltError {
    let retryable = matches!(
        code,
        ErrorCode::RevisionConflict | ErrorCode::TargetBusy | ErrorCode::WorkspaceLocked | ErrorCode::SourceChanged
            | ErrorCode::StorageFailure | ErrorCode::StaleRender | ErrorCode::RenderPending | ErrorCode::HostNotRunning
            | ErrorCode::Unavailable
    );
    let mut e = DltError::new(code, message);
    e.retryable = retryable;
    e
}
pub fn invalid(message: impl Into<String>) -> DltError { err(ErrorCode::InvalidArgument, message) }
pub fn not_found(message: impl Into<String>) -> DltError { err(ErrorCode::NotFound, message) }
pub fn integrity(message: impl Into<String>) -> DltError { err(ErrorCode::IntegrityError, message) }
pub fn storage(message: impl Into<String>) -> DltError { err(ErrorCode::StorageFailure, message) }
pub fn unsupported(message: impl Into<String>) -> DltError { err(ErrorCode::UnsupportedOperation, message) }
pub fn not_found_rev(artifact_id: &str, revision: Revision) -> DltError {
    let mut e = not_found(format!("revision {revision} of artifact {artifact_id} not found"));
    e.affected_ids.push(artifact_id.to_string());
    e
}

pub trait ErrExt {
    fn at(self, revision: Revision) -> Self;
    fn ids<I: IntoIterator<Item = S>, S: Into<Id>>(self, ids: I) -> Self;
    fn diags(self, diagnostics: Vec<Diagnostic>) -> Self;
}
impl ErrExt for DltError {
    fn at(mut self, revision: Revision) -> Self { if self.current_revision.is_none() { self.current_revision = Some(revision); } self }
    fn ids<I: IntoIterator<Item = S>, S: Into<Id>>(mut self, ids: I) -> Self { self.affected_ids.extend(ids.into_iter().map(Into::into)); self }
    fn diags(mut self, diagnostics: Vec<Diagnostic>) -> Self { self.diagnostics.extend(diagnostics); self }
}

/// Deterministic outcome of (request content, durable state): stored as receipt and replayed
/// verbatim. Transient/environmental failures are never stored.
pub fn is_recorded(code: &ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::InvalidArgument | ErrorCode::NotFound | ErrorCode::AlreadyExists | ErrorCode::RevisionConflict
            | ErrorCode::UnsupportedOperation | ErrorCode::ContentLocked | ErrorCode::DataAmbiguous | ErrorCode::Canceled
            | ErrorCode::JobNotActive | ErrorCode::UnsafeRoundtrip
    )
}

pub fn sql(e: rusqlite::Error) -> DltError { storage(format!("sqlite: {e}")) }
pub fn io_store(e: std::io::Error) -> DltError { storage(format!("io: {e}")) }
