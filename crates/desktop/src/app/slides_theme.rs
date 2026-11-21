//! The theme of a slides deck (task 49a): the command `slides.theme`, which sends one `SetTheme`, and its section in the
//! page properties. A control on screen and an agent on the UI channel send the same change set.

use designllm_contracts::{ArtifactBody, Operation, ThemePatch, THEME_COLOR_SLOTS};
use eframe::egui::{Id, Ui};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::properties::{row, section_head};
use crate::app::DesktopApp;
use crate::{commands::CommandSpec, theme, widgets};

/// The four faces of the theme fonts: the `ThemePatch` field and its label.
const FACES: [(&str, &str); 4] = [("major_latin", "제목 글꼴"), ("minor_latin", "본문 글꼴"), ("major_ea", "제목 한글"), ("minor_ea", "본문 한글")];

impl DesktopApp {
    pub(super) fn dispatch_slides_theme(&mut self, spec: &CommandSpec, args: &Value) -> Option<Result<(), String>> {
        (spec.id == "slides.theme").then(|| self.slides_theme(args))
    }

    /// The arguments are the `ThemePatch` of the change; the engine refuses a slot or a face it cannot write. The command
    /// runs in a slides editor only (`command_enabled_for_current_kind`).
    fn slides_theme(&mut self, args: &Value) -> Result<(), String> {
        let patch = ThemePatch::deserialize(args).map_err(|error| format!("테마 값: {error}"))?;
        if patch.is_empty() {
            return Err("바꿀 테마 값이 없습니다".into());
        }
        self.kind_change(vec![Operation::SetTheme { patch }])
    }

    /// `테마` in the page properties of a slides deck: one colour slot at a time, and the four faces. A field shows the
    /// deck's edited value; an empty field keeps the value of the file. A value is sent when its field loses focus.
    pub(in crate::app) fn slides_theme_section(&mut self, ui: &mut Ui, palette: &theme::Palette, first: &mut bool) {
        let Some(ArtifactBody::Slides(document)) = self.artifact.as_ref().map(|artifact| &artifact.body) else {
            return;
        };
        let stored = document.theme.clone().unwrap_or_default();
        let editable = self.ensure_interaction_revision_matches_context().is_ok();
        let mut sends: Vec<Value> = Vec::new();
        section_head(ui, palette, first, "테마", None);
        let slot_id = Id::new("slides-theme-slot");
        let mut slot = ui.data(|data| data.get_temp::<usize>(slot_id)).unwrap_or(4).min(THEME_COLOR_SLOTS.len() - 1);
        row(ui, palette, Some("색"), |ui| {
            if let Some(index) = widgets::select_menu(ui, palette, THEME_COLOR_SLOTS[slot], &THEME_COLOR_SLOTS, Some(slot), 88.0) {
                slot = index;
            }
            let name = THEME_COLOR_SLOTS[slot];
            let current = stored.colors.get(name).map_or("", String::as_str);
            if let Some(value) = committed_text(ui, Id::new(("slides-theme-colour", name)), current, "#RRGGBB", editable) {
                sends.push(json!({ "colors": { name: value } }));
            }
        });
        ui.data_mut(|data| data.insert_temp(slot_id, slot));
        for (field, label) in FACES {
            let current = match field {
                "major_latin" => stored.major_latin.as_deref(),
                "minor_latin" => stored.minor_latin.as_deref(),
                "major_ea" => stored.major_ea.as_deref(),
                _ => stored.minor_ea.as_deref(),
            }
            .unwrap_or("");
            row(ui, palette, Some(label), |ui| {
                if let Some(value) = committed_text(ui, Id::new(("slides-theme-face", field)), current, "", editable) {
                    sends.push(json!({ field: value }));
                }
            });
        }
        ui.add_space(8.0);
        for args in sends {
            self.invoke_ui("slides.theme", args);
        }
    }
}

/// A one-line text field whose draft lives while it has focus. Returns the draft when the field loses focus with a value
/// other than `current` and not blank.
fn committed_text(ui: &mut Ui, id: Id, current: &str, hint: &str, enabled: bool) -> Option<String> {
    let draft_id = id.with("draft");
    let mut draft = ui.data(|data| data.get_temp::<String>(draft_id)).unwrap_or_else(|| current.to_owned());
    let palette = theme::palette(ui.ctx());
    let width = ui.available_width();
    let response = widgets::edit_line(ui, &palette, &mut draft, widgets::Field { hint, width, id: Some(id), enabled, ..Default::default() });
    if response.has_focus() {
        ui.data_mut(|data| data.insert_temp(draft_id, draft.clone()));
    } else {
        ui.data_mut(|data| data.remove::<String>(draft_id));
    }
    (response.lost_focus() && draft != current && !draft.trim().is_empty()).then_some(draft)
}

#[cfg(test)]
mod tests {
    use designllm_contracts::{new_id, Artifact, ArtifactBody, Intent, Operation, ThemePatch};
    use serde_json::{json, Value};

    use crate::app::test_support::{change_set_arguments, content_of, Host, HostBackend, Mcp, Window};

    fn theme_of(artifact: &Artifact) -> Option<ThemePatch> {
        match &artifact.body {
            ArtifactBody::Slides(document) => document.theme.clone(),
            _ => panic!("a slides deck"),
        }
    }

    /// The operations of every `changes.apply` the window sent, by their `op`.
    fn applied_ops(window: &Window) -> Vec<Vec<String>> {
        window
            .calls
            .iter()
            .filter(|(method, _, _)| method == "changes.apply")
            .map(|(_, params, _)| {
                params["operations"].as_array().expect("operations").iter().map(|op| op["op"].as_str().unwrap_or_default().to_owned()).collect()
            })
            .collect()
    }

