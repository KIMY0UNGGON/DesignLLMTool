//! Grouping and ungrouping through the real canvas engine (the flow tests' `Host`), with the window's
//! own operations: the drawn position of every text node must stay the same after wrapping and after
//! the group is taken apart. The drawn position is the union of the node's text lines in the render
//! snapshot's source map.

use std::collections::BTreeMap;

use designllm_contracts::{
    new_id, Artifact, ArtifactBody, ChangeSet, Intent, Node, NodeKind, Operation, RenderSnapshot,
};
use serde_json::json;

use super::{
    dispatch::{undo_change_request, ungroup_operations, wrap_operations},
    figure_flow_tests::Host,
};

/// The stored canvas: its revision, the first page's id and that page's nodes.
fn stored(host: &mut Host, artifact: &str) -> (u64, String, Vec<Node>) {
    let value = host.call("artifact.inspect", json!({ "artifact_id": artifact })).expect("the artifact inspects");
    let artifact: Artifact = serde_json::from_value(value).expect("an artifact");
    let ArtifactBody::Canvas(document) = artifact.body else {
        panic!("a canvas");
    };
    let page = &document.pages[0];
    (artifact.revision, page.id.clone(), page.nodes.clone())
}

/// One change set from the window: the given operations against the stored revision.
fn apply(host: &mut Host, artifact: &str, operations: Vec<Operation>) {
    let (revision, _, _) = stored(host, artifact);
    let change = ChangeSet {
        request_id: new_id(),
        actor: "desktop".into(),
        workspace_id: host.workspace_id.clone(),
        artifact_id: artifact.into(),
        base_revision: revision,
        allowed_intents: [Intent::Content, Intent::Design].into_iter().collect(),
        job_ref: None,
        read_dependencies: Vec::new(),
        operations,
    };
    let value = serde_json::to_value(change).expect("a change serialises");
    if let Err(error) = host.call("changes.apply", value) {
        panic!("the engine refused the change: {error:?}");
    }
}

/// Where each text node is drawn on page 0: the union of its text lines (x, y, right, bottom).
fn drawn(host: &mut Host, artifact: &str) -> BTreeMap<String, (i64, i64, i64, i64)> {
    let (revision, _, _) = stored(host, artifact);
    let value = host
        .call(
            "render.get",
            json!({
                "artifact_id": artifact,
                "expected_revision": revision,
                "page_index": 0,
                "profile": { "dpi": 96, "transparent": false },
            }),
        )
        .expect("the page renders");
    let snapshot: RenderSnapshot = serde_json::from_value(value).expect("a render snapshot");
    let mut boxes: BTreeMap<String, (i64, i64, i64, i64)> = BTreeMap::new();
    for page in &snapshot.scene.pages {
        for source in &page.source_map {
            let bounds = source.bounds;
            let (x, y) = (bounds.x, bounds.y);
            let (right, bottom) = (bounds.x + bounds.width, bounds.y + bounds.height);
            let entry = boxes.entry(source.node_id.clone()).or_insert((x, y, right, bottom));
            *entry = (entry.0.min(x), entry.1.min(y), entry.2.max(right), entry.3.max(bottom));
        }
    }
    boxes
}

fn text(id: &str, x: i64, y: i64, width: i64) -> Node {
    serde_json::from_value(json!({
        "id": id,
        "kind": "text",
        "text": "기록 시작 결과",
        "bounds": { "x": x, "y": y, "width": width, "height": 400_000 },
    }))
    .expect("a text node parses")
}

