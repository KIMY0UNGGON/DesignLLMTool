//! Commands of the kind boards (GUI_DESIGN §8, §9.2): figures (panels, diagram nodes and connectors,
//! plots and axes, caption, data and evidence), slides and canvas pages (remove, reorder, notes),
//! text documents (paragraph format, page setup, named styles, table columns, text ranges) and web
//! source. Each command builds one change set from its arguments, with the selection as the default
//! target, so that a control on screen and an agent on the UI channel reach the same operation.

use std::collections::BTreeSet;

use designllm_contracts::{
    new_id, ArtifactBody, ArtifactKind, CanvasAxisGuides, CanvasPageGuides, Feature, CanvasTable, CanvasTableRow, DltError, Edge, FlowBlock, FlowDocument, Intent, LayoutSpec, Node, NodeExt,
    NodeKind, Operation, PageRole, PageSetup, PanelBody, PathSpec, Rect, Style, StylePatch, Support, TextListKind, TextRange, EMU_PER_CSS_PX, EMU_PER_MM, EMU_PER_PT,
    PATH_MAX_COMMANDS, PATH_MAX_COORDINATE,
};
use designllm_contracts::figure_rules::USER_AUTHORED_ORIGIN;
use serde_json::Value;

use super::{figure_tools, DesktopApp, PendingAction, PendingPageReorderSelection};
use crate::{commands::CommandSpec, figure_layout::tiled_panel_bounds};

fn number(args: &Value, key: &str) -> Option<f64> {
    args.get(key).and_then(Value::as_f64)
}

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn flag(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

fn mm(value: f64) -> i64 {
    (value * EMU_PER_MM as f64).round() as i64
}

fn pt(value: f64) -> i64 {
    (value * EMU_PER_PT as f64).round() as i64
}

/// The operation `figure.put_evidence` sends with `args`: its `evidence`, or, without one, a statement the user writes as
/// evidence whose source is its own text. The agent channel's review gate judges exactly this (49w).
pub(super) fn evidence_operation(args: &Value) -> Result<Operation, String> {
    let mut value = args.get("evidence").cloned().unwrap_or(Value::Null);
    if value.is_null() {
        let statement = text(args, "text").unwrap_or_default().trim().to_owned();
        if statement.is_empty() {
            return Err("근거 내용을 적으세요".into());
        }
        value = serde_json::json!({
            "id": new_id(), "source_hash": designllm_contracts::sha256(statement.as_bytes()), "origin": "user_statement",
            "physical_page_index": null, "region": null, "pdf_geometry": null, "text": statement, "review_state": "user_provided",
        });
    }
    Ok(Operation::PutEvidence { evidence: serde_json::from_value(value).map_err(|error| format!("근거 형식이 맞지 않습니다: {error}"))? })
}

/// The intents a change set needs: the intent of each of its operations.
pub(super) fn intents_of(operations: &[Operation]) -> BTreeSet<Intent> {
    operations.iter().map(Operation::intent).collect()
}

/// The text-style fields a command may carry (`font_size` in pt).
fn text_patch(args: &Value) -> StylePatch {
    StylePatch {
        font_family: text(args, "font_family").map(str::to_owned),
        font_size: number(args, "font_size").map(pt),
        color: text(args, "color").map(str::to_owned),
        bold: flag(args, "bold"),
        italic: flag(args, "italic"),
        underline: flag(args, "underline"),
        strikethrough: flag(args, "strikethrough"),
        ..Default::default()
    }
}

/// Moves `id` to `to` in `ids` (clamped to the end).
fn reordered(mut ids: Vec<String>, id: &str, to: usize) -> Option<Vec<String>> {
    let from = ids.iter().position(|item| item == id)?;
    let moved = ids.remove(from);
    ids.insert(to.min(ids.len()), moved);
    Some(ids)
}

/// The paragraphs of a flow document in reading order, with the section that holds each one.
fn paragraphs(document: &FlowDocument) -> Vec<(&str, &designllm_contracts::Paragraph)> {
    let mut found = Vec::new();
    for section in &document.sections {
        for block in &section.blocks {
            if let FlowBlock::Paragraph(paragraph) = block {
                found.push((section.id.as_str(), paragraph));
            }
        }
    }
    found
}

const MAX_CANVAS_TABLE_CELLS: usize = 10_000;

/// `10000` as `10,000`, for the words of a refusal.
fn thousands(value: usize) -> String {
    let digits = value.to_string();
    digits.char_indices().fold(String::new(), |mut out, (index, digit)| {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
        out
    })
}

fn table_index(args: &Value, key: &str, default: usize, maximum: usize) -> Result<usize, String> {
    let Some(value) = args.get(key) else { return Ok(default) };
    let Some(value) = value.as_u64().and_then(|value| usize::try_from(value).ok()) else {
        return Err(format!("{key}는 0 이상의 정수여야 합니다"));
    };
    if value > maximum {
        return Err(format!("{key}가 허용 범위를 벗어났습니다"));
    }
    Ok(value)
}

fn requested_canvas_table_widths(args: &Value, columns: usize) -> Result<Option<Vec<u32>>, String> {
    let value = args.get("column_widths").ok_or("열 너비 비율을 지정하세요")?;
    if value.is_null() {
        return Ok(None);
    }
    let values = value.as_array().ok_or("열 너비 비율은 양의 정수 배열이어야 합니다")?;
    if values.len() != columns {
        return Err(format!("표의 열 {columns}개에 맞는 너비 비율을 입력하세요"));
    }
    values.iter().map(|value| {
        let width = value.as_u64().and_then(|value| u32::try_from(value).ok())
            .filter(|width| *width > 0)
            .ok_or_else(|| "열 너비 비율은 1 이상의 32비트 정수여야 합니다".to_owned())?;
        Ok(width)
    }).collect::<Result<Vec<_>, _>>().map(Some)
}

fn canvas_table_widths_after_add(widths: Option<&[u32]>, columns: usize, index: usize) -> Result<Option<Vec<u32>>, String> {
    let Some(widths) = widths else { return Ok(None) };
    if widths.len() != columns || widths.iter().any(|width| *width == 0) || columns == 0 || index > columns {
        return Err("표의 열 너비 비율을 읽을 수 없습니다".into());
    }
    let source = index.saturating_sub(1).min(columns - 1);
    let mut scaled = widths.to_vec();
    if scaled[source] == 1 {
        for width in &mut scaled {
            *width = width.checked_mul(2).ok_or("열 너비를 정확히 나눌 수 없습니다")?;
        }
    }
    let split = scaled[source];
    let smaller = split / 2;
    let larger = split - smaller;
    if smaller == 0 {
        return Err("열 너비를 두 개의 양의 비율로 나눌 수 없습니다".into());
    }
    let mut result = Vec::with_capacity(columns + 1);
    for (column, width) in scaled.into_iter().enumerate() {
        if column == source {
            result.push(smaller);
            result.push(larger);
        } else {
            result.push(width);
        }
    }
    Ok(Some(result))
}

fn canvas_table_widths_after_remove(widths: Option<&[u32]>, columns: usize, index: usize) -> Result<Option<Vec<u32>>, String> {
    let Some(widths) = widths else { return Ok(None) };
    if widths.len() != columns || widths.iter().any(|width| *width == 0) || columns <= 1 || index >= columns {
        return Err("표의 열 너비 비율을 읽을 수 없습니다".into());
    }
    let removed = widths[index];
    let mut result = widths.to_vec();
    result.remove(index);
    let merge_into = if index == 0 { 0 } else { index - 1 };
    result[merge_into] = result[merge_into].checked_add(removed).ok_or("열 너비 비율을 합칠 때 범위를 넘었습니다")?;
    Ok(Some(result))
}

fn canvas_table_cell(id: String, style: Style) -> Node {
    Node {
        id,
        kind: NodeKind::Rectangle,
        bounds: Rect { x: 0, y: 0, width: 120 * designllm_contracts::EMU_PER_CSS_PX, height: 36 * designllm_contracts::EMU_PER_CSS_PX },
        text: String::new(),
        style,
        asset: None,
        children: Vec::new(),
        layout: Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..LayoutSpec::default() }),
        interactions: Vec::new(),
        variants: Default::default(),
        ext: NodeExt::default(),
    }
}

fn canvas_table_cell_style(table_node: &Node, table: &CanvasTable, row: usize, column: usize) -> Result<Style, String> {
    let cell_id = table.rows.get(row).and_then(|row| row.cell_ids.get(column)).ok_or("표 셀을 찾지 못했습니다")?;
    table_node.children.iter().find(|cell| &cell.id == cell_id).map(|cell| cell.style.clone()).ok_or_else(|| "표 셀을 찾지 못했습니다".into())
}

impl DesktopApp {
    /// Runs the kind-board commands; `None` for an id this module does not serve.
    pub(super) fn dispatch_kind(&mut self, spec: &CommandSpec, args: &Value) -> Option<Result<(), String>> {
        let result = match spec.id {
            "add.figure_panel" => self.figure_add_diagram_panel(),
            "add.figure_image" => self.figure_image(args),
            "add.diagram_node" => self.figure_add_node(args),
            "add.connector" => self.figure_connect(args),
            "figure.arrange_panels" => self.figure_arrange(),
            "figure.remove_panel" => self.figure_remove_panel(args),
            "figure.reorder_panels" => self.figure_reorder_panels(args),
            "figure.caption" => self.kind_change(vec![Operation::SetCaption { caption: text(args, "caption").unwrap_or_default().to_owned() }]),
            "figure.panel_label" => self.figure_panel_label(args),
            "figure.plot" => self.figure_plot(args),
            "figure.axis_label" => self.figure_axis(args, true),
            "figure.axis_ticks" => self.figure_axis(args, false),
            "figure.put_dataset" => self.figure_put(args, "dataset"),
            "figure.put_evidence" => self.figure_put(args, "evidence"),
            "figure.update_link" => self.figure_update_link(args),
            "figure.open_source" => self.figure_open_source(args),
            "page.remove" => self.page_remove(args),
            "page.reorder" => self.page_reorder(args),
            "page.notes" => self.page_notes(args),
            "page.component" => self.page_component(args),
            "component.instantiate" => self.component_instantiate(args),
            "component.detach" => self.component_detach(args),
            "props.variant_style" => self.variant_style(args),
            "text.replace" => self.text_replace(args),
            "text.style_range" => self.text_style_range(args),
            "text.list" => self.canvas_text_list(args),
            "doc.paragraph_format" => self.paragraph_format(args),
            "doc.page_setup" => self.page_setup(args),
            "doc.named_style" => self.named_style(args),
            "doc.table_columns" => self.table_columns(args),
            "web.replace_source" => self.web_replace_source(args),
            "add.icon" => self.add_content_icon(args),
            "add.path" => self.add_path(args),
            "props.path" => self.props_path(args),
            "data.edit_cell" => self.edit_data_cell(args),
            "data.add_column" | "data.add_row" | "data.remove_row" | "data.remove_column" => self.edit_dataset_structure(spec.id, args),
            "add.table" => self.add_flow_table(args),
            "canvas.table.add" => self.add_canvas_table(args),
            "canvas.table.add_row" => self.canvas_table_add_row(args),
            "canvas.table.remove_row" => self.canvas_table_remove_row(args),
            "canvas.table.add_column" => self.canvas_table_add_column(args),
            "canvas.table.remove_column" => self.canvas_table_remove_column(args),
            "canvas.table.set_header" => self.canvas_table_set_header(args),
            "canvas.table.set_column_widths" => self.canvas_table_set_column_widths(args),
            "canvas.guides.set" => self.canvas_guides_set(args),
            "canvas.guides.clear" => self.canvas_guides_clear(args),
            "view.toggle_guides" => {
                let view = self.view();
                view.guides_visible = !view.guides_visible;
                Ok(())
            }
            "view.toggle_snap" => {
                let view = self.view();
                view.snap_guides = !view.snap_guides;
                Ok(())
            }
            "figure.style" => self.figure_style(args),
            "figure.toggle_sheet" => {
                let view = self.view();
                view.sheet_hidden = !view.sheet_hidden;
                Ok(())
            }
            _ => return None,
        };
        Some(result)
    }

