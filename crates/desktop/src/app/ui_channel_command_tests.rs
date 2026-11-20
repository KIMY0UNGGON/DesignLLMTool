use super::{figure_with_diagram_and_chart, test_png};
use crate::app::{
    link_flow_tests::{add_box, canvas_with_three_boxes, drawn_figure},
    state::FigureForm,
    test_support::{content_of, repository_root, Host, Window},
};
use designllm_contracts::{local_path::AGENT_PATH_PARAM, new_id, ArtifactBody, FlowBlock, PanelBody};
use serde_json::json;

#[test]
fn a_home_run_opens_the_same_animation_as_the_home_button() {
    let mut host = Host::start();
    let canvas = host.create("canvas", "홈");
    let mut window = Window::open(&mut host, &canvas);
    for (id, args, permission) in [
        ("home.create", json!({ "tile": "animation" }), "agent_start"),
        ("home.use_template", json!({ "template": "webgl_animation" }), "change"),
    ] {
        window.step(&mut host, "nav.home", json!({})).unwrap();
        window.step(&mut host, id, args.clone()).unwrap();
        let files = |window: &Window| {
            let ArtifactBody::WebCode(doc) = &window.app.artifact.as_ref().unwrap().body else { panic!("expected WebCode") };
            (doc.files["index.html"].content.clone(), doc.files["animation.js"].content.clone())
        };
        let clicked = files(&window);
        window.step(&mut host, "nav.home", json!({})).unwrap();
        let run = window.agent_run_granted(&mut host, id, args, permission, &["content"]);
        assert_eq!(run["status"], "completed", "{run}");
        assert_eq!(run["result"]["opened_in_view"], true);
        assert_eq!(run["result"]["id"], window.app.active.as_ref().unwrap().id);
        assert_eq!(files(&window), clicked);
    }
}

#[test]
fn home_job_uses_the_granted_intents_and_leaves_the_users_composer_alone() {
    let mut host = Host::start();
    let canvas = host.create("canvas", "홈");
    let mut window = Window::open(&mut host, &canvas);
    window.step(&mut host, "nav.home", json!({})).unwrap();
    window.app.allow_content = true;
    window.app.allow_design = false;
    window.app.home.prompt = "작성 중인 요청".into();
    let args = json!({ "tile": "blank", "prompt": "새 문서", "runtime": "codex", "model": "test-model" });
    window.app.home.attachments.push("C:/user-draft.png".into());
    let starts = |window: &Window| window.calls.iter().filter(|(m, _, _)| m == "job.start").count();
    let pending = window.agent_run_granted(&mut host, "home.create", args.clone(), "agent_start", &["content", "design"]);
    assert_eq!(pending["status"], "delivered", "{pending}");
    assert_eq!(starts(&window), 0);
    let answer = |window: &mut Window, host: &mut Host, button: &str| {
        use crate::app::{test_support::screen, tests::{click_at, find_text_center}};
        let ctx = window.ctx.clone();
        let mut render = |input| ctx.run(input, |ctx| window.app.draw_home(ctx));
        render(screen(1.0, 1000.0, 1000.0));
        let shown = render(screen(1.0, 1000.0, 1000.0));
        let labels = crate::app::test_support::drawn_text(&shown);
        assert!(labels.iter().any(|s| s.contains("codex") && s.contains("test-model")));
        assert!(labels.iter().any(|s| s.contains("새 문서")));
        render(click_at(find_text_center(&shown, button)));
        window.settle(host);
    };
    answer(&mut window, &mut host, "거절");
    let refused = host.call("ui.command.get", json!({ "run_id": pending["run_id"] })).unwrap();
    assert_eq!(refused["status"], "failed", "{refused}");
    assert_eq!(refused["error"]["code"], "CANCELED");
    assert_eq!(starts(&window), 0);
    let pending = window.agent_run_granted(&mut host, "home.create", args, "agent_start", &["content", "design"]);
    assert_eq!(pending["status"], "delivered", "{pending}");
    assert_eq!(starts(&window), 0);
    answer(&mut window, &mut host, "모델 실행");
    let run = host.call("ui.command.get", json!({ "run_id": pending["run_id"] })).unwrap();
    assert_eq!(starts(&window), 1);
    assert_eq!(run["status"], "completed", "{run}");
    let job = &run["result"]["job"];
    assert_eq!(job["artifact_id"], run["result"]["artifact"]["id"]);
    assert_eq!(job["allowed_intents"], json!(["content"]));
    assert_eq!(job["model"], "test-model");
    assert_eq!(job["runtime"], "codex");
    assert_eq!(run["result"]["sources"], json!([]));
    assert_eq!(window.app.active.as_ref().unwrap().id, job["artifact_id"].as_str().unwrap());
    assert_eq!(window.app.home.prompt, "작성 중인 요청");
    assert_eq!(window.app.home.attachments, ["C:/user-draft.png"]);
}

