//! Tests for Sort and UnitScale transforms in the data sheet (`data_sheet.rs`):
//! emitted `figure.dataset.edit` requests, host acceptance for Sort, stale-revision/version
//! refusal, invalid parameter refusal, and headless egui drawing.

use std::{sync::mpsc::{Receiver, Sender}, time::Instant};

use designllm_contracts::{
    Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, ContextSnapshot, DataTransform, Dataset,
    PanelBody,
};
use eframe::egui::{self, Rect};
use serde_json::{json, Value};

use super::super::{
    drafts::Body,
    figure_flow_tests::Host,
    figure_tools::chart_change,
    tests::screen,
    CsvImportDraft, DatasetCellEdit, DesktopApp, PendingAction, Screen,
};
use super::{unit_scale_draft_id, UnitScaleDraft};
use crate::{
    rpc::{RpcReply, RpcWorker, Work},
    theme,
};

struct Harness {
    app: DesktopApp,
    work: Receiver<Work>,
    _replies: Sender<RpcReply>,
    figure_id: String,
    revision: u64,
    dataset: Dataset,
}

impl Harness {
    fn open_figure(host: &mut Host, csv: &str) -> Self {
        let figure_id = host.create("figure", "그림");
        let mut draft = CsvImportDraft::new(host.import_table(csv));
        draft.y_unit = "mg".into();
        let (figure, _) = host.figure(&figure_id);
        let (operations, intents) = chart_change(&figure, &draft, true).expect("chart");
        let revision = host.apply(&figure_id, operations, intents).expect("chart applied");
        let artifact = host.artifact(&figure_id);
        let ArtifactBody::Figure(doc) = &artifact.body else {
            panic!("not a figure");
        };
        let panel_id = doc.panels[0].id.clone();
        let dataset = doc.datasets[0].clone();
        Self::from_artifact(host.workspace_id.clone(), artifact, panel_id, dataset, revision)
    }

    fn from_artifact(
        workspace_id: String,
        artifact: Artifact,
        panel_id: String,
        dataset: Dataset,
        revision: u64,
    ) -> Self {
        let figure_id = artifact.id.clone();
        let (rpc, replies, work) = RpcWorker::detached();
        let mut app = DesktopApp::new(rpc, "작업공간".into(), None, None);
        while work.try_recv().is_ok() {}
        app.screen = Screen::Editor;
        app.active = Some(ArtifactSummary {
            id: figure_id.clone(),
            kind: ArtifactKind::Figure,
            title: "그림".into(),
            revision,
            engine_id: "designllm-figures".into(),
            dirty: false,
            modified_at_ms: None,
            active_jobs: Vec::new(),
        });
        app.context = Some(context(&workspace_id, &figure_id, &panel_id, revision, &app.view_id));
        app.artifact = Some(artifact);
        app.selected_node = Some(panel_id.clone());
        app.selected_nodes = vec![panel_id];
        Self { app, work, _replies: replies, figure_id, revision, dataset }
    }

    fn sent(&mut self) -> Vec<(u64, String, Value)> {
        let mut batch = Vec::new();
        while let Ok(work) = self.work.try_recv() {
            if let Work::Call { ticket, method, params, .. } = work {
                batch.push((ticket, method, params));
            }
        }
        batch
    }
}

fn context(workspace_id: &str, figure_id: &str, panel_id: &str, revision: u64, view_id: &str) -> ContextSnapshot {
    serde_json::from_value(json!({
        "workspace_id": workspace_id,
        "artifact_id": figure_id,
        "revision": revision,
        "view_id": view_id,
        "selection": { "node_ids": [panel_id], "text_range": null },
        "visible_page": 0,
        "selected_properties": { "nodes": null, "stale_selection": null, "render_unavailable": null },
        "capabilities": {
            "dataset": { "read": "supported", "render": "supported", "edit": "supported", "preserve": "supported", "write": "supported", "reason": "" }
        },
        "render_snapshot": null,
        "view_transform": null,
    }))
    .expect("context snapshot")
}

