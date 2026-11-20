//! WebCode files on screen (task 44): the file rows of the layers tab, the open file, and the mapping list of the open
//! file with its value edit. The mapping list is the engine's value. The window's context carries the engine's `inspect`
//! of the selected file (`selected_properties.nodes.items[].mappings`, each `{id, tag}`), and the desktop does not read the
//! HTML. A value edit is one `mapped_web_edit` change set, so it is one undo step. A refusal (a stale hash, an unknown
//! mapping) comes back as the engine's answer and changes nothing.

use designllm_contracts::{ArtifactBody, Operation, WebDocument};
use eframe::egui::{Id, RichText, Ui};
use serde_json::{json, Value};

use super::DesktopApp;
use crate::{commands::CommandSpec, theme, widgets};

/// The value edit of one mapping of the open file. Picking another mapping starts a new, empty edit, so a value typed for
/// one mapping is never sent for another.
#[derive(Clone, Default)]
struct MappingEdit {
    mapping_id: String,
    attribute: bool,
    name: String,
    draft: String,
}

impl MappingEdit {
    /// Whether the edit can be sent: a value, and for an attribute its name. The command takes an empty value as the MCP
    /// tool does (it empties the text); the window does not send one, so an untouched field never clears the element.
    fn ready(&self) -> bool {
        !self.draft.is_empty() && (!self.attribute || !self.name.is_empty())
    }
}

impl DesktopApp {
    /// The commands of this file. `dispatch.rs` asks it after the other command families.
    pub(super) fn dispatch_web_mapping(&mut self, spec: &CommandSpec, args: &Value) -> Option<Result<(), String>> {
        let result = match spec.id {
            "web.open_file" => self.web_open_file(args),
            "web.mapped_edit" => self.web_mapped_edit(args),
            _ => return None,
        };
        Some(result)
    }

    /// Whether `id` names a file of the open WebCode project.
    pub(super) fn is_web_file(&self, id: &str) -> bool {
        self.web_document().is_some_and(|document| document.files.contains_key(id))
    }

    /// The command a layer row runs: a file of the open WebCode project opens, any other row is selected.
    pub(super) fn layer_row_command(&self, id: &str) -> (&'static str, Value) {
        if self.is_web_file(id) {
            ("web.open_file", json!({ "path": id }))
        } else {
            ("select.node", json!({ "node_id": id }))
        }
    }

    /// The open file of a WebCode project: the one the user opened, else the entry file.
    pub(super) fn open_web_path<'a>(&'a self, document: &'a WebDocument) -> &'a str {
        self.active_web_path.as_deref().unwrap_or(&document.entry)
    }