    /// The shared PPTX fixture, opened as a new slides artifact.
    fn imported_deck(host: &mut Host) -> String {
        let path = crate::app::test_support::repository_root().join("fixtures/office/basic-preserve.pptx");
        let opened = host
            .call("artifact.open", json!({ "request_id": new_id(), "actor": "test", "path": path.to_string_lossy(), "kind": "slides" }))
            .expect("the PPTX fixture opens");
        opened["id"].as_str().expect("an artifact id").to_owned()
    }

    /// `args` through the command on `gui` and the same `SetTheme` through the MCP `changes_apply` tool on `mcp_deck`:
    /// the two artifacts after it, and the tool's result.
    fn both_ways(mut host: Host, gui: &str, mcp_deck: &str, args: Value) -> (Artifact, Artifact, Value, Option<String>) {
        let mut window = Window::open(&mut host, gui);
        window.step(&mut host, "slides.theme", args.clone()).expect("the theme is sent");
        assert_eq!(applied_ops(&window), [["set_theme"]], "one change set with one SetTheme");
        let message = window.message();
        let gui_after = host.artifact(gui);
        drop(window);
        let backend = HostBackend::new(host);
        let mcp = Mcp::connect(backend.clone());
        let patch: ThemePatch = serde_json::from_value(args).expect("the arguments are a ThemePatch");
        let (workspace, revision) = backend.with(|host| (host.workspace_id.clone(), host.artifact(mcp_deck).revision));
        let result = mcp.call_result("changes_apply", change_set_arguments(&workspace, mcp_deck, revision, &[Intent::Design], vec![Operation::SetTheme { patch }]));
        let mcp_after = backend.with(|host| host.artifact(mcp_deck));
        (gui_after, mcp_after, result, message)
    }

    #[test]
    fn slides_theme_gui_and_mcp_produce_same_artifact() {
        assert_eq!(crate::commands::core_calls("slides.theme"), Some(&["changes.apply:set_theme"][..]), "the declared core calls");
        let args = json!({ "colors": { "accent1": "#a1b2c3", "dk2": "#102030" }, "minor_latin": "Noto Sans", "minor_ea": "Noto Sans KR" });
        // A new deck, and an imported PPTX.
        for imported in [false, true] {
            let mut host = Host::start();
            let (gui, mcp_deck) = if imported {
                (imported_deck(&mut host), imported_deck(&mut host))
            } else {
                (host.create("slides", "테마"), host.create("slides", "테마"))
            };
            let before = host.artifact(&gui);
            let (gui_after, mcp_after, result, _) = both_ways(host, &gui, &mcp_deck, args.clone());
            assert_ne!(result["isError"], true, "{result}");
            assert_eq!(theme_of(&gui_after).expect("the command stored the theme").colors["accent1"], "#A1B2C3");
            assert_ne!(content_of(&before), content_of(&gui_after), "the command changed nothing");
            assert_eq!(content_of(&gui_after), content_of(&mcp_after), "the command and the MCP tool left different content (imported {imported})");
            assert_eq!((gui_after.revision, mcp_after.revision), (1, 1));
        }
    }

    #[test]
    fn slides_theme_gui_and_mcp_refuse_the_same_bad_value_alike() {
        let mut host = Host::start();
        let (gui, mcp_deck) = (imported_deck(&mut host), imported_deck(&mut host));
        let before = content_of(&host.artifact(&gui));
        let (gui_after, mcp_after, result, message) = both_ways(host, &gui, &mcp_deck, json!({ "colors": { "accent7": "#000000" } }));
        assert_eq!(result["isError"], true, "the tool reports the refusal: {result}");
        assert_eq!(result["structuredContent"]["code"], "INVALID_ARGUMENT", "{result}");
        assert!(message.is_some(), "the window shows the refusal");
        assert_eq!((gui_after.revision, mcp_after.revision), (0, 0));
        assert_eq!(content_of(&gui_after), before, "the command changed the deck");
        assert_eq!(content_of(&mcp_after), before, "the tool changed the deck");
    }

    #[test]
    fn the_theme_command_runs_in_a_slides_editor_only_and_refuses_a_bad_shape_without_sending() {
        let mut host = Host::start();
        let canvas = host.create("canvas", "그림판");
        let mut window = Window::open(&mut host, &canvas);
        assert!(!window.app.command_enabled_for_current_kind("slides.theme"), "a canvas lists the command as disabled");
        assert!(window.step(&mut host, "slides.theme", json!({ "colors": { "accent1": "#000000" } })).is_err(), "not a deck");
        assert!(applied_ops(&window).is_empty(), "nothing was sent");
        drop(window);

        let deck = host.create("slides", "테마");
        let mut window = Window::open(&mut host, &deck);
        assert!(window.app.command_enabled_for_current_kind("slides.theme"));
        assert!(window.step(&mut host, "slides.theme", json!({ "colors": { "accent1": "red" } })).is_err(), "the schema refuses a non-hex colour");
        assert!(window.step(&mut host, "slides.theme", json!({})).is_err(), "an empty patch is not sent");
        assert!(window.step(&mut host, "slides.theme", json!({ "accent1": "#000000" })).is_err(), "a colour outside colors is refused");
        assert!(applied_ops(&window).is_empty(), "nothing was sent");
        drop(window);
        assert_eq!(host.artifact(&deck).revision, 0);
    }
}
