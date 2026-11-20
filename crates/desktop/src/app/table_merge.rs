//! Merging and unmerging the cells of a Canvas table (68 item 14).
//!
//! The properties panel's 셀 section, the board's right-click menu and the agent path (`table.merge_cells`,
//! `table.unmerge_cell`) run the same checks (`span_plan`). A merge is one `set_canvas_table_structure` change that adds a
//! span whose anchor is the top-left cell; an unmerge removes the span of the region that holds the given cell. A merge
//! moves the text of the covered cells to the anchor (PowerPoint's merge) and empties them, in the same change, so one undo
//! restores both. An unmerge leaves the text at the anchor.
//!
//! While a cell's text is edited in place, the window's merge stores the edit first and then runs (`run_span_after_edit`);
//! an agent's merge is refused until the edit ends (`EDITING_REFUSAL`, `ui_run_refusal`).
//!
//! The menu's two commands are made when it opens, for the table and the cells that were pressed (or the selected range
//! that holds the pressed cell), and are kept until it closes: what happens meanwhile does not change what they name.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use designllm_contracts::{
    AlignSelf, ArtifactBody, ArtifactKind, CanvasDocument, CanvasTable, CanvasTableSpan, FigureLink, Interaction, LayoutSpec,
    Node, NodeExt, NodeKind, Operation, Page,
};
use eframe::egui::{self, Id, Pos2, Rect};
use serde_json::{json, Value};

use super::{DesktopApp, PendingAction, Screen};
use crate::theme;

/// An agent's merge or unmerge while a cell's text is edited in place.
pub(super) const EDITING_REFUSAL: &str = "표 칸의 글을 편집하는 중입니다. 편집을 마친 뒤 다시 요청해 주세요";

/// How long a window merge waits for the edit before it to be stored.
const AFTER_EDIT_WAIT: Duration = Duration::from_secs(5);

/// The intrinsic height of a cell made by the table creator (`kind_commands::canvas_table_cell`); a covered cell keeps it.
const TABLE_CELL_HEIGHT: i64 = 36 * designllm_contracts::EMU_PER_CSS_PX;

type SpanCommand = Result<(&'static str, Value), String>;

/// The right-click menu, a merge that waits for an edit to be stored, and the panel's last answer.
#[derive(Default)]
pub(super) struct TableMergeState {
    /// The open menu's 셀 병합 and 병합 해제: each the command made when the menu opened, or why it is not offered.
    menu: Option<[SpanCommand; 2]>,
    /// Where the open menu drew its two items (screen), as laid out last frame.
    pub(super) menu_items: Option<[Rect; 2]>,
    /// A window merge or unmerge sent while a cell was edited: it runs once the edit is stored.
    after_edit: Option<AfterEdit>,
    /// A node a right click outside the edited cell selects once that edit is stored: (edit session, document, node).
    select_after_edit: Option<(u64, String, String)>,
    /// The properties panel's offer for the selection (`Some(true)`: 병합 해제), with the state it was worked out for.
    panel: Option<(PanelKey, Option<bool>)>,
}

struct AfterEdit {
    table_id: String,
    edit: SpanEdit,
    session_id: u64,
    artifact_id: String,
    /// The revision the merge is made against: the one it was asked at, moved on by the edit's own stored text only.
    revision: u64,
    since: Instant,
}

/// What the panel's offer depends on: the document's revision and page, and the selection.
#[derive(PartialEq, Eq)]
struct PanelKey {
    artifact_id: String,
    revision: u64,
    context_revision: u64,
    page_index: u32,
    primary: Option<String>,
    selected: Vec<String>,
}

/// A rectangle on a table's grid: its top-left cell and its size in rows and columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CellRect {
    pub row: usize,
    pub column: usize,
    pub rows: usize,
    pub columns: usize,
}

impl CellRect {
    fn contains(self, (row, column): (usize, usize)) -> bool {
        (self.row..self.row + self.rows).contains(&row) && (self.column..self.column + self.columns).contains(&column)
    }
}

/// One merge, or one unmerge of the region that holds the cell at `row` and `column`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SpanEdit {
    Merge(CellRect),
    Unmerge { row: usize, column: usize },
}

impl SpanEdit {
    /// The command that runs this edit, with its arguments.
    pub(super) fn command(self, table_id: &str) -> (&'static str, Value) {
        match self {
            Self::Merge(rect) => (
                "table.merge_cells",
                json!({ "table_id": table_id, "row": rect.row, "column": rect.column, "rows": rect.rows, "columns": rect.columns }),
            ),
            Self::Unmerge { row, column } => ("table.unmerge_cell", json!({ "table_id": table_id, "row": row, "column": column })),
        }
    }
}