    pub(super) fn web_document(&self) -> Option<&WebDocument> {
        match self.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::WebCode(document)) => Some(document),
            _ => None,
        }
    }

    /// `web.open_file`: the file becomes the open one (`active_web_path`, which the text target follows) and the selection,
    /// which an AI request then names.
    fn web_open_file(&mut self, args: &Value) -> Result<(), String> {
        let path = args.get("path").and_then(Value::as_str).ok_or("파일을 고르세요")?;
        if !self.is_web_file(path) {
            return Err("파일을 찾지 못했습니다".into());
        }
        self.active_web_path = Some(path.to_owned());
        self.select_node(path.to_owned());
        Ok(())
    }

    /// `web.mapped_edit`: one `mapped_web_edit` for the mapping `mapping_id` of the file `path` (the open file by default).
    /// The hash is the file's current one unless `expected_hash` is given.
    fn web_mapped_edit(&mut self, args: &Value) -> Result<(), String> {
        let (path, expected_hash) = self.web_change_target(args, true)?;
        let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
        let operation = Operation::MappedWebEdit { path, expected_hash, mapping_id: text("mapping_id"), property: text("property"), value: text("value") };
        self.kind_change(vec![operation])
    }

    /// The shared design system of the open project (task 49c): the current link, the published systems to link to, and a read of
    /// the list. A WebCode project does not read the list on open, so the row offers the read until it has been read. A pick that
    /// is already the link sends nothing; another pick sends `canvas.design_system.set` with the revision the list showed.
    fn draw_web_design_system(&mut self, ui: &mut Ui, palette: &theme::Palette) {
        let Some(artifact) = self.artifact.as_ref() else { return };
        if !matches!(artifact.body, ArtifactBody::WebCode(_)) {
            return;
        }
        let link: Option<(String, u64)> = artifact.design_system.as_ref().map(|reference| (reference.artifact_id.clone(), reference.revision));
        let state = &self.token_panel;
        let current = match &link {
            None => String::from("연결 안 됨"),
            Some((source, revision)) => state.design_systems.iter().find(|system| system.artifact_id == *source).map_or_else(
                || format!("{source} · r{revision}"),
                |system| format!("{} · r{revision}", system.title),
            ),
        };
        let mut action: Option<(&'static str, Value)> = None;
        ui.add_space(8.0);
        ui.label(RichText::new(current).color(palette.ink_2));
        if state.design_systems_error {
            ui.small(RichText::new("공유 디자인 시스템 목록을 읽지 못했습니다").color(palette.danger));
        }
        if !state.design_systems_loaded || state.design_systems_error {
            let label = if state.design_systems_error { "다시 읽기" } else { "공유 목록 읽기" };
            if widgets::labeled_button(ui, palette, Some("refresh"), label, false).clicked() {
                action = Some(("canvas.design_system.list", json!({})));
            }
        } else {
            if link.is_some() && widgets::text_button(ui, palette, "연결 안 함", palette.ink_2).clicked() {
                action = Some(("canvas.design_system.set", json!({ "design_system": null })));
            }
            if state.design_systems.is_empty() {
                ui.small(RichText::new("공유 디자인 시스템이 없습니다").color(palette.ink_3));
            }
            for system in &state.design_systems {
                let picked = link.as_ref().is_some_and(|(source, revision)| *source == system.artifact_id && *revision == system.revision);
                let color = if picked { palette.accent } else { palette.ink_2 };
                let label = format!("{} · r{}", system.title, system.revision);
                if widgets::text_button(ui, palette, &label, color).clicked() && !picked {
                    action = Some((
                        "canvas.design_system.set",
                        json!({ "design_system": system.artifact_id, "expected_system_revision": system.revision }),
                    ));
                }
            }
            if widgets::labeled_button(ui, palette, Some("refresh"), "공유 목록 새로고침", false).clicked() {
                action = Some(("canvas.design_system.list", json!({})));
            }
        }
        if let Some((id, args)) = action {
            self.invoke_ui(id, args);
        }
    }

    /// The mapping list of the open file, below the layers, and the value edit of the mapping picked from it. The list is the
    /// engine's inspect of the selected file in the window's context. The value goes out as one change when its field is
    /// submitted. A refusal shows as the window's message, and the text stays in the field.
    pub(super) fn draw_web_mappings(&mut self, ui: &mut Ui, palette: &theme::Palette) {
        // The design system row first: it shows without a selected file.
        self.draw_web_design_system(ui, palette);
        let Some(document) = self.web_document() else { return };
        let Some(context) = self.context.as_ref() else { return };
        let Some(path) = context.selection.node_ids.first().filter(|path| document.files.contains_key(path.as_str())) else {
            return;
        };
        let Some(item) = context
            .selected_properties
            .get("nodes")
            .and_then(|nodes| nodes.get("items"))
            .and_then(Value::as_array)
            .and_then(|items| items.iter().find(|item| item.get("path").and_then(Value::as_str) == Some(path.as_str())))
        else {
            return;
        };
        let Some(mappings) = item.get("mappings").and_then(Value::as_array) else { return };
        let key = Id::new(("web-mapping", path.as_str()));
        ui.add_space(8.0);
        if mappings.is_empty() {
            // No mapping to edit: a value typed for one that has left does not come back with a later list.
            ui.data_mut(|data| data.remove::<MappingEdit>(key));
            // A file whose HTML the engine cannot scan carries `mapping_error`; its list is empty for that reason.
            let unscanned = item.get("mapping_error").is_some_and(|error| error.get("code").and_then(Value::as_str) == Some("WEB_HTML_UNSCANNED"));
            ui.label(if unscanned { "HTML을 읽지 못해 매핑을 찾을 수 없습니다" } else { "매핑된 요소가 없습니다" });
            return;
        }
        let mut edit: MappingEdit = ui.data_mut(|data| data.remove_temp(key)).unwrap_or_default();
        let mut chosen = false;
        for (id, tag) in mappings.iter().filter_map(|mapping| Some((mapping.get("id")?.as_str()?, mapping.get("tag")?.as_str()?))) {
            let picked = id == edit.mapping_id;
            chosen |= picked;
            let color = if picked { palette.accent } else { palette.ink_2 };
            if widgets::text_button(ui, palette, &format!("<{tag}> {id}"), color).clicked() && !picked {
                edit = MappingEdit { mapping_id: id.to_owned(), ..MappingEdit::default() };
                chosen = true;
            }
        }
        let mut submitted = None;
        if chosen {
            if let Some(index) = widgets::segmented(ui, palette, &["텍스트", "속성"], usize::from(edit.attribute)) {
                edit.attribute = index == 1;
            }
            if edit.attribute {
                let width = ui.available_width();
                widgets::text_field(ui, palette, &mut edit.name, "속성 이름", width);
            }
            let ready = edit.ready();
            if widgets::field_with_button(ui, &mut edit.draft, "새 값", "적용", ready, false) && edit.ready() {
                let property = if edit.attribute { format!("attr:{}", edit.name) } else { "text".to_owned() };
                submitted = Some(json!({ "path": path, "mapping_id": edit.mapping_id, "property": property, "value": edit.draft }));
            }
            ui.data_mut(|data| data.insert_temp(key, edit));
        }
        if let Some(args) = submitted {
            self.invoke_ui("web.mapped_edit", args);
        }
    }
}

