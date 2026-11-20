//! The arrow keys move the selection (task 68 item 15) through `edit.nudge`, as the menu and the agent do. A press moves 1 px on a
//! canvas or a slide and 10 px with Shift, as design tools do; in a document, whose unit is pt, it would be 1 pt and 10 pt
//! (`step_emu`), but `edit.nudge` has no target outside canvases and slides, so the keys act there only.
//!
//! A press is one change (one undo step). Presses made while the window waits for its last change, or for the reads that follow
//! it, are kept and go out together once the window shows that change: a held key never sends against a revision the window has
//! not read, so key repeat makes no REVISION_CONFLICT.
//!
//! The arrows are the selection's only on the editor screen with no dialog, modal, menu or popup open and no widget holding the
//! keyboard. A focused widget keeps them (a field moves its caret, egui moves a focused button's focus), so the two never act on
//! one press. Locked nodes stay: of a selection with unlocked nodes only those move (`move_selected_by`); a selection of locked
//! nodes only moves nothing and says so once for that selection.

use std::time::Instant;

use designllm_contracts::{ArtifactKind, EMU_PER_PT};
use eframe::egui::{self, Context, Event, Key};
use serde_json::json;

use super::{
    properties::{unit_for, Unit, EMU_PER_PX},
    DesktopApp,
};

/// What the notice says when only locked nodes are selected.
pub(super) const LOCKED_NUDGE: &str = "잠긴 항목은 옮길 수 없습니다";

/// The arrow keys' state between frames.
#[derive(Default)]
pub(super) struct NudgeKeys {
    /// Presses not sent yet because the window waits for its last change.
    held: Option<HeldNudge>,
    /// The locked selection the notice was shown for.
    locked_notice: Option<Vec<String>>,
}

/// Presses kept for one artifact and selection, in EMU.
struct HeldNudge {
    artifact_id: String,
    selection: Vec<String>,
    dx: f64,
    dy: f64,
}

/// The EMU one press moves in a document whose unit is `unit`: 1 px (10 px with Shift) where it is px, 1 pt (10 pt) where it is
/// mm, since a document's sizes read in pt.
pub(super) fn step_emu(unit: Unit, shift: bool) -> f64 {
    let one = match unit {
        Unit::Px => EMU_PER_PX,
        Unit::Mm => EMU_PER_PT as f64,
    };
    if shift { one * 10.0 } else { one }
}

/// The unit direction of an arrow press (y grows downward) and whether Shift is held, when the press is the selection's: Ctrl,
/// Alt and Cmd combinations belong to other shortcuts.
fn arrow_direction(event: &Event) -> Option<((f64, f64), bool)> {
    let Event::Key { key, pressed: true, modifiers, .. } = event else { return None };
    if modifiers.ctrl || modifiers.alt || modifiers.mac_cmd || modifiers.command {
        return None;
    }
    let direction = match key {
        Key::ArrowLeft => (-1.0, 0.0),
        Key::ArrowRight => (1.0, 0.0),
        Key::ArrowUp => (0.0, -1.0),
        Key::ArrowDown => (0.0, 1.0),
        _ => return None,
    };
    Some((direction, modifiers.shift))
}

impl DesktopApp {
    /// Takes this frame's arrow presses out of the input when they are the selection's, and sends what is due.
    pub(super) fn handle_nudge_keys(&mut self, ctx: &Context) {
        let pressed = ctx.input(|input| input.events.iter().any(|event| arrow_direction(event).is_some()));
        if !pressed && self.nudge.held.is_none() {
            return;
        }
        let kind = self.active.as_ref().map(|active| active.kind);
        let Some(kind) = kind.filter(|kind| matches!(kind, ArtifactKind::Canvas | ArtifactKind::Slides)) else {
            self.nudge.held = None;
            return;
        };
        if self.selected_nodes.is_empty() || !self.nudge_keys_free(ctx) {
            self.nudge.held = None;
            return;
        }
        let unit = unit_for(kind);
        let (mut dx, mut dy, mut presses) = (0.0, 0.0, 0usize);
        ctx.input_mut(|input| {
            input.events.retain(|event| {
                let Some(((x, y), shift)) = arrow_direction(event) else { return true };
                let step = step_emu(unit, shift);
                dx += x * step;
                dy += y * step;
                presses += 1;
                false
            });
        });
        if presses > 0 && !self.hold_nudge(dx, dy) {
            return;
        }
        if !self.nudge_settled() {
            return;
        }
        // Presses kept for another selection or document are dropped, not sent to this one.
        let artifact_id = self.active.as_ref().map(|active| active.id.as_str());
        let due = self.nudge.held.take().filter(|held| {
            Some(held.artifact_id.as_str()) == artifact_id && held.selection == self.selected_nodes && (held.dx != 0.0 || held.dy != 0.0)
        });
        if let Some(held) = due {
            let per_unit = unit.emu();
            self.invoke_ui("edit.nudge", json!({ "x": held.dx / per_unit, "y": held.dy / per_unit }));
        }
    }