    /// Sends `operations` as one change set with the intents they need. Shared with `web_mapping.rs` (task 44).
    pub(super) fn kind_change(&mut self, operations: Vec<Operation>) -> Result<(), String> {
        if operations.is_empty() {
            return Err("바꿀 것이 없습니다".into());
        }
        if self.context.is_none() {
            return Err("작업물을 아직 읽는 중입니다".into());
        }
        let intents = intents_of(&operations);
        self.apply_operations(operations, intents);
        Ok(())
    }

    /// The first selected id, if any.
    fn first_selected(&self) -> Option<String> {
        self.selected_nodes.first().cloned().or_else(|| self.selected_node.clone())
    }

    // Figures.

    fn figure_doc(&self) -> Result<designllm_contracts::FigureDocument, String> {
        self.figure_document().ok_or_else(|| "논문 그림이 열려 있지 않습니다".to_owned())
    }

    /// The panel named by `panel_id`, else the panel that owns the selection, else the only panel.
    fn target_panel(&self, args: &Value) -> Result<designllm_contracts::FigurePanel, String> {
        let figure = self.figure_doc()?;
        if let Some(id) = text(args, "panel_id") {
            return figure.panels.iter().find(|panel| panel.id == id).cloned().ok_or_else(|| "패널을 찾지 못했습니다".into());
        }
        if let Some(panel) = self.first_selected().and_then(|id| figure_tools::owning_panel(&figure, &id).cloned()) {
            return Ok(panel);
        }
        match figure.panels.as_slice() {
            [only] => Ok(only.clone()),
            _ => Err("패널을 고르세요".into()),
        }
    }

    fn figure_add_diagram_panel(&mut self) -> Result<(), String> {
        let figure = self.figure_doc()?;
        let operations = figure_tools::panel_additions(&figure, figure_tools::new_diagram_panel(&figure));
        self.kind_change(operations)
    }

    /// `add.figure_image`: the picture at `path` (else one picked in the file dialog) becomes a new image panel.
    fn figure_image(&mut self, args: &Value) -> Result<(), String> {
        let figure = self.figure_doc()?;
        let origin = text(args, "origin").unwrap_or("experimental_original");
        if !matches!(origin, "experimental_original" | "paper_extracted" | "generated_illustration") {
            return Err("그림의 출처를 알 수 없습니다".into());
        }
        // A picture taken from a paper names the evidence it comes from (the figure engine's IMAGE_SOURCE_REF_MISSING).
        let source_ref = text(args, "source_ref").filter(|id| !id.is_empty()).map(str::to_owned);
        if let Some(id) = &source_ref {
            if !figure.evidence.iter().any(|evidence| &evidence.id == id) {
                return Err("근거를 찾지 못했습니다".into());
            }
        } else if origin == "paper_extracted" {
            return Err("논문에서 가져온 그림은 근거를 골라야 합니다".into());
        }
        let path = match text(args, "path").filter(|path| !path.is_empty()) {
            Some(path) => std::path::PathBuf::from(path),
            None => match crate::dialogs::open_image(self.parent_hwnd) {
                Some(path) => path,
                None => return Ok(()),
            },
        };
        self.call("asset.import", serde_json::json!({ "path": path.to_string_lossy(), "require_image": true }), super::PendingAction::FigureImageAsset { origin: origin.to_owned(), source_ref });
        Ok(())
    }

    /// The reply of `asset.import` for an image panel: the panel goes in at the picture's pixel size, in one change.
    pub(super) fn add_figure_image_panel(&mut self, origin: &str, source_ref: Option<String>, value: &Value) {
        let size = |key: &str| value.get("image").and_then(|image| image.get(key)).and_then(Value::as_u64).filter(|pixels| *pixels > 0);
        let (Ok(asset), Some(width), Some(height)) =
            (serde_json::from_value::<designllm_contracts::AssetRef>(value.clone()), size("width_px"), size("height_px"))
        else {
            self.set_error("그림 파일을 읽지 못했습니다".into());
            return;
        };
        let Ok(figure) = self.figure_doc() else {
            return;
        };
        let panel = designllm_contracts::FigurePanel {
            id: new_id(),
            bounds: Default::default(),
            label: figure_tools::panel_label(figure.panels.len()),
            body: PanelBody::Image(designllm_contracts::FigureImage {
                source_ref,
                ..designllm_contracts::FigureImage::new(asset, origin, width as u32, height as u32)
            }),
            style: Default::default(),
        };
        let operations = figure_tools::panel_additions(&figure, panel);
        if let Err(error) = self.kind_change(operations) {
            self.set_error(error);
        }
    }

    fn figure_add_node(&mut self, args: &Value) -> Result<(), String> {
        let figure = self.figure_doc()?;
        let diagram_panel = match self.target_panel(args) {
            Ok(panel) if matches!(panel.body, PanelBody::Diagram(_)) => panel,
            _ => figure
                .panels
                .iter()
                .find(|panel| matches!(panel.body, PanelBody::Diagram(_)))
                .cloned()
                .ok_or("구조도 패널이 없습니다")?,
        };
        let kind = if text(args, "kind") == Some("oval") { NodeKind::Ellipse } else { NodeKind::Rectangle };
        let operation = figure_tools::node_addition(&figure, &diagram_panel.id, kind).ok_or("이 패널에는 더 넣을 자리가 없습니다")?;
        self.kind_change(vec![operation])
    }

    /// A connector between `from` and `to`, or between the two selected nodes of one diagram.
    fn figure_connect(&mut self, args: &Value) -> Result<(), String> {
        let figure = self.figure_doc()?;
        let (from, to) = match (text(args, "from"), text(args, "to")) {
            (Some(from), Some(to)) => (from.to_owned(), to.to_owned()),
            _ => match self.selected_nodes.as_slice() {
                [from, to] => (from.clone(), to.clone()),
                _ => return Err("이을 도형 두 개를 고르세요".into()),
            },
        };
        if from == to {
            return Err("서로 다른 도형을 고르세요".into());
        }
        let panel = figure_tools::owning_panel(&figure, &from).ok_or("도형을 찾지 못했습니다")?;
        if figure_tools::owning_panel(&figure, &to).map(|other| other.id.as_str()) != Some(panel.id.as_str()) {
            return Err("같은 구조도 안의 도형만 이을 수 있습니다".into());
        }
        let label = text(args, "label").unwrap_or_default().to_owned();
        let operation = Operation::ConnectDiagram {
            panel_id: panel.id.clone(),
            edge: Edge { id: new_id(), from, to, label, evidence_ids: Vec::new(), origin: USER_AUTHORED_ORIGIN.into(), style: Default::default() },
        };
        self.kind_change(vec![operation])
    }

    /// Tiles every panel again in its current order.
    fn figure_arrange(&mut self) -> Result<(), String> {
        let figure = self.figure_doc()?;
        let total = figure.panels.len();
        let bounds = figure
            .panels
            .iter()
            .enumerate()
            .map(|(index, panel)| (panel.id.clone(), tiled_panel_bounds(figure.size, index, total)))
            .collect();
        self.kind_change(vec![Operation::ArrangePanels { bounds }])
    }

    fn figure_remove_panel(&mut self, args: &Value) -> Result<(), String> {
        let panel = self.target_panel(args)?;
        self.select_clear();
        self.kind_change(vec![Operation::RemovePanel { panel_id: panel.id }])
    }

    fn figure_reorder_panels(&mut self, args: &Value) -> Result<(), String> {
        let figure = self.figure_doc()?;
        let panel = self.target_panel(args)?;
        let to = number(args, "index").unwrap_or(0.0).max(0.0) as usize;
        let ids = figure.panels.iter().map(|panel| panel.id.clone()).collect();
        let panel_ids = reordered(ids, &panel.id, to).ok_or("패널을 찾지 못했습니다")?;
        self.kind_change(vec![Operation::ReorderPanels { panel_ids }])
    }

    fn figure_panel_label(&mut self, args: &Value) -> Result<(), String> {
        let panel = self.target_panel(args)?;
        let label = text(args, "label").unwrap_or_default().to_owned();
        self.kind_change(vec![Operation::SetPanelLabel { panel_id: panel.id, label }])
    }