/// Each cell's row and column on the table's grid.
pub(super) fn cell_positions(table: &CanvasTable) -> BTreeMap<&str, (usize, usize)> {
    table
        .rows
        .iter()
        .enumerate()
        .flat_map(|(row, table_row)| table_row.cell_ids.iter().enumerate().map(move |(column, id)| (id.as_str(), (row, column))))
        .collect()
}

/// The merged region `anchor` heads, on the grid.
fn span_rect(anchor: (usize, usize), span: &CanvasTableSpan) -> CellRect {
    CellRect { row: anchor.0, column: anchor.1, rows: span.row_span as usize, columns: span.column_span as usize }
}

/// The merged region that holds the cell at `at`: its anchor id and its rectangle.
pub(super) fn region_at<'a>(table: &'a CanvasTable, positions: &BTreeMap<&str, (usize, usize)>, at: (usize, usize)) -> Option<(&'a str, CellRect)> {
    table.cell_spans.iter().find_map(|(anchor, span)| {
        let rect = span_rect(*positions.get(anchor.as_str())?, span);
        rect.contains(at).then_some((anchor.as_str(), rect))
    })
}

/// The table's structure after `edit`, or why the edit is refused, by the core's span rules (`table_span_grid`).
pub(super) fn structure_after(expected: &CanvasTable, positions: &BTreeMap<&str, (usize, usize)>, edit: SpanEdit) -> Result<CanvasTable, String> {
    match edit {
        SpanEdit::Merge(rect) => {
            let rows = expected.rows.len();
            let columns = expected.rows.first().map_or(0, |row| row.cell_ids.len());
            if rect.rows.saturating_mul(rect.columns) < 2 {
                return Err("합칠 두 칸 이상을 고르세요".into());
            }
            if rect.row.saturating_add(rect.rows) > rows || rect.column.saturating_add(rect.columns) > columns {
                return Err("표 밖의 칸은 합칠 수 없습니다".into());
            }
            let header = expected.header_row_id.is_some() && expected.header_row_id.as_ref() == expected.rows.first().map(|row| &row.id);
            if rect.row == 0 && rect.rows > 1 && header {
                return Err("머리글 행은 본문 행과 합칠 수 없습니다".into());
            }
            for (anchor, span) in &expected.cell_spans {
                let Some(&at) = positions.get(anchor.as_str()) else { continue };
                let other = span_rect(at, span);
                let overlaps = other.row < rect.row + rect.rows
                    && other.row + other.rows > rect.row
                    && other.column < rect.column + rect.columns
                    && other.column + other.columns > rect.column;
                if overlaps {
                    return Err("병합된 칸과 걸친 범위는 합칠 수 없습니다. 병합을 해제한 뒤 다시 고르세요".into());
                }
            }
            let anchor = expected.rows[rect.row].cell_ids[rect.column].clone();
            let mut table = expected.clone();
            table.cell_spans.insert(anchor, CanvasTableSpan { row_span: rect.rows as u32, column_span: rect.columns as u32 });
            Ok(table)
        }
        SpanEdit::Unmerge { row, column } => {
            let (anchor, _) = region_at(expected, positions, (row, column)).ok_or("병합된 칸이 아닙니다")?;
            let mut table = expected.clone();
            table.cell_spans.remove(anchor);
            Ok(table)
        }
    }
}

/// The layout the table creator gives a cell; a covered cell must keep it (the core's `default_table_cell_layout`).
fn default_cell_layout(layout: &LayoutSpec) -> bool {
    layout.mode == "absolute"
        && layout.width_mode == "fill"
        && layout.height_mode == "hug"
        && layout.gap == 0
        && layout.padding == 0
        && layout.align == "start"
        && layout.min_width.is_none()
        && layout.max_width.is_none()
        && layout.min_height.is_none()
        && layout.max_height.is_none()
        && layout.breakpoints.is_empty()
        && layout.justify.is_none()
        && layout.padding_sides.is_none()
        && layout.grid_columns.is_none()
        && layout.row_gap.is_none()
        && layout.align_self.is_none_or(|align| align == AlignSelf::Auto)
        && layout.grid_span.is_none()
}

fn walk<'a>(nodes: &'a [Node], visit: &mut impl FnMut(&'a Node)) {
    for node in nodes {
        visit(node);
        walk(&node.children, visit);
    }
}

