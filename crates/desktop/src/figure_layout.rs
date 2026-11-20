//! Pure placement rules for figure panels and diagram nodes created from the desktop controls.
//! Every value is an integer EMU measure in the figure's own coordinate system.

use designllm_contracts::{Node, Rect, Size, EMU_PER_MM};

const MARGIN: i64 = 8 * EMU_PER_MM;
const GAP: i64 = 5 * EMU_PER_MM;
const NODE_MARGIN: i64 = 6 * EMU_PER_MM;
const NODE_GAP: i64 = 5 * EMU_PER_MM;
const NODE_HEIGHT: i64 = 14 * EMU_PER_MM;
const NODE_MAX_WIDTH: i64 = 46 * EMU_PER_MM;
const NODE_MIN_WIDTH: i64 = 22 * EMU_PER_MM;

/// Panel `index` of `total` panels laid out as a grid over the figure. The column count is the one
/// whose cells come closest to a 5:4 landscape shape, so a wide figure puts two panels side by side
/// and a portrait page stacks them. Panels fill the grid row by row.
pub fn tiled_panel_bounds(size: Size, index: usize, total: usize) -> Rect {
    let total = total.max(1);
    let index = index.min(total - 1);
    let margin = MARGIN.min(size.width.min(size.height) / 12).max(0);
    let gap = (margin * GAP / MARGIN).max(0);
    let columns = best_column_count(size, total, margin, gap);
    let rows = total.div_ceil(columns);
    let columns_i = columns as i64;
    let rows_i = rows as i64;
    let cell_width = ((size.width - 2 * margin - (columns_i - 1) * gap) / columns_i).max(1);
    let cell_height = ((size.height - 2 * margin - (rows_i - 1) * gap) / rows_i).max(1);
    let column = (index % columns) as i64;
    let row = (index / columns) as i64;
    Rect {
        x: margin + column * (cell_width + gap),
        y: margin + row * (cell_height + gap),
        width: cell_width,
        height: cell_height,
    }
}

fn best_column_count(size: Size, total: usize, margin: i64, gap: i64) -> usize {
    let mut best = (f64::INFINITY, 1_usize);
    for columns in 1..=total {
        let rows = total.div_ceil(columns);
        let width = (size.width - 2 * margin - (columns as i64 - 1) * gap) as f64 / columns as f64;
        let height = (size.height - 2 * margin - (rows as i64 - 1) * gap) as f64 / rows as f64;
        if width <= 0.0 || height <= 0.0 {
            continue;
        }
        let distance = ((width / height) / 1.25).ln().abs();
        if distance < best.0 - 1e-9 {
            best = (distance, columns);
        }
    }
    best.1
}

/// Slot `index` for a new diagram node, relative to the panel origin (the coordinate system the
/// figure engine uses for diagram nodes). Nodes fill two columns row by row. `None` means the
/// panel has no room left, so the caller can say so instead of creating a node that is rejected.
pub fn diagram_node_bounds(panel: Size, index: usize) -> Option<Rect> {
    let usable_width = panel.width - 2 * NODE_MARGIN;
    let usable_height = panel.height - 2 * NODE_MARGIN;
    if usable_width < NODE_MIN_WIDTH || usable_height < NODE_HEIGHT {
        return None;
    }
    let columns = if usable_width >= 2 * NODE_MIN_WIDTH + NODE_GAP { 2 } else { 1 };
    let width = ((usable_width - (columns - 1) * NODE_GAP) / columns).min(NODE_MAX_WIDTH);
    let rows = (usable_height + NODE_GAP) / (NODE_HEIGHT + NODE_GAP);
    let capacity = (rows * columns) as usize;
    if index >= capacity {
        return None;
    }
    let column = (index as i64) % columns;
    let row = (index as i64) / columns;
    Some(Rect {
        x: NODE_MARGIN + column * (width + NODE_GAP),
        y: NODE_MARGIN + row * (NODE_HEIGHT + NODE_GAP),
        width,
        height: NODE_HEIGHT,
    })
}

fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
}

