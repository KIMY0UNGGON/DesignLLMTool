use designllm_contracts::{design_themes::DesignThemeEntry, new_id, ArtifactKind, Intent};
use eframe::egui::{self, Color32, Sense, Ui, Vec2};
use serde_json::{json, Value};

use super::{properties::section_head, DesktopApp, PendingAction};
use crate::{commands::CommandSpec, theme, widgets};

pub(super) fn design_theme_notice(value: &Value) -> Option<String> {
    fn steps(value: &Value) -> String {
        value.as_array().map(|items| items.iter().map(Value::to_string).collect::<Vec<_>>().join(", ")).unwrap_or_default()
    }
    let notes = value.get("notes")?.as_array()?;
    let mut parts = Vec::new();
    for note in notes {
        match note["kind"].as_str() {
            Some("applied_theme") => {
                let colors = note["colors"].as_object().map(|colors| colors.iter().filter_map(|(name, color)| color.as_str().map(|color| format!("{name} {color}"))).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                let heading = note["fonts"]["heading"].as_str().unwrap_or("");
                let body = note["fonts"]["body"].as_str().unwrap_or("");
                if !colors.is_empty() { parts.push(format!("색 {colors}")); }
                parts.push(format!("글꼴 {heading}, {body}"));
                if note["radius_px"].as_array().is_some_and(|radii| !radii.is_empty()) {
                    parts.push(format!("모서리 {}px", steps(&note["radius_px"])));
                }
            }
            Some("stored_steps") => parts.push(format!("글 크기 {}px · 간격 {}px 저장", steps(&note["type_scale_px"]), steps(&note["spacing_px"]))),
            Some("skipped_type_size") => parts.push(format!("글 크기 {}px 저장 생략(지원 범위 밖)", note["value_px"])),
            Some("skipped_steps") => parts.push(format!("슬라이드에 글 크기 {}px · 모서리 {}px · 간격 {}px 미적용", steps(&note["type_scale_px"]), steps(&note["radius_px"]), steps(&note["spacing_px"]))),
            Some("gradient_kept") => parts.push(format!("{} 그라데이션 유지", note["node_id"].as_str().unwrap_or("?"))),
            Some("skipped_text_color_count") => {
                let reason = note["reason"].as_str().unwrap_or("");
                let why = match reason {
                    "bound_color" => "기존 토큰 연결",
                    "filled_shape" => "도형 위 글",
                    "explicit_color" => "직접 지정한 색",
                    "no_text_color" => "테마 글 색 없음",
                    _ => "기존 설정",
                };
                let items = notes.iter().filter(|item| item["kind"] == "skipped_text_color" && item["reason"] == reason)
                    .map(|item| format!("{} {}", item["node_id"].as_str().unwrap_or("?"), item["value"].as_str().unwrap_or("?")))
                    .collect::<Vec<_>>().join(", ");
                parts.push(format!("{why} {}개 색 유지 ({items})", note["count"]));
            }
            Some(kind @ ("skipped_font_count" | "skipped_background_count")) => {
                let (item_kind, label) = if kind == "skipped_font_count" { ("skipped_font", "글꼴") } else { ("skipped_background", "배경") };
                let items = notes.iter().filter(|item| item["kind"] == item_kind)
                    .map(|item| format!("{} {}", item["node_id"].as_str().unwrap_or("?"), item["value"].as_str().unwrap_or("?")))
                    .collect::<Vec<_>>().join(", ");
                parts.push(format!("기존 토큰 연결 {label} {}개 유지 ({items})", note["count"]));
            }
            _ => {}
        }
    }
    (!parts.is_empty()).then(|| format!("테마 적용 · {}", parts.join(" · ")))
}

impl DesktopApp {
    pub(super) fn fetch_design_themes(&mut self) {
        let query = self.design_theme_query.clone();
        self.call("design_themes.search", json!({"query": query, "limit": 20}), PendingAction::DesignThemesList { query });
    }

    pub(super) fn dispatch_design_theme(&mut self, spec: &CommandSpec, args: &Value) -> Option<Result<(), String>> {
        if spec.id != "design_theme.apply" { return None; }
        Some((|| {
            self.ensure_interaction_revision_matches_context()?;
            let entry: DesignThemeEntry = serde_json::from_value(args.get("entry").cloned().ok_or("테마를 고르세요")?).map_err(|e| format!("테마 값: {e}"))?;
            if !entry.is_valid() { return Err("테마 값이 올바르지 않습니다".into()); }
            let active = self.active.as_ref().ok_or("작업물을 열어 주세요")?;
            let context = self.context.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
            let (actor, intents) = self.change_actor_intents([Intent::Design].into_iter().collect());
            let params = json!({"request_id": new_id(), "actor": actor, "workspace_id": context.workspace_id,
                "artifact_id": active.id, "base_revision": context.revision, "allowed_intents": intents, "theme_id": entry.id});
            self.call("design_themes.apply", params, PendingAction::DesignThemeApply).ok_or("테마 적용 요청을 보내지 못했습니다")?;
            Ok(())
        })())
    }

    pub(super) fn design_theme_section(&mut self, ui: &mut Ui, palette: &theme::Palette, first: &mut bool) {
        if !self.active.as_ref().is_some_and(|a| matches!(a.kind, ArtifactKind::Canvas | ArtifactKind::Slides)) { return; }
        section_head(ui, palette, first, "디자인 테마", None);
        let mut refresh = false;
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 56.0).max(40.0);
            let input = widgets::edit_line(ui, palette, &mut self.design_theme_query, widgets::Field { hint: "검색", width, ..Default::default() });
            refresh = (input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) || widgets::labeled_button(ui, palette, None, "찾기", false).clicked();
        });
        let mut selected = None;
        for entry in &self.design_themes {
            ui.label(crate::i18n::tr(&entry.summary));
            if !entry.tags.is_empty() { ui.label(crate::i18n::tr(&entry.tags.join(" · "))); }
            ui.horizontal_wrapped(|ui| {
                for color in entry.colors.values() {
                    if let Ok(rgb) = u32::from_str_radix(color.trim_start_matches('#'), 16) {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                        ui.painter().rect_filled(rect, 2.0, Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8));
                    }
                }
                if widgets::labeled_button(ui, palette, None, "적용", false).clicked() { selected = Some(entry.clone()); }
            });
            ui.add_space(6.0);
        }
        if refresh { self.fetch_design_themes(); }
        if let Some(entry) = selected { self.invoke_ui("design_theme.apply", json!({"entry": entry})); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{canvas_with_page, content_of, Host, HostBackend, Mcp, Window};
    use designllm_contracts::{design_themes::DesignThemeFonts, ArtifactBody, BindTarget, Node, Operation, TokenRef, TokenSource, TokenValues};
    use std::collections::BTreeMap;

    #[test]
    fn apply_response_notes_reach_the_visible_notice() {
        let mut harness = crate::app::test_support::Harness::new();
        let response = json!({"revision":1, "notes":[
            {"kind":"applied_theme","colors":{"text":"#123456"},"fonts":{"heading":"Title Face","body":"Body Face"}},
            {"kind":"stored_steps","type_scale_px":[13,27],"spacing_px":[7,19]},
            {"kind":"skipped_type_size","value_px":0,"reason":"out_of_range"},
            {"kind":"gradient_kept","node_id":"hero"},
            {"kind":"skipped_text_color","node_id":"button","reason":"filled_shape","value":"#FFFFFF"},
            {"kind":"skipped_text_color_count","reason":"filled_shape","count":1},
            {"kind":"skipped_font","node_id":"heading","value":"Old Font"},
            {"kind":"skipped_font_count","count":1},
            {"kind":"skipped_background","node_id":"back","value":"#111111"},
            {"kind":"skipped_background_count","count":1}
        ]});
        harness.app.handle_payload(&harness.ctx, 1, PendingAction::DesignThemeApply, crate::rpc::RpcPayload::Json(response));
        let (_, message) = harness.message().expect("the response produces a visible notice");
        for part in ["#123456", "Title Face", "Body Face", "13, 27", "7, 19", "0px", "hero", "그라데이션 유지", "1개", "button", "#FFFFFF", "heading Old Font", "back #111111"] {
            assert!(message.contains(part), "missing {part}: {message}");
        }
    }

    #[test]
    fn theme_gui_and_mcp_produce_same_artifact() {
        let entry = DesignThemeEntry { id: "test".into(), tags: vec!["calm".into()], summary: "Quiet".into(), colors: BTreeMap::from([("background".into(), "#EEEEEE".into()), ("text".into(), "#123456".into())]), fonts: DesignThemeFonts { heading: "Heading Sans".into(), body: "Body Sans".into() }, type_scale_px: vec![16.0, 32.0], radius_px: vec![4.0, 8.0], spacing_px: vec![8.0], recorded_at: 1 };
        let mut host = Host::start();
        host.call("design_themes.record", json!({"entry": entry})).expect("record theme");
        let gui = canvas_with_page(&mut host, "Theme");
        let other = canvas_with_page(&mut host, "Theme");
        let nodes: Vec<Node> = [
            json!({"id":"bg", "kind":"rectangle", "bounds":{"x":0,"y":0,"width":12192000,"height":6858000}}),
            json!({"id":"body", "kind":"text", "text":"Body", "bounds":{"x":100000,"y":100000,"width":2000000,"height":500000}, "style":{"font_size":152400}}),
            json!({"id":"title", "kind":"text", "text":"Title", "bounds":{"x":100000,"y":700000,"width":2000000,"height":500000}, "style":{"font_size":304800}}),
            json!({"id":"card", "kind":"rectangle", "bounds":{"x":100000,"y":1400000,"width":2000000,"height":1000000}, "style":{"corner_radius":57150}}),
        ].into_iter().map(|node| serde_json::from_value(node).unwrap()).collect();
        for id in [&gui, &other] {
            let operations = nodes.iter().cloned().map(|node| Operation::AddNode { page_id: "page-1".into(), parent_id: None, node }).collect();
            host.apply(id, operations, [Intent::Content, Intent::Design].into_iter().collect()).expect("add nodes");
            host.apply(id, vec![
                Operation::PutToken { token_id: "size.1".into(), name: "Old size".into(), values: TokenValues::FontSize { light: 152400, dark: 152400 } },
                Operation::BindToken { node_ids: vec!["body".into()], target: BindTarget::StyleFontSize, token: TokenRef { source: TokenSource::Local, token_id: "size.1".into() } },
            ], [Intent::Design].into_iter().collect()).expect("bind the old size");
        }
        let before = host.artifact(&gui);
        let mut window = Window::open(&mut host, &gui);
        window.step(&mut host, "design_theme.apply", json!({"entry": entry})).expect("apply theme");
        let notice = window.message().expect("the returned notes are shown");
        for part in ["#123456", "Heading Sans", "Body Sans", "16.0", "32.0", "8.0"] {
            assert!(notice.contains(part), "missing {part}: {notice}");
        }
        let actual = host.artifact(&gui);
        drop(window);
        let backend = HostBackend::new(host);
        let mcp = Mcp::connect(backend.clone());
        let (workspace, revision) = backend.with(|host| (host.workspace_id.clone(), host.artifact(&other).revision));
        let result = mcp.call("design_themes_apply", json!({"request_id":new_id(), "actor":"mcp", "workspace_id":workspace, "artifact_id":other, "base_revision":revision, "allowed_intents":["design"], "theme_id":"test"}));
        assert!(result["notes"].is_array());
        assert_eq!(actual.revision, before.revision + 1);
        assert_eq!(content_of(&actual), content_of(&backend.with(|host| host.artifact(&other))));
        let ArtifactBody::Canvas(doc) = &actual.body else { panic!("canvas") };
        let ArtifactBody::Canvas(before_doc) = &before.body else { panic!("canvas") };
        let find = |id: &str| doc.pages[0].nodes.iter().find(|node| node.id == id).unwrap();
        assert_eq!(find("body").style.color, "#123456");
        assert_ne!(before_doc.pages[0].nodes.iter().find(|n| n.id == "body").unwrap().style.color, find("body").style.color);
        assert_eq!(find("body").style.font_family, "Body Sans");
        assert_eq!(find("title").style.font_family, "Heading Sans");
        assert_eq!(find("body").style.font_size, 152400);
        assert_eq!(find("title").style.font_size, 304800);
        assert_eq!(find("bg").style.fill, "#EEEEEE");
        assert_eq!(find("card").style.corner_radius, Some(38100));
        for (id, target) in [("body", BindTarget::StyleColor), ("body", BindTarget::StyleFontFamily), ("bg", BindTarget::StyleFill), ("card", BindTarget::StyleCornerRadius)] {
            assert!(doc.bindings.iter().any(|b| b.node_id == id && b.target == target));
        }
    }

    #[test]
    fn theme_tools_keep_entries_across_workspace_restart() {
        let backend = HostBackend::new(Host::start());
        let mcp = Mcp::connect(backend.clone());
        let entry = DesignThemeEntry { id: "persisted".into(), tags: vec!["calm".into()], summary: "Quiet".into(), colors: BTreeMap::new(), fonts: DesignThemeFonts { heading: "Test Sans".into(), body: "Test Sans".into() }, type_scale_px: vec![], radius_px: vec![], spacing_px: vec![], recorded_at: 1 };
        mcp.call("design_themes_record", json!({"entry": entry}));
        backend.with(|host| host.restart());
        let result = mcp.call("design_themes_search", json!({"query": "calm", "limit": 20}));
        assert_eq!(result["entries"][0]["id"], "persisted");
    }
}
