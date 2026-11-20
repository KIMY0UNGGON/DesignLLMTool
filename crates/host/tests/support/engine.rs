//! Test-only canvas engine for the job integration tests. It is never product code: root registers the
//! product engines. It implements just enough of `EngineAdapter` for the core's real commit path:
//! `SetText` (content intent) and `SetStyle` (design intent) on one text node.

use std::collections::BTreeMap;

use designllm_contracts::{
    new_id, Artifact, ArtifactBody, ArtifactKind, CanvasDocument, CapabilityReport, ContentDigest, CreateRequest,
    DltError, DltResult, Diagnostic, Editability, EncodedOutput, EngineAdapter, EngineContext, ErrorCode, ExportRequest, Id,
    Node, NodeKind, Operation, OpenInput, Page, PreparedChange, PreservationVerification, Rect, RenderScene, Size,
    Style,
};
use serde_json::{json, Value};

pub const TEST_ENGINE_ID: &str = "test-canvas";
pub const TITLE_NODE: &str = "node-title";
pub const INITIAL_TEXT: &str = "Initial title";

pub struct TestCanvasEngine;

fn title_node(text: &str) -> Node {
    Node {
        id: TITLE_NODE.into(),
        kind: NodeKind::Text,
        bounds: Rect { x: 0, y: 0, width: 4_000_000, height: 800_000 },
        text: text.into(),
        style: Style::default(),
        asset: None,
        children: vec![],
        layout: None,
        interactions: vec![],
        variants: BTreeMap::new(),
        ext: Default::default(),
    }
}

fn find_node_mut<'a>(nodes: &'a mut [Node], id: &str) -> Option<&'a mut Node> {
    for node in nodes {
        if node.id == id {
            return Some(node);
        }
        if let Some(found) = find_node_mut(&mut node.children, id) {
            return Some(found);
        }
    }
    None
}

fn collect_text(nodes: &[Node], out: &mut String) {
    for node in nodes {
        out.push_str(&node.id);
        out.push('=');
        out.push_str(&node.text);
        out.push('\n');
        collect_text(&node.children, out);
    }
}