    /// The selected chart's plot with the given fields changed: kind, x column, y columns, axis ranges and scales.
    fn figure_plot(&mut self, args: &Value) -> Result<(), String> {
        let panel = self.target_panel(args)?;
        let PanelBody::Plot(mut plot) = panel.body else {
            return Err("차트 패널을 고르세요".into());
        };
        if let Some(kind) = text(args, "kind") {
            plot.kind = kind.to_owned();
        }
        if let Some(column) = text(args, "x_column") {
            plot.x_column = column.to_owned();
        }
        // Error bars (ErrorSpec): none, or sd / sem / ci over one column of half-widths, or range over a lower and an
        // upper column.
        if let Some(kind) = text(args, "error_kind") {
            plot.error = match kind {
                "none" => None,
                "range" => {
                    let (Some(lower), Some(upper)) = (text(args, "error_lower_column"), text(args, "error_upper_column")) else {
                        return Err("범위 오차에는 아래·위 열이 필요합니다".into());
                    };
                    Some(designllm_contracts::ErrorSpec {
                        kind: designllm_contracts::PlotErrorKind::Range,
                        values: designllm_contracts::ErrorValues::Bounds { lower_column: lower.to_owned(), upper_column: upper.to_owned() },
                        sample_count_column: text(args, "error_count_column").map(str::to_owned),
                        confidence_level: None,
                    })
                }
                other => {
                    let column = text(args, "error_column")
                        .map(str::to_owned)
                        .or_else(|| plot.error.as_ref().and_then(|error| match &error.values {
                            designllm_contracts::ErrorValues::Symmetric { column } => Some(column.clone()),
                            _ => None,
                        }))
                        .ok_or("오차 열을 고르세요")?;
                    let error_kind = match other {
                        "sem" => designllm_contracts::PlotErrorKind::Sem,
                        "ci" => designllm_contracts::PlotErrorKind::Ci,
                        _ => designllm_contracts::PlotErrorKind::Sd,
                    };
                    Some(designllm_contracts::ErrorSpec {
                        kind: error_kind,
                        values: designllm_contracts::ErrorValues::Symmetric { column },
                        sample_count_column: text(args, "error_count_column").map(str::to_owned),
                        confidence_level: (other == "ci").then(|| text(args, "confidence_level").unwrap_or("0.95").to_owned()),
                    })
                }
            };
        }
        if let Some(columns) = args.get("y_columns").and_then(Value::as_array) {
            plot.y_columns = columns.iter().filter_map(Value::as_str).map(str::to_owned).collect();
        }
        for (axis, prefix) in [(&mut plot.x_axis, "x"), (&mut plot.y_axis, "y")] {
            if let Some(value) = args.get(format!("{prefix}_minimum")) {
                axis.minimum = axis_bound(value)?;
            }
            if let Some(value) = args.get(format!("{prefix}_maximum")) {
                axis.maximum = axis_bound(value)?;
            }
            if let Some(scale) = args.get(format!("{prefix}_scale")).and_then(Value::as_str) {
                axis.scale = scale.to_owned();
            }
            if let Some(unit) = args.get(format!("{prefix}_unit")).and_then(Value::as_str) {
                axis.unit = (!unit.is_empty()).then(|| unit.to_owned());
            }
        }
        // An error reads columns off the axes, as the properties panel offers them (`error_value_columns`); the engine refuses
        // an axis column, so it is refused here before a round trip.
        if let Some(error) = &plot.error {
            let named: Vec<&str> = match &error.values {
                designllm_contracts::ErrorValues::Symmetric { column } => vec![column.as_str()],
                designllm_contracts::ErrorValues::Asymmetric { minus_column, plus_column } => vec![minus_column.as_str(), plus_column.as_str()],
                designllm_contracts::ErrorValues::Bounds { lower_column, upper_column } => vec![lower_column.as_str(), upper_column.as_str()],
            };
            if named.iter().any(|id| *id == plot.x_column || plot.y_columns.iter().any(|column| column == id)) {
                return Err(ERROR_ON_AXIS.into());
            }
        }
        self.kind_change(vec![Operation::SetPlot { panel_id: panel.id, plot }])
    }

    /// An axis title (`label`) or its tick step (`tick_step`, null for automatic ticks).
    fn figure_axis(&mut self, args: &Value, label: bool) -> Result<(), String> {
        let panel = self.target_panel(args)?;
        if !matches!(panel.body, PanelBody::Plot(_)) {
            return Err("차트 패널을 고르세요".into());
        }
        let axis = text(args, "axis").unwrap_or("x").to_owned();
        let operation = if label {
            Operation::SetAxisLabel { panel_id: panel.id, axis, label: text(args, "label").unwrap_or_default().to_owned() }
        } else {
            let tick_step = match args.get("tick_step") {
                Some(step @ Value::Number(_)) => axis_bound(step)?,
                Some(Value::String(step)) if !step.trim().is_empty() => Some(step.trim().to_owned()),
                _ => None,
            };
            Operation::SetAxisTicks { panel_id: panel.id, axis, tick_step }
        };
        self.kind_change(vec![operation])
    }

    /// A whole dataset or evidence item given as JSON (the window's own CSV and PDF readers build the same values).
    fn figure_put(&mut self, args: &Value, key: &str) -> Result<(), String> {
        self.figure_doc()?;
        let operation = if key == "dataset" {
            let value = args.get(key).cloned().unwrap_or(Value::Null);
            Operation::PutDataset { dataset: serde_json::from_value(value).map_err(|error| format!("자료 형식이 맞지 않습니다: {error}"))? }
        } else {
            evidence_operation(args)?
        };
        self.kind_change(vec![operation])
    }

    /// The picture named by `node_id` (else the selection) takes the figure's newest render.
    fn figure_update_link(&mut self, args: &Value) -> Result<(), String> {
        let node_id = text(args, "node_id").map(str::to_owned).or_else(|| self.first_selected()).ok_or("논문 그림을 고르세요")?;
        let artifact = self.artifact.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
        super::links::link_of(artifact, &node_id).ok_or("연결된 논문 그림이 아닙니다")?;
        self.begin_figure_update(&node_id);
        Ok(())
    }

    /// Opens the figure a linked picture shows.
    fn figure_open_source(&mut self, args: &Value) -> Result<(), String> {
        let node_id = text(args, "node_id").map(str::to_owned).or_else(|| self.first_selected()).ok_or("논문 그림을 고르세요")?;
        let artifact = self.artifact.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
        let link = super::links::link_of(artifact, &node_id).ok_or("연결된 논문 그림이 아닙니다")?.clone();
        let figure = super::links::linked_figure(&link, &self.artifacts).cloned().ok_or("원본 그림을 찾지 못했습니다")?;
        self.screen = super::Screen::Editor;
        self.activate(figure);
        Ok(())
    }

    // Pages (canvas and slides).

