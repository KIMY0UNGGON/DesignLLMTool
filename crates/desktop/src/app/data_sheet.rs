//! The data sheet under a figure's board (figure-board, tokens `component.data-sheet`): when a chart panel is selected,
//! the table it plots is shown in a 240 px strip at the bottom of the board (dragged between 160 and 480), with a bar
//! (`CSV 가져오기`), a header row (type icon, name, unit), numbered rows, numbers right-aligned, empty cells hatched and
//! ambiguous cells in `warn-soft`. Clicking a cell edits it in place; Enter or leaving the cell commits one
//! `data.edit_cell` (the host derives the new dataset version and rebinds the plot). `행 추가` / `열 추가` in the bar and
//! `행 삭제` / `열 삭제` in the menu of a row number or a header are `data.add_row`, `data.add_column`,
//! `data.remove_row` and `data.remove_column`. `정렬` (`Sort`) and `단위 변환` (`UnitScale`) in the bar and header menu
//! emit `figure.dataset.edit` transforms against the current figure revision and dataset version.

use designllm_contracts::{CellState, DataColumn, DataTransform, Dataset, PanelBody};
use eframe::egui::{self, Align2, CornerRadius, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use serde_json::json;

use super::{figure_tools, DatasetCellEdit, DesktopApp};
use crate::{icons, theme, widgets};

const SHEET_DEFAULT: f32 = 240.0;
const SHEET_MIN: f32 = 160.0;
const SHEET_MAX: f32 = 480.0;
const BAR: f32 = 40.0;
const HEADER: f32 = 32.0;
const ROW: f32 = 28.0;
const ROW_NUMBER: f32 = 40.0;
const COLUMN: f32 = 150.0;
const SORT_MENU_ID: &str = "sheet-sort-menu";
const UNIT_SCALE_MENU_ID: &str = "sheet-unit-scale-menu";
const UNIT_SCALE_DRAFT_ID: &str = "sheet-unit-scale-draft";
/// The label column and the field column of the unit-scale form.
const UNIT_FORM_LABEL: f32 = 64.0;
const UNIT_FORM_FIELD: f32 = 200.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct UnitScaleDraft {
    pub dataset_id: String,
    pub version_hash: String,
    pub revision: u64,
    pub column_id: String,
    pub numerator: String,
    pub denominator: String,
    pub output_unit: String,
}

impl DesktopApp {
    /// The dataset the selected chart panel plots, when a chart panel (or something in it) is selected.
    pub(super) fn sheet_dataset(&self) -> Option<Dataset> {
        let figure = self.figure_document()?;
        let selected = self.selected_nodes.first().cloned().or_else(|| self.selected_node.clone())?;
        let panel = figure_tools::owning_panel(&figure, &selected)?;
        let PanelBody::Plot(plot) = &panel.body else {
            return None;
        };
        figure.datasets.iter().find(|dataset| dataset.id == plot.dataset_ref.dataset_id).cloned()
    }

    /// The board's share of `area` when the sheet shows: the board on top, the sheet under it.
    pub(super) fn split_for_sheet(&self, ctx: &egui::Context, area: Rect) -> (Rect, Option<Rect>) {
        if self.view_ref().sheet_hidden || self.sheet_dataset().is_none() {
            return (area, None);
        }
        let height: f32 = ctx.data(|data| data.get_temp(egui::Id::new("sheet-height"))).unwrap_or(SHEET_DEFAULT);
        let height = height.clamp(SHEET_MIN, SHEET_MAX.min(area.height() - 160.0).max(SHEET_MIN));
        let sheet = Rect::from_min_max(Pos2::new(area.left(), area.bottom() - height), area.max);
        let board = Rect::from_min_max(area.min, Pos2::new(area.right(), sheet.top()));
        (board, Some(sheet))
    }

    /// Draws the sheet in `rect`.
    pub(super) fn draw_data_sheet(&mut self, ui: &mut Ui, palette: &theme::Palette, rect: Rect) {
        let Some(dataset) = self.sheet_dataset() else {
            return;
        };
        let painter = ui.painter().clone();
        painter.rect_filled(rect, CornerRadius::ZERO, palette.surface);
        painter.hline(rect.x_range(), rect.top() + 0.5, Stroke::new(theme::hairline(ui.ctx()), palette.line));
        // The top line is a handle: dragging it changes the height (160..480).
        let handle = Rect::from_min_max(Pos2::new(rect.left(), rect.top() - 3.0), Pos2::new(rect.right(), rect.top() + 3.0));
        let response = ui.interact(handle, ui.id().with("sheet-resize"), Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeVertical);
        if response.dragged_by(egui::PointerButton::Primary) {
            let id = egui::Id::new("sheet-height");
            let height: f32 = ui.data(|data| data.get_temp(id)).unwrap_or(SHEET_DEFAULT);
            ui.data_mut(|data| data.insert_temp(id, (height - response.drag_delta().y).clamp(SHEET_MIN, SHEET_MAX)));
        }
        let revision = self.shown_figure_revision();
        let sheet_ready = self.sheet_transform_ready();
        // The bar: CSV import, structure edits, and Sort / UnitScale transforms.
        let bar = Rect::from_min_size(rect.min, Vec2::new(rect.width(), BAR));
        let mut bar_ui = ui.new_child(egui::UiBuilder::new().max_rect(bar.shrink2(Vec2::new(8.0, 4.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        bar_ui.spacing_mut().item_spacing = Vec2::new(theme::GROUP_GAP, 0.0);
        if widgets::labeled_button(&mut bar_ui, palette, Some("ins-data"), "CSV 가져오기", false).clicked() {
            self.invoke_ui("add.csv", json!({}));
        }
        widgets::group_separator(&mut bar_ui, palette);
        if widgets::labeled_button(&mut bar_ui, palette, Some("add-row"), "행 추가", false).clicked() {
            self.invoke_ui("data.add_row", json!({ "dataset_id": dataset.id }));
        }
        if widgets::labeled_button(&mut bar_ui, palette, Some("add-column"), "열 추가", false).clicked() {
            self.invoke_ui("data.add_column", json!({ "dataset_id": dataset.id }));
        }
        widgets::group_separator(&mut bar_ui, palette);
        self.draw_sort_control(&mut bar_ui, palette, &dataset, revision, sheet_ready);
        self.draw_unit_scale_control(&mut bar_ui, palette, &dataset, revision, sheet_ready);
        let grid = Rect::from_min_max(Pos2::new(rect.left(), bar.bottom()), rect.max);
        let mut grid_ui = ui.new_child(egui::UiBuilder::new().max_rect(grid));
        egui::ScrollArea::both().auto_shrink([false, false]).show(&mut grid_ui, |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            self.draw_sheet_grid(ui, palette, &dataset, revision, sheet_ready);
        });
    }

    fn draw_sort_control(
        &mut self,
        ui: &mut Ui,
        palette: &theme::Palette,
        dataset: &Dataset,
        revision: Option<u64>,
        sheet_ready: bool,
    ) {
        let sort_id = egui::Id::new(SORT_MENU_ID);
        let sort_open = egui::Popup::is_id_open(ui.ctx(), sort_id);
        let can_sort = sheet_ready && !dataset.columns.is_empty();
        let sort_button = ui
            .add_enabled_ui(can_sort, |ui| {
                widgets::labeled_button_with(ui, palette, Some("content:sort"), "정렬", false, sort_open, false)
            })
            .inner;
        let mut sort_action: Option<(String, bool)> = None;
        egui::Popup::from_toggle_button_response(&sort_button)
            .id(sort_id)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                if !can_sort {
                    ui.close();
                    return;
                }
                ui.set_min_width(220.0);
                egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                    for column in &dataset.columns {
                        ui.horizontal(|ui| {
                            ui.label(&column.name);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if widgets::labeled_button(ui, palette, None, "내림차순", false).clicked() {
                                    sort_action = Some((column.id.clone(), true));
                                    ui.close();
                                }
                                if widgets::labeled_button(ui, palette, None, "오름차순", false).clicked() {
                                    sort_action = Some((column.id.clone(), false));
                                    ui.close();
                                }
                            });
                        });
                    }
                });
            });
        if let Some((column_id, descending)) = sort_action {
            match revision {
                Some(rev) => {
                    if let Err(err) = self.sort_dataset_column(&dataset.id, &dataset.version_hash, rev, &column_id, descending) {
                        self.set_error(err);
                    }
                }
                None => self.set_error("작업물이 다른 편집으로 바뀌었습니다".into()),
            }
        }
    }

    fn draw_unit_scale_control(
        &mut self,
        ui: &mut Ui,
        palette: &theme::Palette,
        dataset: &Dataset,
        revision: Option<u64>,
        sheet_ready: bool,
    ) {
        let scale_id = egui::Id::new(UNIT_SCALE_MENU_ID);
        let draft_id = unit_scale_draft_id(&dataset.id);
        let scale_open = egui::Popup::is_id_open(ui.ctx(), scale_id);
        let numeric_columns: Vec<&DataColumn> = dataset.columns.iter().filter(|column| is_numeric_column(column)).collect();
        let can_scale = sheet_ready && !numeric_columns.is_empty();
        let scale_button = ui
            .add_enabled_ui(can_scale, |ui| {
                widgets::labeled_button_with(ui, palette, Some("col-number"), "단위 변환", false, scale_open, false)
            })
            .inner;
        if !scale_open && scale_button.clicked() {
            if let (Some(rev), Some(first)) = (revision, numeric_columns.first()) {
                store_unit_scale_draft(ui.ctx(), &dataset.id, Some(new_unit_scale_draft(dataset, rev, &first.id)));
            }
        }
        let mut submit: Option<UnitScaleDraft> = None;
        let mut cancel = false;
        egui::Popup::from_toggle_button_response(&scale_button)
            .id(scale_id)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                let mut draft = ui
                    .ctx()
                    .data(|data| data.get_temp::<Option<UnitScaleDraft>>(draft_id))
                    .flatten()
                    .unwrap_or_else(|| {
                        new_unit_scale_draft(
                            dataset,
                            revision.unwrap_or(0),
                            numeric_columns.first().map(|column| column.id.as_str()).unwrap_or_default(),
                        )
                    });
                // The column list is menu rows (at least 200 wide); the fields under it take the same width.
                ui.set_width(UNIT_FORM_LABEL + 8.0 + UNIT_FORM_FIELD);
                egui::Grid::new("sheet-unit-scale-grid").num_columns(2).spacing([8.0, 6.0]).min_col_width(UNIT_FORM_LABEL).show(ui, |ui| {
                    // The column list is taller than its label: the label sits at the top of the row.
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| ui.label(crate::i18n::tr("열")));
                    ui.vertical(|ui| {
                        ui.set_width(UNIT_FORM_FIELD);
                        for column in &numeric_columns {
                            let label = column_unit_label(column);
                            if widgets::menu_item_verbatim(ui, palette, None, &label, draft.column_id == column.id).clicked() {
                                draft.column_id = column.id.clone();
                            }
                        }
                    });
                    ui.end_row();

                    ui.label(crate::i18n::tr("출력 단위"));
                    widgets::edit_line(ui, palette, &mut draft.output_unit, widgets::Field { width: UNIT_FORM_FIELD, ..Default::default() });
                    ui.end_row();

                    ui.label(crate::i18n::tr("분자"));
                    widgets::edit_line(ui, palette, &mut draft.numerator, widgets::Field { width: UNIT_FORM_FIELD, ..Default::default() });
                    ui.end_row();

                    ui.label(crate::i18n::tr("분모"));
                    widgets::edit_line(ui, palette, &mut draft.denominator, widgets::Field { width: UNIT_FORM_FIELD, ..Default::default() });
                    ui.end_row();
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let can_submit = can_scale
                        && !draft.column_id.is_empty()
                        && !draft.output_unit.trim().is_empty()
                        && !draft.numerator.trim().is_empty()
                        && !draft.denominator.trim().is_empty();
                    if ui.add_enabled_ui(can_submit, |ui| widgets::labeled_button(ui, palette, None, "적용", true)).inner.clicked() {
                        submit = Some(draft.clone());
                        ui.close();
                    }
                    if widgets::labeled_button(ui, palette, None, "취소", false).clicked() {
                        cancel = true;
                        ui.close();
                    }
                });
                if !cancel && submit.is_none() {
                    store_unit_scale_draft(ui.ctx(), &dataset.id, Some(draft));
                }
            });
        if cancel {
            store_unit_scale_draft(ui.ctx(), &dataset.id, None);
        } else if let Some(draft) = submit {
            store_unit_scale_draft(ui.ctx(), &dataset.id, None);
            if let Err(err) = self.unit_scale_dataset_column(&draft) {
                self.set_error(err);
            }
        }
    }

    fn draw_sheet_grid(
        &mut self,
        ui: &mut Ui,
        palette: &theme::Palette,
        dataset: &Dataset,
        revision: Option<u64>,
        sheet_ready: bool,
    ) {
        let columns = dataset.columns.len();
        let width = ROW_NUMBER + COLUMN * columns as f32;
        let height = HEADER + ROW * dataset.rows.len() as f32;
        let (area, _) = ui.allocate_exact_size(Vec2::new(width + 1.0, height + 1.0), Sense::hover());
        let painter = ui.painter().clone();
        let line = Stroke::new(1.0, palette.line);
        let cell_rect = |column: Option<usize>, row: Option<usize>| {
            let x = match column {
                None => area.left(),
                Some(index) => area.left() + ROW_NUMBER + COLUMN * index as f32,
            };
            let w = if column.is_none() { ROW_NUMBER } else { COLUMN };
            let (y, h) = match row {
                None => (area.top(), HEADER),
                Some(index) => (area.top() + HEADER + ROW * index as f32, ROW),
            };
            Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h))
        };
        let caption = theme::font(theme::Type::Caption);
        let body = theme::font(theme::Type::Body);
        let mut remove: Option<(&str, serde_json::Value)> = None;
        let mut sort_action: Option<(String, bool)> = None;
        let mut scale_for_column: Option<String> = None;
        // Header row and row numbers on `surface-2`.
        let header = cell_rect(None, None);
        painter.rect_filled(header, CornerRadius::ZERO, palette.surface_2);
        painter.rect_stroke(header, CornerRadius::ZERO, line, StrokeKind::Inside);
        for (index, column) in dataset.columns.iter().enumerate() {
            let rect = cell_rect(Some(index), None);
            painter.rect_filled(rect, CornerRadius::ZERO, palette.surface_2);
            painter.rect_stroke(rect, CornerRadius::ZERO, line, StrokeKind::Inside);
            let numeric = is_numeric_column(column);
            let icon = if numeric { "col-number" } else { "col-category" };
            let icon_rect = Rect::from_min_size(Pos2::new(rect.left() + 8.0, rect.center().y - 8.0), Vec2::splat(16.0));
            icons::paint(ui, icon_rect, icon, palette.ink_2);
            let name_pos = Pos2::new(icon_rect.right() + 6.0, rect.center().y);
            let name = painter.text(name_pos, Align2::LEFT_CENTER, &column.name, caption.clone(), palette.ink);
            if let Some(unit) = column.unit.as_deref().filter(|unit| !unit.is_empty()) {
                painter.text(Pos2::new(name.right() + 6.0, rect.center().y), Align2::LEFT_CENTER, unit, caption.clone(), palette.ink_3);
            }
            let response = ui.interact(rect, ui.id().with(("sheet-header", index)), Sense::click());
            response.context_menu(|ui| {
                ui.add_enabled_ui(sheet_ready, |ui| {
                    if widgets::menu_item(ui, palette, Some("chev-up"), "오름차순 정렬", "", false).clicked() {
                        sort_action = Some((column.id.clone(), false));
                        ui.close();
                    }
                    if widgets::menu_item(ui, palette, Some("chev-down"), "내림차순 정렬", "", false).clicked() {
                        sort_action = Some((column.id.clone(), true));
                        ui.close();
                    }
                    if numeric && widgets::menu_item(ui, palette, Some("col-number"), "단위 변환", "", false).clicked() {
                        scale_for_column = Some(column.id.clone());
                        ui.close();
                    }
                });
                if widgets::menu_item(ui, palette, Some("delete"), "열 삭제", "", false).clicked() {
                    remove = Some(("data.remove_column", json!({ "dataset_id": dataset.id, "column_id": column.id })));
                    ui.close();
                }
            });
        }
        let editing = self.dataset_edit.clone();
        let mut start_edit = None;
        let mut commit = None;
        for (row_index, row) in dataset.rows.iter().enumerate() {
            let number = cell_rect(None, Some(row_index));
            painter.rect_filled(number, CornerRadius::ZERO, palette.surface_2);
            painter.rect_stroke(number, CornerRadius::ZERO, line, StrokeKind::Inside);
            painter.text(number.center(), Align2::CENTER_CENTER, (row_index + 1).to_string(), caption.clone(), palette.ink_3);
            ui.interact(number, ui.id().with(("sheet-row", row_index)), Sense::click()).context_menu(|ui| {
                if widgets::menu_item(ui, palette, Some("delete"), "행 삭제", "", false).clicked() {
                    remove = Some(("data.remove_row", json!({ "dataset_id": dataset.id, "row_id": row.id })));
                    ui.close();
                }
            });
            for (column_index, column) in dataset.columns.iter().enumerate() {
                let rect = cell_rect(Some(column_index), Some(row_index));
                let cell = row.cells.get(&column.id);
                let numeric = is_numeric_column(column);
                match cell.map(|cell| cell.state) {
                    None | Some(CellState::Missing) => paint_hatch(&painter, rect, palette),
                    Some(CellState::Ambiguous | CellState::Malformed) => {
                        painter.rect_filled(rect, CornerRadius::ZERO, palette.warn_soft);
                        painter.rect_stroke(rect.shrink(0.75), CornerRadius::ZERO, Stroke::new(1.5, palette.warn), StrokeKind::Inside);
                        icons::paint(ui, Rect::from_min_size(Pos2::new(rect.left() + 8.0, rect.center().y - 8.0), Vec2::splat(16.0)), "state-warn", palette.warn);
                    }
                    Some(CellState::Valid) => {}
                }
                painter.rect_stroke(rect, CornerRadius::ZERO, line, StrokeKind::Inside);
                let editing_here = editing.as_ref().is_some_and(|edit| edit.row_id == row.id && edit.column_id == column.id && edit.dataset_id == dataset.id);
                if editing_here {
                    let mut edit = editing.clone().expect("checked");
                    // The editor fills the cell inside its grid lines, as the cell's own rectangle.
                    let field = rect.shrink(1.0);
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(field));
                    let response = widgets::edit_line(&mut child, palette, &mut edit.draft, widgets::Field {
                        width: field.width(),
                        height: Some(field.height()),
                        font: Some(body.clone()),
                        margin: Some(egui::Margin::symmetric(7, 4)),
                        align: if numeric { egui::Align::Max } else { egui::Align::Min },
                        ..Default::default()
                    });
                    if !response.has_focus() && !response.lost_focus() {
                        response.request_focus();
                    }
                    if response.lost_focus() {
                        commit = Some(edit);
                    } else {
                        self.dataset_edit = Some(edit);
                    }
                    continue;
                }
                if let Some(cell) = cell {
                    let (align, x) = if numeric { (Align2::RIGHT_CENTER, rect.right() - 8.0) } else { (Align2::LEFT_CENTER, rect.left() + 8.0) };
                    painter.text(Pos2::new(x, rect.center().y), align, &cell.original, body.clone(), palette.ink);
                }
                if ui.interact(rect, ui.id().with(("sheet-cell", row_index, column_index)), Sense::click()).clicked() {
                    start_edit = Some(DatasetCellEdit {
                        dataset_id: dataset.id.clone(),
                        version_hash: dataset.version_hash.clone(),
                        row_id: row.id.clone(),
                        column_id: column.id.clone(),
                        original: cell.map(|cell| cell.original.clone()).unwrap_or_default(),
                        draft: cell.map(|cell| cell.original.clone()).unwrap_or_default(),
                    });
                }
            }
        }
        if let Some((command, args)) = remove {
            self.invoke_ui(command, args);
        }
        if let Some((column_id, descending)) = sort_action {
            match revision {
                Some(rev) => {
                    if let Err(err) = self.sort_dataset_column(&dataset.id, &dataset.version_hash, rev, &column_id, descending) {
                        self.set_error(err);
                    }
                }
                None => self.set_error("작업물이 다른 편집으로 바뀌었습니다".into()),
            }
        }
        if let Some(column_id) = scale_for_column {
            match revision {
                Some(rev) => {
                    store_unit_scale_draft(ui.ctx(), &dataset.id, Some(new_unit_scale_draft(dataset, rev, &column_id)));
                    egui::Popup::open_id(ui.ctx(), egui::Id::new(UNIT_SCALE_MENU_ID));
                }
                None => self.set_error("작업물이 다른 편집으로 바뀌었습니다".into()),
            }
        }
        if let Some(edit) = commit {
            self.dataset_edit = None;
            if edit.draft != edit.original {
                // Once sent, the value stays in its draft until the host answers (`apply_dataset_cell`). A command refused
                // before that keeps the value as a draft too, so it is not lost with the closed editor.
                let args = json!({ "dataset_id": &edit.dataset_id, "row_id": &edit.row_id, "column_id": &edit.column_id, "value": &edit.draft });
                if let Err(message) = self.invoke("data.edit_cell", args) {
                    self.keep_cell_as_copy(&edit);
                    self.set_error(message);
                }
            }
        } else if let Some(edit) = start_edit {
            self.dataset_edit = Some(edit);
        }
    }

    /// `data.edit_cell`: one cell of a figure's table, sent as `figure.dataset.edit` against the version on screen.
    pub(super) fn edit_data_cell(&mut self, args: &serde_json::Value) -> Result<(), String> {
        let text = |key: &str| args.get(key).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
        let figure = self.figure_document().ok_or("논문 그림이 열려 있지 않습니다")?;
        let dataset_id = Some(text("dataset_id"))
            .filter(|id| !id.is_empty())
            .or_else(|| self.sheet_dataset().map(|dataset| dataset.id))
            .ok_or("자료를 고르세요")?;
        let dataset = figure.datasets.iter().find(|dataset| dataset.id == dataset_id).ok_or("자료를 찾지 못했습니다")?;
        let (row_id, column_id) = (text("row_id"), text("column_id"));
        let original = dataset
            .rows
            .iter()
            .find(|row| row.id == row_id)
            .ok_or("행을 찾지 못했습니다")?
            .cells
            .get(&column_id)
            .map(|cell| cell.original.clone())
            .unwrap_or_default();
        if !dataset.columns.iter().any(|column| column.id == column_id) {
            return Err("열을 찾지 못했습니다".into());
        }
        let edit = DatasetCellEdit { dataset_id, version_hash: dataset.version_hash.clone(), row_id, column_id, original, draft: text("value") };
        if self.run_context.is_some() {
            // A channel edit must not claim, replace or retry a user's open cell draft.
            return self.send_dataset_edit(&edit.dataset_id, None, Some(&edit.version_hash), json!({
                "type": "edit_cell", "row_id": edit.row_id, "column_id": edit.column_id,
                "previous": edit.original, "replacement": edit.draft,
            }));
        }
        self.apply_dataset_cell(edit);
        Ok(())
    }

    /// `data.add_column`, `data.add_row`, `data.remove_row`, `data.remove_column`: one `figure.dataset.edit` on the
    /// sheet's dataset (or `dataset_id`). A column a plot still draws is refused here (the host would refuse the whole
    /// change with COLUMN_NOT_FOUND).
    pub(super) fn edit_dataset_structure(&mut self, command: &str, args: &serde_json::Value) -> Result<(), String> {
        let text = |key: &str| args.get(key).and_then(serde_json::Value::as_str).map(str::to_owned).filter(|value| !value.is_empty());
        let figure = self.figure_document().ok_or("논문 그림이 열려 있지 않습니다")?;
        let dataset_id = text("dataset_id").or_else(|| self.sheet_dataset().map(|dataset| dataset.id)).ok_or("자료를 고르세요")?;
        let dataset = figure.datasets.iter().find(|dataset| dataset.id == dataset_id).ok_or("자료를 찾지 못했습니다")?;
        let edit = match command {
            "data.add_column" => {
                let name = text("name").unwrap_or_else(|| crate::i18n::word_label("열", "Column", dataset.columns.len() + 1));
                if dataset.columns.iter().any(|column| column.name == name) {
                    return Err("같은 이름의 열이 있습니다".into());
                }
                json!({ "type": "add_column", "name": name, "data_type": text("data_type").unwrap_or_else(|| "numeric".into()), "unit": text("unit"), "fill": text("fill") })
            }
            "data.add_row" => {
                let count = args.get("count").and_then(serde_json::Value::as_u64).unwrap_or(1).clamp(1, 1000) as usize;
                // A category cannot be empty (CATEGORY_MISSING): a new row is named `행 n` in each category column,
                // with n past every name already taken. Other cells stay missing until typed in.
                let taken = |name: &str| dataset.rows.iter().any(|row| row.cells.values().any(|cell| cell.original == name));
                let mut next = dataset.rows.len() + 1;
                let rows: Vec<serde_json::Value> = (0..count)
                    .map(|_| {
                        while taken(&crate::i18n::word_label("행", "Row", next)) {
                            next += 1;
                        }
                        let name = crate::i18n::word_label("행", "Row", next);
                        next += 1;
                        let cells: serde_json::Map<String, serde_json::Value> = dataset
                            .columns
                            .iter()
                            .filter(|column| column.data_type == "category")
                            .map(|column| (column.id.clone(), json!(name)))
                            .collect();
                        serde_json::Value::Object(cells)
                    })
                    .collect();
                json!({ "type": "add_rows", "after_row_id": text("after_row_id"), "rows": rows })
            }
            "data.remove_row" => {
                let row_id = text("row_id").ok_or("지울 행을 고르세요")?;
                let row = dataset.rows.iter().find(|row| row.id == row_id).ok_or("행을 찾지 못했습니다")?;
                let previous: std::collections::BTreeMap<&String, &String> = row.cells.iter().map(|(column, cell)| (column, &cell.original)).collect();
                json!({ "type": "remove_row", "row_id": row_id, "previous": previous })
            }
            _ => {
                let column_id = text("column_id").ok_or("지울 열을 고르세요")?;
                let column = dataset.columns.iter().find(|column| column.id == column_id).ok_or("열을 찾지 못했습니다")?;
                if plots_column(&figure, &dataset.id, &column_id) {
                    return Err(format!("{}: 차트가 이 열을 그리고 있어 지울 수 없습니다", column.name));
                }
                json!({ "type": "remove_column", "column_id": column_id, "previous_name": column.name })
            }
        };
        self.send_dataset_edit(&dataset.id, None, None, edit)
    }

    fn sheet_has_pending_edit_or_read(&self) -> bool {
        self.pending.values().any(|request| {
            matches!(
                request.method.as_str(),
                "context.read"
                    | "artifact.inspect"
                    | "figure.dataset.edit"
                    | "changes.apply"
                    | "history.undo"
                    | "history.redo"
                    | "history.undo_change"
            )
        })
    }

    pub(super) fn sheet_transform_ready(&self) -> bool {
        self.shown_figure_revision().is_some()
            && self.dataset_edit.is_none()
            && !self.sheet_has_pending_edit_or_read()
    }

    fn check_dataset_current(
        &self,
        dataset_id: &str,
        expected_revision: Option<u64>,
        expected_version_hash: Option<&str>,
    ) -> Result<(u64, Dataset), String> {
        let figure = self.figure_document().ok_or("논문 그림이 열려 있지 않습니다")?;
        if self.active.is_none() || self.context.is_none() {
            return Err("작업물을 아직 읽는 중입니다".into());
        }
        let Some(revision) = self.shown_figure_revision() else {
            return Err("작업물이 다른 편집으로 바뀌었습니다".into());
        };
        if expected_revision.is_some_and(|expected| expected != revision) {
            return Err("작업물이 다른 편집으로 바뀌었습니다".into());
        }
        if self.dataset_edit.is_some() || self.sheet_has_pending_edit_or_read() {
            return Err("이전 편집이나 새로고침이 끝난 뒤 다시 시도하세요".into());
        }
        let dataset = figure
            .datasets
            .iter()
            .find(|dataset| dataset.id == dataset_id)
            .cloned()
            .ok_or("자료를 찾지 못했습니다")?;
        if expected_version_hash.is_some_and(|expected| expected != dataset.version_hash) {
            return Err("자료가 다른 편집으로 바뀌었습니다".into());
        }
        Ok((revision, dataset))
    }

    fn send_dataset_edit(
        &mut self,
        dataset_id: &str,
        expected_revision: Option<u64>,
        expected_version_hash: Option<&str>,
        edit: serde_json::Value,
    ) -> Result<(), String> {
        let (revision, dataset) = self.check_dataset_current(dataset_id, expected_revision, expected_version_hash)?;
        let (Some(active), Some(context)) = (&self.active, &self.context) else {
            return Err("작업물을 아직 읽는 중입니다".into());
        };
        let params = json!({
            "request_id": designllm_contracts::new_id(),
            "actor": "desktop",
            "workspace_id": context.workspace_id,
            "artifact_id": active.id,
            "base_revision": revision,
            "allowed_intents": ["data"],
            "job_ref": null,
            "dataset_id": dataset.id,
            "version_hash": dataset.version_hash,
            "edits": [edit],
        });
        self.call("figure.dataset.edit", params, super::PendingAction::DatasetEdit);
        Ok(())
    }

    /// Emits `figure.dataset.edit` with a `Sort` transform against the current figure revision and dataset version.
    pub(super) fn sort_dataset_column(
        &mut self,
        dataset_id: &str,
        expected_version_hash: &str,
        expected_revision: u64,
        column_id: &str,
        descending: bool,
    ) -> Result<(), String> {
        let (_, dataset) = self.check_dataset_current(dataset_id, Some(expected_revision), Some(expected_version_hash))?;
        if !dataset.columns.iter().any(|column| column.id == column_id) {
            return Err("열을 찾지 못했습니다".into());
        }
        let transform = DataTransform::Sort {
            column_id: column_id.to_owned(),
            descending,
        };
        let edit = serde_json::to_value(transform).map_err(|err| err.to_string())?;
        self.send_dataset_edit(dataset_id, Some(expected_revision), Some(expected_version_hash), edit)
    }

    /// Emits `figure.dataset.edit` with a `UnitScale` transform for a numeric column against the current figure
    /// revision and dataset version.
    pub(super) fn unit_scale_dataset_column(&mut self, draft: &UnitScaleDraft) -> Result<(), String> {
        let (_, dataset) = self.check_dataset_current(
            &draft.dataset_id,
            Some(draft.revision),
            Some(&draft.version_hash),
        )?;
        let column = dataset
            .columns
            .iter()
            .find(|column| column.id == draft.column_id)
            .ok_or("열을 찾지 못했습니다")?;
        if !is_numeric_column(column) {
            return Err("숫자 열만 단위를 변환할 수 있습니다".into());
        }
        let output_unit = draft.output_unit.trim();
        if output_unit.is_empty() {
            return Err("출력 단위를 입력하세요".into());
        }
        let derived = derived_column_id(&column.id, output_unit);
        if dataset.columns.iter().any(|existing| existing.id == derived) {
            return Err("같은 출력 단위의 열이 이미 있습니다".into());
        }
        let numerator = draft.numerator.trim();
        let denominator = draft.denominator.trim();
        if !is_valid_decimal_text(numerator) || !is_valid_decimal_text(denominator) {
            return Err("변환 비율의 분자와 분모는 숫자여야 합니다".into());
        }
        if is_zero_decimal_text(denominator) {
            return Err("분모는 0이 될 수 없습니다".into());
        }
        let transform = DataTransform::UnitScale {
            column_id: column.id.clone(),
            numerator: numerator.to_owned(),
            denominator: denominator.to_owned(),
            output_unit: output_unit.to_owned(),
        };
        let edit = serde_json::to_value(transform).map_err(|err| err.to_string())?;
        self.send_dataset_edit(&draft.dataset_id, Some(draft.revision), Some(&draft.version_hash), edit)
    }
}

