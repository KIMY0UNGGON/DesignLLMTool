//! Design warnings on canvases and slides (tokens `component.lint-warning`): the engine's lint findings are warnings,
//! not refusals. The window keeps the current revision's findings in `lint_warnings` (from `artifact.validate` after
//! every refresh, and from each change result at once), marks every warned node on the board with an 18 px marker at
//! its top right, and shows `state-warn` in its layer row. The callout is the selection overlay's (selection.rs).

use designllm_contracts::{ArtifactKind, Diagnostic};
use eframe::egui::{self, Context, CornerRadius, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use serde_json::{json, Value};

use super::{DesktopApp, PendingAction};
use crate::{icons, strings, theme};

/// The marker's side (tokens `lint-warning.marker.box.size`).
const MARKER: f32 = 18.0;
/// The gap between the node's frame and the marker.
const MARKER_GAP: f32 = 4.0;

/// The warnings among `diagnostics`: non-blocking lint findings that name a node, one per node and rule (the canvas
/// reports clipped text both as TEXT_OVERFLOW and TEXT_CLIPPED).
pub(super) fn warnings_of(diagnostics: &[Diagnostic]) -> Vec<Diagnostic> {
    let mut kept: Vec<Diagnostic> = Vec::new();
    // The lint's own finding wins over the layout's note for the same rule (TEXT_CLIPPED over TEXT_OVERFLOW).
    let layout_note = |diagnostic: &&Diagnostic| diagnostic.code.eq_ignore_ascii_case("TEXT_OVERFLOW");
    let ordered = diagnostics.iter().filter(|diagnostic| !layout_note(diagnostic)).chain(diagnostics.iter().filter(layout_note));
    for diagnostic in ordered.filter(|diagnostic| !diagnostic.blocking && diagnostic.node_id.is_some()) {
        let Some(label) = strings::lint_label(&diagnostic.code) else {
            continue;
        };
        let duplicate = kept.iter().any(|other| other.node_id == diagnostic.node_id && strings::lint_label(&other.code) == Some(label));
        if !duplicate {
            kept.push(diagnostic.clone());
        }
    }
    kept
}

/// Where a node's marker goes: left = frame right + 4, bottom = frame top − 4; past the board's right edge it moves
/// inside the frame's top right corner.
pub(super) fn marker_rect(frame: Rect, board: Rect) -> Rect {
    let outside = Rect::from_min_size(Pos2::new(frame.right() + MARKER_GAP, frame.top() - MARKER_GAP - MARKER), Vec2::splat(MARKER));
    if outside.right() <= board.right() {
        outside
    } else {
        Rect::from_min_size(Pos2::new(frame.right() - MARKER_GAP - MARKER, frame.top() + MARKER_GAP), Vec2::splat(MARKER))
    }
}

/// The nodes that get a board marker, in warning order: every warned node, except the one node selected alone, whose
/// warning callout is open (lead ruling, tokens `lint-warning.marker.when-selected`; its layer icon stays).
pub(super) fn marker_ids(warnings: &[Diagnostic], selected: &[String]) -> Vec<String> {
    let callout_open = match selected {
        [only] => Some(only.as_str()),
        _ => None,
    };
    let mut ids: Vec<String> = Vec::new();
    for id in warnings.iter().filter_map(|warning| warning.node_id.as_deref()) {
        if Some(id) != callout_open && !ids.iter().any(|kept| kept == id) {
            ids.push(id.to_owned());
        }
    }
    ids
}

impl DesktopApp {
    /// Asks for the findings of the open canvas or slides at its current revision.
    pub(super) fn request_lint(&mut self) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if !matches!(active.kind, ArtifactKind::Canvas | ArtifactKind::Slides) {
            self.lint_warnings.clear();
            return;
        }
        let artifact_id = active.id.clone();
        self.call("artifact.validate", json!({ "artifact_id": artifact_id }), PendingAction::LintWarnings { artifact_id });
    }

    /// The reply of `artifact.validate` or a change result: its warnings replace the ones shown.
    pub(super) fn take_lint(&mut self, artifact_id: &str, value: &Value) {
        if self.active.as_ref().is_none_or(|active| active.id != artifact_id) {
            return;
        }
        let diagnostics: Vec<Diagnostic> = value.get("diagnostics").and_then(|list| serde_json::from_value(list.clone()).ok()).unwrap_or_default();
        self.lint_warnings = warnings_of(&diagnostics);
    }

    /// One marker per warned node, always shown. Pressing it selects the node (its callout then opens); the hint
    /// joins the rule names with ` · `.
    pub(super) fn draw_lint_markers(&mut self, ctx: &Context, ui: &mut Ui, palette: &theme::Palette, origin: Pos2, zoom: f32) {
        if self.lint_warnings.is_empty() {
            return;
        }
        let Some(board) = self.board_rect else {
            return;
        };
        let frames = self.node_frames(origin, zoom);
        let ids = marker_ids(&self.lint_warnings, &self.selected_nodes);
        let mut pressed = None;
        for id in ids {
            let Some(frame) = frames.iter().find_map(|(frame_id, rect)| (*frame_id == id).then_some(*rect)) else {
                continue;
            };
            let rect = marker_rect(frame, board);
            if !board.intersects(rect) {
                continue;
            }
            let painter = ui.painter();
            painter.rect_filled(rect, CornerRadius::same((MARKER / 2.0) as u8), palette.warn_soft);
            painter.rect_stroke(rect, CornerRadius::same((MARKER / 2.0) as u8), Stroke::new(1.0, palette.derived(theme::Derived::WarnRing)), StrokeKind::Inside);
            icons::paint(ui, Rect::from_center_size(rect.center(), Vec2::splat(12.0)), "state-warn", palette.warn);
            let labels: Vec<&str> = self
                .lint_warnings
                .iter()
                .filter(|warning| warning.node_id.as_deref() == Some(id.as_str()))
                .filter_map(|warning| strings::lint_label(&warning.code))
                .collect();
            let response = ui.interact(rect, egui::Id::new(("lint-marker", id.as_str())), Sense::click()).on_hover_text(crate::i18n::tr(&labels.join(" · ")));
            if response.clicked() {
                pressed = Some(id);
            }
        }
        if let Some(id) = pressed {
            self.select_ids(vec![id]);
            ctx.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(code: &str, node: &str, blocking: bool) -> Diagnostic {
        serde_json::from_value(json!({ "code": code, "message": "m", "node_id": node, "blocking": blocking })).expect("a diagnostic")
    }

    #[test]
    fn only_non_blocking_lint_findings_on_a_node_are_warnings_once_per_rule() {
        let diagnostics = vec![
            finding("TEXT_CLIPPED", "a", false),
            finding("TEXT_OVERFLOW", "a", false),
            finding("CONTRAST_LOW", "a", false),
            finding("TOUCH_TARGET_SMALL", "b", true),
            finding("IMAGE_ASSET", "c", false),
        ];
        let codes: Vec<(String, String)> =
            warnings_of(&diagnostics).into_iter().map(|warning| (warning.code, warning.node_id.unwrap_or_default())).collect();
        assert_eq!(codes, [("TEXT_CLIPPED".to_owned(), "a".to_owned()), ("CONTRAST_LOW".to_owned(), "a".to_owned())]);
    }

    #[test]
    fn the_node_selected_alone_shows_its_callout_instead_of_its_marker() {
        let warnings = vec![finding("TEXT_CLIPPED", "a", false), finding("CONTRAST_LOW", "a", false), finding("OUT_OF_PAGE", "b", false)];
        let ids = |selected: &[&str]| marker_ids(&warnings, &selected.iter().map(|id| id.to_string()).collect::<Vec<_>>());
        assert_eq!(ids(&[]), ["a", "b"], "one marker per node");
        assert_eq!(ids(&["a"]), ["b"], "a's callout is open");
        assert_eq!(ids(&["a", "b"]), ["a", "b"], "several selected: markers only");
        assert_eq!(ids(&["c"]), ["a", "b"]);
    }

    #[test]
    fn the_marker_sits_outside_the_top_right_and_moves_in_at_the_board_edge() {
        let board = Rect::from_min_max(Pos2::new(272.0, 44.0), Pos2::new(1120.0, 900.0));
        let frame = Rect::from_min_max(Pos2::new(400.0, 300.0), Pos2::new(500.0, 360.0));
        let outside = marker_rect(frame, board);
        assert_eq!((outside.left(), outside.bottom(), outside.width()), (504.0, 296.0, 18.0));
        let edge = Rect::from_min_max(Pos2::new(1000.0, 300.0), Pos2::new(1110.0, 360.0));
        let inside = marker_rect(edge, board);
        assert_eq!((inside.right(), inside.top()), (1106.0, 304.0));
    }
}