#[test]
fn wrapping_and_ungrouping_two_text_nodes_leave_where_they_are_drawn() {
    let mut host = Host::start();
    let artifact = host.create("canvas", "묶기 검증");
    let (_, page, _) = stored(&mut host, &artifact);
    apply(
        &mut host,
        &artifact,
        vec![
            Operation::AddNode { page_id: page.clone(), parent_id: None, node: text("first", 1_000_000, 1_000_000, 3_000_000) },
            Operation::AddNode { page_id: page.clone(), parent_id: None, node: text("second", 5_000_000, 2_500_000, 2_000_000) },
        ],
    );
    let before = drawn(&mut host, &artifact);
    assert!(before.contains_key("first") && before.contains_key("second"), "both text nodes are drawn: {before:?}");

    // Group the two nodes: the stored nodes, one change set (as the window does).
    let (_, page, nodes) = stored(&mut host, &artifact);
    let members: Vec<Node> = nodes.into_iter().filter(|node| node.id == "first" || node.id == "second").collect();
    apply(&mut host, &artifact, wrap_operations(&members, &page, NodeKind::Group, None));
    let after_wrap = only_original(drawn(&mut host, &artifact), &before);
    assert_eq!(after_wrap, before, "wrapping moves nothing on the page: before {before:?}, after {after_wrap:?}");

    // Take the group apart again.
    let (_, page, nodes) = stored(&mut host, &artifact);
    let group = nodes.iter().find(|node| node.kind == NodeKind::Group).expect("the group is stored").clone();
    assert_eq!(group.children.len(), 2, "both nodes are inside the group");
    let index = nodes.iter().position(|node| node.id == group.id).unwrap_or(0) as u32;
    apply(&mut host, &artifact, ungroup_operations(&group, &page, index));
    assert_eq!(only_original(drawn(&mut host, &artifact), &before), before, "ungrouping puts them back where they were");
    assert!(!stored(&mut host, &artifact).2.iter().any(|node| node.kind == NodeKind::Group), "the group container is removed");
}

/// A column stack with three children whose stored bounds are all (0, 0): the frames the window outlines are where
/// the engine drew them (inside the padding, one below the other), not the stored bounds.
#[test]
fn stack_children_are_outlined_where_the_engine_lays_them_out() {
    let mut host = Host::start();
    let artifact = host.create("canvas", "스택 선택 검증");
    let (_, page, _) = stored(&mut host, &artifact);
    let child = |id: &str| json!({ "id": id, "kind": "rectangle", "bounds": { "x": 0, "y": 0, "width": 800_000, "height": 300_000 } });
    let stack: Node = serde_json::from_value(json!({
        "id": "stack",
        "kind": "rectangle",
        "bounds": { "x": 1_000_000, "y": 1_000_000, "width": 2_000_000, "height": 2_000_000 },
        "layout": { "mode": "column", "width_mode": "fixed", "height_mode": "fixed", "gap": 100_000, "padding": 200_000, "align": "start" },
        "children": [child("a"), child("b"), child("c")],
    }))
    .expect("a stack parses");
    apply(&mut host, &artifact, vec![Operation::AddNode { page_id: page, parent_id: None, node: stack }]);
    let (revision, _, nodes) = stored(&mut host, &artifact);
    let value = host
        .call("render.get", json!({ "artifact_id": artifact, "expected_revision": revision, "page_index": 0, "profile": { "dpi": 96, "transparent": false } }))
        .expect("the page renders");
    let snapshot: RenderSnapshot = serde_json::from_value(value).expect("a render snapshot");
    let drawn = super::multi::laid_out_frames(&snapshot.scene.pages[0]);
    let (a, b, c) = (drawn["a"], drawn["b"], drawn["c"]);
    assert_eq!((a.x, a.y), (1_200_000, 1_200_000), "the first child starts inside the padding: {a:?}");
    assert_eq!(b.y, a.y + a.height + 100_000, "the second is one gap below: {b:?}");
    assert_eq!(c.y, b.y + b.height + 100_000, "the third is one gap below: {c:?}");

    // The on-screen frames follow the drawn frames: three outlines, none overlapping.
    let page_rect = eframe::egui::Rect::from_min_size(eframe::egui::Pos2::ZERO, eframe::egui::vec2(800.0, 600.0));
    let mut frames = Vec::new();
    let stack = nodes.iter().find(|node| node.id == "stack").expect("the stack is stored");
    super::multi::collect_frames(stack, (0, 0), &drawn, page_rect, 0.1, &mut frames);
    let screen = |id: &str| frames.iter().find(|(found, _)| found == id).expect("a frame").1;
    let (sa, sb, sc) = (screen("a"), screen("b"), screen("c"));
    assert!(!sa.intersects(sb.shrink(0.5)) && !sb.intersects(sc.shrink(0.5)) && !sa.intersects(sc), "{sa:?} {sb:?} {sc:?}");
    assert!(sa.min.y > screen("stack").min.y, "the first child is below the stack's top edge");

    // Dragging `a` below `c` puts it last; a small drag keeps it first (the drop uses the drawn frames).
    let siblings = [b, c];
    let centre = |rect: designllm_contracts::Rect| (rect.x + rect.width / 2, rect.y + rect.height / 2);
    assert_eq!(super::multi::drop_index("column", &siblings, (centre(a).0, c.y + c.height + 50_000)), 2);
    assert_eq!(super::multi::drop_index("column", &siblings, (centre(a).0, centre(a).1 + 50_000)), 0);
    assert_eq!(super::multi::drop_index("column", &siblings, (centre(a).0, (centre(b).1 + centre(c).1) / 2)), 1);
    let (_, page, _) = stored(&mut host, &artifact);
    apply(&mut host, &artifact, vec![Operation::MoveNode { node_id: "a".into(), page_id: page, parent_id: Some("stack".into()), index: 2 }]);
    let (_, _, nodes) = stored(&mut host, &artifact);
    let order: Vec<&str> = nodes.iter().find(|node| node.id == "stack").expect("the stack").children.iter().map(|child| child.id.as_str()).collect();
    assert_eq!(order, ["b", "c", "a"], "the drop's index is the place in the stack");
}