#[cfg(test)]
mod design_system_tests {
    use std::time::{Duration, Instant};

    use designllm_contracts::{new_id, ArtifactBody, Intent, Operation, TokenValues};
    use serde_json::{json, Value};

    use crate::app::test_support::{content_of, Host, HostBackend, Mcp, Window};

    /// A published Canvas that defines the color token `token`. Returns its id and the published revision.
    fn publish_palette(host: &mut Host, token: &str) -> (String, u64) {
        let publisher = crate::app::test_support::canvas_with_page(host, "공유 원본");
        host.apply(
            &publisher,
            vec![Operation::PutToken {
                token_id: token.into(),
                name: "Brand ink".into(),
                values: TokenValues::Color { light: "#112233".into(), dark: "#445566".into() },
            }],
            [Intent::Design].into_iter().collect(),
        )
        .expect("the publisher defines its token");
        let (workspace_id, revision) = (host.workspace_id.clone(), host.artifact(&publisher).revision);
        let published = host
            .call(
                "design_system.publish",
                json!({
                    "request_id": new_id(), "actor": "desktop", "workspace_id": workspace_id, "artifact_id": publisher,
                    "expected_revision": revision, "publish": true, "allowed_intents": ["design"],
                }),
            )
            .expect("the publisher is published");
        (publisher, published["revision"].as_u64().expect("the published revision"))
    }

    /// The generated stylesheet and the entry file of a WebCode project.
    fn web_files(artifact: &designllm_contracts::Artifact) -> (String, String) {
        let ArtifactBody::WebCode(document) = &artifact.body else { panic!("a WebCode project") };
        (document.files["design-tokens.css"].content.clone(), document.files[&document.entry].content.clone())
    }