/// Why the cells a merge would cover cannot be hidden, by the core's rules (`TABLE_SPAN_HIDDEN_DATA`, `_HIDDEN_LAYOUT`,
/// `_HIDDEN_GEOMETRY`, `_TOKEN_BINDING`, `_INTERACTION_TARGET`, `_FIGURE_LINK`, `_COMPONENT_OVERRIDE`); else the operations
/// the merge runs before its structure change. The text of the covered cells goes to the anchor, one line per non-empty cell
/// in reading order, and the covered cells are emptied.
fn merge_operations(
    document: &CanvasDocument,
    figure_links: &[FigureLink],
    page: &Page,
    table_node: &Node,
    table: &CanvasTable,
    rect: CellRect,
) -> Result<Vec<Operation>, String> {
    let cells: BTreeMap<&str, &Node> = table_node.children.iter().map(|cell| (cell.id.as_str(), cell)).collect();
    let cell_at = |row: usize, column: usize| -> Result<&Node, String> {
        table.rows.get(row).and_then(|table_row| table_row.cell_ids.get(column)).and_then(|id| cells.get(id.as_str()).copied())
            .ok_or_else(|| "표 칸을 찾지 못했습니다".into())
    };
    let anchor = cell_at(rect.row, rect.column)?;
    let mut lines: Vec<&str> = Vec::new();
    let mut covered: Vec<&Node> = Vec::with_capacity(rect.rows * rect.columns);
    for row in rect.row..rect.row + rect.rows {
        for column in rect.column..rect.column + rect.columns {
            let cell = cell_at(row, column)?;
            if !cell.text.is_empty() {
                lines.push(cell.text.as_str());
            }
            if (row, column) == (rect.row, rect.column) {
                continue;
            }
            if cell.style != anchor.style || cell.asset.is_some() || !cell.children.is_empty() || !cell.interactions.is_empty()
                || !cell.variants.is_empty() || cell.ext != NodeExt::default()
            {
                return Err("가려질 칸의 글자·채우기 서식이나 개체 정보가 첫 칸과 달라 합칠 수 없습니다. 서식을 맞춘 뒤 다시 고르세요".into());
            }
            if !cell.layout.as_ref().is_some_and(default_cell_layout) {
                return Err("가려질 칸의 배치 설정이 기본값과 달라 합칠 수 없습니다".into());
            }
            if cell.bounds.height != TABLE_CELL_HEIGHT {
                return Err("가려질 칸의 높이가 기본값과 달라 합칠 수 없습니다".into());
            }
            covered.push(cell);
        }
    }
    let covered_ids: BTreeSet<&str> = covered.iter().map(|cell| cell.id.as_str()).collect();
    if document.bindings.iter().any(|binding| covered_ids.contains(binding.node_id.as_str())) {
        return Err("가려질 칸에 디자인 토큰이 연결되어 있어 합칠 수 없습니다".into());
    }
    if figure_links.iter().any(|link| covered_ids.contains(link.target_node_id.as_str())) {
        return Err("가려질 칸에 연결된 그림이 있어 합칠 수 없습니다".into());
    }
    let mut refusal = None;
    for other in &document.pages {
        walk(&other.nodes, &mut |node| {
            if refusal.is_some() {
                return;
            }
            let targeted = node.interactions.iter().any(|interaction| match interaction {
                Interaction::ToggleVisibility { node_id } | Interaction::SetVariant { node_id, .. } => covered_ids.contains(node_id.as_str()),
                _ => false,
            });
            if targeted {
                refusal = Some("가려질 칸을 가리키는 상호작용이 있어 합칠 수 없습니다");
            } else if node.ext.instance.as_ref().is_some_and(|instance| {
                instance.component_page_id == page.id && instance.overrides.keys().any(|id| covered_ids.contains(id.as_str()))
            }) {
                refusal = Some("가려질 칸을 컴포넌트 인스턴스에서 따로 고쳐 두어 합칠 수 없습니다");
            }
        });
    }
    if let Some(refusal) = refusal {
        return Err(refusal.into());
    }
    let mut operations = Vec::with_capacity(covered.len() + 1);
    let joined = lines.join("\n");
    if joined != anchor.text {
        operations.push(Operation::SetText { node_id: anchor.id.clone(), text: joined });
    }
    operations.extend(
        covered.iter().filter(|cell| !cell.text.is_empty()).map(|cell| Operation::SetText { node_id: cell.id.clone(), text: String::new() }),
    );
    Ok(operations)
}

/// Where a table sits: inside a component instance (its structure is edited on the component's page), inside a locked group.
#[derive(Clone, Copy, Default)]
struct Owners {
    instance: bool,
    locked_group: bool,
}

/// The node `id` among `nodes`, and what it sits in.
fn find_table<'a>(nodes: &'a [Node], id: &str, owners: Owners) -> Option<(&'a Node, Owners)> {
    for node in nodes {
        if node.id == id {
            return Some((node, owners));
        }
        let inner = Owners {
            instance: owners.instance || node.ext.instance.is_some(),
            locked_group: owners.locked_group || node.ext.locked == Some(true),
        };
        if let Some(found) = find_table(&node.children, id, inner) {
            return Some(found);
        }
    }
    None
}

