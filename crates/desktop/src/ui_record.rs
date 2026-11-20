//! Where the house controls (`widgets`) were drawn in a frame, for the UI-test coordinate read `ui.layout` (task 69,
//! docs/UI_CHANNEL.md "시험용 입력"). Nothing is kept unless a UI-test window started the frame's recording, so an ordinary
//! window only checks an empty cell per control.

use std::cell::RefCell;

use eframe::egui::{Id, Rect, Response, Ui};

/// One control as the frame drew it: its label or hover text, its rectangle and whether it took clicks.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Control {
    pub label: String,
    /// The widget id (keyboard focus is told by it).
    pub id: Id,
    pub rect: Rect,
    pub enabled: bool,
    /// The rectangle meets the clip rectangle it was drawn in (a row scrolled out of its list does not).
    pub visible: bool,
    /// A person's approval: a synthetic press on it ends the run (`ui_test_input`, HUMAN_ONLY).
    pub human_only: bool,
}

thread_local! {
    /// The controls of the frame being drawn; None when no recording runs. The window draws on one thread.
    static FRAME: RefCell<Option<Vec<Control>>> = const { RefCell::new(None) };
}

/// Starts recording this thread's controls for a new frame, dropping what an unfinished frame left.
pub(crate) fn begin() {
    FRAME.with(|frame| *frame.borrow_mut() = Some(Vec::new()));
}

/// Stops the recording and returns the frame's controls in drawing order.
pub(crate) fn finish() -> Vec<Control> {
    FRAME.with(|frame| frame.borrow_mut().take()).unwrap_or_default()
}

/// Records a drawn control while a recording runs.
pub(crate) fn note(ui: &Ui, label: &str, response: &Response) {
    push(ui, label, response, false);
}

/// Records a control only a person may press (task 69 item 8: approvals). Its rectangle is reported, marked.
pub(crate) fn note_human_only(ui: &Ui, label: &str, response: &Response) {
    push(ui, label, response, true);
}

fn push(ui: &Ui, label: &str, response: &Response, human_only: bool) {
    FRAME.with(|frame| {
        if let Some(controls) = frame.borrow_mut().as_mut() {
            controls.push(Control {
                label: label.to_owned(),
                id: response.id,
                rect: response.rect,
                enabled: response.enabled(),
                visible: ui.clip_rect().intersects(response.rect),
                human_only,
            });
        }
    });
}