pub(crate) fn free_diagram_node_bounds(panel: Size, nodes: &[Node]) -> Option<Rect> {
    let slot = diagram_node_bounds(panel, 0)?;
    let available = |b: &Rect| nodes.iter().all(|node| !overlaps(*b, node.bounds));
    if let Some(free) = diagram_node_bounds(panel, nodes.len()).filter(available) {
        return Some(free);
    }
    if let Some(free) = (0..).map_while(|i| diagram_node_bounds(panel, i))
        .find(available)
    {
        return Some(free);
    }

    // A hand-placed node can cover several grid slots while leaving room beside or below it.
    let mut obstacles: Vec<Rect> = nodes.iter().map(|node| node.bounds).collect();
    obstacles.sort_by_key(|b| b.x);
    let mut rows = vec![slot.y];
    rows.extend(obstacles.iter().map(|b| b.y + b.height + NODE_GAP));
    rows.sort_unstable();
    rows.dedup();
    for y in rows {
        if y < NODE_MARGIN { continue; }
        if y + slot.height > panel.height - NODE_MARGIN { break; }
        let mut candidate = Rect { y, ..slot };
        for &b in &obstacles {
            if overlaps(candidate, b) {
                candidate.x = b.x + b.width + NODE_GAP;
            }
        }
        if candidate.x >= NODE_MARGIN && candidate.x + candidate.width <= panel.width - NODE_MARGIN {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inside(inner: Rect, outer: Size) -> bool {
        inner.x >= 0
            && inner.y >= 0
            && inner.width > 0
            && inner.height > 0
            && inner.x + inner.width <= outer.width
            && inner.y + inner.height <= outer.height
    }

    #[test]
    fn single_panel_fills_the_figure_inside_its_margin() {
        let size = Size { width: 170 * EMU_PER_MM, height: 120 * EMU_PER_MM };
        let bounds = tiled_panel_bounds(size, 0, 1);
        assert_eq!(bounds.x, 8 * EMU_PER_MM);
        assert_eq!(bounds.y, 8 * EMU_PER_MM);
        assert_eq!(bounds.width, 154 * EMU_PER_MM);
        assert_eq!(bounds.height, 104 * EMU_PER_MM);
    }

    #[test]
    fn wide_figures_place_two_panels_side_by_side_and_portrait_pages_stack_them() {
        let wide = Size { width: 170 * EMU_PER_MM, height: 120 * EMU_PER_MM };
        let left = tiled_panel_bounds(wide, 0, 2);
        let right = tiled_panel_bounds(wide, 1, 2);
        assert_eq!(left.y, right.y);
        assert!(right.x >= left.x + left.width);

        let portrait = Size::a4();
        let top = tiled_panel_bounds(portrait, 0, 2);
        let bottom = tiled_panel_bounds(portrait, 1, 2);
        assert_eq!(top.x, bottom.x);
        assert!(bottom.y >= top.y + top.height);
    }

    #[test]
    fn panels_never_overlap_and_stay_inside_the_figure() {
        for size in [
            Size { width: 170 * EMU_PER_MM, height: 120 * EMU_PER_MM },
            Size { width: 85 * EMU_PER_MM, height: 70 * EMU_PER_MM },
            Size::a4(),
            Size::slide(),
        ] {
            for total in 1..=9 {
                let panels: Vec<Rect> = (0..total).map(|index| tiled_panel_bounds(size, index, total)).collect();
                for (index, panel) in panels.iter().enumerate() {
                    assert!(inside(*panel, size), "panel {index}/{total} leaves {size:?}: {panel:?}");
                    for other in &panels[index + 1..] {
                        assert!(!overlaps(*panel, *other), "panels overlap for {total} on {size:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn out_of_range_requests_are_clamped_instead_of_panicking() {
        let size = Size { width: 170 * EMU_PER_MM, height: 120 * EMU_PER_MM };
        assert_eq!(tiled_panel_bounds(size, 5, 0), tiled_panel_bounds(size, 0, 1));
        assert_eq!(tiled_panel_bounds(size, 9, 3), tiled_panel_bounds(size, 2, 3));
        let tiny = Size { width: 1, height: 1 };
        let bounds = tiled_panel_bounds(tiny, 0, 4);
        assert!(bounds.width >= 1 && bounds.height >= 1);
    }

    #[test]
    fn diagram_nodes_fill_the_panel_without_overlap_and_then_report_no_room() {
        let panel = Size { width: 77 * EMU_PER_MM, height: 104 * EMU_PER_MM };
        let mut placed = Vec::new();
        for index in 0..64 {
            match diagram_node_bounds(panel, index) {
                Some(bounds) => {
                    assert!(inside(bounds, panel), "node {index} leaves the panel: {bounds:?}");
                    for other in &placed {
                        assert!(!overlaps(bounds, *other), "node {index} overlaps");
                    }
                    placed.push(bounds);
                }
                None => break,
            }
        }
        assert!(placed.len() >= 4, "a half-width panel should hold at least four nodes");
        assert!(placed.len() < 64, "the slot list must end");
        assert!(diagram_node_bounds(panel, placed.len()).is_none());
        assert_eq!(diagram_node_bounds(panel, 0).map(|b| (b.x, b.y)), Some((6 * EMU_PER_MM, 6 * EMU_PER_MM)));
    }

    #[test]
    fn fallback_rows_keep_the_top_and_left_margins() {
        let panel = Size { width: 2 * NODE_MARGIN + NODE_MIN_WIDTH, height: 2 * NODE_MARGIN + NODE_HEIGHT };
        let slot = diagram_node_bounds(panel, 0).unwrap();
        let node = |id, bounds| serde_json::from_value::<Node>(serde_json::json!({
            "id": id, "kind": "rectangle", "bounds": bounds,
        })).unwrap();
        let above = Rect { y: -2 * NODE_HEIGHT - NODE_GAP, ..slot };
        assert!(free_diagram_node_bounds(panel, &[node("occupied", slot), node("above", above)]).is_none());
    }

    #[test]
    fn panels_too_small_for_a_node_have_no_slot() {
        let tiny = Size { width: 20 * EMU_PER_MM, height: 20 * EMU_PER_MM };
        assert!(diagram_node_bounds(tiny, 0).is_none());
    }
}