    fn page_ids(&self) -> Result<Vec<String>, String> {
        match self.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::Canvas(document)) => Ok(document.pages.iter().map(|page| page.id.clone()).collect()),
            Some(ArtifactBody::Slides(document)) => Ok(document.pages.iter().map(|page| page.id.clone()).collect()),
            _ => Err("이 작업물에는 페이지가 없습니다".into()),
        }
    }

    fn page_at(&self, args: &Value) -> Result<(usize, String), String> {
        let ids = self.page_ids()?;
        let index = number(args, "page_index").map_or(self.view_ref().page_index as usize, |value| value.max(0.0) as usize);
        ids.get(index).cloned().map(|id| (index, id)).ok_or_else(|| "페이지를 찾지 못했습니다".into())
    }

    fn page_remove(&mut self, args: &Value) -> Result<(), String> {
        let (index, page_id) = self.page_at(args)?;
        if self.page_ids()?.len() <= 1 {
            return Err("마지막 페이지는 지울 수 없습니다".into());
        }
        self.select_clear();
        let next = index.saturating_sub(1) as u32;
        self.remember_pending_page_reorder_selection(next);
        self.view().page_index = next;
        self.page_index = next;
        self.kind_change(vec![Operation::RemovePage { page_id }])
    }

    fn page_reorder(&mut self, args: &Value) -> Result<(), String> {
        let artifact_id = self.active.as_ref().ok_or("작업물을 열어 주세요")?.id.clone();
        if self.pending_page_reorder_selection.as_ref().is_some_and(|pending| pending.artifact_id == artifact_id) {
            return Err("페이지 순서 변경이 처리 중입니다".into());
        }
        self.pending_page_reorder_selection = None;

        let artifact_revision = self.artifact.as_ref().filter(|artifact| artifact.id == artifact_id)
            .map(|artifact| artifact.revision).ok_or("페이지 목록이 최신 상태가 아닙니다")?;
        let base_revision = self.context.as_ref().filter(|context| context.artifact_id == artifact_id)
            .map(|context| context.revision).ok_or("페이지 목록이 최신 상태가 아닙니다")?;
        let active_revision = self.active.as_ref().map(|active| active.revision).ok_or("작업물을 열어 주세요")?;
        if artifact_revision != base_revision || active_revision != base_revision {
            return Err("페이지 목록이 최신 상태가 아닙니다".into());
        }

        let (_, page_id) = self.page_at(args)?;
        let to = number(args, "to_index").unwrap_or(0.0).max(0.0) as usize;
        let base_page_ids = self.page_ids()?;
        let expected_page_ids = reordered(base_page_ids.clone(), &page_id, to).ok_or("페이지를 찾지 못했습니다")?;
        let selected_page_index = self.view_ref().page_index;
        let token = new_id();
        self.pending_page_reorder_selection = Some(PendingPageReorderSelection {
            token: token.clone(),
            artifact_id: artifact_id.clone(),
            base_revision,
            selected_page_id: base_page_ids.get(selected_page_index as usize).cloned(),
            last_page_index: selected_page_index,
            base_page_ids,
            expected_page_ids: expected_page_ids.clone(),
            committed_revision: None,
        });
        let operations = vec![Operation::ReorderPages { page_ids: expected_page_ids }];
        let intents = intents_of(&operations);
        if self.enqueue_operations_for(operations, intents, PendingAction::PageReorder { token: token.clone() }).is_some() {
            Ok(())
        } else {
            self.cancel_pending_page_reorder_selection(&token);
            Err("페이지 순서 변경 요청을 보내지 못했습니다".into())
        }
    }

    fn page_notes(&mut self, args: &Value) -> Result<(), String> {
        let (_, page_id) = self.page_at(args)?;
        let notes = text(args, "notes").unwrap_or_default().to_owned();
        self.kind_change(vec![Operation::SetPageNotes { page_id, notes }])
    }

    /// The stored guide setting of a canvas page: the value the engine compares with `expected`.
    fn canvas_page_guides(&self, page_id: &str) -> Result<Option<CanvasPageGuides>, String> {
        let Some(ArtifactBody::Canvas(document)) = self.artifact.as_ref().map(|artifact| &artifact.body) else {
            return Err("캔버스 페이지에서만 안내선을 바꿀 수 있습니다".into());
        };
        Ok(document.layout_guides.get(page_id).cloned())
    }

    /// Sets one axis of a page's guides and keeps the other one. `expected` is the setting the window shows, so the
    /// engine refuses the change when another edit has moved it.
    fn canvas_guides_set(&mut self, args: &Value) -> Result<(), String> {
        self.ensure_interaction_revision_matches_context()?;
        let (_, page_id) = self.page_at(args)?;
        let spec = CanvasAxisGuides {
            count: args.get("count").and_then(Value::as_u64).and_then(|count| u32::try_from(count).ok()).ok_or("안내선 개수를 입력하세요")?,
            spacing_emu: args.get("spacing_emu").and_then(Value::as_i64).ok_or("간격을 입력하세요")?,
            leading_margin_emu: args.get("leading_margin_emu").and_then(Value::as_i64).ok_or("앞 여백을 입력하세요")?,
            trailing_margin_emu: args.get("trailing_margin_emu").and_then(Value::as_i64).ok_or("뒤 여백을 입력하세요")?,
        };
        let expected = self.canvas_page_guides(&page_id)?;
        let mut guides = expected.clone().unwrap_or_default();
        match text(args, "axis").ok_or("안내선 축을 고르세요")? {
            "columns" => guides.columns = Some(spec),
            "rows" => guides.rows = Some(spec),
            _ => return Err("안내선 축은 columns 또는 rows여야 합니다".into()),
        }
        self.change(vec![Operation::SetCanvasPageGuides { page_id, expected, guides: Some(guides) }], &[Intent::Design])
    }

    /// Clears a page's guides, or only one axis of them when `axis` is given. A clear that leaves the page as it is (no
    /// guides stored, or the axis is not set) sends no change set.
    fn canvas_guides_clear(&mut self, args: &Value) -> Result<(), String> {
        self.ensure_interaction_revision_matches_context()?;
        let (_, page_id) = self.page_at(args)?;
        let expected = self.canvas_page_guides(&page_id)?;
        let guides = match (text(args, "axis"), expected.clone()) {
            (Some(axis), Some(mut guides)) => {
                match axis {
                    "columns" => guides.columns = None,
                    "rows" => guides.rows = None,
                    _ => return Err("안내선 축은 columns 또는 rows여야 합니다".into()),
                }
                (guides.columns.is_some() || guides.rows.is_some()).then_some(guides)
            }
            _ => None,
        };
        if guides == expected {
            return Ok(());
        }
        self.change(vec![Operation::SetCanvasPageGuides { page_id, expected, guides }], &[Intent::Design])
    }

    fn page_component(&mut self, args: &Value) -> Result<(), String> {
        if !self.canvas_token_snapshot_current() { return Err("페이지 정보가 최신이 아닙니다. 다시 읽은 뒤 시도하세요".into()); }
        let (index, page_id) = self.page_at(args)?;
        let enabled = flag(args, "enabled").ok_or("enabled를 지정하세요")?;
        let Some(ArtifactBody::Canvas(document)) = self.artifact.as_ref().map(|artifact| &artifact.body) else {
            return Err("컴포넌트 소스 페이지는 Canvas에서만 사용할 수 있습니다".into());
        };
        let page = document.pages.get(index).ok_or("페이지를 찾지 못했습니다")?;
        let role = if enabled { Some(PageRole::Component) } else { None };
        if page.role == role { return Ok(()) }
        self.kind_change(vec![Operation::SetPageRole { page_id, role }])
    }

    fn component_instantiate(&mut self, args: &Value) -> Result<(), String> {
        if !self.canvas_token_snapshot_current() { return Err("페이지 정보가 최신이 아닙니다. 다시 읽은 뒤 시도하세요".into()); }
        let component_page_id = text(args, "component_page_id").filter(|id| !id.is_empty()).ok_or("컴포넌트 페이지를 고르세요")?.to_owned();
        let page_index = number(args, "page_index").map(|value| value.max(0.0) as usize).unwrap_or(self.view_ref().page_index as usize);
        let parent_id = text(args, "parent_id").map(str::to_owned);
        let (page_id, target_size, source_size, target_is_component) = match self.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::Canvas(document)) => {
                let target = document.pages.get(page_index).ok_or("대상 페이지를 찾지 못했습니다")?;
                let source = document.pages.iter().find(|page| page.id == component_page_id).ok_or("컴포넌트 페이지를 찾지 못했습니다")?;
                if source.role != Some(PageRole::Component) { return Err("컴포넌트 원본으로 표시된 페이지를 고르세요".into()); }
                (target.id.clone(), target.size, source.size, target.role == Some(PageRole::Component))
            }
            _ => return Err("컴포넌트는 Canvas에서만 사용할 수 있습니다".into()),
        };
        if target_is_component { return Err("일반 페이지를 고르고 표시된 컴포넌트 페이지를 원본으로 사용하세요".into()); }
        let bounds = match args.get("bounds") {
            Some(value) => serde_json::from_value::<Rect>(value.clone()).map_err(|error| format!("bounds 형식이 잘못되었습니다: {error}"))?,
            None => Rect {
                x: if parent_id.is_some() { 0 } else { (target_size.width - source_size.width) / 2 },
                y: if parent_id.is_some() { 0 } else { (target_size.height - source_size.height) / 2 },
                width: source_size.width,
                height: source_size.height,
            },
        };
        let instance_id = text(args, "instance_id").filter(|id| !id.is_empty()).ok_or("instance_id를 지정하세요")?.to_owned();
        self.kind_change(vec![Operation::InstantiateComponent { page_id, parent_id, instance_id, component_page_id, bounds }])
    }

    fn component_detach(&mut self, args: &Value) -> Result<(), String> {
        if !self.canvas_token_snapshot_current() { return Err("선택 정보가 최신이 아닙니다. 다시 읽은 뒤 시도하세요".into()); }
        let instance_id = text(args, "node_id").map(str::to_owned).or_else(|| self.first_selected()).ok_or("연결을 해제할 컴포넌트를 고르세요")?;
        let artifact = self.artifact.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
        if super::doc_tree::find_node(artifact, &instance_id).and_then(|node| node.ext.instance.as_ref()).is_none() {
            return Err("선택한 노드는 컴포넌트 인스턴스가 아닙니다".into());
        }
        self.kind_change(vec![Operation::DetachInstance { instance_id }])
    }

    /// A style for one state of the selected node (`hover`, `pressed`, `dark`, …).
    fn variant_style(&mut self, args: &Value) -> Result<(), String> {
        let node_id = text(args, "node_id").map(str::to_owned).or_else(|| self.first_selected()).ok_or("대상을 고르세요")?;
        let variant = text(args, "variant").unwrap_or_default().trim().to_owned();
        if variant.is_empty() {
            return Err("상태 이름을 적으세요".into());
        }
        let patch = StylePatch {
            fill: text(args, "fill").map(str::to_owned),
            stroke: text(args, "stroke").map(str::to_owned),
            opacity_milli: number(args, "opacity").map(|percent| (percent.clamp(0.0, 100.0) * 10.0).round() as u32),
            ..text_patch(args)
        };
        self.kind_change(vec![Operation::SetVariantStyle { node_id, variant, patch }])
    }

    // Text documents.

    fn flow(&self) -> Result<&FlowDocument, String> {
        match self.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::Flow(document)) => Ok(document),
            _ => Err("문서가 열려 있지 않습니다".into()),
        }
    }

    /// The text range of the arguments (`paragraph_id`, `start`, `end`), else the selected one.
    fn text_range(&self, args: &Value) -> Result<TextRange, String> {
        if let Some(paragraph_id) = text(args, "paragraph_id") {
            let start = number(args, "start").unwrap_or(0.0).max(0.0) as u32;
            let end = number(args, "end").map_or(start, |value| value.max(0.0) as u32);
            return Ok(TextRange { paragraph_id: paragraph_id.to_owned(), start: start.min(end), end: start.max(end) });
        }
        self.context
            .as_ref()
            .and_then(|context| context.selection.text_range.clone())
            .ok_or_else(|| "글 범위를 고르세요".into())
    }

    fn text_replace(&mut self, args: &Value) -> Result<(), String> {
        let range = self.text_range(args)?;
        let value = text(args, "text").unwrap_or_default().to_owned();
        self.kind_change(vec![Operation::ReplaceText { range, text: value }])
    }

    fn text_style_range(&mut self, args: &Value) -> Result<(), String> {
        let range = self.text_range(args)?;
        if range.start == range.end {
            return Err("글자를 먼저 고르세요".into());
        }
        let patch = text_patch(args);
        if patch == StylePatch::default() {
            return Err("바꿀 모양을 고르세요".into());
        }
        self.kind_change(vec![Operation::SetTextStyleRange { range, patch }])
    }

    fn canvas_text_list(&mut self, args: &Value) -> Result<(), String> {
        if self.active.as_ref().is_none_or(|active| active.kind != ArtifactKind::Canvas) {
            return Err("목록 서식은 Canvas 텍스트에서만 지원됩니다".into());
        }
        let supported = self.canvas_list_support().is_some_and(|(read, edit)| {
            matches!(read, Support::Supported | Support::Approximate | Support::PreserveOnly)
                && matches!(edit, Support::Supported | Support::Approximate)
        });
        if !supported {
            return Err("이 작업물에서는 목록 서식을 지원하지 않습니다".into());
        }
        let list = match text(args, "list") {
            Some("bullet") => Some(TextListKind::Bullet),
            Some("numbered") => Some(TextListKind::Numbered),
            Some("none") => None,
            _ => return Err("글머리표, 번호 또는 없음을 고르세요".into()),
        };
        let mut node_ids = self.selected_nodes.clone();
        if let Some(primary) = &self.selected_node {
            if !node_ids.contains(primary) {
                node_ids.insert(0, primary.clone());
            }
        }
        if node_ids.is_empty() {
            return Err("목록을 적용할 텍스트를 고르세요".into());
        }
        let artifact = self.artifact.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
        if node_ids.iter().any(|id| super::doc_tree::find_node(artifact, id).is_none_or(|node| node.kind != NodeKind::Text)) {
            return Err("텍스트만 골라 주세요".into());
        }
        let operations = node_ids.into_iter().map(|node_id| Operation::SetNodeList { node_id, list }).collect();
        self.kind_change(operations)
    }

    /// The paragraph of the arguments, else the selected one, else the first.
    fn target_paragraph(&self, args: &Value) -> Result<designllm_contracts::Paragraph, String> {
        let document = self.flow()?;
        let wanted = text(args, "paragraph_id")
            .map(str::to_owned)
            .or_else(|| self.context.as_ref().and_then(|context| context.selection.text_range.as_ref().map(|range| range.paragraph_id.clone())))
            .or_else(|| self.first_selected());
        let all = paragraphs(document);
        let found = match wanted {
            Some(id) => all.iter().find(|(_, paragraph)| paragraph.id == id),
            None => all.first(),
        };
        found.map(|(_, paragraph)| (*paragraph).clone()).ok_or_else(|| "문단을 고르세요".into())
    }

    /// The selected paragraph's format with the given fields changed (indent in mm, spacing in pt).
    fn paragraph_format(&mut self, args: &Value) -> Result<(), String> {
        let paragraph = self.target_paragraph(args)?;
        let mut format = paragraph.format.clone();
        if let Some(align) = text(args, "align") {
            format.align = align.to_owned();
        }
        if let Some(value) = number(args, "indent") {
            format.indent = mm(value);
        }
        if let Some(value) = number(args, "space_before") {
            format.space_before = pt(value.max(0.0));
        }
        if let Some(value) = number(args, "space_after") {
            format.space_after = pt(value.max(0.0));
        }
        if let Some(value) = number(args, "line_spacing") {
            // A pitch set here is exact (or single at 0), so a proportional spacing gives way (task 25, decision 3).
            format.line_spacing = pt(value.max(0.0));
            format.line_spacing_percent = None;
        }
        self.kind_change(vec![Operation::SetParagraphFormat { paragraph_id: paragraph.id, format }])
    }

    /// The section of the selected paragraph (else the first) with its paper, orientation or margins changed (mm).
    fn page_setup(&mut self, args: &Value) -> Result<(), String> {
        let document = self.flow()?;
        let selected = self.target_paragraph(args).ok().map(|paragraph| paragraph.id);
        let section = text(args, "section_id")
            .and_then(|id| document.sections.iter().find(|section| section.id == id))
            .or_else(|| {
                selected.as_deref().and_then(|id| {
                    document.sections.iter().find(|section| {
                        section.blocks.iter().any(|block| matches!(block, FlowBlock::Paragraph(paragraph) if paragraph.id == id))
                    })
                })
            })
            .or_else(|| document.sections.first())
            .ok_or("쪽 설정을 찾지 못했습니다")?;
        let mut setup: PageSetup = section.setup.clone();
        let section_id = section.id.clone();
        match text(args, "paper") {
            Some("a4") => setup.size = designllm_contracts::Size::a4(),
            Some("letter") => setup.size = designllm_contracts::Size { width: mm(215.9), height: mm(279.4) },
            _ => {}
        }
        if let (Some(width), Some(height)) = (number(args, "width"), number(args, "height")) {
            setup.size = designllm_contracts::Size { width: mm(width), height: mm(height) };
        }
        if let Some(orientation) = text(args, "orientation") {
            let landscape = setup.size.width > setup.size.height;
            if (orientation == "landscape") != landscape {
                setup.size = designllm_contracts::Size { width: setup.size.height, height: setup.size.width };
            }
        }
        for (key, field) in [
            ("margin_left", &mut setup.margin_left),
            ("margin_top", &mut setup.margin_top),
            ("margin_right", &mut setup.margin_right),
            ("margin_bottom", &mut setup.margin_bottom),
        ] {
            if let Some(value) = number(args, key) {
                *field = mm(value.max(0.0));
            }
        }
        self.kind_change(vec![Operation::SetPageSetup { section_id, setup }])
    }

    fn named_style(&mut self, args: &Value) -> Result<(), String> {
        self.flow()?;
        let name = text(args, "name").unwrap_or_default().trim().to_owned();
        if name.is_empty() {
            return Err("스타일 이름을 적으세요".into());
        }
        let patch = text_patch(args);
        let clear_overrides = flag(args, "clear_overrides").unwrap_or(false);
        self.kind_change(vec![Operation::SetNamedStyle { name, patch, clear_overrides }])
    }

    /// The column widths (mm) of the table named by `table_id`, else of the selected table, else the first.
    fn table_columns(&mut self, args: &Value) -> Result<(), String> {
        let document = self.flow()?;
        let tables: Vec<&designllm_contracts::Table> = document
            .sections
            .iter()
            .flat_map(|section| &section.blocks)
            .filter_map(|block| match block {
                FlowBlock::Table(table) => Some(table),
                _ => None,
            })
            .collect();
        let wanted = text(args, "table_id").map(str::to_owned).or_else(|| self.first_selected());
        let table = wanted
            .as_deref()
            .and_then(|id| tables.iter().find(|table| table.id == id))
            .or_else(|| tables.first())
            .ok_or("표가 없습니다")?;
        let column_widths: Vec<i64> = args
            .get("column_widths")
            .and_then(Value::as_array)
            .map(|widths| widths.iter().filter_map(Value::as_f64).map(mm).collect())
            .unwrap_or_else(|| table.column_widths.clone());
        if column_widths.len() != table.column_widths.len() || column_widths.iter().any(|width| *width <= 0) {
            return Err(format!("열 너비 {}개를 0보다 크게 적으세요", table.column_widths.len()));
        }
        let table_id = table.id.clone();
        self.kind_change(vec![Operation::SetTableDimensions { table_id, column_widths }])
    }

    /// The style of a figure element (panel, diagram box or connector) in figure units: stroke width and font size in pt.
    /// Only fields the figure engine draws on every element are offered (`figures::guard::field_policy`: fill, stroke,
    /// stroke width, colour, font size, bold, italic, opacity, dash); the rest it refuses for some elements.
    fn figure_style(&mut self, args: &Value) -> Result<(), String> {
        self.figure_doc()?;
        let target = text(args, "target_id").map(str::to_owned).or_else(|| self.first_selected()).ok_or("바꿀 요소를 고르세요")?;
        let patch = StylePatch {
            fill: text(args, "fill").map(str::to_owned),
            stroke: text(args, "stroke").map(str::to_owned),
            stroke_width: number(args, "stroke_width").map(|value| pt(value.max(0.0))),
            opacity_milli: number(args, "opacity").map(|percent| (percent.clamp(0.0, 100.0) * 10.0).round() as u32),
            stroke_dash: text(args, "dash").map(|value| match value {
                "dashed" => designllm_contracts::StrokeDash::Dashed,
                "dotted" => designllm_contracts::StrokeDash::Dotted,
                _ => designllm_contracts::StrokeDash::Solid,
            }),
            ..text_patch(args)
        };
        if patch == StylePatch::default() {
            return Err("바꿀 모양을 고르세요".into());
        }
        self.kind_change(vec![Operation::SetStyle { node_ids: vec![target], patch }])
    }

    /// A table in a text document after the selected block: `rows` × `columns` (default 2 × 2), the text width shared
    /// equally, one empty paragraph per cell. The paragraph has no run, so the format adapter gives it the file's base
    /// character shape and the pitch derived from it (task 25), not the 12 pt of a default-style run.
    fn add_flow_table(&mut self, args: &Value) -> Result<(), String> {
        let document = self.flow()?;
        let anchor = self.selection_anchor();
        let Some(super::links::Placement::Flow { section_id, index, content_width }) = super::links::flow_placement(document, anchor.as_deref()) else {
            return Err("표를 넣을 위치를 찾지 못했습니다".into());
        };
        let rows = number(args, "rows").unwrap_or(2.0).clamp(1.0, 50.0) as usize;
        let columns = number(args, "columns").unwrap_or(2.0).clamp(1.0, 20.0) as usize;
        let width = content_width / columns as i64;
        let cell = || designllm_contracts::TableCell {
            id: new_id(),
            blocks: vec![FlowBlock::Paragraph(designllm_contracts::Paragraph {
                id: new_id(),
                runs: Vec::new(),
                style_ref: None,
                format: Default::default(),
            })],
            column_span: 1,
            row_span: 1,
        };
        let table = designllm_contracts::Table {
            id: new_id(),
            column_widths: vec![width; columns],
            rows: (0..rows).map(|_| (0..columns).map(|_| cell()).collect()).collect(),
            style: Default::default(),
        };
        self.kind_change(vec![Operation::InsertFlowBlock { section_id, index, block: FlowBlock::Table(table) }])
    }

    pub(super) fn canvas_table_context_current(&self) -> bool {
        let (Some(active), Some(artifact), Some(context)) = (&self.active, &self.artifact, &self.context) else { return false };
        let ArtifactBody::Canvas(document) = &artifact.body else { return false };
        active.kind == ArtifactKind::Canvas
            && artifact.kind == ArtifactKind::Canvas
            && active.id == artifact.id
            && artifact.id == context.artifact_id
            && active.revision == artifact.revision
            && artifact.revision == context.revision
            && context.visible_page == self.view_ref().page_index
            && document.pages.get(context.visible_page as usize).is_some()
    }

    fn ensure_canvas_table_context_current(&self) -> Result<(), String> {
        if self.canvas_table_context_current() {
            Ok(())
        } else {
            Err("작업물 상태가 바뀌었습니다. 다시 불러온 뒤 시도하세요".into())
        }
    }

    fn ensure_canvas_table_write_supported(&self) -> Result<(), String> {
        self.ensure_canvas_table_context_current()?;
        if !self.feature_write_supported(Feature::Tables) {
            return Err("현재 작업물은 표를 추가할 수 없습니다".into());
        }
        Ok(())
    }

    fn ensure_canvas_table_edit_supported(&self) -> Result<(), String> {
        self.ensure_canvas_table_context_current()?;
        if !self.feature_edit_supported(Feature::Tables) {
            return Err("현재 작업물은 표 구조를 편집할 수 없습니다".into());
        }
        Ok(())
    }

    fn add_canvas_table(&mut self, args: &Value) -> Result<(), String> {
        self.ensure_canvas_table_write_supported()?;
        let rows = table_index(args, "rows", 2, MAX_CANVAS_TABLE_CELLS)?;
        let columns = table_index(args, "columns", 2, designllm_contracts::MAX_GRID_COLUMNS as usize)?;
        if rows == 0 || columns == 0 || rows.checked_mul(columns).is_none_or(|count| count > MAX_CANVAS_TABLE_CELLS) {
            return Err(format!("표는 1–{}개의 셀과 1–{}개의 열을 지원합니다", thousands(MAX_CANVAS_TABLE_CELLS), designllm_contracts::MAX_GRID_COLUMNS));
        }
        let (page_id, page_size) = {
            let Some(ArtifactBody::Canvas(document)) = self.artifact.as_ref().map(|artifact| &artifact.body) else {
                return Err("캔버스 작업물이 열려 있지 않습니다".into());
            };
            let page = document.pages.get(self.view_ref().page_index as usize).ok_or("표를 넣을 페이지를 고르세요")?;
            (page.id.clone(), page.size)
        };
        let row_height = 36 * designllm_contracts::EMU_PER_CSS_PX;
        let width = page_size.width.saturating_mul(2) / 3;
        let bounds = Rect {
            x: page_size.width.saturating_sub(width) / 2,
            y: 40 * designllm_contracts::EMU_PER_CSS_PX,
            width,
            height: row_height.saturating_mul(rows as i64),
        };
        let (expected_selection, expected_page, expected_selection_generation) = self.selection_commit_guard()?;
        let table_id = new_id();
        let mut table_rows = Vec::with_capacity(rows);
        let mut children = Vec::with_capacity(rows * columns);
        for row_index in 0..rows {
            let row_id = new_id();
            let mut cell_ids = Vec::with_capacity(columns);
            for _ in 0..columns {
                let id = new_id();
                let header = row_index == 0;
                let style = Style {
                    font_size: 14 * EMU_PER_PT,
                    fill: if header { "#F3F4F6" } else { "#FFFFFF" }.into(),
                    stroke: "#CBD5E1".into(),
                    stroke_width: designllm_contracts::EMU_PER_CSS_PX,
                    bold: header,
                    ..Style::default()
                };
                children.push(canvas_table_cell(id.clone(), style));
                cell_ids.push(id);
            }
            table_rows.push(CanvasTableRow { id: row_id, cell_ids });
        }
        let header_row_id = table_rows.first().map(|row| row.id.clone());
        let node = Node {
            id: table_id.clone(), kind: NodeKind::Group, bounds, text: String::new(), style: Style::default(), asset: None,
            children,
            layout: Some(LayoutSpec { mode: "grid".into(), height_mode: "hug".into(), grid_columns: Some(columns as u32), gap: 0, row_gap: Some(0), ..LayoutSpec::default() }),
            interactions: vec![], variants: Default::default(),
            ext: NodeExt { name: Some("표".into()), table: Some(CanvasTable { rows: table_rows, header_row_id, column_widths: None, cell_spans: Default::default() }), ..NodeExt::default() },
        };
        let operations = vec![Operation::AddNode { page_id, parent_id: None, node }];
        let allowed_intents = intents_of(&operations);
        self.enqueue_selecting_commit(
            operations,
            allowed_intents,
            PendingAction::ApplyWithSelectionIfUnchanged { node_ids: vec![table_id], expected_selection, expected_page, expected_selection_generation },
        );
        Ok(())
    }

    fn canvas_table_target<'a>(&'a self, args: &Value) -> Result<(String, String, &'a Node, CanvasTable), String> {
        self.ensure_canvas_table_edit_supported()?;
        let table_id = text(args, "table_id").filter(|id| !id.trim().is_empty()).ok_or("표를 지정하세요")?;
        let Some(artifact) = self.artifact.as_ref().filter(|artifact| artifact.kind == ArtifactKind::Canvas) else {
            return Err("Canvas 표는 Canvas 작업물에서만 편집할 수 있습니다".into());
        };
        let ArtifactBody::Canvas(document) = &artifact.body else { return Err("Canvas 표를 찾지 못했습니다".into()) };
        let page_index = self.context.as_ref().map(|context| context.visible_page as usize).ok_or("작업물 상태를 읽는 중입니다")?;
        let page = document.pages.get(page_index).ok_or("현재 페이지를 다시 읽어 주세요")?;
        let node = self
            .selected_canvas_table_node()
            .filter(|node| node.id == table_id)
            .ok_or("현재 선택된 표를 편집할 수 없습니다")?;
        if node.kind != NodeKind::Group {
            return Err("표는 그룹으로 선택해야 합니다".into());
        }
        let table = node.ext.table.as_ref().ok_or("선택한 그룹은 표가 아닙니다")?.clone();
        if table.rows.is_empty() || table.rows.iter().any(|row| row.cell_ids.is_empty()) {
            return Err("표 구조를 읽을 수 없습니다".into());
        }
        Ok((table_id.to_owned(), page.id.clone(), node, table))
    }

    fn apply_canvas_table_structure(
        &mut self,
        table_id: String,
        expected: CanvasTable,
        table: CanvasTable,
        mut operations: Vec<Operation>,
    ) -> Result<(), String> {
        operations.push(Operation::SetCanvasTableStructure { table_id: table_id.clone(), expected, table });
        let allowed_intents = intents_of(&operations);
        self.apply_operations_for(operations, allowed_intents, PendingAction::Apply);
        Ok(())
    }

    fn canvas_table_add_row(&mut self, args: &Value) -> Result<(), String> {
        let (table_id, page_id, node, expected) = self.canvas_table_target(args)?;
        let index = table_index(args, "index", expected.rows.len(), expected.rows.len())?;
        if expected.rows.len().saturating_add(1).saturating_mul(expected.rows[0].cell_ids.len()) > MAX_CANVAS_TABLE_CELLS {
            return Err("표가 지원할 수 있는 셀 수를 넘었습니다".into());
        }
        if index == 0 && expected.header_row_id.is_some() {
            return Err("머리글 행 앞에는 행을 넣을 수 없습니다".into());
        }
        let source_row = if expected.header_row_id.is_some() && index <= 1 {
            1.min(expected.rows.len() - 1)
        } else {
            index.saturating_sub(1).min(expected.rows.len() - 1)
        };
        let row_id = new_id();
        let cell_ids: Vec<String> = (0..expected.rows[0].cell_ids.len()).map(|_| new_id()).collect();
        let mut next = expected.clone();
        next.rows.insert(index, CanvasTableRow { id: row_id, cell_ids: cell_ids.clone() });
        let mut operations = Vec::with_capacity(cell_ids.len());
        for (column, cell_id) in cell_ids.into_iter().enumerate() {
            let style = canvas_table_cell_style(node, &expected, source_row, column)?;
            operations.push(Operation::AddNode { page_id: page_id.clone(), parent_id: Some(table_id.clone()), node: canvas_table_cell(cell_id, style) });
        }
        self.apply_canvas_table_structure(table_id, expected, next, operations)
    }

    fn canvas_table_remove_row(&mut self, args: &Value) -> Result<(), String> {
        let (table_id, _page_id, _node, expected) = self.canvas_table_target(args)?;
        if expected.rows.len() <= 1 {
            return Err("표에는 행이 하나 이상 있어야 합니다".into());
        }
        let index = table_index(args, "index", expected.rows.len() - 1, expected.rows.len() - 1)?;
        let mut next = expected.clone();
        let removed = next.rows.remove(index);
        if next.header_row_id.as_deref() == Some(removed.id.as_str()) {
            next.header_row_id = None;
        }
        let operations = removed.cell_ids.into_iter().map(|node_id| Operation::RemoveNode { node_id }).collect();
        self.apply_canvas_table_structure(table_id, expected, next, operations)
    }

    fn canvas_table_add_column(&mut self, args: &Value) -> Result<(), String> {
        let (table_id, page_id, node, expected) = self.canvas_table_target(args)?;
        let columns = expected.rows[0].cell_ids.len();
        if columns >= designllm_contracts::MAX_GRID_COLUMNS as usize {
            return Err(format!("표는 {}열까지 지원합니다", designllm_contracts::MAX_GRID_COLUMNS));
        }
        if expected.rows.len().saturating_mul(columns.saturating_add(1)) > MAX_CANVAS_TABLE_CELLS {
            return Err("표가 지원할 수 있는 셀 수를 넘었습니다".into());
        }
        let index = table_index(args, "index", columns, columns)?;
        let mut next = expected.clone();
        next.column_widths = canvas_table_widths_after_add(expected.column_widths.as_deref(), columns, index)?;
        let mut operations = Vec::with_capacity(expected.rows.len());
        for (row_index, row) in next.rows.iter_mut().enumerate() {
            let source_column = index.saturating_sub(1).min(columns - 1);
            let style = canvas_table_cell_style(node, &expected, row_index, source_column)?;
            let cell_id = new_id();
            row.cell_ids.insert(index, cell_id.clone());
            operations.push(Operation::AddNode { page_id: page_id.clone(), parent_id: Some(table_id.clone()), node: canvas_table_cell(cell_id, style) });
        }
        self.apply_canvas_table_structure(table_id, expected, next, operations)
    }

    fn canvas_table_remove_column(&mut self, args: &Value) -> Result<(), String> {
        let (table_id, _page_id, _node, expected) = self.canvas_table_target(args)?;
        let columns = expected.rows[0].cell_ids.len();
        if columns <= 1 {
            return Err("표에는 열이 하나 이상 있어야 합니다".into());
        }
        let index = table_index(args, "index", columns - 1, columns - 1)?;
        let mut next = expected.clone();
        next.column_widths = canvas_table_widths_after_remove(expected.column_widths.as_deref(), columns, index)?;
        let mut operations = Vec::with_capacity(next.rows.len());
        for row in &mut next.rows {
            operations.push(Operation::RemoveNode { node_id: row.cell_ids.remove(index) });
        }
        self.apply_canvas_table_structure(table_id, expected, next, operations)
    }

    fn canvas_table_set_column_widths(&mut self, args: &Value) -> Result<(), String> {
        let (table_id, _page_id, _node, expected) = self.canvas_table_target(args)?;
        let columns = expected.rows[0].cell_ids.len();
        let column_widths = requested_canvas_table_widths(args, columns)?;
        self.kind_change(vec![Operation::SetCanvasTableColumnWidths {
            table_id,
            expected: expected.column_widths,
            column_widths,
        }])
    }

    fn canvas_table_set_header(&mut self, args: &Value) -> Result<(), String> {
        let (table_id, _page_id, _node, expected) = self.canvas_table_target(args)?;
        let enabled = flag(args, "enabled").ok_or("머리글 사용 여부를 지정하세요")?;
        let mut next = expected.clone();
        next.header_row_id = if enabled { Some(next.rows[0].id.clone()) } else { None };
        self.apply_canvas_table_structure(table_id, expected, next, Vec::new())
    }

    /// A content icon as a Path node on the current page: 48 px at (40, 40), `ink`-coloured line of the icon's own
    /// weight (1.5 on 24, scaled with the size), as the templates place icons.
    fn add_content_icon(&mut self, args: &Value) -> Result<(), String> {
        let name = text(args, "icon").unwrap_or_default();
        let (path, icon) = crate::content_icons::path_of(name).ok_or("없는 아이콘입니다")?;
        let page_id = match self.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::Canvas(document)) => document.pages.get(self.page_index as usize).map(|page| page.id.clone()),
            Some(ArtifactBody::Slides(document)) => document.pages.get(self.page_index as usize).map(|page| page.id.clone()),
            _ => None,
        }
        .ok_or("아이콘을 넣을 페이지가 없습니다")?;
        let size = (number(args, "size").unwrap_or(48.0).clamp(8.0, 2000.0) * EMU_PER_CSS_PX as f64).round() as i64;
        let node = designllm_contracts::Node {
            id: new_id(),
            kind: NodeKind::Path,
            bounds: designllm_contracts::Rect { x: 40 * EMU_PER_CSS_PX, y: 40 * EMU_PER_CSS_PX, width: size, height: size },
            text: String::new(),
            style: designllm_contracts::Style {
                fill: "transparent".into(),
                stroke: text(args, "color").unwrap_or("#20252D").to_owned(),
                stroke_width: (size + 8) / 16,
                stroke_cap: Some(designllm_contracts::StrokeCap::Round),
                stroke_join: Some(designllm_contracts::StrokeJoin::Round),
                ..Default::default()
            },
            asset: None,
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: Default::default(),
            ext: designllm_contracts::NodeExt { path: Some(path), icon: Some(icon), ..Default::default() },
        };
        self.kind_change(vec![Operation::AddNode { page_id, parent_id: None, node }])
    }

    /// A path from SVG path data and a view box, placed like a content icon: the longer side is 48 px, at (40, 40) of
    /// the current page, and the box keeps the view box's proportions. `PathSpec::from_svg` decides whether the data is
    /// valid, so a refused path sends no change set.
    pub(super) fn add_path(&mut self, args: &Value) -> Result<(), String> {
        let data = text(args, "d").unwrap_or_default();
        check_path_data_length(data)?;
        let view_box_text = text(args, "view_box").unwrap_or_default();
        let view_box = parse_view_box(view_box_text)?;
        let path = PathSpec::from_svg(data, view_box).map_err(|error| format!("경로 데이터를 넣지 않았습니다. {}", path_reason(&error, data)))?;
        let page_id = match self.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::Canvas(document)) => document.pages.get(self.page_index as usize).map(|page| page.id.clone()),
            // Slides take no path from this command: their VectorPaths write support is approximate, and the form does not offer it.
            Some(ArtifactBody::Slides(_)) => return Err("슬라이드에는 경로를 넣을 수 없습니다".into()),
            _ => None,
        }
        .ok_or("경로를 넣을 페이지가 없습니다")?;
        let side = 48.0 * EMU_PER_CSS_PX as f64;
        let longest = view_box.width.max(view_box.height) as f64;
        let bounds = Rect {
            x: 40 * EMU_PER_CSS_PX,
            y: 40 * EMU_PER_CSS_PX,
            width: (view_box.width as f64 * side / longest).round().max(1.0) as i64,
            height: (view_box.height as f64 * side / longest).round().max(1.0) as i64,
        };
        let node = Node {
            id: new_id(),
            kind: NodeKind::Path,
            bounds,
            text: String::new(),
            style: Style {
                fill: "transparent".into(),
                stroke: "#20252D".into(),
                stroke_width: (bounds.width.max(bounds.height) + 8) / 16,
                stroke_cap: Some(designllm_contracts::StrokeCap::Round),
                stroke_join: Some(designllm_contracts::StrokeJoin::Round),
                ..Default::default()
            },
            asset: None,
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: Default::default(),
            ext: NodeExt { path: Some(path), ..Default::default() },
        };
        if self.context.is_none() {
            return Err("작업물을 아직 읽는 중입니다".into());
        }
        let operations = vec![Operation::AddNode { page_id, parent_id: None, node }];
        let intents = intents_of(&operations);
        // The change set is answered later: the action keeps the text, so a refusal can put it back in the form.
        let artifact_id = self.artifact.as_ref().map(|artifact| artifact.id.clone()).unwrap_or_default();
        let action = PendingAction::PathAdd { artifact_id, d: data.to_owned(), view_box: view_box_text.to_owned() };
        self.apply_operations_for(operations, intents, action);
        Ok(())
    }

    // Web.

    fn web_replace_source(&mut self, args: &Value) -> Result<(), String> {
        let (path, expected_hash) = self.web_change_target(args, false)?;
        let content = text(args, "content").unwrap_or_default().to_owned();
        self.kind_change(vec![Operation::ReplaceWebSource { path, expected_hash, content }])
    }

    /// The file a web source change is for and the hash it expects: `path` from `args`, else the open file (`open_file`) or
    /// the entry file, and `expected_hash` from `args`, else the file's current hash. Shared with `web_mapping.rs` (task 44).
    pub(super) fn web_change_target(&self, args: &Value, open_file: bool) -> Result<(String, String), String> {
        let document = self.web_document().ok_or("웹 화면이 열려 있지 않습니다")?;
        let path = match text(args, "path") {
            Some(path) => path,
            None if open_file => self.open_web_path(document),
            None => &document.entry,
        };
        let file = document.files.get(path).ok_or("파일을 찾지 못했습니다")?;
        let expected_hash = text(args, "expected_hash").unwrap_or(&file.sha256).to_owned();
        Ok((path.to_owned(), expected_hash))
    }
}