/// Why no cells of `table_node` can be merged: a table inside a component instance, a locked table or cell, a table inside a
/// locked group.
fn table_refusal(table_node: &Node, owners: Owners) -> Option<&'static str> {
    if owners.instance {
        return Some("컴포넌트 인스턴스 안의 표는 원본 페이지에서 합치세요");
    }
    if table_node.ext.locked == Some(true) || table_node.children.iter().any(|cell| cell.ext.locked == Some(true)) {
        return Some("잠긴 표는 셀을 합칠 수 없습니다");
    }
    if owners.locked_group {
        return Some("잠긴 그룹 안의 표는 셀을 합칠 수 없습니다");
    }
    None
}

/// The integer argument `key`, at least `minimum`.
fn grid_arg(args: &Value, key: &str, minimum: u64) -> Result<usize, String> {
    let value = args.get(key).filter(|value| !value.is_null()).ok_or_else(|| format!("{key} 값을 지정하세요"))?;
    value
        .as_u64()
        .filter(|value| *value >= minimum)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| format!("{key}는 {minimum} 이상의 정수여야 합니다"))
}

fn table_id_arg(args: &Value) -> Result<String, String> {
    args.get("table_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "table_id 값을 지정하세요".into())
}

/// What a span edit sends: the table's structure before and after, and the text moves that come before it.
struct SpanPlan {
    expected: CanvasTable,
    table: CanvasTable,
    operations: Vec<Operation>,
}

/// Why a merge that waited for an edit is dropped (`Some(Ok(reason))`), or `Some(Err(()))` while the edit is still being
/// stored; `None` once the edit is gone and nothing above applies.
fn after_edit_drop_reason(app: &DesktopApp, waiting: &AfterEdit) -> Option<Result<&'static str, ()>> {
    if app.active.as_ref().is_none_or(|active| active.id != waiting.artifact_id) {
        return Some(Ok("문서가 바뀌어 기다리던 셀 병합·해제를 하지 않았습니다"));
    }
    if app.screen != Screen::Editor {
        return Some(Ok("편집 화면을 떠나 기다리던 셀 병합·해제를 하지 않았습니다"));
    }
    if waiting.since.elapsed() >= AFTER_EDIT_WAIT {
        return Some(Ok("편집 중인 글이 제때 저장되지 않아 셀 병합·해제를 하지 않았습니다. 다시 시도해 주세요"));
    }
    if let Some(session) = app.inline_text_edit.as_ref() {
        if session.id != waiting.session_id {
            return Some(Ok("다른 글 편집이 시작되어 기다리던 셀 병합·해제를 하지 않았습니다"));
        }
        if session.conflict.is_some() {
            return Some(Ok("편집 중인 글을 저장하지 못해 셀 병합·해제를 하지 않았습니다. 입력은 그대로 두었습니다"));
        }
        return Some(Err(()));
    }
    None
}

impl DesktopApp {
    /// `table.merge_cells`: the rectangle given by `row`, `column`, `rows`, `columns` becomes one merged cell.
    pub(super) fn table_merge_cells(&mut self, args: &Value) -> Result<(), String> {
        let table_id = table_id_arg(args)?;
        let rect = CellRect {
            row: grid_arg(args, "row", 0)?,
            column: grid_arg(args, "column", 0)?,
            rows: grid_arg(args, "rows", 1)?,
            columns: grid_arg(args, "columns", 1)?,
        };
        self.run_span_edit(table_id, SpanEdit::Merge(rect))
    }

    /// `table.unmerge_cell`: the merged region that holds the cell at `row`, `column` is split again.
    pub(super) fn table_unmerge_cell(&mut self, args: &Value) -> Result<(), String> {
        let table_id = table_id_arg(args)?;
        let edit = SpanEdit::Unmerge { row: grid_arg(args, "row", 0)?, column: grid_arg(args, "column", 0)? };
        self.run_span_edit(table_id, edit)
    }

    fn run_span_edit(&mut self, table_id: String, edit: SpanEdit) -> Result<(), String> {
        if let Some((session_id, artifact_id, revision)) =
            self.inline_text_edit.as_ref().map(|session| (session.id, session.artifact_id.clone(), session.revision))
        {
            if self.run_context.is_some() {
                return Err(EDITING_REFUSAL.into());
            }
            // The window's merge: the edited text is stored first, then the merge reads it (`run_span_after_edit`).
            self.table_merge.after_edit = Some(AfterEdit { table_id, edit, session_id, artifact_id, revision, since: Instant::now() });
            self.finish_inline_text_edit();
            return Ok(());
        }
        let plan = self.span_plan(&table_id, edit)?;
        self.apply_canvas_table_structure(table_id, plan.expected, plan.table, plan.operations)
    }