/// The drawn boxes of the nodes that existed before (the group container is drawn too, and it goes away
/// when the group is taken apart).
fn only_original(
    drawn: BTreeMap<String, (i64, i64, i64, i64)>,
    before: &BTreeMap<String, (i64, i64, i64, i64)>,
) -> BTreeMap<String, (i64, i64, i64, i64)> {
    drawn.into_iter().filter(|(id, _)| before.contains_key(id)).collect()
}

/// Applies one batch that adds `node` and returns the batch's request id (the id undo names).
fn apply_batch(host: &mut Host, artifact: &str, node: Node) -> String {
    let (revision, page, _) = stored(host, artifact);
    let request_id = new_id();
    let change = ChangeSet {
        request_id: request_id.clone(),
        actor: "desktop".into(),
        workspace_id: host.workspace_id.clone(),
        artifact_id: artifact.into(),
        base_revision: revision,
        allowed_intents: [Intent::Content].into_iter().collect(),
        job_ref: None,
        read_dependencies: Vec::new(),
        operations: vec![Operation::AddNode { page_id: page, parent_id: None, node }],
    };
    host.call("changes.apply", serde_json::to_value(change).expect("a change serialises")).expect("the batch applies");
    request_id
}

#[test]
fn undoing_one_batch_takes_back_exactly_that_change() {
    let mut host = Host::start();
    let artifact = host.create("canvas", "되돌리기 검증");
    apply_batch(&mut host, &artifact, text("keep", 1_000_000, 1_000_000, 3_000_000));
    let second = apply_batch(&mut host, &artifact, text("batch", 5_000_000, 2_500_000, 2_000_000));
    let shown = drawn(&mut host, &artifact);
    assert!(shown.contains_key("keep") && shown.contains_key("batch"), "both batches are drawn: {shown:?}");
    let (revision, _, _) = stored(&mut host, &artifact);
    let request = undo_change_request(&host.workspace_id, &artifact, revision, second);
    host.call("history.undo_change", serde_json::to_value(request).expect("a request serialises"))
        .expect("the batch is taken back");
    let after = drawn(&mut host, &artifact);
    assert!(after.contains_key("keep") && !after.contains_key("batch"), "only the named batch is undone: {after:?}");
}