/// The longest path data `add.path` and the path fields take, in characters. Longer input is refused, never cut.
const PATH_DATA_MAX_CHARS: usize = 1_048_576;

/// Refuses path data longer than `PATH_DATA_MAX_CHARS`, whole: the form or the field keeps the text it was given. Shared by
/// `add.path` and the path section's fields (properties.rs `path_from_fields`).
pub(super) fn check_path_data_length(data: &str) -> Result<(), String> {
    if data.len() > PATH_DATA_MAX_CHARS && data.chars().count() > PATH_DATA_MAX_CHARS {
        return Err("경로 데이터는 1,048,576자까지 넣을 수 있습니다. 입력은 잘라내지 않고 그대로 두었습니다".into());
    }
    Ok(())
}

/// A view box typed as `min-x min-y width height`, numbers separated by spaces or commas, in PathSpec units (SVG user
/// units × 1000). The size must be positive after that scaling, as the canvas requires, and every value must lie within
/// `PATH_MAX_COORDINATE`, so the scaled number is never saturated by the conversion.
pub(super) fn parse_view_box(input: &str) -> Result<Rect, String> {
    let scaled = input
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<f64>().ok().filter(|value| value.is_finite()).map(|value| (value * 1000.0).round()))
        .collect::<Option<Vec<f64>>>()
        .filter(|numbers| numbers.len() == 4)
        .ok_or("viewBox는 최소 x, 최소 y, 너비, 높이 숫자 네 개로 입력하세요")?;
    if scaled.iter().any(|value| value.abs() > PATH_MAX_COORDINATE as f64) {
        return Err(format!("viewBox 값은 ±{} 이하여야 합니다", PATH_MAX_COORDINATE / 1000));
    }
    let numbers: Vec<i64> = scaled.iter().map(|value| *value as i64).collect();
    let view_box = Rect { x: numbers[0], y: numbers[1], width: numbers[2], height: numbers[3] };
    if view_box.width <= 0 || view_box.height <= 0 {
        return Err("viewBox의 너비와 높이는 0보다 커야 합니다".into());
    }
    Ok(view_box)
}