    /// The table `table_id` of the open Canvas document: its page, its node and what it sits in.
    fn locate_table(&self, table_id: &str) -> Option<(&CanvasDocument, &Page, &Node, Owners)> {
        let artifact = self.artifact.as_ref().filter(|artifact| artifact.kind == ArtifactKind::Canvas)?;
        let ArtifactBody::Canvas(document) = &artifact.body else { return None };
        document.pages.iter().find_map(|page| {
            find_table(&page.nodes, table_id, Owners::default()).map(|(node, owners)| (document, page, node, owners))
        })
    }

    /// The change a span edit of table `table_id` makes, or why it is refused. The table is found on the document, not by
    /// the selection: a merge is made from selected cells, and then no table node is the selection.
    fn span_plan(&self, table_id: &str, edit: SpanEdit) -> Result<SpanPlan, String> {
        self.ensure_canvas_table_edit_supported()?;
        let (document, page, table_node, owners) = self.locate_table(table_id).ok_or("표를 찾지 못했습니다")?;
        let table = table_node.ext.table.as_ref().filter(|_| table_node.kind == NodeKind::Group).ok_or("지정한 노드는 표가 아닙니다")?;
        if let Some(refusal) = table_refusal(table_node, owners) {
            return Err(refusal.into());
        }
        let after = structure_after(table, &cell_positions(table), edit)?;
        let operations = match edit {
            SpanEdit::Merge(rect) => {
                let figure_links = self.artifact.as_ref().map_or(&[][..], |artifact| artifact.figure_links.as_slice());
                merge_operations(document, figure_links, page, table_node, table, rect)?
            }
            SpanEdit::Unmerge { .. } => Vec::new(),
        };
        Ok(SpanPlan { expected: table.clone(), table: after, operations })
    }

    /// Whether `edit` of table `table_id` would be sent now, else why not.
    pub(super) fn span_edit_check(&self, table_id: &str, edit: SpanEdit) -> Result<(), String> {
        self.span_plan(table_id, edit).map(|_| ())
    }

    /// The edit `cells` of table `table_id` ask for: the unmerge of a merged cell (its anchor, with or without cells it
    /// covers), else a merge of the rectangle they form. `Err` says why they are not an edit.
    pub(super) fn span_request_for(&self, table_id: &str, cells: &[String]) -> Result<SpanEdit, String> {
        let (_, _, table_node, owners) = self.locate_table(table_id).ok_or("표를 찾지 못했습니다")?;
        if let Some(refusal) = table_refusal(table_node, owners) {
            return Err(refusal.into());
        }
        let table = table_node.ext.table.as_ref().ok_or("지정한 노드는 표가 아닙니다")?;
        let grid = cell_positions(table);
        let positions: Vec<(usize, usize)> =
            cells.iter().map(|id| grid.get(id.as_str()).copied().ok_or("같은 표의 셀을 고르세요")).collect::<Result<_, _>>()?;
        if positions.is_empty() || positions.iter().collect::<BTreeSet<_>>().len() != positions.len() {
            return Err("같은 표의 셀을 고르세요".into());
        }
        let region = |at: (usize, usize)| region_at(table, &grid, at).map(|(anchor, _)| anchor);
        // The anchor with only cells it covers (a covered cell alone is no unmerge target).
        let one_region = cells
            .iter()
            .find(|id| table.cell_spans.contains_key(id.as_str()))
            .map(String::as_str)
            .filter(|anchor| positions.iter().all(|at| region(*at) == Some(*anchor)));
        if let Some(anchor) = one_region {
            let (row, column) = grid[anchor];
            return Ok(SpanEdit::Unmerge { row, column });
        }
        if cells.len() < 2 {
            return Err("합칠 두 칸 이상을 고르세요".into());
        }
        let top = positions.iter().map(|(row, _)| *row).min().unwrap_or(0);
        let left = positions.iter().map(|(_, column)| *column).min().unwrap_or(0);
        let bottom = positions.iter().map(|(row, _)| *row).max().unwrap_or(0);
        let right = positions.iter().map(|(_, column)| *column).max().unwrap_or(0);
        let rect = CellRect { row: top, column: left, rows: bottom - top + 1, columns: right - left + 1 };
        // No duplicates (checked above): as many cells as the rectangle holds fill it.
        if rect.rows.checked_mul(rect.columns) != Some(cells.len()) {
            return Err("직사각형 범위의 셀만 합칠 수 있습니다".into());
        }
        Ok(SpanEdit::Merge(rect))
    }