    /// Adds a frame's presses to the ones kept for the current selection. False when the selection is locked nodes only: nothing
    /// is kept, and the notice says why once for that selection.
    fn hold_nudge(&mut self, dx: f64, dy: f64) -> bool {
        let artifact = self.artifact.as_ref();
        let locked: Vec<bool> = self.selected_nodes.iter()
            .filter_map(|id| artifact.and_then(|artifact| super::doc_tree::find_node(artifact, id)))
            .map(super::multi::is_locked)
            .collect();
        if !locked.is_empty() && locked.iter().all(|locked| *locked) {
            self.nudge.held = None;
            if self.nudge.locked_notice.as_ref() != Some(&self.selected_nodes) {
                self.nudge.locked_notice = Some(self.selected_nodes.clone());
                self.notices.info(LOCKED_NUDGE, Instant::now());
            }
            return false;
        }
        self.nudge.locked_notice = None;
        let artifact_id = self.active.as_ref().map(|active| active.id.clone()).unwrap_or_default();
        match self.nudge.held.as_mut() {
            Some(held) if held.artifact_id == artifact_id && held.selection == self.selected_nodes => {
                held.dx += dx;
                held.dy += dy;
            }
            _ => self.nudge.held = Some(HeldNudge { artifact_id, selection: self.selected_nodes.clone(), dx, dy }),
        }
        true
    }

    /// Whether the window shows the revision it last committed: no change of it is on its way, and its context and artifact are
    /// read at that revision.
    fn nudge_settled(&self) -> bool {
        let Some(revision) = self.active.as_ref().map(|active| active.revision) else { return false };
        !self.pending.values().any(|pending| pending.apply.is_some())
            && self.context.as_ref().is_some_and(|context| context.revision == revision)
            && self.artifact.as_ref().is_some_and(|artifact| artifact.revision == revision)
    }

    /// Whether the arrow keys are the selection's: the editor shows, no widget has the keyboard, and no dialog, modal, menu,
    /// popup, text editor or drag is open.
    fn nudge_keys_free(&self, ctx: &Context) -> bool {
        self.screen_is_editor()
            && !ctx.wants_keyboard_input()
            && !egui::Popup::is_any_open(ctx)
            && ctx.memory(|memory| memory.top_modal_layer()).is_none()
            && self.drag.is_none()
            && self.inline_text_edit.is_none()
            && self.layer_rename.is_none()
            && self.image_crop_editor.is_none()
            && !self.board_memo_editor_shown()
            && self.board_memo_mutation.is_none()
            && self.page_section_editor.is_none()
            && self.page_section_mutation.is_none()
            && self.export_confirmation.is_none()
            && self.figure_form.is_none()
            && !self.close_confirmation_open
            && self.pending_switch.is_none()
            && !self.has_home_confirmation()
            && self.drafts.recovered().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A press is 1 px (9,525 EMU) where the unit is px, 10 px with Shift; 1 pt (12,700 EMU) and 10 pt where it is mm.
    #[test]
    fn a_press_moves_one_px_or_one_pt_by_the_document_unit_and_ten_with_shift() {
        assert_eq!(step_emu(Unit::Px, false), 9_525.0);
        assert_eq!(step_emu(Unit::Px, true), 95_250.0);
        assert_eq!(step_emu(Unit::Mm, false), 12_700.0);
        assert_eq!(step_emu(Unit::Mm, true), 127_000.0);
        assert_eq!(unit_for(ArtifactKind::Canvas), Unit::Px);
        assert_eq!(unit_for(ArtifactKind::Slides), Unit::Px);
        assert_eq!(unit_for(ArtifactKind::Docx), Unit::Mm, "a document's step is the pt one");
        // The command's argument is in the document's unit; the window rounds it back to the same EMU (`edit.nudge`).
        let per_mm = Unit::Mm.emu();
        assert_eq!((step_emu(Unit::Mm, false) / per_mm * per_mm).round() as i64, 12_700);
        assert_eq!((step_emu(Unit::Mm, true) / per_mm * per_mm).round() as i64, 127_000);
    }
}