/// The Korean reason for a refused path. The parser names a reason code and, for a syntax problem, the byte it is at; the
/// byte is shown as a character position in `data`, or as the end of the input.
pub(super) fn path_reason(error: &DltError, data: &str) -> String {
    let Some(diagnostic) = error.diagnostics.first() else {
        return "경로 데이터를 읽을 수 없습니다".into();
    };
    let reason = match diagnostic.code.as_str() {
        "PATH_EMPTY" => "경로 데이터가 비어 있습니다".to_owned(),
        "PATH_START" => "경로는 M 또는 m으로 시작해야 합니다".to_owned(),
        "PATH_COMMAND" => "명령 글자(M, L, C 등)가 필요합니다".to_owned(),
        "PATH_UNKNOWN_COMMAND" => "알 수 없는 명령 글자입니다".to_owned(),
        "PATH_NUMBER" => "숫자가 필요합니다".to_owned(),
        "PATH_FLAG" => "호 플래그는 0 또는 1이어야 합니다".to_owned(),
        "PATH_COMMA" => "쉼표는 인자 사이에 한 개만 쓰고, 뒤에는 숫자가 와야 합니다".to_owned(),
        "PATH_Z" => "Z 명령 뒤에는 인자를 쓸 수 없습니다".to_owned(),
        "PATH_RANGE" => format!("좌표가 허용 범위(±{})를 벗어났습니다", PATH_MAX_COORDINATE / 1000),
        "PATH_COMMANDS" => format!("명령은 {PATH_MAX_COMMANDS}개까지 쓸 수 있습니다"),
        "PATH_VIEW_BOX" => "viewBox의 너비와 높이는 0보다 커야 합니다".to_owned(),
        "PATH_VIEW_BOX_RANGE" => format!("viewBox 값은 ±{} 이하여야 합니다", PATH_MAX_COORDINATE / 1000),
        "PATH_CONTROL_RANGE" => format!("호의 제어점이 허용 범위(±{})를 벗어났습니다", PATH_MAX_COORDINATE / 1000),
        _ => "경로 데이터를 읽을 수 없습니다".to_owned(),
    };
    match diagnostic.params.get("byte").and_then(Value::as_u64).map(|byte| byte as usize) {
        Some(byte) if byte >= data.len() => format!("{reason} (입력 끝)"),
        Some(byte) => match data.get(..byte) {
            Some(head) => format!("{reason} ({}번째 글자)", head.chars().count() + 1),
            None => reason,
        },
        None => reason,
    }
}