#[test]
fn sort_emits_figure_dataset_edit_for_ascending_and_descending_and_host_applies_it() {
    let mut host = Host::start();
    let mut harness = Harness::open_figure(&mut host, "id,value\nA,30\nB,10\nC,20\n");

    let initial_hash = harness.dataset.version_hash.clone();
    harness
        .app
        .sort_dataset_column(&harness.dataset.id, &initial_hash, harness.revision, "value", false)
        .expect("ascending sort emits request");
    let sent = harness.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1, "figure.dataset.edit");
    assert_eq!(sent[0].2["artifact_id"], harness.figure_id);
    assert_eq!(sent[0].2["base_revision"], harness.revision);
    assert_eq!(sent[0].2["allowed_intents"], json!(["data"]));
    assert_eq!(sent[0].2["dataset_id"], harness.dataset.id);
    assert_eq!(sent[0].2["version_hash"], initial_hash);
    assert_eq!(
        sent[0].2["edits"],
        json!([{ "type": "sort", "column_id": "value", "descending": false }])
    );

    // Local dataset is not mutated without a recorded transform from the host.
    assert_eq!(
        harness
            .app
            .figure_document()
            .expect("figure")
            .datasets[0]
            .rows
            .iter()
            .map(|r| r.cells["id"].original.as_str())
            .collect::<Vec<_>>(),
        vec!["A", "B", "C"]
    );

    // The host accepts the emitted request, records Sort, reorders rows, and rebinds the plot.
    host.call("figure.dataset.edit", sent[0].2.clone()).expect("host accepts ascending sort");
    let (after_asc, rev_after_asc) = host.figure(&harness.figure_id);
    let ds_asc = &after_asc.datasets[0];
    assert_ne!(ds_asc.version_hash, initial_hash);
    assert_eq!(
        ds_asc.rows.iter().map(|r| r.cells["id"].original.as_str()).collect::<Vec<_>>(),
        vec!["B", "C", "A"]
    );
    assert!(matches!(
        ds_asc.transforms.last(),
        Some(DataTransform::Sort { column_id, descending: false }) if column_id == "value"
    ));
    let PanelBody::Plot(plot) = &after_asc.panels[0].body else { panic!("plot") };
    assert_eq!(plot.dataset_ref.version_hash, ds_asc.version_hash);

    // Sync harness to new revision and emit descending sort.
    let artifact_asc = host.artifact(&harness.figure_id);
    let panel_id = after_asc.panels[0].id.clone();
    let mut desc_harness = Harness::from_artifact(
        host.workspace_id.clone(),
        artifact_asc,
        panel_id,
        ds_asc.clone(),
        rev_after_asc,
    );
    desc_harness
        .app
        .sort_dataset_column(&ds_asc.id, &ds_asc.version_hash, rev_after_asc, "value", true)
        .expect("descending sort emits request");
    let desc_sent = desc_harness.sent();
    assert_eq!(desc_sent.len(), 1);
    assert_eq!(
        desc_sent[0].2["edits"],
        json!([{ "type": "sort", "column_id": "value", "descending": true }])
    );

    host.call("figure.dataset.edit", desc_sent[0].2.clone()).expect("host accepts descending sort");
    let (after_desc, _) = host.figure(&harness.figure_id);
    assert_eq!(
        after_desc.datasets[0].rows.iter().map(|r| r.cells["id"].original.as_str()).collect::<Vec<_>>(),
        vec!["A", "C", "B"]
    );
}