    /// Linking a WebCode project to a published design system gives the same project by the window's command and by the MCP
    /// tool `design_system_set` on a second project built the same way (BRIEF 17).
    #[test]
    fn web_design_system_gui_and_mcp_same_result() {
        let mut host = Host::start();
        let (publisher, published) = publish_palette(&mut host, "brand-ink");
        let gui = host.create("web_code", "웹 화면");
        let before = host.artifact(&gui);
        let mut window = Window::open(&mut host, &gui);
        window
            .step(&mut host, "canvas.design_system.set", json!({ "design_system": publisher, "expected_system_revision": published }))
            .expect("the window links the project to the published system");
        drop(window);
        let gui_after = host.artifact(&gui);
        assert_ne!(content_of(&before), content_of(&gui_after), "the command changed nothing");
        let (css, entry) = web_files(&gui_after);
        assert!(css.contains("--dl-brand-ink: #112233;") && css.contains("--dl-brand-ink: #445566;"), "the stylesheet holds both values");
        assert_eq!(entry.matches("<link rel=\"stylesheet\" href=\"design-tokens.css\">").count(), 1, "the entry links the stylesheet once");

        let backend = HostBackend::new(host);
        let mcp = Mcp::connect(backend.clone());
        let mcp_project = backend.with(|host| host.create("web_code", "웹 화면"));
        let (workspace_id, revision) = backend.with(|host| (host.workspace_id.clone(), host.artifact(&mcp_project).revision));
        mcp.call(
            "design_system_set",
            json!({
                "request_id": new_id(), "actor": "mcp", "workspace_id": workspace_id, "artifact_id": mcp_project,
                "expected_revision": revision, "design_system": publisher, "expected_system_revision": published,
                "allowed_intents": ["design", "content"],
            }),
        );
        let mcp_after = backend.with(|host| host.artifact(&mcp_project));
        assert_eq!(content_of(&gui_after), content_of(&mcp_after), "the command and the MCP tool left different content");
    }

    /// The intents of one command in the registry the host holds (`ui.command.list`), as an agent reads them.
    fn listed_intents(registry: &Value, id: &str) -> Vec<String> {
        registry["commands"]
            .as_array()
            .and_then(|commands| commands.iter().find(|command| command["id"] == id))
            .and_then(|command| command["intents"].as_array())
            .map(|intents| intents.iter().map(|intent| intent.as_str().expect("an intent name").to_owned()).collect())
            .expect("the command is listed with its intents")
    }

    /// The registry a WebCode window publishes lists the link with content as well as design, since the link writes the entry file.
    /// An agent run that allows the listed intents links the project. A Canvas window still lists design alone.
    #[test]
    fn webcode_listing_and_agent_run_carry_the_content_intent() {
        let mut host = Host::start();
        let (publisher, published) = publish_palette(&mut host, "brand-ink");
        let gui = host.create("web_code", "웹 화면");
        let mut window = Window::open(&mut host, &gui);
        window.app.publish_ui_if_due();
        window.settle(&mut host);
        let view_id = window.app.ui.view_id.clone();
        let registry = host.call("ui.command.list", json!({ "view_id": view_id })).expect("the WebCode registry is published");
        let intents = listed_intents(&registry, "canvas.design_system.set");
        assert!(intents.iter().any(|intent| intent == "content"), "a WebCode link writes the entry file, so content is listed: {intents:?}");
        assert!(intents.iter().any(|intent| intent == "design"), "the link is a design change too: {intents:?}");

        let state_seq = registry["state_seq"].as_u64().expect("the published state sequence");
        host.call(
            "ui.command.run",
            json!({
                "view_id": view_id, "expected_state_seq": state_seq, "command_id": "canvas.design_system.set",
                "actor": "agent", "permission": "change", "allowed_intents": intents,
                "args": { "design_system": publisher, "expected_system_revision": published },
            }),
        )
        .expect("the run with the listed intents is queued");
        window.app.last_event_poll = Instant::now() - Duration::from_secs(10);
        window.app.poll_events();
        window.settle(&mut host);
        assert_eq!(
            host.artifact(&gui).design_system.as_ref().map(|reference| reference.artifact_id.clone()),
            Some(publisher.clone()),
            "the run with the listed intents links the project"
        );
        drop(window);

        let canvas = host.create("canvas", "그림판");
        let mut canvas_window = Window::open(&mut host, &canvas);
        canvas_window.app.publish_ui_if_due();
        canvas_window.settle(&mut host);
        let canvas_view = canvas_window.app.ui.view_id.clone();
        let canvas_registry = host.call("ui.command.list", json!({ "view_id": canvas_view })).expect("the Canvas registry is published");
        assert_eq!(listed_intents(&canvas_registry, "canvas.design_system.set"), vec!["design".to_string()], "a Canvas link changes design alone");
    }

