//! Review gates open only for the user (`md/05-decisions/figures.md`, "검토 게이트는 사람이 연다"). Only the desktop app's
//! own click may mark evidence as reviewed, approve a figure's visual review, or clear an existing visual review: those
//! states lift the publish block of `EVIDENCE_UNREVIEWED_CITED` and the export block of the visual review. Every other path
//! (an MCP session, a raw bridge or cli connection, an agent's run on the UI channel, a proposal applied by one of them) is
//! refused here. Unreviewed evidence and a pending or needs-revision review are allowed.
//!
//! The path is told by the connection and the run, not by the `actor` string, which is no identity. The judgement lives in
//! this one module: the host (`designllm-host` `conn.rs`), the MCP server (`mcp.rs`) and the desktop UI channel call it.
//! Operations the contract cannot read are left to the core, which refuses them with its own error.
//!
//! An undo, a redo or an undo_change restores a snapshot instead of sending operations. Its review changes are judged as the
//! operations they amount to ([`review_operations`]), so such a path cannot bring back an approval the user took away, nor
//! take one back.

use designllm_contracts::figure_rules::{self, PAPER_EVIDENCE_ORIGIN};
use designllm_contracts::{new_id, Artifact, ArtifactBody, Diagnostic, DltError, ErrorCode, Evidence, Operation, UiState, VisualReviewState};
use serde::Deserialize;
use serde_json::{json, Value};

/// Diagnostic code of the refusal.
pub const REVIEW_USER_ONLY: &str = "REVIEW_USER_ONLY";
/// `Evidence.review_state` of a record a person reviewed.
pub const REVIEWED: &str = figure_rules::REVIEW_STATE_REVIEWED;
/// `Evidence.review_state` of a record a person provided.
pub const USER_PROVIDED: &str = figure_rules::REVIEW_STATE_USER_PROVIDED;
/// The id the figure engine reports for its visual review.
pub const VISUAL_REVIEW_ID: &str = figure_rules::VISUAL_REVIEW_ID;

/// The id of the gate `operation` would open (an evidence id or [`VISUAL_REVIEW_ID`]), or None.
pub fn opened_gate(operation: &Operation) -> Option<String> {
    match operation {
        Operation::PutEvidence { evidence } => {
            let reviewed = evidence.review_state == REVIEWED
                // The publish gate counts only unreviewed PDF records: user_provided would lift it the same way.
                || (evidence.origin == PAPER_EVIDENCE_ORIGIN && evidence.review_state == USER_PROVIDED);
            reviewed.then(|| evidence.id.clone())
        }
        Operation::SetFigureVisualReview { expected, review } => {
            // Approving again, over an approval, is refused too: the engine records the figure's digest of now, so it would
            // move a stale approval onto content the user has not seen.
            let approves = review.as_ref().is_some_and(|r| r.state == VisualReviewState::Approved);
            let clears = review.is_none() && expected.is_some();
            (approves || clears).then(|| VISUAL_REVIEW_ID.to_owned())
        }
        _ => None,
    }
}

/// CONTENT_LOCKED with one diagnostic when any of `operations` would open a review gate, for a path other than the user's.
/// `affected_ids` names each evidence record and the visual review concerned, in operation order, each once.
pub fn operations_refusal<'a>(operations: impl IntoIterator<Item = &'a Operation>) -> Option<DltError> {
    let mut ids: Vec<String> = Vec::new();
    for id in operations.into_iter().filter_map(opened_gate) {
        if !ids.contains(&id) { ids.push(id); }
    }
    if ids.is_empty() { return None; }
    let diagnostic = Diagnostic {
        code: REVIEW_USER_ONLY.to_owned(),
        message: "사용자만 검토를 승인할 수 있다".to_owned(),
        node_id: None,
        blocking: true,
        params: Default::default(),
    };
    let mut error = DltError::new(
        ErrorCode::ContentLocked,
        "only the user can approve a review: this path may not mark evidence reviewed, approve the visual review or clear it; nothing was applied",
    );
    error.affected_ids = ids;
    error.diagnostics.push(diagnostic);
    Some(error)
}