#[test]
fn unit_scale_emits_figure_dataset_edit_with_exact_transform_fields() {
    let mut host = Host::start();
    let mut harness = Harness::open_figure(&mut host, "id,value\nA,0.25\nB,0.50\n");
    let before_columns = harness.dataset.columns.len();

    let draft = UnitScaleDraft {
        dataset_id: harness.dataset.id.clone(),
        version_hash: harness.dataset.version_hash.clone(),
        revision: harness.revision,
        column_id: "value".into(),
        numerator: "1000".into(),
        denominator: "1".into(),
        output_unit: "ug".into(),
    };
    harness.app.unit_scale_dataset_column(&draft).expect("unit scale emits request");

    let sent = harness.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1, "figure.dataset.edit");
    assert_eq!(sent[0].2["artifact_id"], harness.figure_id);
    assert_eq!(sent[0].2["base_revision"], harness.revision);
    assert_eq!(sent[0].2["allowed_intents"], json!(["data"]));
    assert_eq!(sent[0].2["dataset_id"], harness.dataset.id);
    assert_eq!(sent[0].2["version_hash"], harness.dataset.version_hash);
    assert_eq!(
        sent[0].2["edits"],
        json!([{
            "type": "unit_scale",
            "column_id": "value",
            "numerator": "1000",
            "denominator": "1",
            "output_unit": "ug"
        }])
    );

    // Local dataset values and columns are never mutated without a recorded transform from the host.
    let local_ds = &harness.app.figure_document().expect("figure").datasets[0];
    assert_eq!(local_ds.columns.len(), before_columns);
    assert!(local_ds.transforms.is_empty());
}

#[test]
fn stale_revision_and_stale_version_refuse_sort_and_unit_scale_without_emitting() {
    let mut host = Host::start();
    let mut harness = Harness::open_figure(&mut host, "id,value\nA,10\nB,20\n");
    let ds_id = harness.dataset.id.clone();
    let ds_hash = harness.dataset.version_hash.clone();
    let rev = harness.revision;
    let valid_draft = UnitScaleDraft {
        dataset_id: ds_id.clone(),
        version_hash: ds_hash.clone(),
        revision: rev,
        column_id: "value".into(),
        numerator: "1000".into(),
        denominator: "1".into(),
        output_unit: "ug".into(),
    };

    // 1. Context revision disagrees with artifact revision.
    harness.app.context.as_mut().expect("context").revision = rev + 1;
    assert!(!harness.app.sheet_transform_ready());
    assert!(harness.app.sort_dataset_column(&ds_id, &ds_hash, rev, "value", false).is_err());
    assert!(harness.app.unit_scale_dataset_column(&valid_draft).is_err());
    assert!(harness.sent().is_empty());
    harness.app.context.as_mut().expect("context").revision = rev;

    // 2. Active summary revision disagrees with artifact revision.
    harness.app.active.as_mut().expect("active").revision = rev + 1;
    assert!(harness.app.sort_dataset_column(&ds_id, &ds_hash, rev, "value", true).is_err());
    assert!(harness.app.unit_scale_dataset_column(&valid_draft).is_err());
    assert!(harness.sent().is_empty());
    harness.app.active.as_mut().expect("active").revision = rev;

    // 3. Context points to another figure artifact.
    harness.app.context.as_mut().expect("context").artifact_id = "other-figure".into();
    assert!(harness.app.sort_dataset_column(&ds_id, &ds_hash, rev, "value", false).is_err());
    assert!(harness.app.unit_scale_dataset_column(&valid_draft).is_err());
    assert!(harness.sent().is_empty());
    harness.app.context.as_mut().expect("context").artifact_id = harness.figure_id.clone();

    // 4. Draft/UI expected revision is stale.
    let stale_rev_draft = UnitScaleDraft { revision: rev - 1, ..valid_draft.clone() };
    assert!(harness.app.sort_dataset_column(&ds_id, &ds_hash, rev - 1, "value", false).is_err());
    assert!(harness.app.unit_scale_dataset_column(&stale_rev_draft).is_err());
    assert!(harness.sent().is_empty());

    // 5. Draft/UI expected dataset version_hash is stale.
    let stale_hash_draft = UnitScaleDraft { version_hash: "stale-hash".into(), ..valid_draft.clone() };
    assert!(harness.app.sort_dataset_column(&ds_id, "stale-hash", rev, "value", false).is_err());
    assert!(harness.app.unit_scale_dataset_column(&stale_hash_draft).is_err());
    assert!(harness.sent().is_empty());

    // 6. Pending context read / dataset edit blocks transforms.
    let _ = harness.app.call("context.read", json!({ "artifact_id": harness.figure_id }), PendingAction::Context);
    let _ = harness.sent();
    assert!(!harness.app.sheet_transform_ready());
    assert!(harness.app.sort_dataset_column(&ds_id, &ds_hash, rev, "value", false).is_err());
    assert!(harness.app.unit_scale_dataset_column(&valid_draft).is_err());
    assert!(harness.sent().is_empty());
    harness.app.pending.clear();

    // 7. In-place cell edit open blocks transforms.
    harness.app.dataset_edit = Some(DatasetCellEdit {
        dataset_id: ds_id.clone(),
        version_hash: ds_hash.clone(),
        row_id: harness.dataset.rows[0].id.clone(),
        column_id: "value".into(),
        original: "10".into(),
        draft: "15".into(),
    });
    assert!(!harness.app.sheet_transform_ready());
    assert!(harness.app.sort_dataset_column(&ds_id, &ds_hash, rev, "value", false).is_err());
    assert!(harness.app.unit_scale_dataset_column(&valid_draft).is_err());
    assert!(harness.sent().is_empty());
}

