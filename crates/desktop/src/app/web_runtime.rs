//! A-110: whether this build can open WebView2 previews. The answer is read once per process from the web-preview runtime
//! rule (`designllm_web_preview::runtime_unavailable_reason`). Without a runtime the commands that open a preview window are
//! off, with the reason; editing stays as it is (GUI_DESIGN.md §8: the reason and the code editor remain).

use std::sync::OnceLock;

use super::DesktopApp;

/// The commands that open a WebView2 preview window.
pub(super) const WEB_RUNTIME_COMMANDS: &[&str] = &["web.preview", "view.play"];

/// macOS and Linux have no WebView2: the preview is a Windows feature there.
const NO_RUNTIME: &str = if cfg!(windows) {
    "웹 미리보기를 사용하려면 WebView2 런타임을 설치한 뒤 앱을 다시 시작하세요. 코드 편집은 가능합니다."
} else {
    "웹 미리보기는 Windows에서만 열 수 있습니다. 코드 편집은 가능합니다."
};

/// The reason this process cannot open previews, or `None`. Asked once; the technical detail goes to the log of the run.
pub(super) fn web_runtime_reason_at_start() -> Option<String> {
    static REASON: OnceLock<Option<String>> = OnceLock::new();
    REASON
        .get_or_init(|| {
            designllm_web_preview::runtime_unavailable_reason().map(|detail| {
                eprintln!("web preview off: {detail}");
                NO_RUNTIME.to_owned()
            })
        })
        .clone()
}

impl DesktopApp {
    /// Why `id` cannot run for want of a preview runtime, or `None`.
    pub(super) fn web_runtime_refusal(&self, id: &str) -> Option<&str> {
        WEB_RUNTIME_COMMANDS.contains(&id).then_some(self.web_runtime_unavailable.as_deref()).flatten()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::test_support::{canvas_with_page, drawn_text, screen, Host, Window};

    fn listed_enabled(host: &mut Host, window: &mut Window, id: &str) -> bool {
        window.app.publish_ui_if_due();
        window.settle(host);
        let list = host.call("ui.command.list", json!({ "view_id": window.app.ui.view_id.clone() })).expect("the registry");
        list["commands"].as_array().unwrap().iter().find(|command| command["id"] == id).expect("listed")["enabled"] == true
    }

    #[test]
    fn without_a_preview_runtime_the_preview_commands_are_off_with_the_reason_and_editing_stays() {
        let mut host = Host::start();
        let canvas = canvas_with_page(&mut host, "재생");
        let mut window = Window::open(&mut host, &canvas);
        // The direct canvas play button follows runtime availability.
        window.app.web_runtime_unavailable = None;
        assert!(window.app.command_enabled_for_current_kind("view.play"), "with a runtime the canvas plays");

        window.app.web_runtime_unavailable = Some(super::NO_RUNTIME.to_owned());
        window.app.ui.commands_sent = false;
        assert!(!window.app.command_enabled_for_current_kind("view.play"), "no runtime: play is off");
        assert!(listed_enabled(&mut host, &mut window, "add.rect"), "editing stays, for an agent too");
        assert_eq!(window.step(&mut host, "view.play", json!({})), Err(super::NO_RUNTIME.to_owned()), "the refusal names the reason");
        window.step(&mut host, "add.rect", json!({})).expect("a rectangle is still added");

        let web = host.create("web_code", "웹");
        let mut window = Window::open(&mut host, &web);
        window.app.web_runtime_unavailable = Some(super::NO_RUNTIME.to_owned());
        assert!(!window.app.command_enabled_for_current_kind("web.preview"), "no runtime: the web preview is off");
        assert!(window.app.command_enabled_for_current_kind("web.replace_source"), "the code editing stays");
        assert_eq!(window.step(&mut host, "web.preview", json!({})), Err(super::NO_RUNTIME.to_owned()));
        window.draw(&mut host, 1.0);
        let output = window.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
            window.app.draw_frame(ctx);
        });
        let shown = drawn_text(&output);
        assert!(shown.iter().any(|text| text == super::NO_RUNTIME), "the board shows the reason: {shown:?}");
        if cfg!(windows) {
            assert!(super::NO_RUNTIME.contains("설치한 뒤 앱을 다시 시작하세요"), "the reason explains when to restart");
        }
        assert!(!shown.iter().any(|text| text == "웹 미리보기 열기"), "and no preview button");
    }
}