fn is_numeric_column(column: &DataColumn) -> bool {
    matches!(column.data_type.as_str(), "numeric" | "count")
}

fn derived_column_id(column_id: &str, output_unit: &str) -> String {
    format!("{column_id}@{output_unit}")
}

/// The engine's decimal text: a sign, ASCII digits with at most one '.', and at most `DECIMAL_MAX_DIGITS` digits.
fn is_valid_decimal_text(text: &str) -> bool {
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    if body.is_empty() || body.bytes().filter(|&b| b == b'.').count() > 1 {
        return false;
    }
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    (!int_part.is_empty() || !frac_part.is_empty())
        && int_part.len() + frac_part.len() <= designllm_contracts::figure_rules::DECIMAL_MAX_DIGITS
        && int_part.bytes().all(|b| b.is_ascii_digit())
        && frac_part.bytes().all(|b| b.is_ascii_digit())
}

fn is_zero_decimal_text(text: &str) -> bool {
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    body.bytes().all(|b| b == b'0' || b == b'.')
}

fn column_unit_label(column: &DataColumn) -> String {
    match column.unit.as_deref().filter(|unit| !unit.is_empty()) {
        Some(unit) => format!("{} ({unit})", column.name),
        None => column.name.clone(),
    }
}

fn new_unit_scale_draft(dataset: &Dataset, revision: u64, column_id: &str) -> UnitScaleDraft {
    UnitScaleDraft {
        dataset_id: dataset.id.clone(),
        version_hash: dataset.version_hash.clone(),
        revision,
        column_id: column_id.to_owned(),
        numerator: "1".into(),
        denominator: "1".into(),
        output_unit: String::new(),
    }
}

