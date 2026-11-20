use eframe::egui::{epaint::Shape, CentralPanel, Context, Event, Modifiers, PointerButton, Pos2, RawInput, Rect, Vec2};
use designllm_contracts::{Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, FlowDocument, PageSetup, Section};

use super::super::{DesktopApp, Screen};
use crate::{rpc::{RpcWorker, Work}, theme};

fn draw(ctx: &Context, app: &mut DesktopApp, events: Vec<Event>) -> eframe::egui::FullOutput {
    let mut input = RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 700.0))),
        ..Default::default()
    };
    input.events = events;
    ctx.run(input, |ctx| {
        CentralPanel::default().show(ctx, |ui| app.draw_properties(ui, &theme::palette(ctx)));
    })
}

fn text_center(output: &eframe::egui::FullOutput, expected: &str) -> Pos2 {
    fn find(shape: &Shape, expected: &str) -> Option<Pos2> {
        match shape {
            Shape::Text(text) if text.galley.text() == expected => Some(text.pos + text.galley.size() / 2.0),
            Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, expected)),
            _ => None,
        }
    }
    output.shapes.iter().find_map(|clipped| find(&clipped.shape, expected))
        .unwrap_or_else(|| panic!("properties panel did not draw {expected:?}"))
}

#[test]
fn flow_print_button_queues_the_pdf_all_export_menu_without_sending_an_export() {
    let (rpc, _replies, work) = RpcWorker::detached();
    let mut app = DesktopApp::new(rpc, "작업공간".into(), None, None);
    let body = ArtifactBody::Flow(FlowDocument {
        sections: vec![Section { id: "s1".into(), setup: PageSetup::default(), blocks: Vec::new() }],
        styles: Default::default(),
    });
    let mut artifact = Artifact::new(ArtifactKind::Docx, "보고서", "docx.v1", body);
    artifact.id = "flow-print-test".into();
    artifact.revision = 1;
    app.active = Some(ArtifactSummary {
        id: artifact.id.clone(), kind: artifact.kind, title: artifact.title.clone(), revision: artifact.revision,
        engine_id: artifact.engine_id.clone(), dirty: false, modified_at_ms: None, active_jobs: Vec::new(),
    });
    app.artifact = Some(artifact);
    app.screen = Screen::Editor;
    let ctx = Context::default();
    theme::install(&ctx);

    let initial = draw(&ctx, &mut app, Vec::new());
    let button = text_center(&initial, "PDF로 인쇄…");
    draw(&ctx, &mut app, vec![
        Event::PointerMoved(button),
        Event::PointerButton { pos: button, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
        Event::PointerButton { pos: button, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE },
    ]);

    assert_eq!(app.export_format, "pdf");
    assert_eq!(app.export_scope, "all");
    assert_eq!(app.smoke_open.as_deref(), Some("export"), "the existing export menu handles preview and loss consent");
    assert!(!work.try_iter().any(|item| matches!(item, Work::Call { method, .. } if method == "artifact.export")));
}
