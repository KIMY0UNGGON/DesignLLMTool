//! Page-local guide geometry and snapping helpers over the persisted Canvas guide settings.

use crate::{CanvasAxisGuides, EMU_PER_CSS_PX};

type AxisSpec = CanvasAxisGuides;

pub const MAX_GUIDE_COUNT: u32 = 24;
pub const SCREEN_SNAP_TOLERANCE_PX: f64 = 8.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuideError {
    InvalidPageSize,
    NoAxes,
    CountOutOfRange,
    NegativeSpacingOrMargin,
    NoTrackSpace,
    InvalidRect,
    InvalidViewScale,
    Overflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Track {
    pub start_emu: i64,
    pub end_emu: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AxisLayout {
    pub tracks: Vec<Track>,
    /// Track edges, sorted and unique. Margins and gutters are represented by the gaps between them.
    pub lines_emu: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageGuideLayout {
    /// Column tracks use x coordinates and page width.
    pub columns: Option<AxisLayout>,
    /// Row tracks use y coordinates and page height.
    pub rows: Option<AxisLayout>,
}

impl PageGuideLayout {
    pub fn vertical_lines(&self) -> &[i64] {
        self.columns.as_ref().map_or(&[], |axis| axis.lines_emu.as_slice())
    }

    pub fn horizontal_lines(&self) -> &[i64] {
        self.rows.as_ref().map_or(&[], |axis| axis.lines_emu.as_slice())
    }
}

/// Calculate page-local column and row tracks. Remainder EMUs are spread as evenly as possible,
/// with half-up rounding of cumulative boundaries; the last track always ends at the trailing margin.
pub fn calculate_page_guides(
    page_width_emu: i64,
    page_height_emu: i64,
    columns: Option<AxisSpec>,
    rows: Option<AxisSpec>,
) -> Result<PageGuideLayout, GuideError> {
    if page_width_emu <= 0 || page_height_emu <= 0 {
        return Err(GuideError::InvalidPageSize);
    }
    if columns.is_none() && rows.is_none() {
        return Err(GuideError::NoAxes);
    }
    Ok(PageGuideLayout {
        columns: columns.map(|spec| calculate_axis(page_width_emu, spec)).transpose()?,
        rows: rows.map(|spec| calculate_axis(page_height_emu, spec)).transpose()?,
    })
}

fn calculate_axis(size_emu: i64, spec: AxisSpec) -> Result<AxisLayout, GuideError> {
    if spec.count == 0 || spec.count > MAX_GUIDE_COUNT {
        return Err(GuideError::CountOutOfRange);
    }
    if spec.spacing_emu < 0 || spec.leading_margin_emu < 0 || spec.trailing_margin_emu < 0 {
        return Err(GuideError::NegativeSpacingOrMargin);
    }

    let count = i128::from(spec.count);
    let gaps = i128::from(spec.spacing_emu) * (count - 1);
    let available = i128::from(size_emu)
        - i128::from(spec.leading_margin_emu)
        - i128::from(spec.trailing_margin_emu)
        - gaps;
    if available < count {
        return Err(GuideError::NoTrackSpace);
    }

    let base = available / count;
    let remainder = available % count;
    let mut cursor = i128::from(spec.leading_margin_emu);
    let mut tracks = Vec::with_capacity(spec.count as usize);
    let mut lines_emu = Vec::with_capacity(spec.count as usize * 2);

    for index in 0..spec.count {
        let before = rounded_remainder(index, remainder, count);
        let after = rounded_remainder(index + 1, remainder, count);
        let width = base + after - before;
        let end = cursor + width;
        tracks.push(Track { start_emu: to_i64(cursor)?, end_emu: to_i64(end)? });
        lines_emu.push(to_i64(cursor)?);
        lines_emu.push(to_i64(end)?);
        cursor = end + i128::from(spec.spacing_emu);
    }

    lines_emu.sort_unstable();
    lines_emu.dedup();
    debug_assert_eq!(tracks.last().map(|track| track.end_emu), Some(size_emu - spec.trailing_margin_emu));
    Ok(AxisLayout { tracks, lines_emu })
}

/// Number of remainder EMUs assigned through the first `tracks` tracks. Rounding cumulative
/// boundaries distributes extras across the axis instead of collecting them at one edge.
fn rounded_remainder(tracks: u32, remainder: i128, count: i128) -> i128 {
    (i128::from(tracks) * remainder * 2 + count) / (count * 2)
}

fn to_i64(value: i128) -> Result<i64, GuideError> {
    i64::try_from(value).map_err(|_| GuideError::Overflow)
}

/// Convert the fixed 8 physical-pixel snap distance to page EMU for the current zoom and display
/// scale. `pixels_per_point` is eframe's physical-pixels-per-screen-point value.
pub fn snap_tolerance_emu(zoom: f64, pixels_per_point: f64) -> Result<i64, GuideError> {
    if !zoom.is_finite() || zoom <= 0.0 || !pixels_per_point.is_finite() || pixels_per_point <= 0.0 {
        return Err(GuideError::InvalidViewScale);
    }
    let screen_scale = zoom * pixels_per_point;
    if !screen_scale.is_finite() || screen_scale <= 0.0 {
        return Err(GuideError::InvalidViewScale);
    }
    let tolerance = SCREEN_SNAP_TOLERANCE_PX * EMU_PER_CSS_PX as f64 / screen_scale;
    if !tolerance.is_finite() || tolerance > i64::MAX as f64 {
        return Err(GuideError::Overflow);
    }
    Ok(tolerance.round() as i64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x_emu: i64,
    pub y_emu: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmuRect {
    pub x_emu: i64,
    pub y_emu: i64,
    pub width_emu: i64,
    pub height_emu: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapFeature {
    Start,
    Center,
    End,
    Anchor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapHit {
    pub line_emu: i64,
    pub feature: SnapFeature,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RectSnap {
    pub rect: EmuRect,
    pub x: Option<SnapHit>,
    pub y: Option<SnapHit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveEdge {
    Start,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeHandle {
    /// `None` leaves that axis unchanged (for example, dragging a horizontal edge).
    pub x: Option<ActiveEdge>,
    pub y: Option<ActiveEdge>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateSnap {
    pub anchor: Point,
    pub cursor: Point,
    pub rect: EmuRect,
    pub anchor_x: Option<SnapHit>,
    pub anchor_y: Option<SnapHit>,
    pub cursor_x: Option<SnapHit>,
    pub cursor_y: Option<SnapHit>,
}

/// Snap the selection's start, center, or end to the nearest vertical/horizontal guide. Equal
/// distances prefer Start, then Center, then End; equal target distances prefer the lower line.
/// A zero-length axis (horizontal or vertical Line) is a single point and snaps as Start.
pub fn snap_move(
    rect: EmuRect,
    vertical_lines_emu: &[i64],
    horizontal_lines_emu: &[i64],
    zoom: f64,
    pixels_per_point: f64,
) -> Result<RectSnap, GuideError> {
    let tolerance = i128::from(snap_tolerance_emu(zoom, pixels_per_point)?);
    let (left, right, top, bottom) = rect_edges(rect)?;
    let x = best_candidate(left, right, vertical_lines_emu, tolerance, AxisMode::Translate);
    let y = best_candidate(top, bottom, horizontal_lines_emu, tolerance, AxisMode::Translate);
    let left = x.map_or(left, |candidate| candidate.low);
    let right = x.map_or(right, |candidate| candidate.high);
    let top = y.map_or(top, |candidate| candidate.low);
    let bottom = y.map_or(bottom, |candidate| candidate.high);
    Ok(RectSnap {
        rect: rect_from_edges(left, top, right, bottom)?,
        x: x.map(|candidate| candidate.hit),
        y: y.map(|candidate| candidate.hit),
    })
}

/// Snap only the actively dragged edge on each resize axis. Center alignment is supported by
/// moving that edge twice the center error while keeping the opposite edge fixed. A zero-length
/// axis has no dragged edge, so its handle is ignored; a snap never collapses a side to zero.
pub fn snap_resize(
    rect: EmuRect,
    handle: ResizeHandle,
    vertical_lines_emu: &[i64],
    horizontal_lines_emu: &[i64],
    zoom: f64,
    pixels_per_point: f64,
) -> Result<RectSnap, GuideError> {
    let tolerance = i128::from(snap_tolerance_emu(zoom, pixels_per_point)?);
    let (left, right, top, bottom) = rect_edges(rect)?;
    let x = handle.x.and_then(|edge| {
        best_candidate(left, right, vertical_lines_emu, tolerance, AxisMode::Resize(edge))
    });
    let y = handle.y.and_then(|edge| {
        best_candidate(top, bottom, horizontal_lines_emu, tolerance, AxisMode::Resize(edge))
    });
    let left = x.map_or(left, |candidate| candidate.low);
    let right = x.map_or(right, |candidate| candidate.high);
    let top = y.map_or(top, |candidate| candidate.low);
    let bottom = y.map_or(bottom, |candidate| candidate.high);
    Ok(RectSnap {
        rect: rect_from_edges(left, top, right, bottom)?,
        x: x.map(|candidate| candidate.hit),
        y: y.map(|candidate| candidate.hit),
    })
}

/// Snap the creation anchor once, then snap the moving endpoint or the draft rectangle's center.
/// The caller should retain the returned anchor for subsequent pointer-move events. A click returns
/// a 0x0 rect; the GUI caller must refuse to commit a generic (non-Line) rectangle with a zero side.
pub fn snap_create(
    anchor: Point,
    cursor: Point,
    vertical_lines_emu: &[i64],
    horizontal_lines_emu: &[i64],
    zoom: f64,
    pixels_per_point: f64,
) -> Result<CreateSnap, GuideError> {
    let tolerance = i128::from(snap_tolerance_emu(zoom, pixels_per_point)?);
    let x_undragged = cursor.x_emu == anchor.x_emu;
    let y_undragged = cursor.y_emu == anchor.y_emu;
    let (anchor_x, anchor_x_hit) = snap_anchor(anchor.x_emu, vertical_lines_emu, tolerance)?;
    let (anchor_y, anchor_y_hit) = snap_anchor(anchor.y_emu, horizontal_lines_emu, tolerance)?;
    let anchor = Point { x_emu: anchor_x, y_emu: anchor_y };

    let (cursor_x, cursor_x_hit) = if x_undragged {
        (anchor.x_emu, None)
    } else {
        snap_create_axis(anchor.x_emu, cursor.x_emu, vertical_lines_emu, tolerance)?
    };
    let (cursor_y, cursor_y_hit) = if y_undragged {
        (anchor.y_emu, None)
    } else {
        snap_create_axis(anchor.y_emu, cursor.y_emu, horizontal_lines_emu, tolerance)?
    };
    let cursor = Point { x_emu: cursor_x, y_emu: cursor_y };
    let rect = rect_from_points(anchor, cursor)?;
    Ok(CreateSnap {
        anchor,
        cursor,
        rect,
        anchor_x: anchor_x_hit,
        anchor_y: anchor_y_hit,
        cursor_x: cursor_x_hit,
        cursor_y: cursor_y_hit,
    })
}

fn snap_anchor(value: i64, lines: &[i64], tolerance: i128) -> Result<(i64, Option<SnapHit>), GuideError> {
    let best = lines
        .iter()
        .copied()
        .map(|line| ((i128::from(line) - i128::from(value)).abs(), line))
        .filter(|(distance, _)| *distance <= tolerance)
        .min_by_key(|(distance, line)| (*distance, *line));
    Ok(match best {
        Some((_, line)) => (line, Some(SnapHit { line_emu: line, feature: SnapFeature::Anchor })),
        None => (value, None),
    })
}

fn snap_create_axis(
    anchor: i64,
    cursor: i64,
    lines: &[i64],
    tolerance: i128,
) -> Result<(i64, Option<SnapHit>), GuideError> {
    if anchor == cursor {
        return Ok((cursor, None));
    }
    let (low, high) = (i128::from(anchor.min(cursor)), i128::from(anchor.max(cursor)));
    let edge = if cursor < anchor { ActiveEdge::Start } else { ActiveEdge::End };
    let Some(candidate) = best_candidate(low, high, lines, tolerance, AxisMode::Resize(edge)) else {
        return Ok((cursor, None));
    };
    let endpoint = if edge == ActiveEdge::Start { candidate.low } else { candidate.high };
    Ok((to_i64(endpoint)?, Some(candidate.hit)))
}

#[derive(Debug, Clone, Copy)]
enum AxisMode {
    Translate,
    Resize(ActiveEdge),
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    low: i128,
    high: i128,
    hit: SnapHit,
}

fn best_candidate(
    low: i128,
    high: i128,
    lines: &[i64],
    tolerance: i128,
    mode: AxisMode,
) -> Option<Candidate> {
    // A zero-length axis has no dragged edge to resize; translation still moves its single point.
    if high < low || (high == low && matches!(mode, AxisMode::Resize(_))) {
        return None;
    }
    // Translation keeps the length, so a point may move. A resized edge must keep at least 1 EMU,
    // so a resize never creates a zero-length side.
    let min_length: i128 = match mode {
        AxisMode::Translate => 0,
        AxisMode::Resize(_) => 1,
    };
    let center = low + (high - low) / 2;
    let features: &[(SnapFeature, i128, u8)] = match mode {
        AxisMode::Translate => &[
            (SnapFeature::Start, low, 0),
            (SnapFeature::Center, center, 1),
            (SnapFeature::End, high, 2),
        ],
        AxisMode::Resize(ActiveEdge::Start) => &[
            (SnapFeature::Start, low, 0),
            (SnapFeature::Center, center, 1),
        ],
        AxisMode::Resize(ActiveEdge::End) => &[
            (SnapFeature::Center, center, 1),
            (SnapFeature::End, high, 2),
        ],
    };

    let mut best: Option<((i128, u8, i64), Candidate)> = None;
    for &(feature, value, priority) in features {
        for &line in lines {
            let difference = i128::from(line) - value;
            let distance = difference.abs();
            if distance > tolerance {
                continue;
            }
            let correction = match (mode, feature) {
                (AxisMode::Resize(_), SnapFeature::Center) => difference * 2,
                _ => difference,
            };
            let (candidate_low, candidate_high) = match mode {
                AxisMode::Translate => (low + correction, high + correction),
                AxisMode::Resize(ActiveEdge::Start) => (low + correction, high),
                AxisMode::Resize(ActiveEdge::End) => (low, high + correction),
            };
            if candidate_high - candidate_low < min_length
                || candidate_low < i128::from(i64::MIN)
                || candidate_high > i128::from(i64::MAX)
                || candidate_high - candidate_low > i128::from(i64::MAX)
            {
                continue;
            }

            let candidate = Candidate {
                low: candidate_low,
                high: candidate_high,
                hit: SnapHit { line_emu: line, feature },
            };
            let key = (distance, priority, line);
            if best.as_ref().is_none_or(|(best_key, _)| key < *best_key) {
                best = Some((key, candidate));
            }
        }
    }
    best.map(|(_, candidate)| candidate)
}

/// One zero-length side is a valid horizontal or vertical Line (Canvas Line rule). Negative sides
/// and a 0x0 rect are rejected. Generic rectangles are not checked here; the GUI caller refuses them.
fn rect_edges(rect: EmuRect) -> Result<(i128, i128, i128, i128), GuideError> {
    if rect.width_emu < 0 || rect.height_emu < 0 || (rect.width_emu == 0 && rect.height_emu == 0) {
        return Err(GuideError::InvalidRect);
    }
    let left = i128::from(rect.x_emu);
    let top = i128::from(rect.y_emu);
    let right = left + i128::from(rect.width_emu);
    let bottom = top + i128::from(rect.height_emu);
    if right > i128::from(i64::MAX) || bottom > i128::from(i64::MAX) {
        return Err(GuideError::Overflow);
    }
    Ok((left, right, top, bottom))
}

fn rect_from_edges(left: i128, top: i128, right: i128, bottom: i128) -> Result<EmuRect, GuideError> {
    if right < left || bottom < top || (right == left && bottom == top) {
        return Err(GuideError::InvalidRect);
    }
    if left < i128::from(i64::MIN)
        || top < i128::from(i64::MIN)
        || right > i128::from(i64::MAX)
        || bottom > i128::from(i64::MAX)
    {
        return Err(GuideError::Overflow);
    }
    Ok(EmuRect {
        x_emu: to_i64(left)?,
        y_emu: to_i64(top)?,
        width_emu: to_i64(right - left)?,
        height_emu: to_i64(bottom - top)?,
    })
}

fn rect_from_points(a: Point, b: Point) -> Result<EmuRect, GuideError> {
    let left = i128::from(a.x_emu.min(b.x_emu));
    let right = i128::from(a.x_emu.max(b.x_emu));
    let top = i128::from(a.y_emu.min(b.y_emu));
    let bottom = i128::from(a.y_emu.max(b.y_emu));
    Ok(EmuRect {
        x_emu: to_i64(left)?,
        y_emu: to_i64(top)?,
        width_emu: to_i64(right - left)?,
        height_emu: to_i64(bottom - top)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axis(count: u32, spacing: i64, leading: i64, trailing: i64) -> AxisSpec {
        AxisSpec { count, spacing_emu: spacing, leading_margin_emu: leading, trailing_margin_emu: trailing }
    }

    #[test]
    fn remainder_is_spread_and_end_line_matches_trailing_margin() {
        let guides = calculate_page_guides(100, 80, Some(axis(3, 1, 1, 2)), None).unwrap();
        let columns = guides.columns.unwrap();
        assert_eq!(columns.tracks, vec![
            Track { start_emu: 1, end_emu: 33 },
            Track { start_emu: 34, end_emu: 65 },
            Track { start_emu: 66, end_emu: 98 },
        ]);
        assert_eq!(columns.lines_emu, vec![1, 33, 34, 65, 66, 98]);
    }

    #[test]
    fn guide_parameters_reject_bad_counts_and_empty_track_area() {
        assert_eq!(calculate_page_guides(100, 80, Some(axis(0, 0, 0, 0)), None), Err(GuideError::CountOutOfRange));
        assert_eq!(calculate_page_guides(100, 80, Some(axis(25, 0, 0, 0)), None), Err(GuideError::CountOutOfRange));
        assert_eq!(calculate_page_guides(10, 80, Some(axis(2, 7, 1, 1)), None), Err(GuideError::NoTrackSpace));
        assert_eq!(calculate_page_guides(100, 80, Some(axis(1, -1, 0, 0)), None), Err(GuideError::NegativeSpacingOrMargin));
    }

    #[test]
    fn tolerance_is_eight_physical_pixels_at_each_zoom_scale() {
        assert_eq!(snap_tolerance_emu(1.0, 1.0), Ok(76_200));
        assert_eq!(snap_tolerance_emu(2.0, 2.0), Ok(19_050));
        assert_eq!(snap_tolerance_emu(0.0, 1.0), Err(GuideError::InvalidViewScale));
    }

    #[test]
    fn move_snaps_center_and_resolves_feature_and_line_ties_deterministically() {
        let rect = EmuRect { x_emu: 100, y_emu: 0, width_emu: 20, height_emu: 10 };
        let centered = snap_move(rect, &[111], &[], 8.0, 1.0).unwrap();
        assert_eq!(centered.rect.x_emu, 101);
        assert_eq!(centered.x.unwrap(), SnapHit { line_emu: 111, feature: SnapFeature::Center });

        let ending = snap_move(
            EmuRect { x_emu: 0, y_emu: 0, width_emu: 19_000, height_emu: 10 },
            &[20_000], &[], 8.0, 1.0,
        ).unwrap();
        assert_eq!(ending.rect.x_emu, 1_000);
        assert_eq!(ending.x.unwrap().feature, SnapFeature::End);

        let tied = snap_move(
            EmuRect { x_emu: 10, y_emu: 0, width_emu: 10, height_emu: 10 },
            &[11, 9], &[], 8.0, 1.0,
        ).unwrap();
        assert_eq!(tied.rect.x_emu, 9, "equal line distance selects the lower coordinate");
        assert_eq!(tied.x.unwrap().feature, SnapFeature::Start, "equal feature distance prefers start");
    }

    #[test]
    fn resize_snaps_the_dragged_edge_or_center_and_preserves_the_opposite_edge() {
        let rect = EmuRect { x_emu: 0, y_emu: 0, width_emu: 100, height_emu: 40 };
        let edge = snap_resize(
            rect, ResizeHandle { x: Some(ActiveEdge::Start), y: None }, &[10], &[], 8.0, 1.0,
        ).unwrap();
        assert_eq!(edge.rect, EmuRect { x_emu: 10, y_emu: 0, width_emu: 90, height_emu: 40 });
        assert_eq!(edge.x.unwrap().feature, SnapFeature::Start);

        let center = snap_resize(
            rect, ResizeHandle { x: Some(ActiveEdge::End), y: None }, &[60], &[], 8.0, 1.0,
        ).unwrap();
        assert_eq!(center.rect, EmuRect { x_emu: 0, y_emu: 0, width_emu: 120, height_emu: 40 });
        assert_eq!(center.x.unwrap().feature, SnapFeature::Center);

        let end = snap_resize(
            EmuRect { x_emu: 0, y_emu: 0, width_emu: 20_000, height_emu: 40 },
            ResizeHandle { x: Some(ActiveEdge::End), y: None }, &[20_001], &[], 8.0, 1.0,
        ).unwrap();
        assert_eq!(end.rect.width_emu, 20_001);
        assert_eq!(end.x.unwrap().feature, SnapFeature::End);
    }

    #[test]
    fn create_snaps_anchor_and_cursor_center_without_moving_the_anchor_afterward() {
        let result = snap_create(
            Point { x_emu: 2, y_emu: 0 }, Point { x_emu: 100, y_emu: 10 },
            &[0, 60], &[], 8.0, 1.0,
        ).unwrap();
        assert_eq!(result.anchor.x_emu, 0);
        assert_eq!(result.cursor.x_emu, 120);
        assert_eq!(result.rect, EmuRect { x_emu: 0, y_emu: 0, width_emu: 120, height_emu: 10 });
        assert_eq!(result.anchor_x.unwrap().feature, SnapFeature::Anchor);
        assert_eq!(result.cursor_x.unwrap().feature, SnapFeature::Center);

        let ending = snap_create(
            Point { x_emu: 0, y_emu: 0 }, Point { x_emu: 20_000, y_emu: 1 },
            &[20_000], &[], 8.0, 1.0,
        ).unwrap();
        assert_eq!(ending.cursor.x_emu, 20_000);
        assert_eq!(ending.cursor_x.unwrap().feature, SnapFeature::End);

        let click = snap_create(
            Point { x_emu: 2, y_emu: 2 }, Point { x_emu: 2, y_emu: 2 },
            &[0], &[0], 8.0, 1.0,
        ).unwrap();
        assert_eq!(click.anchor, Point { x_emu: 0, y_emu: 0 });
        assert_eq!(click.cursor, click.anchor, "snapping the pointer-down anchor does not create accidental size");
        assert_eq!(click.rect.width_emu, 0);
        assert_eq!(click.rect.height_emu, 0);
    }

    #[test]
    fn snap_boundary_is_inclusive_and_overflow_is_rejected() {
        let tolerance = snap_tolerance_emu(1.0, 1.0).unwrap();
        let rect = EmuRect { x_emu: 0, y_emu: 0, width_emu: 10, height_emu: 10 };
        assert!(snap_move(rect, &[-tolerance], &[], 1.0, 1.0).unwrap().x.is_some());
        assert!(snap_move(rect, &[-tolerance - 1], &[], 1.0, 1.0).unwrap().x.is_none());
        assert_eq!(
            snap_move(
                EmuRect { x_emu: i64::MAX - 5, y_emu: 0, width_emu: 10, height_emu: 1 },
                &[], &[], 1.0, 1.0,
            ),
            Err(GuideError::Overflow),
        );
    }

    #[test]
    fn one_zero_side_is_a_line_while_empty_and_negative_rects_are_rejected() {
        let horizontal = EmuRect { x_emu: 0, y_emu: 50, width_emu: 100, height_emu: 0 };
        let vertical = EmuRect { x_emu: 50, y_emu: 0, width_emu: 0, height_emu: 100 };
        assert!(snap_move(horizontal, &[], &[], 1.0, 1.0).is_ok());
        assert!(snap_move(vertical, &[], &[], 1.0, 1.0).is_ok());

        let no_handle = ResizeHandle { x: None, y: None };
        for invalid in [
            EmuRect { x_emu: 0, y_emu: 0, width_emu: 0, height_emu: 0 },
            EmuRect { x_emu: 0, y_emu: 0, width_emu: -1, height_emu: 10 },
            EmuRect { x_emu: 0, y_emu: 0, width_emu: 10, height_emu: -1 },
        ] {
            assert_eq!(snap_move(invalid, &[], &[], 1.0, 1.0), Err(GuideError::InvalidRect));
            assert_eq!(snap_resize(invalid, no_handle, &[], &[], 1.0, 1.0), Err(GuideError::InvalidRect));
        }
    }

    #[test]
    fn horizontal_line_move_snaps_span_features_and_its_single_y_point() {
        let line = EmuRect { x_emu: 0, y_emu: 50, width_emu: 100, height_emu: 0 };
        // Center (50) is 2 from guide 52; Start is 52 away and End is 48 away.
        let by_center = snap_move(line, &[52], &[], 1.0, 1.0).unwrap();
        assert_eq!(by_center.rect, EmuRect { x_emu: 2, y_emu: 50, width_emu: 100, height_emu: 0 });
        assert_eq!(by_center.x, Some(SnapHit { line_emu: 52, feature: SnapFeature::Center }));
        assert_eq!(by_center.y, None);

        // The y point 50 is 3 from guide 47 and 6 from guide 56.
        let by_point = snap_move(line, &[], &[47, 56], 1.0, 1.0).unwrap();
        assert_eq!(by_point.rect, EmuRect { x_emu: 0, y_emu: 47, width_emu: 100, height_emu: 0 });
        assert_eq!(by_point.y, Some(SnapHit { line_emu: 47, feature: SnapFeature::Start }));
    }

    #[test]
    fn point_axis_tie_prefers_the_lower_line() {
        let line = EmuRect { x_emu: 0, y_emu: 50, width_emu: 100, height_emu: 0 };
        let tied = snap_move(line, &[], &[52, 48], 1.0, 1.0).unwrap();
        assert_eq!(tied.rect.y_emu, 48);
        assert_eq!(tied.y, Some(SnapHit { line_emu: 48, feature: SnapFeature::Start }));
    }

    #[test]
    fn vertical_line_move_is_the_transposed_horizontal_case() {
        let horizontal = EmuRect { x_emu: 0, y_emu: 50, width_emu: 100, height_emu: 0 };
        let vertical = EmuRect { x_emu: 50, y_emu: 0, width_emu: 0, height_emu: 100 };
        let h = snap_move(horizontal, &[52], &[47], 1.0, 1.0).unwrap();
        let v = snap_move(vertical, &[47], &[52], 1.0, 1.0).unwrap();
        assert_eq!(h.rect, EmuRect { x_emu: 2, y_emu: 47, width_emu: 100, height_emu: 0 });
        assert_eq!(v.rect, EmuRect { x_emu: 47, y_emu: 2, width_emu: 0, height_emu: 100 });
        assert_eq!(v.x, Some(SnapHit { line_emu: 47, feature: SnapFeature::Start }));
        assert_eq!(v.y, Some(SnapHit { line_emu: 52, feature: SnapFeature::Center }));
    }

    #[test]
    fn horizontal_line_endpoints_resize_only_the_active_end_and_never_collapse() {
        let line = EmuRect { x_emu: 0, y_emu: 50, width_emu: 100, height_emu: 0 };
        let start = ResizeHandle { x: Some(ActiveEdge::Start), y: None };
        let end = ResizeHandle { x: Some(ActiveEdge::End), y: None };

        let start_edge = snap_resize(line, start, &[10], &[], 1.0, 1.0).unwrap();
        assert_eq!(start_edge.rect, EmuRect { x_emu: 10, y_emu: 50, width_emu: 90, height_emu: 0 });
        assert_eq!(start_edge.x, Some(SnapHit { line_emu: 10, feature: SnapFeature::Start }));

        let end_edge = snap_resize(line, end, &[102], &[], 1.0, 1.0).unwrap();
        assert_eq!(end_edge.rect, EmuRect { x_emu: 0, y_emu: 50, width_emu: 102, height_emu: 0 });
        assert_eq!(end_edge.x, Some(SnapHit { line_emu: 102, feature: SnapFeature::End }));

        // Center alignment moves the active end by twice the error: guide 60 is 10 from center 50.
        let end_center = snap_resize(line, end, &[60], &[], 1.0, 1.0).unwrap();
        assert_eq!(end_center.rect, EmuRect { x_emu: 0, y_emu: 50, width_emu: 120, height_emu: 0 });
        assert_eq!(end_center.x, Some(SnapHit { line_emu: 60, feature: SnapFeature::Center }));

        let start_center = snap_resize(line, start, &[40], &[], 1.0, 1.0).unwrap();
        assert_eq!(start_center.rect, EmuRect { x_emu: -20, y_emu: 50, width_emu: 120, height_emu: 0 });
        assert_eq!(start_center.x, Some(SnapHit { line_emu: 40, feature: SnapFeature::Center }));

        // Reaching or crossing the opposite endpoint would make a point or a negative side: no snap.
        for guide in [0_i64, -5] {
            let rejected = snap_resize(line, end, &[guide], &[], 1.0, 1.0).unwrap();
            assert_eq!(rejected.rect, line);
            assert_eq!(rejected.x, None);
        }
        let start_onto_end = snap_resize(line, start, &[100], &[], 1.0, 1.0).unwrap();
        assert_eq!(start_onto_end.rect, line);
        assert_eq!(start_onto_end.x, None);
    }

    #[test]
    fn vertical_line_endpoints_resize_on_y_and_ignore_the_zero_length_x_handle() {
        let vertical = EmuRect { x_emu: 50, y_emu: 0, width_emu: 0, height_emu: 100 };
        let top = ResizeHandle { x: None, y: Some(ActiveEdge::Start) };
        let bottom = ResizeHandle { x: None, y: Some(ActiveEdge::End) };

        let top_edge = snap_resize(vertical, top, &[], &[7], 1.0, 1.0).unwrap();
        assert_eq!(top_edge.rect, EmuRect { x_emu: 50, y_emu: 7, width_emu: 0, height_emu: 93 });
        assert_eq!(top_edge.y, Some(SnapHit { line_emu: 7, feature: SnapFeature::Start }));

        let bottom_edge = snap_resize(vertical, bottom, &[], &[103], 1.0, 1.0).unwrap();
        assert_eq!(bottom_edge.rect, EmuRect { x_emu: 50, y_emu: 0, width_emu: 0, height_emu: 103 });
        assert_eq!(bottom_edge.y, Some(SnapHit { line_emu: 103, feature: SnapFeature::End }));

        // Transposed from the horizontal Start case above: y 0 -> 10, height 100 -> 90.
        let transposed = snap_resize(vertical, top, &[], &[10], 1.0, 1.0).unwrap();
        assert_eq!(transposed.rect, EmuRect { x_emu: 50, y_emu: 10, width_emu: 0, height_emu: 90 });

        // A zero-length axis has no dragged edge: its handle is ignored instead of creating a new side.
        let ignored = snap_resize(vertical, ResizeHandle { x: Some(ActiveEdge::Start), y: None }, &[49], &[], 1.0, 1.0).unwrap();
        assert_eq!(ignored.rect, vertical);
        assert_eq!(ignored.x, None);
    }

    #[test]
    fn resize_ties_prefer_the_start_feature_then_the_lower_line() {
        let line = EmuRect { x_emu: 0, y_emu: 50, width_emu: 100, height_emu: 0 };
        let start = ResizeHandle { x: Some(ActiveEdge::Start), y: None };
        // Guide 25 is 25 from Start (0) and 25 from Center (50): Start wins.
        let feature_tie = snap_resize(line, start, &[25], &[], 1.0, 1.0).unwrap();
        assert_eq!(feature_tie.rect, EmuRect { x_emu: 25, y_emu: 50, width_emu: 75, height_emu: 0 });
        assert_eq!(feature_tie.x, Some(SnapHit { line_emu: 25, feature: SnapFeature::Start }));
        // Guides -5 and 5 are both 5 from Start: the lower guide wins.
        let line_tie = snap_resize(line, start, &[5, -5], &[], 1.0, 1.0).unwrap();
        assert_eq!(line_tie.rect, EmuRect { x_emu: -5, y_emu: 50, width_emu: 105, height_emu: 0 });
    }

    #[test]
    fn overflowing_candidates_are_skipped_and_the_next_candidate_is_used() {
        let near_max = EmuRect { x_emu: i64::MAX - 10, y_emu: 0, width_emu: 10, height_emu: 0 };
        // Start (distance 2) would push the far side past i64::MAX; Center (distance 3) wins.
        let moved = snap_move(near_max, &[i64::MAX - 8], &[], 1.0, 1.0).unwrap();
        assert_eq!(moved.rect, EmuRect { x_emu: i64::MAX - 13, y_emu: 0, width_emu: 10, height_emu: 0 });
        assert_eq!(moved.x, Some(SnapHit { line_emu: i64::MAX - 8, feature: SnapFeature::Center }));

        // End moves to MAX-4. Center (distance 1, doubled to 2 EMU) would overflow and is skipped.
        let resized = snap_resize(
            near_max,
            ResizeHandle { x: Some(ActiveEdge::End), y: None },
            &[i64::MAX - 4],
            &[],
            1.0,
            1.0,
        ).unwrap();
        assert_eq!(resized.rect, EmuRect { x_emu: i64::MAX - 10, y_emu: 0, width_emu: 6, height_emu: 0 });
        assert_eq!(resized.x, Some(SnapHit { line_emu: i64::MAX - 4, feature: SnapFeature::End }));

        let overflow = EmuRect { x_emu: i64::MAX - 5, y_emu: 0, width_emu: 10, height_emu: 0 };
        assert_eq!(snap_move(overflow, &[], &[], 1.0, 1.0), Err(GuideError::Overflow));
    }

    #[test]
    fn normal_rectangles_keep_collapse_refusal_and_corner_resize() {
        let rect = EmuRect { x_emu: 0, y_emu: 0, width_emu: 100, height_emu: 40 };
        // Both candidates would make the width zero, which a rectangle never accepts: no snap.
        let collapsed = snap_resize(rect, ResizeHandle { x: Some(ActiveEdge::End), y: None }, &[0], &[], 1.0, 1.0).unwrap();
        assert_eq!(collapsed.rect, rect);
        assert_eq!(collapsed.x, None);

        let corner = snap_resize(
            rect,
            ResizeHandle { x: Some(ActiveEdge::End), y: Some(ActiveEdge::End) },
            &[60],
            &[30],
            1.0,
            1.0,
        ).unwrap();
        assert_eq!(corner.rect, EmuRect { x_emu: 0, y_emu: 0, width_emu: 120, height_emu: 60 });
        assert_eq!(corner.x, Some(SnapHit { line_emu: 60, feature: SnapFeature::Center }));
        assert_eq!(corner.y, Some(SnapHit { line_emu: 30, feature: SnapFeature::Center }));

        let moved = snap_move(rect, &[52], &[], 1.0, 1.0).unwrap();
        assert_eq!(moved.rect, EmuRect { x_emu: 2, y_emu: 0, width_emu: 100, height_emu: 40 });
    }
}