/// Where a node sits on its page: the sum of its ancestors' origins (child bounds are relative to the parent).
pub(super) fn page_origin_of(nodes: &[designllm_contracts::Node], id: &str, offset: (i64, i64)) -> Option<(i64, i64, designllm_contracts::Node)> {
    for node in nodes {
        if node.id == id {
            return Some((offset.0 + node.bounds.x, offset.1 + node.bounds.y, node.clone()));
        }
        if let Some(found) = page_origin_of(&node.children, id, (offset.0 + node.bounds.x, offset.1 + node.bounds.y)) {
            return Some(found);
        }
    }
    None
}

impl DesktopApp {
    /// Style effects and grid fields of the selection (the properties panel's controls call these).
    pub(super) fn dispatch_effects(&mut self, spec: &CommandSpec, args: &Value) -> Option<Result<(), String>> {
        let unit = self.effect_unit();
        let result = match spec.id {
            "props.line_height" => match number(args, "value") {
                Some(multiple) if multiple > 0.0 => {
                    self.effect_style(StylePatch { line_height_milli: Some((multiple * 1000.0).round() as u32), ..Default::default() })
                }
                _ => self.effect_style(StylePatch { clear: [designllm_contracts::StyleField::LineHeight].into_iter().collect(), ..Default::default() }),
            },
            "props.letter_spacing" => {
                let value = number(args, "value").unwrap_or(0.0);
                self.effect_style(StylePatch { letter_spacing: Some((value * unit).round() as i64), ..Default::default() })
            }
            "props.vertical_align" => {
                let align = match text(args, "align") {
                    Some("middle") => designllm_contracts::VerticalAlign::Middle,
                    Some("bottom") => designllm_contracts::VerticalAlign::Bottom,
                    _ => designllm_contracts::VerticalAlign::Top,
                };
                self.effect_style(StylePatch { vertical_align: Some(align), ..Default::default() })
            }
            "props.stroke_style" => {
                let dash = text(args, "dash").map(|value| match value {
                    "dashed" => designllm_contracts::StrokeDash::Dashed,
                    "dotted" => designllm_contracts::StrokeDash::Dotted,
                    _ => designllm_contracts::StrokeDash::Solid,
                });
                let cap = text(args, "cap").map(|value| match value {
                    "round" => designllm_contracts::StrokeCap::Round,
                    "square" => designllm_contracts::StrokeCap::Square,
                    _ => designllm_contracts::StrokeCap::Butt,
                });
                let join = text(args, "join").map(|value| match value {
                    "round" => designllm_contracts::StrokeJoin::Round,
                    "bevel" => designllm_contracts::StrokeJoin::Bevel,
                    _ => designllm_contracts::StrokeJoin::Miter,
                });
                if dash.is_none() && cap.is_none() && join.is_none() {
                    return Some(Err("바꿀 선 모양을 고르세요".into()));
                }
                self.effect_style(StylePatch { stroke_dash: dash, stroke_cap: cap, stroke_join: join, ..Default::default() })
            }
            "props.gradient" => {
                let from = text(args, "from").unwrap_or("#FFFFFF").to_owned();
                let to = text(args, "to").unwrap_or("#000000").to_owned();
                let angle = number(args, "angle").unwrap_or(180.0);
                let gradient = designllm_contracts::Gradient {
                    angle_millideg: (angle.rem_euclid(360.0) * 1000.0).round() as i32,
                    stops: vec![
                        designllm_contracts::GradientStop { pos_milli: 0, color: from },
                        designllm_contracts::GradientStop { pos_milli: 1000, color: to },
                    ],
                };
                self.effect_style(StylePatch { gradient: Some(gradient), ..Default::default() })
            }
            "props.gradient_remove" => {
                self.effect_style(StylePatch { clear: [designllm_contracts::StyleField::Gradient].into_iter().collect(), ..Default::default() })
            }
            "props.grid" => {
                let columns = number(args, "columns").map(|value| value.max(1.0) as u32);
                let row_gap = number(args, "row_gap").map(|value| (value.max(0.0) * unit).round() as i64);
                self.effect_layout(None, move |layout| {
                    layout.mode = "grid".into();
                    if let Some(columns) = columns {
                        layout.grid_columns = Some(columns);
                    }
                    if row_gap.is_some() {
                        layout.row_gap = row_gap;
                    }
                })
            }
            "props.grid_span" => {
                let span = number(args, "span").unwrap_or(1.0).max(1.0) as u32;
                self.effect_layout(text(args, "node_id"), move |layout| layout.grid_span = Some(span))
            }
            "props.padding_sides" => {
                let side = |key: &str| (number(args, key).unwrap_or(0.0).max(0.0) * unit).round() as i64;
                let sides = [side("top"), side("right"), side("bottom"), side("left")];
                self.effect_layout(None, move |layout| layout.padding_sides = Some(sides))
            }
            "props.align_self" => {
                let align = match text(args, "align") {
                    Some("start") => designllm_contracts::AlignSelf::Start,
                    Some("center") => designllm_contracts::AlignSelf::Center,
                    Some("end") => designllm_contracts::AlignSelf::End,
                    Some("stretch") => designllm_contracts::AlignSelf::Stretch,
                    _ => designllm_contracts::AlignSelf::Auto,
                };
                // node_ids (several nodes, one undo step), else node_id, else every selected node.
                let ids: Vec<String> = match (args.get("node_ids").and_then(Value::as_array), text(args, "node_id")) {
                    (Some(list), _) => list.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
                    (None, Some(id)) => vec![id.to_owned()],
                    (None, None) => self.selected_nodes.clone(),
                };
                self.effect_layout_many(&ids, |layout| layout.align_self = Some(align))
            }
            "layer.move_to" => self.layer_move_to(args),
            _ => return None,
        };
        Some(result)
    }