#[test]
fn opening_a_path_reports_the_import_and_a_missing_path_never_opens_the_previous_file() {
    let mut host = Host::start();
    let canvas = host.create("canvas", "열기");
    let mut window = Window::open(&mut host, &canvas);
    let path = repository_root().join("fixtures/office/basic-preserve.docx");
    let args = json!({ "path": path });
    window.step(&mut host, "nav.open_file", args.clone()).unwrap();
    assert!(last_request(&window, "artifact.open").get(AGENT_PATH_PARAM).is_none(), "the user's own open is not marked");
    // Reopening a DOCX allocates new paragraph and run IDs.
    let paragraphs = |window: &Window| {
        let ArtifactBody::Flow(doc) = &window.app.artifact.as_ref().unwrap().body else { panic!("expected DOCX") };
        doc.sections.iter().flat_map(|section| &section.blocks).filter_map(|block| match block {
            FlowBlock::Paragraph(p) => Some(p.runs.iter().map(|run| run.text.as_str()).collect::<String>()),
            _ => None,
        }).collect::<Vec<_>>()
    };
    let clicked = paragraphs(&window);
    assert!(!clicked.is_empty());
    let run = window.agent_run(&mut host, "nav.open_file", args);
    assert_eq!(run["status"], "completed", "{run}");
    assert_eq!(last_request(&window, "artifact.open")[AGENT_PATH_PARAM], true, "the agent's path keeps the core's agent rules");
    assert_eq!(run["result"]["id"], window.app.active.as_ref().unwrap().id);
    assert_eq!(paragraphs(&window), clicked);
    let mark = window.calls.len();
    let missing = window.agent_run(&mut host, "nav.open_file", json!({}));
    assert_eq!(missing["status"], "failed");
    assert!(!window.calls[mark..].iter().any(|(method, _, _)| method == "artifact.open"));
    let absent = path.with_file_name(format!("{}.docx", new_id()));
    assert_eq!(window.agent_run(&mut host, "nav.open_file", json!({ "path": absent }))["status"], "failed");
}

#[test]
fn selection_export_writes_the_same_pixels_and_reports_overwrite_refusal() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.step(&mut host, "select.node", json!({ "node_id": "a" })).unwrap();
    let path = std::env::temp_dir().join(format!("selection-{}.png", new_id()));
    let args = json!({ "format": "png", "target_path": path, "background": "transparent" });
    window.step(&mut host, "file.export_selection", args.clone()).unwrap();
    assert!(last_request(&window, "artifact.export").get(AGENT_PATH_PARAM).is_none(), "the user's own export is not marked");
    let clicked = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    let run = window.agent_run_granted(&mut host, "file.export_selection", args.clone(), "file_write", &[]);
    assert_eq!(run["status"], "completed", "{run}");
    assert_eq!(run["result"]["target_path"], json!(path));
    assert_eq!(std::fs::read(&path).unwrap(), clicked);
    let request = &window.calls.iter().rev().find(|(method, _, _)| method == "artifact.export").unwrap().1;
    assert_eq!(request["options"]["node_ids"], json!(["a"]));
    assert_eq!(request[AGENT_PATH_PARAM], true);
    let refused = window.agent_run_granted(&mut host, "file.export_selection", args, "file_write", &[]);
    assert_eq!(refused["status"], "failed", "{refused}");
    assert_eq!(std::fs::read(&path).unwrap(), clicked);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn chart_opens_the_same_form_without_committing() {
    let mut host = Host::start();
    let figure = host.create_figure();
    let mut window = Window::open(&mut host, &figure);
    let before = host.artifact(&figure).revision;
    window.step(&mut host, "add.chart", json!({})).unwrap();
    assert!(window.app.figure_form == Some(FigureForm::Chart));
    window.app.figure_form = None;
    let run = window.agent_run_granted(&mut host, "add.chart", json!({}), "read", &[]);
    assert_eq!(run["status"], "completed", "{run}");
    assert!(window.app.figure_form == Some(FigureForm::Chart));
    assert_eq!(host.artifact(&figure).revision, before);
}