    /// 셀 병합 (`unmerge` false) or 병합 해제 of `cells` in table `table_id`: the command, or why it is not offered (every
    /// check of the command itself, `span_edit_check`).
    pub(super) fn span_command_for(&self, table_id: &str, cells: &[String], unmerge: bool) -> SpanCommand {
        let edit = self.span_request_for(table_id, cells)?;
        match (edit, unmerge) {
            (SpanEdit::Unmerge { .. }, false) => return Err("이미 병합된 칸입니다".into()),
            (SpanEdit::Merge(_), true) => return Err("병합된 칸을 고르세요".into()),
            _ => {}
        }
        self.span_edit_check(table_id, edit)?;
        Ok(edit.command(table_id))
    }

    /// The properties panel's offer for the selection (`Some(true)`: 병합 해제, `Some(false)`: 병합), worked out again only
    /// when the document's revision, the page or the selection changed.
    pub(super) fn panel_span_offer(&mut self) -> Option<bool> {
        let (Some(active), Some(context)) = (self.active.as_ref(), self.context.as_ref()) else { return None };
        let fresh = self.table_merge.panel.as_ref().is_some_and(|(key, _)| {
            key.artifact_id == active.id
                && key.revision == active.revision
                && key.context_revision == context.revision
                && key.page_index == self.view_ref().page_index
                && key.primary == self.selected_node
                && key.selected == self.selected_nodes
        });
        if !fresh {
            let key = PanelKey {
                artifact_id: active.id.clone(),
                revision: active.revision,
                context_revision: context.revision,
                page_index: self.view_ref().page_index,
                primary: self.selected_node.clone(),
                selected: self.selected_nodes.clone(),
            };
            let offer = self.canvas_table_span_operation().map(|(unmerge, _)| unmerge);
            self.table_merge.panel = Some((key, offer));
        }
        self.table_merge.panel.as_ref().and_then(|(_, offer)| *offer)
    }

    /// Runs what waited for a cell's edit to be stored (called every frame): the selection a right click outside the edit
    /// asked for, and a window merge. The merge runs once the window shows the revision the edit's text was stored at; it is
    /// dropped, with its reason, when the document or the screen changed, another change came first, another edit began,
    /// the edit could not be stored, or it waited too long.
    pub(super) fn run_span_after_edit(&mut self) {
        if let Some((session_id, artifact_id, node_id)) = self.table_merge.select_after_edit.as_ref() {
            match self.inline_text_edit.as_ref() {
                Some(session) if session.id == *session_id => {}
                Some(_) => self.table_merge.select_after_edit = None,
                None => {
                    let current = self.active.as_ref().is_some_and(|active| &active.id == artifact_id);
                    let node_id = node_id.clone();
                    self.table_merge.select_after_edit = None;
                    if current {
                        self.select_ids(vec![node_id]);
                    }
                }
            }
        }
        let Some(waiting) = self.table_merge.after_edit.as_mut() else { return };
        // The edit's own stored text moves the revision the merge is made against.
        if let Some(session) = self.inline_text_edit.as_ref().filter(|session| session.id == waiting.session_id) {
            waiting.revision = waiting.revision.max(session.revision);
        }
        let Some(waiting) = self.table_merge.after_edit.as_ref() else { return };
        match after_edit_drop_reason(self, waiting) {
            Some(Err(())) => return,
            Some(Ok(reason)) => {
                self.table_merge.after_edit = None;
                self.set_error(reason.into());
                return;
            }
            None => {}
        }
        let reading = self.artifact_revision_change_pending(&waiting.artifact_id)
            || self.pending.values().any(|pending| matches!(pending.action, PendingAction::Inspect | PendingAction::Context));
        if reading || !self.canvas_table_context_current() {
            return;
        }
        let Some(AfterEdit { table_id, edit, revision, .. }) = self.table_merge.after_edit.take() else { return };
        if self.active.as_ref().is_none_or(|active| active.revision != revision) {
            self.set_error("다른 변경이 먼저 반영되어 기다리던 셀 병합·해제를 하지 않았습니다. 표를 확인한 뒤 다시 고르세요".into());
            return;
        }
        let result = self
            .span_plan(&table_id, edit)
            .and_then(|plan| self.apply_canvas_table_structure(table_id, plan.expected, plan.table, plan.operations));
        if let Err(message) = result {
            self.set_error(message);
        }
    }

    /// The open menu's 셀 병합 and 병합 해제, as made when it opened.
    pub(super) fn table_menu_commands(&self) -> Option<&[SpanCommand; 2]> {
        self.table_merge.menu.as_ref()
    }

    /// Whether a window merge waits for a cell's edit to be stored.
    pub(super) fn span_waits_for_edit(&self) -> bool {
        self.table_merge.after_edit.is_some()
    }