    /// EMU per display unit for lengths in effects: px on canvases and slides, pt elsewhere.
    fn effect_unit(&self) -> f64 {
        match self.active.as_ref().map(|item| item.kind) {
            Some(designllm_contracts::ArtifactKind::Canvas | designllm_contracts::ArtifactKind::Slides | designllm_contracts::ArtifactKind::WebCode) => EMU_PER_CSS_PX as f64,
            _ => EMU_PER_PT as f64,
        }
    }

    fn effect_style(&mut self, patch: StylePatch) -> Result<(), String> {
        let node_ids = self.selected_nodes.clone();
        if node_ids.is_empty() {
            return Err("바꿀 대상을 고르세요".into());
        }
        self.kind_change(vec![Operation::SetStyle { node_ids, patch }])
    }

    /// The layout of `node_id` (else the selected node) with one change, as one change set.
    fn effect_layout(&mut self, node_id: Option<&str>, edit: impl FnOnce(&mut designllm_contracts::LayoutSpec)) -> Result<(), String> {
        let id = node_id.map(str::to_owned).or_else(|| self.first_selected()).ok_or("레이아웃을 바꿀 대상을 고르세요")?;
        let artifact = self.artifact.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
        let node = super::doc_tree::find_node(artifact, &id).cloned().ok_or("대상을 찾지 못했습니다")?;
        let mut layout = node.layout.clone().unwrap_or_default();
        edit(&mut layout);
        self.kind_change(vec![Operation::SetLayout { node_id: node.id, layout }])
    }

    /// The same layout change on every node in `ids`, as one change set (one undo step).
    fn effect_layout_many(&mut self, ids: &[String], edit: impl Fn(&mut designllm_contracts::LayoutSpec)) -> Result<(), String> {
        if ids.is_empty() {
            return Err("레이아웃을 바꿀 대상을 고르세요".into());
        }
        let artifact = self.artifact.as_ref().ok_or("작업물을 아직 읽는 중입니다")?;
        let mut operations = Vec::with_capacity(ids.len());
        for id in ids {
            let node = super::doc_tree::find_node(artifact, id).ok_or("대상을 찾지 못했습니다")?;
            let mut layout = node.layout.clone().unwrap_or_default();
            edit(&mut layout);
            operations.push(Operation::SetLayout { node_id: node.id.clone(), layout });
        }
        self.kind_change(operations)
    }

    /// Moves a layer into another parent (or onto the page) at `index`, keeping its place on the page.
    fn layer_move_to(&mut self, args: &Value) -> Result<(), String> {
        let node_id = text(args, "node_id").unwrap_or_default().to_owned();
        let parent_id = text(args, "parent_id").map(str::to_owned).filter(|id| !id.is_empty());
        let index = number(args, "index").unwrap_or(0.0).max(0.0) as u32;
        if parent_id.as_deref() == Some(node_id.as_str()) {
            return Err("자기 안으로는 옮길 수 없습니다".into());
        }
        let pages = match self.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::Canvas(document)) => document.pages.clone(),
            Some(ArtifactBody::Slides(document)) => document.pages.clone(),
            _ => return Err("이 작업물에서는 레이어를 옮길 수 없습니다".into()),
        };
        let (page, (x, y, node)) = pages
            .iter()
            .find_map(|page| page_origin_of(&page.nodes, &node_id, (0, 0)).map(|found| (page, found)))
            .ok_or("레이어를 찾지 못했습니다")?;
        let target_page = match number(args, "page_index") {
            Some(index) => pages.get(index.max(0.0) as usize).ok_or("페이지를 찾지 못했습니다")?,
            None => page,
        };
        let (origin_x, origin_y) = match &parent_id {
            Some(parent) => {
                let (px, py, parent_node) = page_origin_of(&target_page.nodes, parent, (0, 0)).ok_or("넣을 상위 레이어를 찾지 못했습니다")?;
                if page_origin_of(&node.children, parent, (0, 0)).is_some() {
                    return Err("자기 안으로는 옮길 수 없습니다".into());
                }
                if !matches!(parent_node.kind, NodeKind::Group | NodeKind::Rectangle) {
                    return Err("그룹이나 프레임 안으로만 옮길 수 있습니다".into());
                }
                (px, py)
            }
            None => (0, 0),
        };
        let bounds = designllm_contracts::Rect { x: x - origin_x, y: y - origin_y, ..node.bounds };
        let mut operations = vec![Operation::MoveNode { node_id: node_id.clone(), page_id: target_page.id.clone(), parent_id, index }];
        if bounds != node.bounds {
            operations.push(Operation::SetGeometry { node_id, bounds });
        }
        self.kind_change(operations)
    }
}

/// A number as the figure engine stores axis values: no trailing zeros.
/// Why `figure.plot` refuses error bars that read an axis column.
pub(super) const ERROR_ON_AXIS: &str = "오차 열에는 x축이나 y축에 쓴 열을 고를 수 없습니다";

/// An axis bound or tick step as `figure.plot` and `figure.axis_ticks` take it: a JSON number becomes its exact decimal text, a
/// string is kept as it is (the engine reads it), and null clears the bound. A number with more digits than the engine reads
/// is refused here, not rounded.
fn axis_bound(value: &Value) -> Result<Option<String>, String> {
    let Value::Number(number) = value else {
        return Ok(value.as_str().map(str::to_owned));
    };
    // An integer keeps the digits it was sent with (an f64 would round one past 2^53). A fraction is an f64 in the JSON
    // value: `Display` of an f64 is the shortest text that reads back to the same number, without an exponent.
    let text = if number.is_i64() || number.is_u64() {
        number.to_string()
    } else {
        number.as_f64().filter(|fraction| fraction.is_finite()).map_or_else(String::new, |fraction| fraction.to_string())
    };
    let digits = text.bytes().filter(u8::is_ascii_digit).count();
    if digits == 0 || digits > designllm_contracts::figure_rules::DECIMAL_MAX_DIGITS {
        return Err(format!("축 범위는 {}자리까지의 숫자여야 합니다", designllm_contracts::figure_rules::DECIMAL_MAX_DIGITS));
    }
    Ok(Some(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reordering_moves_one_id_and_keeps_the_others_in_order() {
        let ids = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        assert_eq!(reordered(ids.clone(), "a", 2).unwrap(), ["b", "c", "a"]);
        assert_eq!(reordered(ids.clone(), "c", 0).unwrap(), ["c", "a", "b"]);
        assert_eq!(reordered(ids.clone(), "b", 9).unwrap(), ["a", "c", "b"]);
        assert!(reordered(ids, "x", 0).is_none());
    }

    #[test]
    fn a_change_set_asks_for_the_intent_of_each_operation() {
        let operations = vec![
            Operation::SetCaption { caption: String::new() },
            Operation::SetAxisTicks { panel_id: "p".into(), axis: "y".into(), tick_step: None },
        ];
        assert_eq!(intents_of(&operations), [Intent::Content, Intent::Design].into_iter().collect());
    }

    #[test]
    fn text_fields_are_converted_to_emu_and_missing_ones_stay_unchanged() {
        let patch = text_patch(&serde_json::json!({ "font_size": 10, "bold": true }));
        assert_eq!(patch.font_size, Some(10 * EMU_PER_PT));
        assert_eq!(patch.bold, Some(true));
        assert_eq!(patch.italic, None);
        assert_eq!(patch.color, None);
    }
}