    /// The window detaches a WebCode project with `design_system: null`: the stylesheet keeps its values and the entry loses the link.
    #[test]
    fn a_webcode_detach_keeps_the_stylesheet_and_drops_the_link() {
        let mut host = Host::start();
        let (publisher, published) = publish_palette(&mut host, "brand-ink");
        let gui = host.create("web_code", "웹 화면");
        let mut window = Window::open(&mut host, &gui);
        window
            .step(&mut host, "canvas.design_system.set", json!({ "design_system": publisher, "expected_system_revision": published }))
            .expect("the window links the project");
        window.step(&mut host, "canvas.design_system.set", json!({ "design_system": null })).expect("the window detaches the project");
        drop(window);
        let after = host.artifact(&gui);
        assert!(after.design_system.is_none(), "the project is detached");
        let (css, entry) = web_files(&after);
        assert!(css.contains("--dl-brand-ink: #112233;") && css.contains("--dl-brand-ink: #445566;"), "the stylesheet keeps its values");
        assert!(!entry.contains("<link rel=\"stylesheet\" href=\"design-tokens.css\">"), "the entry loses the link");
    }

    /// A link over a stylesheet whose value another client changed while the project was detached is refused, and the window names
    /// the reason. The header stays, so the file still reads as generated and its edit is what the sync refuses. The edit is kept and
    /// the project stays detached. (While linked, Core refuses any edit to the generated stylesheet.)
    #[test]
    fn a_webcode_link_refused_by_an_edited_stylesheet_names_the_reason() {
        let mut host = Host::start();
        let (first, first_revision) = publish_palette(&mut host, "brand-ink");
        let (second, second_revision) = publish_palette(&mut host, "brand-ink");
        let gui = host.create("web_code", "웹 화면");
        let mut window = Window::open(&mut host, &gui);
        window
            .step(&mut host, "canvas.design_system.set", json!({ "design_system": first, "expected_system_revision": first_revision }))
            .expect("the window links the project to the first source");
        window.step(&mut host, "canvas.design_system.set", json!({ "design_system": null })).expect("the window detaches the project");
        drop(window);
        let (generated, _) = web_files(&host.artifact(&gui));
        let edited = generated.replacen("--dl-brand-ink: #112233;", "--dl-brand-ink: #ABCDEF;", 1);
        assert_ne!(edited, generated, "the light value is in the stylesheet");
        let expected_hash = match &host.artifact(&gui).body {
            ArtifactBody::WebCode(document) => document.files["design-tokens.css"].sha256.clone(),
            _ => panic!("a WebCode project"),
        };
        let edit = Operation::ReplaceWebSource { path: "design-tokens.css".into(), expected_hash, content: edited.clone() };
        host.apply(&gui, vec![edit], [Intent::Content].into_iter().collect()).expect("another client edits the detached stylesheet");

        let mut window = Window::open(&mut host, &gui);
        window.step(&mut host, "canvas.design_system.list", json!({})).expect("the window reads the published systems");
        window
            .step(&mut host, "canvas.design_system.set", json!({ "design_system": second, "expected_system_revision": second_revision }))
            .expect("the window sends the link to the second source");
        assert_eq!(window.message().as_deref(), Some("원본 파일이 바뀌었습니다. 다시 열어 주세요"), "the edited stylesheet is named");
        drop(window);
        let after = host.artifact(&gui);
        assert!(after.design_system.is_none(), "the refused link leaves the project detached");
        let (css, _) = web_files(&after);
        assert_eq!(css, edited, "the edit is kept");
    }

    /// A dotted token id is refused by the engine (WEB_TOKEN_ID). The window names that reason, and the project keeps its files
    /// and has no link.
    #[test]
    fn a_webcode_link_refused_for_a_dotted_token_id_names_the_reason() {
        let mut host = Host::start();
        let (publisher, published) = publish_palette(&mut host, "brand.ink");
        let gui = host.create("web_code", "웹 화면");
        let before = host.artifact(&gui);
        let mut window = Window::open(&mut host, &gui);
        window
            .step(&mut host, "canvas.design_system.set", json!({ "design_system": publisher, "expected_system_revision": published }))
            .expect("the window sends the link");
        assert_eq!(window.message().as_deref(), Some("디자인 토큰 이름은 a-z, 0-9, '-'만 쓸 수 있습니다"), "the engine's reason is named");
        drop(window);
        let after = host.artifact(&gui);
        assert_eq!(content_of(&before), content_of(&after), "a refused link changes no file");
        assert!(after.design_system.is_none(), "a refused link leaves the project unlinked");
    }
}