    /// Runs a span command the panel or the menu prepared; a refusal is shown.
    pub(super) fn run_span_action(&mut self, action: SpanCommand) {
        match action {
            Ok((command, args)) => self.invoke_ui(command, args),
            Err(message) => self.set_error(message),
        }
    }

    /// The table cell of `table_node` drawn under `position` (a covered cell gives its merged cell's anchor).
    fn cell_under(&self, table_node: &Node, page_index: u32, position: Pos2, page_rect: Rect) -> Option<String> {
        let drawn = self.page_textures.get(&page_index)?;
        let table = table_node.ext.table.as_ref()?;
        let positions = cell_positions(table);
        let cell = table_node.children.iter().find(|cell| {
            drawn.frames.get(&cell.id).is_some_and(|frame| super::page_to_screen(*frame, drawn.page.size, page_rect).contains(position))
        })?;
        let at = *positions.get(cell.id.as_str())?;
        Some(region_at(table, &positions, at).map_or(cell.id.as_str(), |(anchor, _)| anchor).to_owned())
    }

    /// The table (and its cell) a right click on page `page_index` pressed: the topmost thing under the pointer must be the
    /// table, one of its cells, its border band or the edited cell's editor. `clicked` is the widget the click went to; the
    /// board itself (`board_id`) takes it only where no node region is, so while a cell's text is edited (the regions are
    /// not drawn) the topmost drawn node there decides.
    fn right_click_target(&self, clicked: Option<Id>, board_id: Id, page_index: u32, position: Pos2, page_rect: Rect) -> Option<(String, Option<String>)> {
        let clicked = clicked?;
        let ArtifactBody::Canvas(document) = &self.artifact.as_ref()?.body else { return None };
        let page = document.pages.get(page_index as usize)?;
        let drawn = self.page_textures.get(&page_index)?;
        let table_with_cell = |table: &Node| Some((table.id.clone(), self.cell_under(table, page_index, position, page_rect)));
        // The node `id` is the table or a cell of it: the table, and the cell under the pointer.
        let as_table = |id: &str| {
            let mut found = None;
            walk(&page.nodes, &mut |node| {
                if found.is_none()
                    && node.ext.table.as_ref().is_some_and(|table| node.id == id || table.rows.iter().any(|row| row.cell_ids.iter().any(|cell| cell == id)))
                {
                    found = Some(node);
                }
            });
            found.and_then(table_with_cell)
        };
        if let Some(session) = self.inline_text_edit.as_ref().filter(|session| session.page_index == page_index) {
            if clicked == self.text_editor_id {
                return as_table(&session.node_id);
            }
            if clicked == board_id {
                // The topmost node drawn under the pointer (the last one in drawing order).
                let mut topmost = None;
                walk(&page.nodes, &mut |node| {
                    if node.ext.hidden != Some(true)
                        && drawn.frames.get(&node.id).is_some_and(|frame| super::page_to_screen(*frame, drawn.page.size, page_rect).contains(position))
                    {
                        topmost = Some(node.id.as_str());
                    }
                });
                return as_table(topmost?);
            }
        }
        for (index, source) in drawn.page.source_map.iter().enumerate() {
            if Id::new(("scene-region", page_index, source.node_id.as_str(), index)) == clicked {
                return as_table(&source.node_id);
            }
        }
        let mut band = None;
        walk(&page.nodes, &mut |node| {
            if band.is_none() && node.ext.table.is_some()
                && (0..4).any(|side: usize| Id::new(("table-border-band", page_index, node.id.as_str(), side)) == clicked)
            {
                band = Some(node);
            }
        });
        band.and_then(table_with_cell)
    }

    /// Whether the selection is only cells of `table`, and their range (merged regions included) holds `cell`.
    fn selection_holds_cell(&self, table: &CanvasTable, cell: &str) -> bool {
        let positions = cell_positions(table);
        let Some(&at) = positions.get(cell) else { return false };
        let mut range: Option<CellRect> = None;
        for id in &self.selected_nodes {
            let Some(&(row, column)) = positions.get(id.as_str()) else { return false };
            let rect = table.cell_spans.get(id).map_or(CellRect { row, column, rows: 1, columns: 1 }, |span| span_rect((row, column), span));
            range = Some(match range {
                None => rect,
                Some(so_far) => {
                    let (top, left) = (so_far.row.min(rect.row), so_far.column.min(rect.column));
                    let bottom = (so_far.row + so_far.rows).max(rect.row + rect.rows);
                    let right = (so_far.column + so_far.columns).max(rect.column + rect.columns);
                    CellRect { row: top, column: left, rows: bottom - top, columns: right - left }
                }
            });
        }
        range.is_some_and(|range| range.contains(at))
    }

