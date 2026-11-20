//! Wire vocabulary shared by the host and every client.
//!
//! Every frame is a `designllm_contracts::IpcMessage` (`kind` = request | response | event) that
//! carries `protocol_version`. A peer with another version receives PROTOCOL_MISMATCH.
//! Methods in this module are answered by the host itself; all other methods go to the core.

use designllm_contracts::{
    new_id, CoreEvent, DltError, DltResult, ErrorCode, IpcMessage, RpcRequest, RpcResponse, PROTOCOL_VERSION,
};
use serde_json::{json, Value};

/// Connection handshake. The host merges its own fields into the core's `system.hello` result.
pub const METHOD_HELLO: &str = "system.hello";
/// Stops the host after canceling active jobs. Only gui and cli connections may call it.
pub const METHOD_STOP: &str = "host.stop";
/// Keeps a connection counted as alive. Clients send it periodically.
pub const METHOD_HEARTBEAT: &str = "host.heartbeat";
/// Host status: mode, pid, connection and job counts.
pub const METHOD_STATUS: &str = "host.status";
/// Recent stdout progress lines of a job attempt. Progress text is never a mutation source.
pub const METHOD_JOB_PROGRESS: &str = "host.job_progress";
/// Starts pushing `IpcMessage::Event` frames with `event_seq` greater than `after_seq`.
pub const METHOD_EVENTS_SUBSCRIBE: &str = "events.subscribe";

/// Methods handled by the host. The MCP bridge never exposes them.
pub const HOST_METHODS: &[&str] = &[
    METHOD_HELLO,
    METHOD_STOP,
    METHOD_HEARTBEAT,
    METHOD_STATUS,
    METHOD_JOB_PROGRESS,
    METHOD_EVENTS_SUBSCRIBE,
];

/// Core methods a job-bound connection may call. Must equal `designllm_core::BOUND_JOB_METHODS`;
/// the host integration tests assert that equality. The core enforces the same list.
pub const JOB_SCOPE_METHODS: &[&str] = &[
    "system.hello",
    "workspace.list",
    "artifact.inspect",
    "artifact.validate",
    "artifact.save_state",
    "changes.validate",
    "changes.apply",
    "history.undo",
    "history.redo",
    "render.get",
    "render.hit_test",
    "render.caret_rect",
    "render.selection_rects",
    "asset.read",
    "asset.lookup",
    "context.read",
    "figure.render_link",
    "job.get",
    "proposal.create",
    "proposal.get",
    "proposal.preview",
    "events.poll",
    "source.extract_pdf",
    "source.import_csv",
    "figure.dataset.edit",
    // P1, mirrors designllm_core::BOUND_JOB_METHODS (the host tests compare the two lists by name).
    "history.undo_change",
    "job.set_client",
    "clients.list",
    "flow.styles",
    "templates.list",
    "templates.preview",
];

/// Every core RPC method. Must equal `designllm_core::METHODS`; the host tests assert that every method
/// except `system.hello` has an MCP tool.
pub const CORE_METHODS: &[&str] = &[
    "system.hello",
    "workspace.list",
    "design_system.list",
    "artifact.create",
    "artifact.open",
    "artifact.inspect",
    "artifact.validate",
    "artifact.save",
    "artifact.save_state",
    "artifact.export",
    "changes.validate",
    "changes.apply",
    "history.undo",
    "history.redo",
    "render.get",
    "render.hit_test",
    "render.caret_rect",
    "render.selection_rects",
    "asset.import",
    "asset.read",
    "asset.lookup",
    "selection.set",
    "targets.reserve",
    "targets.release",
    "targets.list",
    "clients.list",
    "templates.list",
    "templates.preview",
    "flow.styles",
    "view.transform.set",
    "context.read",
    "figure.render_link",
    "design_system.publish",
    "design_system.set",
    "job.start",
    "job.get",
    "job.cancel",
    "job.resume",
    "proposal.create",
    "proposal.get",
    "proposal.preview",
    "proposal.apply",
    "events.poll",
    "source.extract_pdf",
    "source.import_csv",
    "web.materialize",
    "figure.dataset.edit",
    // P1: one change batch undone alone, and the agent methods of the GUI view (docs/UI_CHANNEL.md).
    "history.undo_change",
    "ui.state.read",
    "ui.command.list",
    "ui.command.run",
    "ui.command.get",
    "ui.screenshot",
];

/// Client roles sent in `system.hello.client_role`.
pub const CLIENT_ROLES: &[&str] = &["gui", "bridge", "cli"];

/// Builds a request with a fresh UUID id and the current protocol version.
pub fn request(method: &str, params: Value) -> RpcRequest {
    RpcRequest { protocol_version: PROTOCOL_VERSION, id: new_id(), method: method.to_string(), params }
}

/// Serializes one envelope to JSON bytes (the frame payload).
pub fn encode(message: &IpcMessage) -> DltResult<Vec<u8>> {
    serde_json::to_vec(message).map_err(|e| DltError::invalid(format!("cannot encode IpcMessage: {e}")))
}