#[test]
fn each_sheet_run_replays_the_gui_edit_in_one_revision_including_plot_rebinding() {
    let mut host = Host::start();
    let figure = figure_with_diagram_and_chart(&mut host);
    let mut window = Window::open(&mut host, &figure);
    for id in ["data.edit_cell", "data.add_row", "data.add_column", "data.remove_column", "data.remove_row"] {
        let (doc, _) = host.figure(&figure);
        let dataset = doc.datasets.last().unwrap();
        let mut args = json!({ "dataset_id": dataset.id });
        match id {
            "data.edit_cell" => {
                args["row_id"] = json!(dataset.rows[0].id);
                args["column_id"] = json!(dataset.columns.iter().find(|column| column.data_type == "numeric").unwrap().id);
                args["value"] = json!("13");
            }
            "data.add_column" => args["name"] = json!("메모"),
            "data.remove_column" => args["column_id"] = json!(dataset.columns.iter().find(|column| column.name == "메모").unwrap().id),
            "data.remove_row" => args["row_id"] = json!(dataset.rows.last().unwrap().id),
            _ => {}
        }
        let before = content_of(&host.artifact(&figure));
        window.step(&mut host, id, args.clone()).unwrap();
        let clicked = content_of(&host.artifact(&figure));
        assert_ne!(before, clicked, "{id}");
        window.step(&mut host, "edit.undo", json!({})).unwrap();
        assert_eq!(content_of(&host.artifact(&figure)), before, "{id} undoes in one step");
        let revision = host.artifact(&figure).revision;
        let run = window.agent_run_granted(&mut host, id, args, "change", &["data"]);
        assert_eq!(run["status"], "completed", "{id}: {run}");
        assert_eq!(run["result"]["core_result"]["revision"], revision + 1);
        assert_eq!(content_of(&host.artifact(&figure)), clicked, "{id}");
        let request = &window.calls.iter().rev().find(|(method, _, _)| method == "figure.dataset.edit").unwrap().1;
        assert_eq!(request["actor"], "agent");
        assert_eq!(request["allowed_intents"], json!(["data"]));
    }
}

#[test]
fn an_imported_figure_panel_is_one_commit_and_a_failed_import_is_a_failed_run() {
    let mut host = Host::start();
    let figure = drawn_figure(&mut host, "이미지", 0);
    let mut window = Window::open(&mut host, &figure);
    let path = test_png(64, 48);
    let args = json!({ "path": path, "origin": "generated_illustration" });
    let panel_state = |host: &mut Host| {
        let (doc, _) = host.figure(&figure);
        let panel = doc.panels.last().unwrap();
        assert!(matches!(&panel.body, PanelBody::Image(_)));
        json!([doc.panels[0].bounds, panel.label, panel.bounds, panel.body])
    };
    window.step(&mut host, "add.figure_image", args.clone()).unwrap();
    let clicked = panel_state(&mut host);
    window.step(&mut host, "edit.undo", json!({})).unwrap();
    let before = host.artifact(&figure).revision;
    let run = window.agent_run_granted(&mut host, "add.figure_image", args.clone(), "change", &["content", "design"]);
    assert_eq!(run["status"], "completed", "{run}");
    assert_eq!(run["result"]["core_result"]["revision"], before + 1);
    let prepared = run["result"]["prepared_asset_sha256"].as_str().unwrap();
    assert_eq!(panel_state(&mut host), clicked);
    assert_eq!(prepared, designllm_contracts::sha256(&std::fs::read(&path).unwrap()));
    let non_image = b"figure import must not store this text";
    std::fs::write(&path, non_image).unwrap();
    let failed = window.agent_run_granted(&mut host, "add.figure_image", args.clone(), "change", &["content", "design"]);
    assert_eq!(failed["status"], "failed", "{failed}");
    assert!(host.call("asset.lookup", json!({ "sha256": designllm_contracts::sha256(non_image) })).is_err());
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_dir(path.parent().unwrap()).unwrap();
    let failed = window.agent_run_granted(&mut host, "add.figure_image", args, "change", &["content", "design"]);
    assert_eq!(failed["status"], "failed", "{failed}");
    assert_eq!(host.artifact(&figure).revision, before + 1);
}

#[test]
fn leaving_the_figure_during_import_reports_the_prepared_asset_without_a_commit() {
    let mut host = Host::start();
    let figure = host.create_figure();
    let mut window = Window::open(&mut host, &figure);
    window.app.publish_ui_if_due();
    window.settle(&mut host);
    let path = test_png(12, 10);
    let hash = designllm_contracts::sha256(&std::fs::read(&path).unwrap());
    let run = host.call("ui.command.run", json!({
        "view_id": window.app.ui.view_id, "expected_state_seq": window.app.ui.state_seq,
        "command_id": "add.figure_image", "permission": "change", "actor": "agent",
        "allowed_intents": ["content", "design"], "args": { "path": path },
    })).unwrap();
    let batch = host.call("events.poll", json!({
        "after_seq": window.app.last_event_seq, "view_id": window.app.ui.view_id, "view_token": window.app.ui.token,
    })).unwrap();
    let before = host.artifact(&figure).revision;
    window.app.take_ui_runs(&batch);
    window.app.invoke("nav.home", json!({})).unwrap();
    window.settle(&mut host);
    let result = host.call("ui.command.get", json!({ "run_id": run["run_id"] })).unwrap();
    assert_eq!(result["status"], "failed", "{result}");
    assert_eq!(result["error"]["code"], "REVISION_CONFLICT");
    assert_eq!(result["error"]["affected_ids"], json!([hash]));
    assert_eq!(host.artifact(&figure).revision, before);
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_dir(path.parent().unwrap()).unwrap();
}