#[test]
fn invalid_sort_or_unit_scale_inputs_are_refused_without_emitting() {
    let mut host = Host::start();
    let mut harness = Harness::open_figure(&mut host, "id,value\nA,10\nB,20\n");
    let ds_id = harness.dataset.id.clone();
    let ds_hash = harness.dataset.version_hash.clone();
    let rev = harness.revision;

    // Missing column for Sort.
    assert!(harness.app.sort_dataset_column(&ds_id, &ds_hash, rev, "missing", false).is_err());

    // Non-numeric column for UnitScale.
    let non_numeric = UnitScaleDraft {
        dataset_id: ds_id.clone(),
        version_hash: ds_hash.clone(),
        revision: rev,
        column_id: "id".into(),
        numerator: "1000".into(),
        denominator: "1".into(),
        output_unit: "ug".into(),
    };
    assert!(harness.app.unit_scale_dataset_column(&non_numeric).is_err());

    // Empty output unit.
    let empty_unit = UnitScaleDraft { column_id: "value".into(), output_unit: "   ".into(), ..non_numeric.clone() };
    assert!(harness.app.unit_scale_dataset_column(&empty_unit).is_err());

    // Zero or invalid denominator / numerator.
    let zero_den = UnitScaleDraft { column_id: "value".into(), denominator: "0.0".into(), ..non_numeric.clone() };
    assert!(harness.app.unit_scale_dataset_column(&zero_den).is_err());
    let bad_num = UnitScaleDraft { column_id: "value".into(), numerator: "1,5".into(), ..non_numeric.clone() };
    assert!(harness.app.unit_scale_dataset_column(&bad_num).is_err());
    // More digits than the engine reads (18) is refused here, before a round trip to the engine.
    let long_num = UnitScaleDraft { column_id: "value".into(), numerator: "1234567890.123456789".into(), ..non_numeric.clone() };
    assert!(harness.app.unit_scale_dataset_column(&long_num).is_err());

    // Duplicate derived column id (`value@ug`).
    if let Some(Artifact { body: ArtifactBody::Figure(doc), .. }) = &mut harness.app.artifact {
        let mut dup = doc.datasets[0].columns[1].clone();
        dup.id = "value@ug".into();
        doc.datasets[0].columns.push(dup);
    }
    let dup_draft = UnitScaleDraft { column_id: "value".into(), ..non_numeric };
    assert!(harness.app.unit_scale_dataset_column(&dup_draft).is_err());

    assert!(harness.sent().is_empty());
}

