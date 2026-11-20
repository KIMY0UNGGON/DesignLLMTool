//! 82: a connection without the gui role (a raw bridge or cli connection, with no MCP server in front) is refused an operation
//! hidden from agents, in `changes.*` and `proposal.create` and in the stored change set of `proposal.apply`. This build hides
//! nothing, so the tests tell `review_refusal_with` their own hidden tag and check the same request passes with none hidden.

use std::path::PathBuf;
use std::sync::Arc;

use designllm_bridge::operation_exposure::OPERATION_NOT_EXPOSED;
use designllm_contracts::{
    new_id, AssetStore, DltError, DltResult, EncodedOutput, ErrorCode, HitTestResult, Point, Rect, RenderPage, RenderProfile,
    RenderProvider, RenderScene, ShapedText, Style, TextPosition, TextRange,
};
use designllm_core::Workspace;
use serde_json::{json, Value};

use super::{core_request, review_refusal_with};

// The integration tests' test canvas engine (SetText and SetStyle on one text node), so a proposal can be stored.
#[allow(dead_code)]
#[path = "../tests/support/engine.rs"]
mod engine;

struct NoRenderer;

impl RenderProvider for NoRenderer {
    fn shape_text(&self, _text: &str, _style: &Style) -> DltResult<ShapedText> {
        Err(DltError::unsupported("no renderer in this test"))
    }
    fn encode(&self, _scene: &RenderScene, _format: &str, _profile: &RenderProfile, _assets: &dyn AssetStore) -> DltResult<EncodedOutput> {
        Err(DltError::unsupported("no renderer in this test"))
    }
    fn font_catalog_fingerprint(&self) -> String {
        "host-82-no-renderer".into()
    }
    fn available_fonts(&self) -> Vec<String> {
        Vec::new()
    }
    fn hit_test(&self, _page: &RenderPage, _point: Point) -> DltResult<Option<HitTestResult>> {
        Err(DltError::unsupported("no renderer in this test"))
    }
    fn caret_rect(&self, _page: &RenderPage, _position: &TextPosition) -> DltResult<Rect> {
        Err(DltError::unsupported("no renderer in this test"))
    }
    fn selection_rects(&self, _page: &RenderPage, _range: &TextRange) -> DltResult<Vec<Rect>> {
        Err(DltError::unsupported("no renderer in this test"))
    }
}

fn hide_set_text(tag: &str) -> bool {
    tag == "set_text"
}

fn hide_nothing(_tag: &str) -> bool {
    false
}

/// A workspace with one test canvas artifact (its id is returned).
fn workspace(name: &str) -> (PathBuf, Workspace, String) {
    let root = std::env::temp_dir().join(format!("designllm-host-82-{name}-{}", new_id()));
    std::fs::create_dir_all(&root).expect("the test workspace folder");
    let mut ws = Workspace::open(&root, vec![Arc::new(engine::TestCanvasEngine)], Arc::new(NoRenderer)).expect("the workspace opens");
    let created = ws.dispatch(core_request(
        "artifact.create",
        json!({ "request_id": new_id(), "actor": "user", "kind": "canvas", "title": "Test canvas" }),
    ));
    let artifact = created.result.as_ref().and_then(|r| r["id"].as_str()).map(str::to_owned);
    let artifact = artifact.unwrap_or_else(|| panic!("artifact.create: {:?}", created.error));
    (root, ws, artifact)
}

/// A change set whose second operation is the one the test hides.
fn change_set(ws: &Workspace, artifact: &str) -> Value {
    json!({
        "request_id": new_id(), "actor": "agent", "workspace_id": ws.id(), "artifact_id": artifact, "base_revision": 0,
        "allowed_intents": ["content", "design"],
        "operations": [
            { "op": "set_geometry", "node_id": engine::TITLE_NODE, "bounds": { "x": 0, "y": 0, "width": 1, "height": 1 } },
            { "op": "set_text", "node_id": engine::TITLE_NODE, "text": "x" },
        ],
    })
}

fn assert_not_exposed(error: Option<DltError>, what: &str) {
    let error = error.unwrap_or_else(|| panic!("{what}: not refused"));
    assert_eq!(error.code, ErrorCode::UnsupportedOperation, "{what}: {error:?}");
    assert_eq!(error.diagnostics.len(), 1, "{what}: {error:?}");
    assert_eq!(error.diagnostics[0].code, OPERATION_NOT_EXPOSED, "{what}");
}

fn finish(root: PathBuf, ws: Workspace) {
    drop(ws);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_raw_connection_is_refused_a_hidden_operation_in_changes_and_proposal_create() {
    let (root, mut ws, artifact) = workspace("changes");
    let change_set = change_set(&ws, &artifact);
    let requests = [
        ("changes.apply", change_set.clone()),
        ("changes.validate", change_set.clone()),
        ("proposal.create", json!({ "change_set": change_set })),
    ];
    for (method, params) in requests {
        let request = core_request(method, params);
        assert_not_exposed(review_refusal_with(&mut ws, &request, None, hide_set_text), method);
        assert!(review_refusal_with(&mut ws, &request, None, hide_nothing).is_none(), "{method}: exposed, it reaches the core");
    }
    finish(root, ws);
}

#[test]
fn a_raw_connection_is_refused_proposal_apply_of_a_stored_hidden_operation() {
    let (root, mut ws, artifact) = workspace("proposal");
    // Stored while nothing was hidden (as by the desktop app, or before the operation became hidden).
    let created = ws.dispatch(core_request("proposal.create", json!({ "change_set": change_set(&ws, &artifact) })));
    let proposal_id = created.result.as_ref().and_then(|r| r["id"].as_str()).map(str::to_owned);
    let proposal_id = proposal_id.unwrap_or_else(|| panic!("the proposal is stored: {:?}", created.error));
    let request = core_request("proposal.apply", json!({ "proposal_id": proposal_id }));
    assert_not_exposed(review_refusal_with(&mut ws, &request, None, hide_set_text), "proposal.apply");
    assert!(review_refusal_with(&mut ws, &request, None, hide_nothing).is_none(), "exposed, the stored proposal reaches the core");
    finish(root, ws);
}
