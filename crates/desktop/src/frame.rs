//! Where the editor's parts go in one frame (GUI_DESIGN §4, §11). Pure geometry, so the rules can be
//! checked without a window: the top bar, the left sidebar, the right properties panel, the board
//! and the floating groups on the board.

use eframe::egui::{Pos2, Rect, Vec2};

use crate::theme;

/// The board keeps at least this much width when both side panels are docked (§11).
pub const BOARD_MIN_WITH_BOTH: f32 = 640.0;
/// With only the properties panel docked the board keeps this much (800 px windows: 320 + 480).
pub const BOARD_MIN_WITH_ONE: f32 = 480.0;
/// Space kept between floating groups and the board edge.
pub const FLOAT_MARGIN: f32 = 12.0;
/// Distance between a selection and the toolbar above it (§4).
pub const SELECTION_TOOLBAR_GAP: f32 = 10.0;

/// The parts of one editor frame in screen points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameLayout {
    pub top: Rect,
    /// The left sidebar. Present when it is open; `left_docked` says whether it takes room from
    /// the board or floats over it.
    pub left: Option<Rect>,
    pub left_docked: bool,
    pub right: Option<Rect>,
    pub board: Rect,
}

/// Clamps a panel width to its allowed range.
pub fn clamp_sidebar(width: f32) -> f32 {
    width.clamp(theme::SIDEBAR_MIN, theme::SIDEBAR_MAX)
}

pub fn clamp_panel(width: f32) -> f32 {
    width.clamp(theme::PANEL_MIN, theme::PANEL_MAX)
}

/// The frame for a window `screen` with the given panels.
pub fn editor_frame(screen: Rect, left_open: bool, left_width: f32, right_open: bool, right_width: f32) -> FrameLayout {
    let top = Rect::from_min_size(screen.min, Vec2::new(screen.width(), theme::TOP_BAR_HEIGHT));
    let rest = Rect::from_min_max(Pos2::new(screen.left(), top.bottom()), screen.max);
    let left_width = clamp_sidebar(left_width);
    // The properties panel keeps 480 px for the board, so it narrows on small windows.
    let right_width = if right_open {
        clamp_panel(right_width).min((screen.width() - BOARD_MIN_WITH_ONE).max(theme::PANEL_MIN))
    } else {
        0.0
    };
    let left_docked = left_open && rest.width() - left_width - right_width >= BOARD_MIN_WITH_BOTH;
    let left = left_open.then(|| {
        Rect::from_min_size(rest.min, Vec2::new(left_width, rest.height()))
    });
    let right = right_open.then(|| {
        Rect::from_min_size(
            Pos2::new(rest.right() - right_width, rest.top()),
            Vec2::new(right_width, rest.height()),
        )
    });
    let board_left = if left_docked { rest.left() + left_width } else { rest.left() };
    let board_right = rest.right() - right_width;
    let board = Rect::from_min_max(Pos2::new(board_left, rest.top()), Pos2::new(board_right.max(board_left), rest.bottom()));
    FrameLayout { top, left, left_docked, right, board }
}