#[test]
fn data_sheet_draws_sort_and_unit_scale_controls_in_headless_egui() {
    let mut host = Host::start();
    let mut harness = Harness::open_figure(&mut host, "id,value\nA,10\nB,20\n");
    let ctx = egui::Context::default();
    theme::install(&ctx);
    let palette = theme::palette(&ctx);
    let sheet_rect = Rect::from_min_max(egui::pos2(0.0, 400.0), egui::pos2(900.0, 640.0));

    let _ = ctx.run(screen(1.0, 900.0, 640.0), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            harness.app.draw_data_sheet(ui, &palette, sheet_rect);
        });
    });
    assert!(harness.app.sheet_transform_ready());
    assert!(harness.sent().is_empty());

    // The form's draft is kept per dataset: a draft of another dataset is not this sheet's.
    let other = UnitScaleDraft {
        dataset_id: "ds-other".into(),
        version_hash: "v".into(),
        revision: 1,
        column_id: "x".into(),
        numerator: "1".into(),
        denominator: "1".into(),
        output_unit: "m".into(),
    };
    ctx.data_mut(|data| data.insert_temp(unit_scale_draft_id("ds-other"), Some(other)));
    assert!(ctx.data(|data| data.get_temp::<Option<UnitScaleDraft>>(unit_scale_draft_id(&harness.dataset.id))).is_none());

    // Also verify headless rendering when revision is stale.
    harness.app.context.as_mut().expect("context").revision = harness.revision + 1;
    let _ = ctx.run(screen(1.0, 900.0, 640.0), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            harness.app.draw_data_sheet(ui, &palette, sheet_rect);
        });
    });
    assert!(!harness.app.sheet_transform_ready());
    assert!(harness.sent().is_empty());
}

#[test]
fn a_cell_value_for_a_table_that_changed_is_kept_as_a_copy_and_the_user_is_told() {
    let mut host = Host::start();
    let mut harness = Harness::open_figure(&mut host, "id,value\nA,0.25\nB,0.50\n");
    let dataset = harness.dataset.clone();
    let row = &dataset.rows[0];
    let edit = DatasetCellEdit {
        dataset_id: dataset.id.clone(),
        version_hash: "version-from-before".into(),
        row_id: row.id.clone(),
        column_id: "value".into(),
        original: row.cells["value"].original.clone(),
        draft: "0.30".into(),
    };
    harness.app.dataset_edit = Some(edit.clone());
    harness.app.apply_dataset_cell(edit);
    assert!(harness.app.dataset_edit.is_none(), "the editor closes");
    assert!(harness.sent().iter().all(|(_, method, _)| method != "figure.dataset.edit"), "nothing is sent for a table that changed");
    let kept = harness.app.drafts.recovered();
    assert_eq!(kept.len(), 1, "the value is kept as a copy");
    assert!(matches!(&kept[0].body, Body::Cell { draft, .. } if draft == "0.30"));
    assert!(
        harness.app.notices.current(Instant::now()).is_some_and(|notice| notice.text.contains("초안 목록에 남겼습니다")),
        "the user is told where the value went"
    );
}

#[test]
fn the_cell_editor_fills_its_cell_inside_the_grid_lines() {
    let mut host = Host::start();
    let mut harness = Harness::open_figure(&mut host, "id,value\nA,10\nB,20\n");
    let dataset = harness.dataset.clone();
    harness.app.dataset_edit = Some(DatasetCellEdit {
        dataset_id: dataset.id.clone(),
        version_hash: dataset.version_hash.clone(),
        row_id: dataset.rows[0].id.clone(),
        column_id: "value".into(),
        original: "10".into(),
        draft: "10".into(),
    });
    let ctx = egui::Context::default();
    theme::install(&ctx);
    let palette = theme::palette(&ctx);
    let sheet_rect = Rect::from_min_max(egui::pos2(0.0, 400.0), egui::pos2(900.0, 640.0));
    for _ in 0..3 {
        let _ = ctx.run(screen(1.0, 900.0, 640.0), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                harness.app.draw_data_sheet(ui, &palette, sheet_rect);
            });
        });
    }
    let focused = ctx.memory(|memory| memory.focused()).expect("the cell editor takes the focus");
    let editor = ctx.read_response(focused).expect("the editor was drawn").rect;
    assert!((editor.height() - (super::ROW - 2.0)).abs() < 0.5, "the editor is the cell's height inside its lines: {editor:?}");
    assert!((editor.width() - (super::COLUMN - 2.0)).abs() < 0.5, "and its width: {editor:?}");
}