/// [`operations_refusal`] of a ChangeSet given as JSON. Only `put_evidence` and `set_figure_visual_review` entries are read.
pub fn change_set_refusal(change_set: &Value) -> Option<DltError> {
    let operations: Vec<Operation> = change_set
        .get("operations")?
        .as_array()?
        .iter()
        .filter(|value| matches!(value.get("op").and_then(Value::as_str), Some("put_evidence" | "set_figure_visual_review")))
        .filter_map(|value| Operation::deserialize(value).ok())
        .collect();
    operations_refusal(&operations)
}

/// [`change_set_refusal`] of the params of a `changes.apply` or `changes.validate` request. None for any other method.
pub fn agent_refusal(method: &str, arguments: &Value) -> Option<DltError> {
    if !matches!(method, "changes.apply" | "changes.validate") { return None; }
    change_set_refusal(arguments)
}

/// The review operations that moving figure `from` to `to` amounts to: `put_evidence` of each evidence record of `to` that
/// `from` does not hold as it is, and `set_figure_visual_review` when the visual review differs. Empty for any other kind.
pub fn review_operations(from: Artifact, to: Artifact) -> Vec<Operation> {
    let (ArtifactBody::Figure(from), ArtifactBody::Figure(to)) = (from.body, to.body) else { return Vec::new() };
    let held = |evidence: &Evidence| from.evidence.iter().any(|record| record.id == evidence.id && same_record(record, evidence));
    let mut operations: Vec<Operation> =
        to.evidence.into_iter().filter(|evidence| !held(evidence)).map(|evidence| Operation::PutEvidence { evidence }).collect();
    if from.visual_review != to.visual_review {
        operations.push(Operation::SetFigureVisualReview { expected: from.visual_review, review: to.visual_review });
    }
    operations
}

/// Evidence has no equality of its own; two records are the same when they serialize the same.
fn same_record(a: &Evidence, b: &Evidence) -> bool {
    serde_json::to_value(a).ok().is_some_and(|a| serde_json::to_value(b).ok() == Some(a))
}

/// [`operations_refusal`] of an undo, redo or undo_change that would move each artifact from the first of its pair to the
/// second (`designllm_core::Workspace::history_preview`).
pub fn history_refusal(transitions: Vec<(Artifact, Artifact)>) -> Option<DltError> {
    let operations: Vec<Operation> = transitions.into_iter().flat_map(|(from, to)| review_operations(from, to)).collect();
    operations_refusal(&operations)
}

/// The history request the window sends for an agent's `ui.command.run` (params `run`) of `edit.undo`, `edit.redo` or
/// `ai.undo_change` (`designllm-desktop` `apply.rs` `history`, `dispatch.rs` `undo_batch`): the run's actor and allowed
/// intents, on the artifact and revision `view` shows. The window runs it only while it still shows that revision, so the
/// request judged now is the one it would send. None for any other command and for a view without an artifact.
pub fn ui_run_history_call(run: &Value, view: &UiState, workspace_id: &str) -> Option<(&'static str, Value)> {
    let method = match run.get("command_id")?.as_str()? {
        "edit.undo" => "history.undo",
        "edit.redo" => "history.redo",
        "ai.undo_change" => "history.undo_change",
        _ => return None,
    };
    let mut params = json!({
        "request_id": new_id(), "actor": run.get("actor")?, "workspace_id": workspace_id, "artifact_id": view.artifact_id.as_ref()?,
        "expected_revision": view.revision?, "allowed_intents": run.get("allowed_intents").cloned().unwrap_or_else(|| json!([])),
        "job_ref": null,
    });
    if method == "history.undo_change" {
        params["change_id"] = run.get("args").and_then(|args| args.get("change_id")).cloned().unwrap_or_default();
    }
    Some((method, params))
}