/// Rejects a peer with another `protocol_version` before the envelope is typed.
pub fn check_version(value: &Value) -> DltResult<()> {
    match value.get("protocol_version").and_then(Value::as_u64) {
        Some(v) if v == u64::from(PROTOCOL_VERSION) => Ok(()),
        Some(v) => Err(mismatch(v)),
        None => Err(DltError::invalid("protocol_version is required")),
    }
}

/// Parses one frame payload into an envelope, checking the version first.
pub fn decode(payload: &[u8]) -> DltResult<IpcMessage> {
    let value: Value =
        serde_json::from_slice(payload).map_err(|e| DltError::invalid(format!("invalid JSON frame: {e}")))?;
    check_version(&value)?;
    serde_json::from_value(value).map_err(|e| DltError::invalid(format!("invalid IpcMessage: {e}")))
}

/// PROTOCOL_MISMATCH for a peer that speaks `peer` instead of this build's version.
pub fn mismatch(peer: u64) -> DltError {
    DltError::new(
        ErrorCode::ProtocolMismatch,
        format!("this build speaks protocol {PROTOCOL_VERSION}; the peer sent {peer}"),
    )
}

/// Error with a fixed code, message, and retry flag.
pub fn error(code: ErrorCode, message: impl Into<String>, retryable: bool) -> DltError {
    let mut e = DltError::new(code, message);
    e.retryable = retryable;
    e
}

/// Successful response envelope.
pub fn response_ok(id: &str, value: Value) -> RpcResponse {
    RpcResponse::success(id.to_string(), value)
}

/// Failed response envelope.
pub fn response_err(id: &str, error: DltError) -> RpcResponse {
    RpcResponse::failure(id.to_string(), error)
}

/// Event envelope with the stable `event_seq` assigned by the core (or host for synthetic events).
pub fn event(event_seq: u64, name: &str, artifact_id: Option<String>, detail: Value) -> CoreEvent {
    CoreEvent {
        protocol_version: PROTOCOL_VERSION,
        event_seq,
        artifact_id,
        revision: None,
        event: name.to_string(),
        detail,
    }
}

/// `{ "job_id": .., "attempt_id": .. }` as carried in hello params.
pub fn job_ref_value(job_id: &str, attempt_id: &str) -> Value {
    json!({ "job_id": job_id, "attempt_id": attempt_id })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_is_kind_tagged_and_versioned() {
        let msg = IpcMessage::Request(request("context.read", json!({"view_id": "v"})));
        let value: Value = serde_json::from_slice(&encode(&msg).unwrap()).unwrap();
        assert_eq!(value["kind"], "request");
        assert_eq!(value["protocol_version"], 1);
        assert_eq!(value["method"], "context.read");
        assert!(value["id"].is_string());
    }

    #[test]
    fn wrong_version_is_protocol_mismatch_not_a_parse_error() {
        let v2 = br#"{"kind":"request","protocol_version":2,"id":"a","method":"x","params":{}}"#;
        let err = decode(v2).unwrap_err();
        assert_eq!(err.code, ErrorCode::ProtocolMismatch);
        let missing = br#"{"kind":"request","id":"a","method":"x"}"#;
        assert_eq!(decode(missing).unwrap_err().code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn events_and_responses_roundtrip_through_decode() {
        let ev = IpcMessage::Event(event(7, "job.status", Some("art".into()), json!({"status": "running"})));
        let bytes = encode(&ev).unwrap();
        match decode(&bytes).unwrap() {
            IpcMessage::Event(e) => {
                assert_eq!(e.event_seq, 7);
                assert_eq!(e.event, "job.status");
            }
            other => panic!("expected event, got {other:?}"),
        }
        let resp = IpcMessage::Response(response_err("r1", error(ErrorCode::HostNotRunning, "no host", true)));
        match decode(&encode(&resp).unwrap()).unwrap() {
            IpcMessage::Response(r) => {
                assert!(r.result.is_none());
                assert_eq!(r.error.unwrap().code, ErrorCode::HostNotRunning);
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[test]
    fn job_scope_list_contains_only_core_bound_methods_shape() {
        assert!(JOB_SCOPE_METHODS.contains(&"changes.apply"));
        assert!(JOB_SCOPE_METHODS.contains(&"figure.render_link"));
        assert!(JOB_SCOPE_METHODS.contains(&"templates.list"));
        assert!(JOB_SCOPE_METHODS.contains(&"templates.preview"));
        assert!(!JOB_SCOPE_METHODS.contains(&"job.start"));
        assert!(!JOB_SCOPE_METHODS.contains(&"host.stop"));
        assert!(!JOB_SCOPE_METHODS.contains(&"artifact.create"));
        for host in HOST_METHODS {
            assert!(!JOB_SCOPE_METHODS.contains(host) || *host == METHOD_HELLO);
        }
    }
}