impl EngineAdapter for TestCanvasEngine {
    fn id(&self) -> &'static str {
        TEST_ENGINE_ID
    }

    fn kinds(&self) -> Vec<ArtifactKind> {
        vec![ArtifactKind::Canvas]
    }

    fn create(&self, request: &CreateRequest, _context: &EngineContext<'_>) -> DltResult<Artifact> {
        let body = ArtifactBody::Canvas(CanvasDocument {
            pages: vec![Page { nodes: vec![title_node(INITIAL_TEXT)], ..Page::new(new_id(), Size::slide()) }],
            styles: BTreeMap::new(),
            ..Default::default()
        });
        Ok(Artifact::new(ArtifactKind::Canvas, request.title.clone(), TEST_ENGINE_ID, body))
    }

    fn open(&self, _input: &OpenInput, _context: &EngineContext<'_>) -> DltResult<Artifact> {
        Err(DltError::unsupported("the test engine does not open files"))
    }

    fn capabilities(&self, _artifact: &Artifact) -> CapabilityReport {
        BTreeMap::new()
    }

    fn supported_persisted_features(&self) -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::new()
    }

    fn persisted_features(&self, artifact: &Artifact) -> DltResult<std::collections::BTreeSet<String>> {
        let ArtifactBody::Canvas(document) = &artifact.body else {
            return Err(DltError::new(ErrorCode::IntegrityError, "test canvas artifact has no canvas body"));
        };
        fn has_unhandled_feature(nodes: &[Node]) -> bool {
            nodes.iter().any(|node| {
                !node.ext.set_fields().is_empty()
                    || !node.style.non_default_v12_fields().is_empty()
                    || node.variants.values().any(|patch| patch.fields().iter().any(|field| field.is_v12()))
                    || node.children.iter().any(|child| has_unhandled_feature(std::slice::from_ref(child)))
            })
        }
        if document.styles.values().any(|style| !style.non_default_v12_fields().is_empty())
            || document.pages.iter().any(|page| page.name.is_some() || page.nodes.iter().any(|node| has_unhandled_feature(std::slice::from_ref(node))))
        {
            return Err(DltError::new(ErrorCode::SchemaUnsupported, "test canvas does not persist v1.2 feature fields"));
        }
        Ok(std::collections::BTreeSet::new())
    }

    fn inspect(&self, artifact: &Artifact, node_ids: &[Id]) -> DltResult<Value> {
        let ArtifactBody::Canvas(doc) = &artifact.body else {
            return Err(DltError::invalid("not a canvas"));
        };
        let mut found = Vec::new();
        for page in &doc.pages {
            for node in &page.nodes {
                if node_ids.contains(&node.id) {
                    found.push(json!({ "id": node.id, "text": node.text }));
                }
            }
        }
        Ok(Value::Array(found))
    }

    fn prepare_change(
        &self,
        artifact: &Artifact,
        operations: &[Operation],
        _context: &EngineContext<'_>,
    ) -> DltResult<PreparedChange> {
        let mut next = artifact.clone();
        let ArtifactBody::Canvas(doc) = &mut next.body else {
            return Err(DltError::invalid("not a canvas"));
        };
        let mut changed = Vec::new();
        for op in operations {
            match op {
                Operation::SetText { node_id, text } => {
                    let node = doc
                        .pages
                        .iter_mut()
                        .find_map(|p| find_node_mut(&mut p.nodes, node_id))
                        .ok_or_else(|| DltError::new(designllm_contracts::ErrorCode::NotFound, format!("no node {node_id}")))?;
                    node.text = text.clone();
                    changed.push(node_id.clone());
                }
                Operation::SetStyle { node_ids, patch } => {
                    for node_id in node_ids {
                        let node = doc
                            .pages
                            .iter_mut()
                            .find_map(|p| find_node_mut(&mut p.nodes, node_id))
                            .ok_or_else(|| DltError::new(designllm_contracts::ErrorCode::NotFound, format!("no node {node_id}")))?;
                        patch.apply(&mut node.style);
                        changed.push(node_id.clone());
                    }
                }
                other => {
                    return Err(DltError::unsupported(format!(
                        "the test engine does not support operation {}",
                        serde_json::to_value(other).ok().and_then(|v| v.get("op").cloned()).unwrap_or(Value::Null)
                    )))
                }
            }
        }
        Ok(PreparedChange { artifact: next, changed_ids: changed, diagnostics: Vec::new() })
    }

    fn validate(&self, _artifact: &Artifact, _context: &EngineContext<'_>) -> Vec<Diagnostic> {
        Vec::new()
    }

    fn content_digest(&self, artifact: &Artifact) -> DltResult<ContentDigest> {
        let ArtifactBody::Canvas(doc) = &artifact.body else {
            return Err(DltError::invalid("not a canvas"));
        };
        let mut content = String::new();
        for page in &doc.pages {
            collect_text(&page.nodes, &mut content);
        }
        Ok(ContentDigest { content, data: String::new() })
    }

    fn layout(&self, _artifact: &Artifact, _context: &EngineContext<'_>) -> DltResult<RenderScene> {
        Err(DltError::unsupported("the test engine has no layout"))
    }

    fn encode_native(&self, artifact: &Artifact, _context: &EngineContext<'_>) -> DltResult<EncodedOutput> {
        let bytes = serde_json::to_vec(artifact).map_err(|e| DltError::invalid(e.to_string()))?;
        Ok(EncodedOutput { bytes, mime: "application/json".into(), editability: Editability::None, diagnostics: vec![] })
    }

    fn verify_saved(
        &self,
        _artifact: &Artifact,
        _bytes: &[u8],
        _context: &EngineContext<'_>,
    ) -> DltResult<PreservationVerification> {
        Ok(PreservationVerification::default())
    }

    fn export(&self, _artifact: &Artifact, _request: &ExportRequest, _context: &EngineContext<'_>) -> DltResult<EncodedOutput> {
        Err(DltError::unsupported("the test engine does not export"))
    }
}
