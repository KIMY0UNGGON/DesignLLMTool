//! A gradient clear returns a node to its solid fill. Each check applies operations to a document the way the engine stores a
//! change, so the stored node is what the renderer, the saved file, and a later edit all read.

use super::*;
use designllm_contracts::{
    AssetRef, AssetStore, CanvasDocument, DltError, DltResult, ErrorCode, Gradient, GradientStop, Node, NodeExt, NodeKind,
    Operation, Page, Rect, Size, Style, StyleField, StylePatch,
};

/// The checks below use no asset, so every asset request is refused.
struct NoAssets;

impl AssetStore for NoAssets {
    fn put(&self, _: &[u8], _: &str) -> DltResult<AssetRef> {
        Err(DltError::unsupported("no assets"))
    }
    fn read(&self, _: &AssetRef) -> DltResult<Vec<u8>> {
        Err(DltError::unsupported("no assets"))
    }
    fn lookup(&self, hash: &str) -> DltResult<AssetRef> {
        Err(DltError::new(ErrorCode::NotFound, hash))
    }
}

fn gradient() -> Gradient {
    Gradient {
        angle_millideg: 90_000,
        stops: vec![
            GradientStop { pos_milli: 0, color: "#2F64E0".into() },
            GradientStop { pos_milli: 1000, color: "#FFFFFF".into() },
        ],
    }
}

fn clear_gradient() -> StylePatch {
    StylePatch { clear: [StyleField::Gradient].into_iter().collect(), ..StylePatch::default() }
}

/// One page with one node, `shape`, of `kind` and `style`.
fn document_with(kind: NodeKind, style: Style) -> CanvasDocument {
    let node = Node {
        id: "shape".into(),
        kind,
        bounds: Rect { x: 0, y: 0, width: 300, height: 200 },
        text: String::new(),
        style,
        asset: None,
        children: Vec::new(),
        layout: None,
        interactions: Vec::new(),
        variants: Default::default(),
        ext: NodeExt::default(),
    };
    CanvasDocument {
        pages: vec![Page { nodes: vec![node], ..Page::new("page", Size::slide()) }],
        ..Default::default()
    }
}

/// Applies one style patch to `shape` and returns the IDs the engine reports as changed.
fn set_shape_style(document: &mut CanvasDocument, patch: StylePatch) -> DltResult<Vec<String>> {
    let mut links = Vec::new();
    apply(document, &mut links, &NoAssets, &[Operation::SetStyle { node_ids: vec!["shape".into()], patch }])
        .map(|(changed, _)| changed)
}

fn stored_style(document: &CanvasDocument) -> &Style {
    &document.pages[0].nodes[0].style
}

#[test]
fn gradient_clear_returns_the_node_to_its_solid_fill() {
    for kind in [NodeKind::Rectangle, NodeKind::Ellipse] {
        let style = Style { fill: "#112233".into(), gradient: Some(gradient()), ..Style::default() };
        let mut document = document_with(kind, style);
        let changed = set_shape_style(&mut document, clear_gradient()).expect("a gradient clear is accepted");
        assert_eq!(changed, vec!["shape".to_string()], "{kind:?}");
        let solid = Style { fill: "#112233".into(), ..Style::default() };
        assert_eq!(
            serde_json::to_value(stored_style(&document)).expect("style json"),
            serde_json::to_value(&solid).expect("solid style json"),
            "{kind:?}: the node draws its solid fill again"
        );
    }
}

#[test]
fn gradient_clear_is_open_for_every_kind_that_can_hold_a_gradient() {
    for kind in [NodeKind::Rectangle, NodeKind::Ellipse, NodeKind::Path, NodeKind::Text] {
        crate::canvas::guard::check_patch_kind(&clear_gradient(), kind, "shape")
            .unwrap_or_else(|error| panic!("{kind:?}: {error:?}"));
    }
}

#[test]
fn gradient_clear_redo_and_undo_give_the_same_documents() {
    let style = Style { fill: "#112233".into(), gradient: Some(gradient()), ..Style::default() };
    let original = document_with(NodeKind::Rectangle, style);
    let mut cleared = original.clone();
    set_shape_style(&mut cleared, clear_gradient()).expect("the clear");
    // Redo: the same clear on the earlier document gives the same document again.
    let mut redone = original.clone();
    set_shape_style(&mut redone, clear_gradient()).expect("the clear, applied again");
    assert_eq!(serde_json::to_value(&redone).expect("redone"), serde_json::to_value(&cleared).expect("cleared"));
    // Undo restores the earlier document. Setting the gradient on the cleared one gives that document back.
    let mut reset = cleared.clone();
    set_shape_style(&mut reset, StylePatch { gradient: Some(gradient()), ..StylePatch::default() }).expect("the gradient again");
    assert_eq!(serde_json::to_value(&reset).expect("reset"), serde_json::to_value(&original).expect("original"));
}

#[test]
fn gradient_clear_survives_a_document_round_trip_and_stays_cleared() {
    let style = Style { fill: "#112233".into(), gradient: Some(gradient()), ..Style::default() };
    let mut document = document_with(NodeKind::Rectangle, style);
    set_shape_style(&mut document, clear_gradient()).expect("the clear");
    let saved = serde_json::to_value(&document).expect("saved document");
    assert!(saved["pages"][0]["nodes"][0]["style"].get("gradient").is_none(), "{saved}");
    let reopened: CanvasDocument = serde_json::from_value(saved.clone()).expect("reopened document");
    assert_eq!(reopened.pages[0].nodes[0].style.gradient, None);
    assert_eq!(serde_json::to_value(&reopened).expect("reopened json"), saved);
}

#[test]
fn gradient_clear_patch_round_trips_as_the_gradient_name() {
    let clear = clear_gradient();
    let wire = serde_json::to_value(&clear).expect("patch json");
    assert_eq!(wire["clear"], serde_json::json!(["gradient"]));
    let back: StylePatch = serde_json::from_value(wire).expect("patch read back");
    assert_eq!(back, clear);
}

#[test]
fn gradient_set_and_clear_in_one_patch_is_refused_by_name() {
    let both = StylePatch { gradient: Some(gradient()), clear: [StyleField::Gradient].into_iter().collect(), ..StylePatch::default() };
    let error = both.check().expect_err("a field cannot be set and cleared in one patch");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(error.message.contains("gradient"), "{}", error.message);
}