    /// The cells the menu of table `table_id` acts on, and the selection the right press makes: the pressed cell, unless the
    /// selection is only cells of this table whose range holds it; the table, when the press is on no cell and the selection
    /// is not of this table. On a table whose cells cannot be merged the selection stays. While a cell's text is edited, a
    /// press on that cell (or the merged cell it heads) keeps the edit; a press elsewhere stores the edit, as a click outside
    /// does, and selects what was pressed once the edit is stored. `None`: no menu (the edit is composing).
    fn table_menu_cells(&mut self, table_id: &str, cell: Option<String>) -> Option<Vec<String>> {
        let (refused, holds, of_table) = {
            let (_, _, table_node, owners) = self.locate_table(table_id)?;
            let table = table_node.ext.table.as_ref()?;
            let positions = cell_positions(table);
            let of_table = !self.selected_nodes.is_empty()
                && self.selected_nodes.iter().all(|id| id == table_id || positions.contains_key(id.as_str()));
            (table_refusal(table_node, owners).is_some(), cell.as_deref().is_some_and(|cell| self.selection_holds_cell(table, cell)), of_table)
        };
        if let Some(session) = self.inline_text_edit.as_ref() {
            if session.composing || self.text_adapter.is_ime_active() {
                return None;
            }
            let (session_id, artifact_id, edited) = (session.id, session.artifact_id.clone(), session.node_id.clone());
            if cell.as_deref() == Some(edited.as_str()) {
                return Some(vec![edited]);
            }
            self.finish_inline_text_edit();
            if refused {
                return Some(cell.into_iter().collect());
            }
            let pressed = cell.clone().unwrap_or_else(|| table_id.to_owned());
            if self.inline_text_edit.is_some() {
                self.table_merge.select_after_edit = Some((session_id, artifact_id, pressed));
            } else {
                self.select_ids(vec![pressed]);
            }
            return Some(cell.into_iter().collect());
        }
        if !refused {
            match &cell {
                Some(_) if holds => {}
                Some(cell) => self.select_ids(vec![cell.clone()]),
                None if of_table => {}
                None => self.select_ids(vec![table_id.to_owned()]),
            }
        }
        Some(self.selected_nodes.clone())
    }

    /// The board's right-click menu of a Canvas table: 셀 병합 and 병합 해제, an item that cannot run disabled with its reason.
    /// `pressed`: where a right click on the board landed this frame (page, point, the page's screen rectangle). The menu
    /// opens only when the topmost thing pressed is a table (`right_click_target`); its commands are made then.
    pub(super) fn table_cell_context_menu(&mut self, ctx: &egui::Context, response: &egui::Response, pressed: Option<(u32, Pos2, Rect)>) {
        let mut opened = false;
        if let Some((page_index, position, page_rect)) = pressed {
            let clicked = ctx.interaction_snapshot(|interaction| interaction.clicked);
            self.table_merge.menu = None;
            if let Some((table_id, cell)) = self.right_click_target(clicked, response.id, page_index, position, page_rect) {
                self.focus_page(page_index);
                if let Some(cells) = self.table_menu_cells(&table_id, cell) {
                    self.table_merge.menu = Some([false, true].map(|unmerge| self.span_command_for(&table_id, &cells, unmerge)));
                    opened = true;
                }
            }
        }
        let Some(items) = self.table_merge.menu.as_ref() else {
            self.table_merge.menu_items = None;
            return;
        };
        let palette = theme::palette(ctx);
        let mut chosen = None;
        let mut rects = [Rect::NOTHING; 2];
        let shown = egui::Popup::context_menu(response)
            .id(Id::new("table-cell-menu"))
            .open_memory(opened.then_some(egui::SetOpenCommand::Bool(true)))
            .show(|ui| {
                for (index, (action, (icon, label))) in items.iter().zip([("add", "셀 병합"), ("minus", "병합 해제")]).enumerate() {
                    let item = ui.add_enabled_ui(action.is_ok(), |ui| crate::widgets::menu_item(ui, &palette, Some(icon), label, "", false)).inner;
                    rects[index] = item.rect;
                    match action {
                        Ok(action) if item.clicked() => {
                            chosen = Some(action.clone());
                            ui.close();
                        }
                        Ok(_) => {}
                        Err(reason) => {
                            item.on_disabled_hover_text(reason.as_str());
                        }
                    }
                }
            });
        if shown.is_none() {
            self.table_merge.menu = None;
            self.table_merge.menu_items = None;
            return;
        }
        self.table_merge.menu_items = Some(rects);
        if let Some(action) = chosen {
            self.run_span_action(Ok(action));
        }
    }
}
