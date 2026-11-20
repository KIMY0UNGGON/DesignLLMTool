use designllm_contracts::{
    Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, CanvasDocument, CanvasPageSection,
    ContextSnapshot, Selection, Size,
};
use eframe::egui::{Context, RawInput};

use super::{
    remove_page_section, replace_page_section, section_copy_payload, section_started_at,
    DesktopApp, SectionEditor, SectionMutation,
};
use crate::rpc::RpcWorker;

fn section(id: &str, name: &str, start_page_id: &str) -> CanvasPageSection {
    CanvasPageSection { id: id.into(), name: name.into(), start_page_id: start_page_id.into() }
}

fn app_with_canvas(revision: u64, pages: Vec<designllm_contracts::Page>, sections: Vec<CanvasPageSection>) -> DesktopApp {
    let (rpc, _replies, _work) = RpcWorker::detached();
    let mut app = DesktopApp::new(rpc, "workspace".into(), None, None);
    let mut artifact = Artifact::new(
        ArtifactKind::Canvas,
        "Canvas",
        "engine",
        ArtifactBody::Canvas(CanvasDocument { pages, page_sections: sections, ..Default::default() }),
    );
    artifact.id = "a1".into();
    artifact.revision = revision;
    app.active = Some(ArtifactSummary {
        id: "a1".into(),
        kind: ArtifactKind::Canvas,
        title: "Canvas".into(),
        revision,
        engine_id: "engine".into(),
        dirty: true,
        modified_at_ms: None,
        active_jobs: Vec::new(),
    });
    app.artifact = Some(artifact);
    app.context = Some(ContextSnapshot {
        workspace_id: "w1".into(),
        artifact_id: "a1".into(),
        revision,
        view_id: app.view_id.clone(),
        selection: Selection { node_ids: Vec::new(), text_range: None },
        visible_page: 0,
        selected_properties: serde_json::Value::Null,
        capabilities: Default::default(),
        capability_extensions: Default::default(),
        render_snapshot: None,
        view_transform: None,
    });
    app.screen = super::super::Screen::Editor;
    app
}

fn page(id: &str) -> designllm_contracts::Page {
    designllm_contracts::Page::new(id, Size::a4())
}

#[test]
fn section_headers_follow_page_start_ids_not_the_canonical_array_order() {
    // The stored array is canonicalized by ID, while the sidebar walks pages in display order.
    let sections = vec![section("a", "Later", "p3"), section("z", "First", "p1")];
    let pages = ["p1", "p2", "p3"];
    let headers: Vec<_> = pages.iter().filter_map(|page_id| {
        section_started_at(&sections, page_id).map(|section| (*page_id, section.name.as_str()))
    }).collect();
    assert_eq!(headers, [("p1", "First"), ("p3", "Later")]);
    assert!(section_started_at(&sections, "p2").is_none(), "pages before the next marker remain unsectioned");
}

#[test]
fn section_add_rename_and_remove_preserve_other_start_markers() {
    let first = section("a", "First", "p1");
    let later = section("z", "Later", "p3");
    let inserted = section("m", "Middle", "p2");
    let after_add = replace_page_section(&[first.clone(), later.clone()], &inserted, true).expect("add");
    assert_eq!(after_add.iter().map(|section| section.id.as_str()).collect::<Vec<_>>(), ["a", "m", "z"]);

    let renamed = section("m", "Renamed", "p2");
    let after_rename = replace_page_section(&after_add, &renamed, false).expect("rename");
    assert_eq!(section_started_at(&after_rename, "p2").map(|section| section.name.as_str()), Some("Renamed"));

    let after_remove = remove_page_section(&after_rename, "m").expect("remove marker");
    assert_eq!(after_remove, [first, later]);
}

#[test]
fn section_draft_survives_external_revision_change_and_missing_start_page() {
    let original = section("s1", "Keep this section", "p1");
    let editor = SectionEditor {
        artifact_id: "a1".into(),
        revision: 1,
        expected: vec![original.clone()],
        section: original.clone(),
        page_index: 0,
        is_new: false,
        message: None,
    };
    let mut changed_revision = app_with_canvas(2, vec![page("p1")], vec![original.clone()]);
    changed_revision.page_section_editor = Some(editor.clone());
    let ctx = Context::default();
    let _ = ctx.run(RawInput::default(), |ctx| changed_revision.draw_page_section_editor(ctx));
    let preserved = changed_revision.page_section_editor.as_ref().expect("revision conflict keeps editor");
    assert!(preserved.message.as_deref().is_some_and(|message| message.contains("바뀌었습니다")));
    assert_eq!(preserved.section, original);
    assert!(section_copy_payload(&preserved.section, preserved.page_index).contains("Keep this section"));

    let deleted = section("s2", "Page was removed", "p-old");
    let mut missing_page = app_with_canvas(2, Vec::new(), Vec::new());
    missing_page.page_section_editor = Some(SectionEditor {
        artifact_id: "a1".into(),
        revision: 2,
        expected: vec![deleted.clone()],
        section: deleted.clone(),
        page_index: 0,
        is_new: false,
        message: None,
    });
    let _ = ctx.run(RawInput::default(), |ctx| missing_page.draw_page_section_editor(ctx));
    let preserved = missing_page.page_section_editor.as_ref().expect("missing page keeps editor");
    assert!(preserved.message.is_some());
    assert_eq!(preserved.section, deleted);
    let copy = section_copy_payload(&preserved.section, preserved.page_index);
    assert!(copy.contains("Page was removed") && copy.contains("p-old"));
}

#[test]
fn pending_section_save_blocks_close_until_its_reply() {
    let section = section("s1", "Pending name", "p1");
    let mut app = app_with_canvas(1, vec![page("p1")], Vec::new());
    app.page_section_mutation = Some(SectionMutation {
        token: "token".into(),
        artifact_id: "a1".into(),
        revision: 1,
        expected: Vec::new(),
        desired: vec![section.clone()],
        editor: Some(SectionEditor {
            artifact_id: "a1".into(),
            revision: 1,
            expected: Vec::new(),
            section,
            page_index: 0,
            is_new: true,
            message: None,
        }),
        message: None,
        pending: true,
        retry_allowed: false,
        outcome_uncertain: false,
        held_edit_id: None,
    });

    assert!(app.page_section_request_in_flight());
    assert!(app.has_close_work());
    assert!(app.held_edit_close_warning().is_some_and(|warning| warning.contains("저장 요청의 결과")));
    let ctx = Context::default();
    let _ = ctx.run(RawInput::default(), |ctx| app.discard_held_edits_and_close(ctx));
    assert!(app.page_section_mutation.is_some(), "a pending section operation cannot be discarded by closing");
}