#[test]
fn link_insert_and_update_commit_only_after_render_and_select_the_same_picture_as_the_gui() {
    let mut host = Host::start();
    let figure = drawn_figure(&mut host, "링크", 1);
    let mut windows = [false, true].map(|_| {
        let canvas = host.create("canvas", "캔버스");
        Window::open(&mut host, &canvas)
    });
    for id in ["add.figure_link", "figure.update_link"] {
        if id == "figure.update_link" {
            add_box(&mut host, &figure);
        }
        let args = if id == "add.figure_link" { json!({ "figure_id": figure }) } else { json!({}) };
        let mut states = Vec::new();
        for (index, window) in windows.iter_mut().enumerate() {
            let before = window.app.active.as_ref().unwrap().revision;
            if index == 0 {
                window.step(&mut host, id, args.clone()).unwrap();
            } else {
                let run = window.agent_run(&mut host, id, args.clone());
                assert_eq!(run["status"], "completed", "{run}");
                assert_eq!(run["result"]["core_result"]["revision"], before + 1);
                let commit = &window.calls.iter().rev().find(|(method, _, _)| method == "changes.apply").unwrap().1;
                assert_eq!(commit["actor"], "agent");
                assert_eq!(commit["allowed_intents"], json!(["content"]));
            }
            let artifact = window.app.artifact.as_ref().unwrap();
            assert_eq!(artifact.revision, before + 1);
            let link = &artifact.figure_links[0];
            assert_eq!(window.app.selected_node.as_ref(), Some(&link.target_node_id));
            let node = crate::app::doc_tree::find_node(artifact, &link.target_node_id).unwrap();
            states.push(json!([link.figure_id, link.figure_revision, link.render_hash, node.bounds, node.asset]));
        }
        assert_eq!(states[0], states[1], "{id}");
        assert_eq!(states[1][1], host.artifact(&figure).revision);
    }
    let run = windows[1].agent_run(&mut host, "add.figure_link", json!({ "figure_id": "missing" }));
    assert_eq!(run["status"], "failed", "{run}");
}

/// The last request the window sent with `method`.
fn last_request(window: &Window, method: &str) -> serde_json::Value {
    window.calls.iter().rev().find(|(sent, _, _)| sent == method).unwrap_or_else(|| panic!("no {method} request")).1.clone()
}

/// 66c r3: an agent's file.save and file.export give target_path the same local-path check as the other file commands,
/// before anything reaches the core; a local target is saved with the agent mark.
#[test]
fn agent_save_and_export_refuse_unc_and_verbatim_targets_before_sending() {
    let mut host = Host::start();
    let canvas = host.create("canvas", "경로");
    let mut window = Window::open(&mut host, &canvas);
    window.step(&mut host, "nav.open_file", json!({ "path": repository_root().join("fixtures/office/basic-preserve.docx") })).unwrap();
    for (command, target) in [
        ("file.save", r"\server\share\copy.docx"),
        ("file.save", r"\?\C:\copy.docx"),
        ("file.export", r"\server\share\copy.pdf"),
        ("file.export", r"\?\C:\copy.pdf"),
    ] {
        let mut args = json!({ "target_path": target });
        if command == "file.export" { args["format"] = json!("pdf"); }
        let mark = window.calls.len();
        let run = window.agent_run_granted(&mut host, command, args, "file_write", &[]);
        assert_eq!(run["status"], "failed", "{command} {target}: {run}");
        assert_eq!(run["error"]["code"], "INVALID_ARGUMENT", "{command} {target}: {run}");
        assert_eq!(run["error"]["message"], "로컬 파일의 절대 경로를 target_path로 지정해 주세요", "{command} {target}");
        assert!(!window.calls[mark..].iter().any(|(method, _, _)| method.starts_with("artifact.")), "{command} {target} sends nothing");
    }
    let target = std::env::temp_dir().join(format!("agent-save-{}.docx", new_id()));
    let run = window.agent_run_granted(&mut host, "file.save", json!({ "target_path": target }), "file_write", &[]);
    assert_eq!(run["status"], "completed", "{run}");
    assert_eq!(last_request(&window, "artifact.save")[AGENT_PATH_PARAM], true);
    std::fs::remove_file(target).unwrap();
}