/// Where the unit scale form of the dataset `dataset_id` keeps its draft between frames: one per dataset, so a draft never
/// shows up in the form of another dataset.
pub(super) fn unit_scale_draft_id(dataset_id: &str) -> egui::Id {
    egui::Id::new((UNIT_SCALE_DRAFT_ID, dataset_id))
}

fn store_unit_scale_draft(ctx: &egui::Context, dataset_id: &str, draft: Option<UnitScaleDraft>) {
    ctx.data_mut(|data| data.insert_temp(unit_scale_draft_id(dataset_id), draft));
}

/// Whether a plot of `figure` over `dataset_id` names `column_id` (x, y or an error column).
pub(super) fn plots_column(figure: &designllm_contracts::FigureDocument, dataset_id: &str, column_id: &str) -> bool {
    figure.panels.iter().any(|panel| match &panel.body {
        PanelBody::Plot(plot) if plot.dataset_ref.dataset_id == dataset_id => {
            plot.x_column == column_id
                || plot.y_columns.iter().any(|column| column == column_id)
                || plot.error.as_ref().is_some_and(|error| {
                    serde_json::to_value(error).is_ok_and(|value| value.to_string().contains(&format!("\"{column_id}\"")))
                })
        }
        _ => false,
    })
}

/// An empty cell: 135° hatching, `#FFFFFF` 5 then `line-strong` 1 (tokens `data-sheet.empty`).
fn paint_hatch(painter: &egui::Painter, rect: Rect, palette: &theme::Palette) {
    painter.rect_filled(rect, CornerRadius::ZERO, egui::Color32::WHITE);
    let clipped = painter.with_clip_rect(rect);
    let step = 6.0 * std::f32::consts::SQRT_2;
    let mut offset = -rect.height();
    while offset < rect.width() {
        let start = Pos2::new(rect.left() + offset, rect.bottom());
        let end = Pos2::new(start.x + rect.height(), rect.top());
        clipped.line_segment([start, end], Stroke::new(1.0, palette.line_strong));
        offset += step;
    }
}

#[cfg(test)]
#[path = "data_sheet_transform_tests.rs"]
mod data_sheet_transform_tests;