/// Where the selection toolbar goes: above the selection, or below it when the space above is
/// taken by the board's top edge or by a floating group that must stay visible (§4).
pub fn selection_toolbar(selection: Rect, board: Rect, size: Vec2, keep_clear: Option<Rect>) -> Rect {
    let above_top = selection.top() - SELECTION_TOOLBAR_GAP - size.y;
    let mut top = above_top;
    let candidate = |top: f32| Rect::from_min_size(Pos2::new(selection.center().x - size.x / 2.0, top), size);
    let fits_above = top >= board.top() + FLOAT_MARGIN
        && keep_clear.is_none_or(|group| !candidate(top).intersects(group));
    if !fits_above {
        top = selection.bottom() + SELECTION_TOOLBAR_GAP;
    }
    let mut rect = candidate(top);
    // Keep it inside the board horizontally, then vertically as a last resort.
    if rect.left() < board.left() + FLOAT_MARGIN {
        rect = rect.translate(Vec2::new(board.left() + FLOAT_MARGIN - rect.left(), 0.0));
    }
    if rect.right() > board.right() - FLOAT_MARGIN {
        rect = rect.translate(Vec2::new(board.right() - FLOAT_MARGIN - rect.right(), 0.0));
    }
    if rect.bottom() > board.bottom() - FLOAT_MARGIN {
        rect = rect.translate(Vec2::new(0.0, board.bottom() - FLOAT_MARGIN - rect.bottom()));
    }
    rect
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Pos2;

    fn screen(width: f32, height: f32) -> Rect {
        Rect::from_min_size(Pos2::ZERO, Vec2::new(width, height))
    }

    /// Overlap with shared edges not counted (touching panels are fine).
    fn overlaps(a: Rect, b: Rect) -> bool {
        a.min.x < b.max.x - 0.01 && b.min.x < a.max.x - 0.01 && a.min.y < b.max.y - 0.01 && b.min.y < a.max.y - 0.01
    }

    fn inside(inner: Rect, outer: Rect) -> bool {
        inner.left() >= outer.left() - 0.01
            && inner.top() >= outer.top() - 0.01
            && inner.right() <= outer.right() + 0.01
            && inner.bottom() <= outer.bottom() + 0.01
    }

    #[test]
    fn at_1440_both_panels_are_docked_and_the_board_takes_the_rest() {
        let layout = editor_frame(screen(1440.0, 900.0), true, 272.0, true, 320.0);
        assert!(layout.left_docked);
        assert_eq!(layout.board.width(), 1440.0 - 272.0 - 320.0);
        assert_eq!(layout.board.top(), theme::TOP_BAR_HEIGHT);
        assert_eq!(layout.board.bottom(), 900.0);
    }

    #[test]
    fn the_left_sidebar_docks_only_when_the_board_keeps_640_px() {
        // 1232 - 272 - 320 = 640 exactly: docked. One pixel less: it floats over the board.
        let docked = editor_frame(screen(1232.0, 700.0), true, 272.0, true, 320.0);
        assert!(docked.left_docked);
        assert_eq!(docked.board.width(), 640.0);
        let floating = editor_frame(screen(1231.0, 700.0), true, 272.0, true, 320.0);
        assert!(!floating.left_docked);
        assert_eq!(floating.board.left(), 0.0, "a floating sidebar does not move the board");
        assert!(floating.left.is_some(), "the sidebar is still drawn, over the board");
    }

    #[test]
    fn at_800_the_properties_panel_docks_and_the_board_keeps_480() {
        let layout = editor_frame(screen(800.0, 560.0), true, 272.0, true, 320.0);
        assert!(!layout.left_docked);
        let right = layout.right.expect("right panel");
        assert_eq!(right.width(), 320.0);
        assert_eq!(layout.board.width(), 480.0);
    }

    #[test]
    fn a_wide_properties_panel_narrows_on_a_small_window_but_keeps_its_minimum() {
        let layout = editor_frame(screen(800.0, 560.0), false, 272.0, true, 420.0);
        let right = layout.right.expect("right panel");
        assert_eq!(right.width(), 320.0, "420 would leave the board 380 < 480");
        assert!(layout.board.width() >= 480.0);
    }

    #[test]
    fn every_part_stays_on_the_screen_and_the_board_does_not_overlap_the_panels() {
        for width in [800.0_f32, 960.0, 1232.0, 1440.0, 1920.0] {
            for (left_open, right_open) in [(true, true), (true, false), (false, true), (false, false)] {
                let frame = editor_frame(screen(width, 900.0), left_open, 300.0, right_open, 360.0);
                let whole = screen(width, 900.0);
                assert!(inside(frame.top, whole));
                assert!(inside(frame.board, whole), "board leaves the window at {width}");
                if let Some(right) = frame.right {
                    assert!(!overlaps(right, frame.board), "right panel overlaps the board at {width}");
                }
                if frame.left_docked {
                    let left = frame.left.expect("docked sidebar");
                    assert!(!overlaps(left, frame.board));
                }
            }
        }
    }

    #[test]
    fn a_selection_toolbar_sits_above_the_selection_when_there_is_room() {
        let board = Rect::from_min_size(Pos2::new(0.0, 44.0), Vec2::new(900.0, 700.0));
        let selection = Rect::from_min_size(Pos2::new(300.0, 300.0), Vec2::new(200.0, 40.0));
        let size = Vec2::new(240.0, 42.0);
        let toolbar = selection_toolbar(selection, board, size, None);
        assert!(toolbar.bottom() <= selection.top());
        assert!((selection.top() - toolbar.bottom() - SELECTION_TOOLBAR_GAP).abs() < 0.01);
        assert!(inside(toolbar, board));
    }

    #[test]
    fn a_selection_at_the_top_of_the_board_flips_its_toolbar_below_and_never_covers_the_selection() {
        let board = Rect::from_min_size(Pos2::new(0.0, 44.0), Vec2::new(900.0, 700.0));
        let selection = Rect::from_min_size(Pos2::new(300.0, 50.0), Vec2::new(200.0, 40.0));
        let size = Vec2::new(240.0, 42.0);
        let toolbar = selection_toolbar(selection, board, size, None);
        assert!(toolbar.top() >= selection.bottom(), "the toolbar flips below the selection");
        assert!(!toolbar.intersects(selection));
        assert!(inside(toolbar, board));
    }

    #[test]
    fn a_selection_toolbar_avoids_a_floating_group_it_would_cover() {
        let board = Rect::from_min_size(Pos2::new(0.0, 44.0), Vec2::new(900.0, 700.0));
        let selection = Rect::from_min_size(Pos2::new(300.0, 120.0), Vec2::new(200.0, 40.0));
        let size = Vec2::new(240.0, 42.0);
        let above = selection_toolbar(selection, board, size, None);
        let group = Rect::from_min_size(Pos2::new(above.left(), above.top() - 4.0), Vec2::new(size.x, 20.0));
        let moved = selection_toolbar(selection, board, size, Some(group));
        assert!(!moved.intersects(group));
        assert!(!moved.intersects(selection));
    }
}
