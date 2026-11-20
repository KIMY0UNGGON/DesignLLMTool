//! Creating and editing figure content from the desktop: panels, diagram nodes and connectors,
//! tables read from CSV/TSV files, and text evidence read from PDF pages. Every change is an
//! ordinary change set sent to the host; the plot rules below only decide what the controls
//! offer, the figure engine still validates every result.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Instant,
};

use designllm_contracts::{
    new_id, Artifact, ArtifactBody, ArtifactKind, AssetRef, AxisSpec, DataColumn, Dataset,
    DatasetRef, Diagram, DltError, Edge, Evidence, Feature, FigureDocument, FigurePanel, Intent, Node,
    NodeKind, Operation, PanelBody, PlotDisplayMode, PlotSpec, Size, Style,
};
use designllm_contracts::figure_rules::{PAPER_EVIDENCE_ORIGIN, USER_AUTHORED_ORIGIN};
use eframe::egui::{
    self, Align, Align2, Context, Grid, Id, Layout, Rect, RichText, ScrollArea, TextEdit, Ui,
};

use super::{
    state::{ColumnDeclaration, CsvStep},
    CsvImportDraft, DatasetCellEdit, DesktopApp, FigureForm,
    HeldPdfEvidence, PdfEvidenceTarget, PendingAction, PendingRequest,
};
use super::pdf_coverage::{carry_pdf_coverage, coverage_is_consistent};
use crate::{
    figure_layout::{free_diagram_node_bounds, tiled_panel_bounds},
    rpc::RpcPayload,
    theme,
    widgets::field_with_button,
};

const PLOT_KINDS: [(&str, &str); 3] = [("bar", "막대"), ("line", "선"), ("scatter", "산점도")];

/// The colour of the first data series (the figure engine's own first palette colour).
const SERIES_COLOR: &str = "#1F4E79";

/// A plot panel draws its first series in the panel's fill colour, and `Style::default()` fills
/// with white: a chart made with the default style has lines, markers and bars that cannot be seen.
fn plot_style() -> Style {
    Style {
        fill: SERIES_COLOR.into(),
        ..Style::default()
    }
}

/// "(a)", "(b)", … for the first 26 panels, then "(27)", … — the lettering of multi-panel figures.
pub(super) fn panel_label(index: usize) -> String {
    match u8::try_from(index).ok().filter(|index| *index < 26) {
        Some(index) => format!("({})", char::from(b'a' + index)),
        None => format!("({})", index + 1),
    }
}

/// Appends `panel` and re-tiles every panel into the grid for the new count, so the figure never
/// ends up with overlapping panels (the engine rejects those).
pub(super) fn panel_additions(figure: &FigureDocument, mut panel: FigurePanel) -> Vec<Operation> {
    let total = figure.panels.len() + 1;
    panel.bounds = tiled_panel_bounds(figure.size, figure.panels.len(), total);
    let mut operations = vec![Operation::AddPanel {
        index: figure.panels.len() as u32,
        panel,
    }];
    let bounds = figure
        .panels
        .iter()
        .enumerate()
        .map(|(index, existing)| (existing.id.clone(), tiled_panel_bounds(figure.size, index, total)))
        .collect::<BTreeMap<_, _>>();
    if !bounds.is_empty() {
        operations.push(Operation::ArrangePanels { bounds });
    }
    operations
}

pub(super) fn new_diagram_panel(figure: &FigureDocument) -> FigurePanel {
    FigurePanel {
        id: new_id(),
        bounds: Default::default(),
        label: panel_label(figure.panels.len()),
        body: PanelBody::Diagram(Diagram {
            nodes: Vec::new(),
            edges: Vec::new(),
            evidence_ids: BTreeMap::new(),
            node_origins: BTreeMap::new(),
        }),
        style: Style::default(),
    }
}

/// The x column types a plot kind accepts (the figure engine's rule).
fn x_allowed(kind: &str, data_type: &str) -> bool {
    match kind {
        "bar" => data_type == "category",
        "scatter" => data_type == "numeric",
        "line" => matches!(data_type, "category" | "numeric"),
        _ => false,
    }
}

fn x_columns<'a>(dataset: &'a Dataset, kind: &str) -> Vec<&'a DataColumn> {
    dataset
        .columns
        .iter()
        .filter(|column| x_allowed(kind, &column.data_type))
        .collect()
}

fn y_columns<'a>(dataset: &'a Dataset, x_column: &str) -> Vec<&'a DataColumn> {
    dataset
        .columns
        .iter()
        .filter(|column| column.data_type == "numeric" && column.id != x_column)
        .collect()
}

/// The column types a table can declare, with the words the window shows for them.
const COLUMN_TYPES: [(&str, &str); 4] = [
    ("category", "범주"),
    ("numeric", "숫자"),
    ("count", "개수"),
    ("text", "텍스트"),
];

fn column_type_label(data_type: &str) -> &'static str {
    COLUMN_TYPES
        .iter()
        .find(|(name, _)| *name == data_type)
        .map_or("", |(_, label)| *label)
}

/// The type and unit each column of `dataset` has now, in file order.
fn declarations_of(dataset: &Dataset) -> Vec<ColumnDeclaration> {
    dataset
        .columns
        .iter()
        .map(|column| ColumnDeclaration {
            name: column.name.clone(),
            data_type: column.data_type.clone(),
            unit: column.unit.clone().unwrap_or_default(),
        })
        .collect()
}

impl CsvImportDraft {
    /// A table whose columns are declared (read with declarations, or already in the figure): the
    /// chart is described next.
    pub(super) fn new(dataset: Dataset) -> Self {
        let mut draft = Self {
            columns: declarations_of(&dataset),
            dataset,
            step: CsvStep::Chart,
            x_column: String::new(),
            y_column: String::new(),
            plot_kind: "line".into(),
            x_unit: String::new(),
            y_unit: String::new(),
        };
        draft.repair_columns();
        draft.sync_units("", "");
        draft
    }

    /// A table read with inferred column types. The user declares the real types and units first,
    /// starting from what the import inferred.
    pub(super) fn to_declare(dataset: Dataset) -> Self {
        Self {
            columns: declarations_of(&dataset),
            dataset,
            step: CsvStep::Columns,
            x_column: String::new(),
            y_column: String::new(),
            plot_kind: "line".into(),
            x_unit: String::new(),
            y_unit: String::new(),
        }
    }

    /// The table read with the declared columns arrived: the chart is described from its columns,
    /// whose declared units are offered as the axis units.
    pub(super) fn adopt(&mut self, declared: Dataset) {
        self.columns = declarations_of(&declared);
        self.dataset = declared;
        self.step = CsvStep::Chart;
        self.x_column.clear();
        self.y_column.clear();
        self.repair_columns();
        self.sync_units("", "");
    }

    /// Back to the column declaration, starting from the types the table has now.
    pub(super) fn reopen_declaration(&mut self) {
        self.columns = declarations_of(&self.dataset);
        self.step = CsvStep::Columns;
    }

    /// `source.import_csv` with the declared columns: every header column once, a unit only on a
    /// numeric column and only when one was typed (the host refuses empty units and units on
    /// other types, and never infers either).
    pub(super) fn declaration_request(&self) -> serde_json::Value {
        let columns: Vec<serde_json::Value> = self
            .columns
            .iter()
            .map(|column| {
                let mut entry = serde_json::json!({ "name": column.name, "data_type": column.data_type });
                let unit = column.unit.trim();
                if column.data_type == "numeric" && !unit.is_empty() {
                    entry["unit"] = serde_json::json!(unit);
                }
                entry
            })
            .collect();
        serde_json::json!({ "sha256": self.dataset.original_asset_hash, "columns": columns })
    }

    fn column_unit(&self, id: &str) -> String {
        self.dataset
            .columns
            .iter()
            .find(|column| column.id == id)
            .and_then(|column| column.unit.clone())
            .unwrap_or_default()
    }

    /// Offers a column's declared unit as the axis unit when the axis switches to that column;
    /// units the user typed stay while the column stays.
    pub(super) fn sync_units(&mut self, before_x: &str, before_y: &str) {
        if self.x_column != before_x {
            self.x_unit = self.column_unit(&self.x_column);
        }
        if self.y_column != before_y {
            self.y_unit = self.column_unit(&self.y_column);
        }
    }

    /// Keeps the chosen columns valid for the chosen kind, picking the first fit when they are not.
    pub(super) fn repair_columns(&mut self) {
        let x_valid = x_columns(&self.dataset, &self.plot_kind)
            .iter()
            .any(|column| column.id == self.x_column);
        if !x_valid {
            self.x_column = x_columns(&self.dataset, &self.plot_kind)
                .first()
                .map(|column| column.id.clone())
                .unwrap_or_default();
        }
        let y_valid = y_columns(&self.dataset, &self.x_column)
            .iter()
            .any(|column| column.id == self.y_column);
        if !y_valid {
            self.y_column = y_columns(&self.dataset, &self.x_column)
                .first()
                .map(|column| column.id.clone())
                .unwrap_or_default();
        }
    }

    fn x_is_numeric(&self) -> bool {
        self.dataset
            .columns
            .iter()
            .any(|column| column.id == self.x_column && column.data_type == "numeric")
    }

    /// Why no chart can be made from these columns, when the table itself is the reason.
    fn blocker(&self) -> Option<&'static str> {
        if self.x_column.is_empty() {
            Some("이 종류의 X축으로 쓸 수 있는 열이 없습니다")
        } else if self.y_column.is_empty() {
            Some("Y축으로 쓸 수 있는 숫자 열이 없습니다")
        } else {
            None
        }
    }

    /// The plot this draft describes. Units are required by the engine and never inferred, so a
    /// chart needs the unit of the y axis, and of the x axis when x is numeric.
    pub(super) fn plot_spec(&self, dataset_ref: DatasetRef) -> Option<PlotSpec> {
        let x = self.dataset.columns.iter().find(|column| column.id == self.x_column)?;
        let y = self.dataset.columns.iter().find(|column| column.id == self.y_column)?;
        if !x_allowed(&self.plot_kind, &x.data_type) || y.data_type != "numeric" || x.id == y.id {
            return None;
        }
        let y_unit = Some(self.y_unit.trim().to_owned()).filter(|unit| !unit.is_empty())?;
        let x_unit = if x.data_type == "numeric" {
            Some(Some(self.x_unit.trim().to_owned()).filter(|unit| !unit.is_empty())?)
        } else {
            None
        };
        let axis = |label: &str, unit: Option<String>| AxisSpec {
            label: label.to_owned(),
            unit,
            scale: "linear".into(),
            minimum: None,
            maximum: None,
            tick_step: None,
        };
        // No error bars. Legacy plots keep the render they had (no `display`): the key is shown only when a plot asks for it.
        Some(PlotSpec::new(
            self.plot_kind.clone(),
            dataset_ref,
            x.id.clone(),
            vec![y.id.clone()],
            axis(&x.name, x_unit),
            axis(&y.name, Some(y_unit)),
        ))
    }
}

/// The operation that adds an empty rectangle or ellipse to a diagram panel, in the next free
/// slot. `None` when the panel is not a diagram or has no room left.
pub(super) fn node_addition(figure: &FigureDocument, panel_id: &str, kind: NodeKind) -> Option<Operation> {
    let panel = figure.panels.iter().find(|panel| panel.id == panel_id)?;
    let PanelBody::Diagram(diagram) = &panel.body else {
        return None;
    };
    let bounds = free_diagram_node_bounds(panel.bounds.size(), &diagram.nodes)?;
    Some(Operation::AddDiagramNode {
        panel_id: panel.id.clone(),
        node: Node {
            id: new_id(),
            kind,
            bounds,
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: BTreeMap::new(),
            ext: Default::default(),
        },
        evidence_ids: Vec::new(),
        origin: USER_AUTHORED_ORIGIN.into(),
    })
}

/// The parts of a figure that can be clicked and dragged, in drawing order: every panel, then every
/// diagram node on top of its panel, in figure coordinates. The figure draws many more parts (axis
/// lines, tick labels, markers) but those are not elements; clicking them selects their panel.
pub(super) fn figure_regions(figure: &FigureDocument) -> Vec<(String, designllm_contracts::Rect)> {
    let mut regions: Vec<_> = figure
        .panels
        .iter()
        .map(|panel| (panel.id.clone(), panel.bounds))
        .collect();
    for panel in &figure.panels {
        if let PanelBody::Diagram(diagram) = &panel.body {
            for node in &diagram.nodes {
                regions.push((
                    node.id.clone(),
                    designllm_contracts::Rect {
                        x: panel.bounds.x + node.bounds.x,
                        y: panel.bounds.y + node.bounds.y,
                        ..node.bounds
                    },
                ));
            }
        }
    }
    regions
}

/// One operation per piece of text the PDF reader returned for the chosen page.
pub(super) fn evidence_additions(evidence: Vec<Evidence>) -> Vec<Operation> {
    evidence
        .into_iter()
        .map(|evidence| Operation::PutEvidence { evidence })
        .collect()
}

/// What one PDF page read does to the evidence an open figure holds.
#[derive(Default)]
struct EvidenceImport {
    /// One `PutEvidence` for each record the figure does not hold yet.
    additions: Vec<Operation>,
    /// Records the figure holds word for word. They are not sent again, so their review state stays.
    unchanged: usize,
    /// IDs the figure holds with other content. The window sends nothing while there is one.
    conflicts: Vec<String>,
}

/// Decides what a PDF page read does to the evidence of a figure. A record the figure lacks is added. A record it
/// holds word for word is left alone. A record with the ID of a held record but other content is a conflict: it must
/// not replace the held one. Records of the same read are judged against each other too.
fn evidence_import(figure: &FigureDocument, read: Vec<Evidence>) -> EvidenceImport {
    let mut import = EvidenceImport::default();
    let mut added: Vec<Evidence> = Vec::new();
    for record in read {
        let known = figure
            .evidence
            .iter()
            .chain(added.iter())
            .find(|held| held.id == record.id)
            .map(|held| same_reading(held, &record));
        match known {
            Some(true) => import.unchanged += 1,
            Some(false) => {
                if !import.conflicts.contains(&record.id) {
                    import.conflicts.push(record.id);
                }
            }
            None => added.push(record),
        }
    }
    import.additions = evidence_additions(added);
    import
}

/// Whether two records are one reading of the same PDF text: source, page, geometry, region and text all agree. The
/// review state is the one field a fresh read does not carry. Every other field counts, so a field added later makes a
/// difference a conflict instead of a silent overwrite.
fn same_reading(held: &Evidence, read: &Evidence) -> bool {
    match (reading_json(held), reading_json(read)) {
        (Some(held), Some(read)) => held == read,
        _ => false,
    }
}

/// The record as JSON without its review state. A record that cannot be serialized never counts as the same.
fn reading_json(evidence: &Evidence) -> Option<serde_json::Value> {
    let mut value = serde_json::to_value(evidence).ok()?;
    value.as_object_mut()?.remove("review_state");
    Some(value)
}

const REVIEW_UNREVIEWED: &str = "unreviewed";
const REVIEW_REVIEWED: &str = "reviewed";
const REVIEW_USER_PROVIDED: &str = "user_provided";

/// Whether the user can change this record's review state, and to which state: `Some(true)` marks an unreviewed
/// PDF extraction as reviewed, `Some(false)` takes that mark back. Other records have no review toggle.
pub(super) fn review_target(evidence: &Evidence) -> Option<bool> {
    if evidence.origin != PAPER_EVIDENCE_ORIGIN {
        return None;
    }
    match evidence.review_state.as_str() {
        REVIEW_UNREVIEWED => Some(true),
        REVIEW_REVIEWED => Some(false),
        _ => None,
    }
}

/// The operation that moves one PDF extraction to `reviewed` or back to `unreviewed`. It is the record as the window
/// holds it with only `review_state` changed, so text, source hash, page, region and PDF geometry stay as the PDF
/// reader returned them. `None` when the record is not in the figure or has no toggle in that direction.
pub(super) fn review_change(figure: &FigureDocument, evidence_id: &str, reviewed: bool) -> Option<Operation> {
    let current = figure.evidence.iter().find(|evidence| evidence.id == evidence_id)?;
    if review_target(current) != Some(reviewed) {
        return None;
    }
    let mut evidence = current.clone();
    let next = if reviewed { REVIEW_REVIEWED } else { REVIEW_UNREVIEWED };
    evidence.review_state = next.to_owned();
    Some(Operation::PutEvidence { evidence })
}

/// The operation that sets whether a plot panel shows its error key (`SetPlotDisplay`, a design change). It names the mode the
/// plot has in `figure` as `expected_display`, so the host refuses it when the plot has changed since. `None` when the panel is
/// not a plot or already has the wanted mode.
pub(super) fn plot_display_change(figure: &FigureDocument, panel_id: &str, error_key: bool) -> Option<Operation> {
    let panel = figure.panels.iter().find(|panel| panel.id == panel_id)?;
    let PanelBody::Plot(plot) = &panel.body else {
        return None;
    };
    let wanted = error_key.then_some(PlotDisplayMode::ErrorKey);
    (plot.display != wanted).then(|| Operation::SetPlotDisplay {
        panel_id: panel.id.clone(),
        expected_display: plot.display,
        display: wanted,
    })
}

/// The words the window shows for a review state. States the host does not define are shown as they are.
fn review_state_label(state: &str) -> &str {
    match state {
        REVIEW_UNREVIEWED => "미검토",
        REVIEW_REVIEWED => "검토됨",
        REVIEW_USER_PROVIDED => "사용자 입력",
        other => other,
    }
}

/// What the window says when records read from a PDF page are held because the figure they were read for is not shown at
/// the revision they were read at. Nothing is written; the records stay held until the user copies or discards them.
const PDF_HELD_MOVED_ON: &str = "그림이나 작업물이 바뀌어 PDF 근거를 넣지 않고 보류했습니다. 복사하거나 버리고, 현재 그림에는 PDF 쪽을 다시 가져오세요";
/// What the window says while a cell edit is open: a change would end the edit and its draft.
const PDF_HELD_DATASET_EDIT: &str = "자료 값을 편집하는 중이라 PDF 근거를 넣지 않고 보류했습니다. 편집을 닫은 뒤 다시 넣을 수 있습니다";
/// What the window says when a send that could not start leaves the records held.
const PDF_HELD_SEND_FAILED: &str = "변경을 보내지 못해 PDF 근거를 보류했습니다. 다시 넣기로 재시도할 수 있습니다";

/// What a `source.extract_pdf` reply says about the page the import asked for.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PageOutcome {
    /// The reply has no `page_reports`: the earlier reply shape, read as before.
    Legacy,
    /// The page was read and has no text.
    NoText,
    /// The page was read, and the reply accounts for every record of it.
    Read,
    /// The page was not read safely, or the reply does not account for it: nothing from it goes into the figure. The
    /// lines are the reader's diagnostics as `code: message`.
    Unsafe(Vec<String>),
}

/// What the reply says about the requested page. `records` is the number of evidence records the reply carries.
pub(super) fn page_outcome(reply: &serde_json::Value, page_index: u32, records: usize) -> PageOutcome {
    // A reply whose page count, read pages and page reports contradict each other or the request shows no page as read.
    if !coverage_is_consistent(reply, page_index) {
        return PageOutcome::Unsafe(Vec::new());
    }
    let Some(reports) = reply.get("page_reports").and_then(serde_json::Value::as_array) else {
        return PageOutcome::Legacy;
    };
    let Some(report) = reports.iter().find(|report| {
        report.get("physical_page_index").and_then(serde_json::Value::as_u64) == Some(u64::from(page_index))
    }) else {
        return PageOutcome::Unsafe(Vec::new());
    };
    if report.get("read").and_then(serde_json::Value::as_bool) != Some(true) {
        let lines: Vec<String> = report
            .get("diagnostics")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let code = item.get("code")?.as_str()?;
                let message = item.get("message").and_then(serde_json::Value::as_str).unwrap_or_default();
                Some(format!("{code}: {message}"))
            })
            .collect();
        return PageOutcome::Unsafe(lines);
    }
    match report.get("evidence_count").and_then(serde_json::Value::as_u64) {
        Some(0) if records == 0 => PageOutcome::NoText,
        Some(count) if records > 0 && count == records as u64 => PageOutcome::Read,
        _ => PageOutcome::Unsafe(Vec::new()),
    }
}

/// What the window says about pages it did not read safely: the pages (as the window numbers them), then the reader's
/// diagnostics when it gave any.
fn unsafe_pages_message(pages: &str, lines: &[String]) -> String {
    if lines.is_empty() {
        format!("{pages}쪽을 안전하게 읽지 못해 PDF 근거를 넣지 않았습니다")
    } else {
        format!("{pages}쪽을 안전하게 읽지 못해 PDF 근거를 넣지 않았습니다. 진단: {}", lines.join("; "))
    }
}

/// What the window says for a page read that gave no text: the page and why nothing of it went in. The page counts as read
/// (core puts it in `read_pages`), so the words differ from `unsafe_pages_message`, which is for pages not read safely.
fn no_text_message(page_index: u32) -> String {
    format!("{}쪽에는 글자가 없어 PDF 근거를 넣지 않았습니다", page_index.saturating_add(1))
}

/// What the window says when `source.extract_pdf` refuses a request. A refusal that names pages (the reader did not read
/// them safely) gives those pages and the reader's diagnostics; any other refusal keeps its own message.
pub(super) fn pdf_read_failure_message(error: &DltError) -> String {
    if error.affected_ids.is_empty() {
        return super::user_error(error);
    }
    // The reader numbers physical pages from 0; the window shows them from 1.
    let pages: Vec<String> = error
        .affected_ids
        .iter()
        .map(|id| id.parse::<u32>().map_or_else(|_| id.clone(), |page| page.saturating_add(1).to_string()))
        .collect();
    let lines: Vec<String> = error
        .diagnostics
        .iter()
        .map(|item| format!("{}: {}", item.code, item.message))
        .collect();
    unsafe_pages_message(&pages.join(", "), &lines)
}

/// What the window says when a read holds a record the figure has with other content.
fn conflict_message(count: usize) -> String {
    format!(
        "그림에 이미 있는 근거와 내용이 다른 항목 {count}개가 있어 PDF 근거를 넣지 않고 보류했습니다. 기존 근거는 그대로 두었습니다. 복사하거나 버리세요"
    )
}

/// The text of one held set, one line per record: the text, then its source in brackets (file name, page, PDF hash). The
/// source is the record's own page and hash, so a record that came from another page or file shows it. This is what the
/// copy button puts on the clipboard.
fn held_pdf_copy_text(held: &HeldPdfEvidence) -> String {
    held.records
        .iter()
        .map(|record| {
            let page = record.physical_page_index.map_or_else(|| "-".to_owned(), |index| format!("{}쪽", index.saturating_add(1)));
            format!("{} [{} {page} sha256:{}]", record.text, held.target.file_name, record.source_hash)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the window says when the records of a read do not all come from the PDF file and physical page the import asked
/// for. They stay held (copy or discard) and nothing is written.
const PDF_SOURCE_MISMATCH: &str = "읽은 근거의 PDF 파일이나 쪽이 요청과 달라 그림에 넣지 않았습니다. 복사하거나 버리세요";

/// Whether every record of a read comes from the PDF file the import read and from the physical page it asked for. A target
/// whose file was not imported has no hash to compare with, so only an empty read matches it.
fn read_matches_target(target: &PdfEvidenceTarget, records: &[Evidence]) -> bool {
    let Some(sha256) = target.asset_sha256.as_deref() else {
        return records.is_empty();
    };
    records
        .iter()
        .all(|record| record.source_hash == sha256 && record.physical_page_index == Some(target.page_index))
}

/// What the window says while a PDF import has not added its records yet.
const PDF_IMPORT_WAIT: &str = "PDF 근거를 가져오는 중입니다. 끝나면 다시 시도할 수 있습니다";
/// What the window says when a PDF import's records are not put into the figure, because the window shows another
/// document now or the figure changed since the import started. Nothing is written.
const PDF_IMPORT_MOVED_ON: &str = "그림이나 작업물이 바뀌어 PDF 근거를 넣지 않았습니다. 같은 그림을 열어 두고 근거 추가를 다시 누르세요";
/// What the window says while a change to the open figure is on its way.
const FIGURE_CHANGE_WAIT: &str = "figure 변경을 보내는 중입니다. 끝나면 다시 시도할 수 있습니다";
/// What the window says while the figure is read again after a change, so the figure on screen is not the latest yet.
const FIGURE_READ_WAIT: &str = "최신 작업물 상태를 읽고 있습니다. 완료될 때까지 기다리세요";

/// A PDF import that has not added its records yet: the file is read, then its text is extracted.
fn pdf_import_on_its_way(request: &PendingRequest) -> bool {
    matches!(request.action, PendingAction::PdfAssetImport { .. } | PendingAction::PdfEvidenceImport)
}

/// A request whose reply changes the open figure: a change set, an undo or redo, a cell edit, or an image or evidence
/// commit. Changes are sent against the revision on screen, so a change sent meanwhile is refused against this one, or
/// this one against it. The set mirrors `artifact_write_has_pending_edits` (ui_channel.rs), which is private there.
fn figure_change_on_its_way(request: &PendingRequest) -> bool {
    request.apply.is_some()
        || matches!(
            request.method.as_str(),
            "changes.apply" | "history.undo" | "history.redo" | "history.undo_change" | "figure.dataset.edit"
        )
        || matches!(
            request.action,
            PendingAction::Apply
                | PendingAction::FigureDataset
                | PendingAction::FigureEvidence
                | PendingAction::FigureImageAsset { .. }
        )
}

/// A read of the figure's state, which a change makes necessary: the figure on screen is older until it lands.
fn figure_read_on_its_way(request: &PendingRequest) -> bool {
    matches!(request.method.as_str(), "context.read" | "artifact.inspect")
}

/// Why figure changes have to wait for the requests on their way, `None` when none of them is in the way. An import
/// is named first, then a change, then a read.
fn figure_wait_reason<'a>(requests: impl IntoIterator<Item = &'a PendingRequest>) -> Option<&'static str> {
    let (mut import, mut change, mut read) = (false, false, false);
    for request in requests {
        import |= pdf_import_on_its_way(request);
        change |= figure_change_on_its_way(request);
        read |= figure_read_on_its_way(request);
    }
    if import {
        Some(PDF_IMPORT_WAIT)
    } else if change {
        Some(FIGURE_CHANGE_WAIT)
    } else if read {
        Some(FIGURE_READ_WAIT)
    } else {
        None
    }
}

/// The operation that draws a connector from one diagram node to another.
pub(super) fn connection(panel_id: &str, from: &str, to: &str) -> Operation {
    Operation::ConnectDiagram {
        panel_id: panel_id.to_owned(),
        edge: Edge {
            id: new_id(),
            from: from.to_owned(),
            to: to.to_owned(),
            label: String::new(),
            evidence_ids: Vec::new(),
            origin: USER_AUTHORED_ORIGIN.into(),
            style: Style::default(),
        },
    }
}

/// The change that adds the imported table to the figure and, with `make_plot`, a chart panel that
/// plots it, with the intents it needs. A table the figure already holds is reused at its current
/// version, never replaced. `None` when there is nothing to add or the chart is not fully described.
pub(super) fn chart_change(
    figure: &FigureDocument,
    draft: &CsvImportDraft,
    make_plot: bool,
) -> Option<(Vec<Operation>, BTreeSet<Intent>)> {
    let held = figure.datasets.iter().find(|dataset| dataset.id == draft.dataset.id);
    let dataset_ref = DatasetRef {
        dataset_id: draft.dataset.id.clone(),
        version_hash: held.map_or_else(
            || draft.dataset.version_hash.clone(),
            |dataset| dataset.version_hash.clone(),
        ),
    };
    let mut operations = Vec::new();
    if held.is_none() {
        operations.push(Operation::PutDataset {
            dataset: draft.dataset.clone(),
        });
    }
    let mut intents = BTreeSet::from([Intent::Data]);
    if make_plot {
        let plot = draft.plot_spec(dataset_ref)?;
        let panel = FigurePanel {
            id: new_id(),
            bounds: Default::default(),
            label: panel_label(figure.panels.len()),
            body: PanelBody::Plot(plot),
            style: plot_style(),
        };
        operations.extend(panel_additions(figure, panel));
        intents.insert(Intent::Content);
        intents.insert(Intent::Design);
    }
    (!operations.is_empty()).then_some((operations, intents))
}

fn dataset_origin_label(origin: &str) -> &'static str {
    match origin {
        "user_experimental" => "실험 자료",
        "simulation" => "시뮬레이션",
        "example" => "예시",
        "image_estimate" => "이미지 추정",
        "derived_transform" => "수정한 자료",
        _ => "자료",
    }
}

/// The panel a selected id belongs to: the panel itself, or the diagram that holds the node or
/// connector.
pub(super) fn owning_panel<'a>(figure: &'a FigureDocument, id: &str) -> Option<&'a FigurePanel> {
    figure.panels.iter().find(|panel| {
        panel.id == id
            || match &panel.body {
                PanelBody::Diagram(diagram) => {
                    diagram.nodes.iter().any(|node| node.id == id)
                        || diagram.edges.iter().any(|edge| edge.id == id)
                }
                _ => false,
            }
    })
}

fn node_title(index: usize, node: &Node) -> String {
    let text = node.text.trim();
    if !text.is_empty() {
        return super::truncate(text, 14);
    }
    let kind = if node.kind == NodeKind::Ellipse { "원" } else { "도형" };
    format!("{kind} {}", index + 1)
}

impl DesktopApp {
    pub(super) fn figure_document(&self) -> Option<FigureDocument> {
        match &self.artifact {
            Some(Artifact {
                body: ArtifactBody::Figure(figure),
                ..
            }) => Some(figure.clone()),
            _ => None,
        }
    }

    /// Click and drag handling on a figure's preview. Regions come from the figure's own panels and
    /// nodes, not from the drawn parts: asking the host about an axis label would be refused
    /// ("not found"), and a drag must start from the panel's bounds, not from a tick label's.
    pub(super) fn draw_figure_overlays(&mut self, ui: &mut Ui, page_size: Size, page_rect: Rect) {
        let Some(figure) = self.figure_document() else {
            return;
        };
        for (index, (id, bounds)) in figure_regions(&figure).into_iter().enumerate() {
            let rect = super::page_to_screen(bounds, page_size, page_rect);
            if rect.width() < 1.0 || rect.height() < 1.0 {
                continue;
            }
            let key = Id::new(("figure-region", id.as_str(), index));
            if self
                .interact_with_region(ui, key, 0, page_rect, &id, bounds, page_size, rect)
                .is_some()
            {
                self.select_node(id);
            }
        }
    }

    pub(super) fn open_figure_form(&mut self, form: FigureForm) {
        self.notices.clear();
        self.figure_form = Some(form);
        self.focus_popup_field = true;
    }

    fn add_figure_node(&mut self, panel_id: &str, kind: NodeKind) {
        if !self.feature_edit_supported(Feature::Diagram) {
            return;
        }
        let Some(figure) = self.figure_document() else {
            return;
        };
        let Some(operation) = node_addition(&figure, panel_id, kind) else {
            return;
        };
        self.apply_operations(vec![operation], [Intent::Content].into_iter().collect());
    }

    fn connect_figure_nodes(&mut self, panel_id: &str, from: &str, to: &str) {
        if !self.feature_edit_supported(Feature::Diagram) || from == to {
            return;
        }
        self.apply_operations(
            vec![connection(panel_id, from, to)],
            [Intent::Content].into_iter().collect(),
        );
    }

    pub(super) fn import_csv_source(&mut self) {
        if !self.feature_edit_supported(Feature::Dataset) || self.csv_path.trim().is_empty() {
            return;
        }
        self.call(
            "asset.import",
            serde_json::json!({ "path": self.csv_path.trim() }),
            PendingAction::CsvAssetImport,
        );
    }

    /// Starts a PDF evidence import. The figure on screen, its revision and the page are captured here. Before the page is
    /// read, and again before its records are committed, the window checks that it still shows that figure at that revision.
    pub(super) fn import_pdf_evidence(&mut self) {
        // The records are committed against the revision on screen: a PDF import starts only when no change to the
        // figure, and no refresh of it, is on its way.
        if !self.feature_edit_supported(Feature::Evidence)
            || self.pdf_path.trim().is_empty()
            || figure_wait_reason(self.requests_on_their_way()).is_some()
        {
            return;
        }
        let Ok(page) = self.pdf_page.trim().parse::<u32>() else {
            return;
        };
        if page == 0 {
            return;
        }
        let file_name = Path::new(self.pdf_path.trim()).file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let target = self.artifact.as_ref().map(|artifact| PdfEvidenceTarget {
            artifact_id: artifact.id.clone(),
            revision: artifact.revision,
            page_index: page - 1,
            file_name,
            asset_sha256: None,
            awaiting: None,
        });
        let Some(target) = target.filter(|target| self.pdf_evidence_target_is_current(target)) else {
            self.set_error("그림을 아직 읽는 중이라 PDF 근거를 가져오지 않았습니다. 읽은 뒤 다시 가져오세요".into());
            return;
        };
        let page_index = target.page_index;
        self.pdf_evidence_target = Some(target);
        let sent = self.call(
            "asset.import",
            serde_json::json!({ "path": self.pdf_path.trim() }),
            PendingAction::PdfAssetImport { page_index },
        );
        self.await_pdf_reply(sent);
    }

    /// Records the ticket of the request the import now waits for. A request the window did not send ends the import.
    fn await_pdf_reply(&mut self, sent: Option<u64>) {
        let Some(ticket) = sent else {
            self.pdf_evidence_target = None;
            return;
        };
        if let Some(target) = self.pdf_evidence_target.as_mut() {
            target.awaiting = Some(ticket);
        }
    }

    /// Whether the window still shows the figure a PDF import was started for, at the revision it was started at. The open
    /// document and the figure read for it must both name that figure, so a switch to another document or a change fails.
    fn pdf_evidence_target_is_current(&self, target: &PdfEvidenceTarget) -> bool {
        let shows_target = self.active.as_ref().is_some_and(|active| {
            active.id == target.artifact_id && active.kind == ArtifactKind::Figure && active.revision == target.revision
        });
        let reads_target = self.artifact.as_ref().is_some_and(|artifact| {
            artifact.id == target.artifact_id
                && artifact.kind == ArtifactKind::Figure
                && artifact.revision == target.revision
                && matches!(artifact.body, ArtifactBody::Figure(_))
        });
        shows_target && reads_target
    }

    /// Whether a reply is the answer the import in progress waits for. A reply with another ticket answers an import that was
    /// replaced, and a reply with no import in progress answers one that was discarded. Neither changes anything or shows a
    /// message.
    pub(super) fn pdf_reply_is_current(&self, ticket: u64) -> bool {
        self.pdf_evidence_target.as_ref().is_some_and(|target| target.awaiting == Some(ticket))
    }

    /// The requests on their way for the open document that the window still waits for. Another document's requests hold
    /// nothing back here. A PDF import whose reply is not the one the import awaits (it was discarded or replaced) holds no
    /// change or read back either.
    pub(super) fn requests_on_their_way(&self) -> impl Iterator<Item = &PendingRequest> + '_ {
        let open = self.active.as_ref().map(|active| active.id.as_str());
        self.pending
            .iter()
            .filter(move |(_, request)| request.artifact_id.as_deref() == open)
            .filter(|(ticket, request)| !pdf_import_on_its_way(request) || self.pdf_reply_is_current(**ticket))
            .map(|(_, request)| request)
    }

    /// The reply of `asset.import` for a PDF import. The page is read only while the figure is still the one the import
    /// started for.
    pub(super) fn pdf_asset_imported(&mut self, ticket: u64, page_index: u32, value: serde_json::Value) {
        if !self.pdf_reply_is_current(ticket) {
            return;
        }
        let current = self
            .pdf_evidence_target
            .as_ref()
            .is_some_and(|target| target.page_index == page_index && self.pdf_evidence_target_is_current(target));
        if !current {
            self.pdf_import_moved_on();
            return;
        }
        match serde_json::from_value::<AssetRef>(value) {
            Ok(asset) => {
                self.forget_pdf_coverage_of_other_file(&asset.sha256);
                // The page is read from this file only: its hash is kept with the import, and every record must carry it.
                if let Some(target) = self.pdf_evidence_target.as_mut() {
                    target.asset_sha256 = Some(asset.sha256.clone());
                }
                let params = serde_json::json!({ "sha256": asset.sha256, "pages": [page_index] });
                let sent = self.call("source.extract_pdf", params, PendingAction::PdfEvidenceImport);
                self.await_pdf_reply(sent);
            }
            Err(_) => {
                self.pdf_evidence_target = None;
                self.set_error("PDF 파일을 가져오지 못했습니다".into());
            }
        }
    }

    /// The reply of `source.extract_pdf` for a PDF import: the page's records go to the figure the import started for. A
    /// page the reply does not show as read safely adds nothing, and the window names it. A page read without text is
    /// reported the same way whichever figure the window shows.
    pub(super) fn pdf_evidence_read(&mut self, ticket: u64, reply: &serde_json::Value) {
        if !self.pdf_reply_is_current(ticket) {
            return;
        }
        // A current reply is one its target awaits (`pdf_reply_is_current`), so the target is there.
        let Some(target) = self.pdf_evidence_target.take() else { return };
        let read: Vec<Evidence> = reply
            .get("evidence")
            .cloned()
            .and_then(|value| serde_json::from_value::<Vec<Evidence>>(value).ok())
            .unwrap_or_default();
        let outcome = page_outcome(reply, target.page_index, read.len());
        self.record_pdf_reading(&target, reply, !matches!(outcome, PageOutcome::Unsafe(_)));
        match outcome {
            PageOutcome::Unsafe(lines) => self.set_error(unsafe_pages_message(&target.page_index.saturating_add(1).to_string(), &lines)),
            PageOutcome::NoText => self.set_error(no_text_message(target.page_index)),
            PageOutcome::Legacy if read.is_empty() => self.set_error(no_text_message(target.page_index)),
            PageOutcome::Legacy | PageOutcome::Read => self.hold_pdf_evidence(read, target),
        }
    }

    /// Holds the records one page read produced, under a new id, and tries to put them into the figure they were read for.
    fn hold_pdf_evidence(&mut self, records: Vec<Evidence>, target: PdfEvidenceTarget) {
        // The import is over: the held records wait for no reply.
        let target = PdfEvidenceTarget { awaiting: None, ..target };
        let id = new_id();
        self.held_pdf_evidence.push(HeldPdfEvidence {
            id: id.clone(),
            target,
            records,
            message: String::new(),
            ticket: None,
        });
        self.try_put_held_pdf_evidence(&id);
    }

    /// Puts one held set into the figure it was read for, when nothing stands in the way. The figure is checked right before
    /// the change is sent, and the change carries the revision the read was started at. Records the figure holds word for
    /// word are not sent again. A conflict, an open cell edit, a change or read on its way, or a figure that is not shown at
    /// that revision keeps the set held, and the window says why.
    fn try_put_held_pdf_evidence(&mut self, id: &str) {
        let Some(held) = self.held_pdf_evidence.iter().find(|held| held.id == id).cloned() else {
            return;
        };
        if held.ticket.is_some() {
            return;
        }
        if !self.pdf_evidence_target_is_current(&held.target) {
            self.note_held_pdf(id, PDF_HELD_MOVED_ON);
            return;
        }
        if !read_matches_target(&held.target, &held.records) {
            self.set_held_pdf_message(id, PDF_SOURCE_MISMATCH.to_owned());
            self.set_error(PDF_SOURCE_MISMATCH.into());
            return;
        }
        let Some(Artifact { body: ArtifactBody::Figure(figure), .. }) = self.artifact.as_ref() else {
            self.note_held_pdf(id, PDF_HELD_MOVED_ON);
            return;
        };
        let import = evidence_import(figure, held.records);
        if !import.conflicts.is_empty() {
            let message = conflict_message(import.conflicts.len());
            self.set_held_pdf_message(id, message.clone());
            self.set_error(message);
            return;
        }
        if import.additions.is_empty() {
            self.held_pdf_evidence.retain(|held| held.id != id);
            self.notices.info(format!("이미 넣은 근거 {}개는 그대로 두었습니다", import.unchanged), Instant::now());
            return;
        }
        if let Some(reason) = self.held_pdf_figure_wait() {
            self.note_held_pdf(id, reason);
            return;
        }
        let intents: BTreeSet<Intent> = [Intent::Content].into_iter().collect();
        match self.apply_changes(import.additions, intents, held.target.revision, PendingAction::FigureEvidence) {
            Some(ticket) => {
                if let Some(entry) = self.held_pdf_evidence.iter_mut().find(|entry| entry.id == id) {
                    entry.ticket = Some(ticket);
                    entry.message.clear();
                }
            }
            None => self.note_held_pdf(id, PDF_HELD_SEND_FAILED),
        }
    }

    /// Why a change to the figure has to wait: an open cell edit, or what is on its way (`None` when nothing blocks it).
    fn held_pdf_figure_wait(&self) -> Option<&'static str> {
        if self.dataset_edit.is_some() {
            Some(PDF_HELD_DATASET_EDIT)
        } else {
            figure_wait_reason(self.requests_on_their_way())
        }
    }

    /// Why one held set cannot go in right now, in the words the window shows; `None` when a retry may send it.
    fn held_pdf_wait(&self, held: &HeldPdfEvidence) -> Option<&'static str> {
        if held.ticket.is_some() {
            Some(FIGURE_CHANGE_WAIT)
        } else if !self.pdf_evidence_target_is_current(&held.target) {
            Some(PDF_HELD_MOVED_ON)
        } else if !read_matches_target(&held.target, &held.records) {
            Some(PDF_SOURCE_MISMATCH)
        } else {
            self.held_pdf_figure_wait()
        }
    }

    /// Records why a held set waits, in the set and in the window.
    fn note_held_pdf(&mut self, id: &str, message: &str) {
        self.set_held_pdf_message(id, message.to_owned());
        self.notices.warning(message, Instant::now());
    }

    fn set_held_pdf_message(&mut self, id: &str, message: String) {
        if let Some(entry) = self.held_pdf_evidence.iter_mut().find(|entry| entry.id == id) {
            entry.message = message;
        }
    }

    /// The retry button of a held set: its records go in when they can now, and stay held when they cannot.
    pub(super) fn retry_held_pdf_evidence(&mut self, id: &str) {
        self.try_put_held_pdf_evidence(id);
    }

    /// The discard button of a held set. A set whose commit is on its way stays until the host answers.
    pub(super) fn discard_held_pdf_evidence(&mut self, id: &str) {
        self.held_pdf_evidence.retain(|held| held.id != id || held.ticket.is_some());
    }

    /// The host accepted the commit of held records: they are in the figure now. `reply` is the host's answer, which names the
    /// revision the commit made.
    pub(super) fn pdf_evidence_committed(&mut self, ticket: u64, reply: &serde_json::Value) {
        if let Some(held) = self.held_pdf_evidence.iter().find(|held| held.ticket == Some(ticket)) {
            let committed_revision = reply.get("revision").and_then(serde_json::Value::as_u64);
            carry_pdf_coverage(&mut self.pdf_coverage, &held.target, committed_revision);
        }
        self.held_pdf_evidence.retain(|held| held.ticket != Some(ticket));
    }

    /// The host refused the commit of held records, or its answer did not come through: they stay held with the reason.
    pub(super) fn pdf_evidence_commit_failed(&mut self, ticket: u64, reason: &str) {
        if let Some(entry) = self.held_pdf_evidence.iter_mut().find(|entry| entry.ticket == Some(ticket)) {
            entry.ticket = None;
            entry.message = format!("그림에 넣지 못해 보류했습니다: {reason}");
        }
    }

    /// Ends a PDF import the window will not finish. Nothing more is read or written, and the window says so. The same
    /// figure can be opened and its import started again.
    fn pdf_import_moved_on(&mut self) {
        self.pdf_evidence_target = None;
        self.notices.warning(PDF_IMPORT_MOVED_ON, Instant::now());
    }

    /// A PDF reply that failed: a failed import is over, and a refused commit of held records keeps them held, with the reason.
    pub(super) fn pdf_evidence_import_failed(&mut self, action: &PendingAction, ticket: u64, reason: &str) {
        match action {
            PendingAction::PdfAssetImport { .. } | PendingAction::PdfEvidenceImport => {
                if self.pdf_reply_is_current(ticket) {
                    self.pdf_evidence_target = None;
                }
            }
            PendingAction::FigureEvidence => self.pdf_evidence_commit_failed(ticket, reason),
            _ => {}
        }
    }

    /// A PDF reply the window dropped because the open document changed (`process_replies`). A page read that came back
    /// is held for the figure it was started for, so its records are not lost. A commit answers for the set it carried.
    pub(super) fn pdf_evidence_reply_dropped(
        &mut self,
        action: &PendingAction,
        ticket: u64,
        result: &Result<RpcPayload, DltError>,
    ) {
        match (action, result) {
            (PendingAction::PdfEvidenceImport, Ok(RpcPayload::Json(reply))) => self.pdf_evidence_read(ticket, reply),
            (PendingAction::PdfAssetImport { .. } | PendingAction::PdfEvidenceImport, _) => {
                if self.pdf_reply_is_current(ticket) {
                    self.pdf_import_moved_on();
                }
            }
            (PendingAction::FigureEvidence, Ok(RpcPayload::Json(reply))) => self.pdf_evidence_committed(ticket, reply),
            (PendingAction::FigureEvidence, Ok(_)) => self.pdf_evidence_committed(ticket, &serde_json::Value::Null),
            (PendingAction::FigureEvidence, Err(error)) => {
                self.pdf_evidence_commit_failed(ticket, &super::user_error(error));
            }
            _ => {}
        }
    }

    /// Adds the imported table to the figure, and with `make_plot` a chart panel that plots it.
    fn add_csv_data(&mut self, make_plot: bool) {
        let Some(draft) = self.csv_draft.clone() else {
            return;
        };
        let Some(figure) = self.figure_document() else {
            return;
        };
        let Some((operations, intents)) = chart_change(&figure, &draft, make_plot) else {
            return;
        };
        self.apply_operations_for(operations, intents, PendingAction::FigureDataset);
    }

    /// The inspector part for figures: what the selected panel offers, then the tables and the
    /// evidence the figure holds.
    pub(super) fn draw_figure_details(&mut self, ui: &mut Ui, figure: &FigureDocument) {
        let selected = self.selected_node.clone();
        if let Some(panel) = selected.as_deref().and_then(|id| owning_panel(figure, id)) {
            if selected.as_deref() == Some(panel.id.as_str()) {
                let supported = self.feature_edit_supported(Feature::FigurePanels);
                self.draw_geometry_fields(ui, "figure-panel-geometry", supported, "패널 위치 적용");
            }
            match &panel.body {
                PanelBody::Diagram(diagram) => {
                    ui.separator();
                    self.draw_diagram_tools(ui, panel, diagram);
                }
                PanelBody::Plot(plot) if selected.as_deref() == Some(panel.id.as_str()) => {
                    ui.separator();
                    self.draw_plot_display(ui, panel, plot);
                }
                _ => {}
            }
        }
        self.draw_equation(ui, figure);
        self.r07_draw_figure(ui, figure);
        if !figure.datasets.is_empty() {
            ui.separator();
            self.draw_datasets(ui, figure);
        }
        if let Some(text) = self.pdf_coverage_text() {
            ui.separator();
            ui.label(RichText::new(text).color(theme::MUTED));
        }
        if !self.held_pdf_evidence.is_empty() {
            ui.separator();
            self.draw_held_pdf_evidence(ui);
        }
        if !figure.evidence.is_empty() {
            ui.separator();
            let can_review = self.evidence_review_available();
            let has_toggle = figure.evidence.iter().any(|evidence| review_target(evidence).is_some());
            if has_toggle && self.feature_edit_supported(Feature::Evidence) {
                if let Some(reason) = self.review_wait_reason() {
                    ui.label(RichText::new(reason).color(theme::MUTED));
                }
            }
            let mut review = None;
            for evidence in &figure.evidence {
                let page = evidence
                    .physical_page_index
                    .map(|index| format!("{}쪽", index.saturating_add(1)))
                    .unwrap_or_else(|| "원문".into());
                let target = review_target(evidence);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(page).color(theme::TEAL));
                    ui.label(review_state_label(&evidence.review_state));
                    if let Some(reviewed) = target {
                        let label = if reviewed { "검토 완료로 표시" } else { "검토 해제" };
                        if ui.add_enabled(can_review, egui::Button::new(label)).clicked() {
                            review = review_change(figure, &evidence.id, reviewed);
                        }
                    }
                });
                ui.label(super::truncate(&evidence.text, 180));
                ui.add_space(4.0);
            }
            if let Some(operation) = review {
                self.apply_evidence_review(operation);
            }
        }
    }

    /// The review buttons work when the host accepts evidence edits and `review_wait_reason` names no reason to wait.
    /// A change that succeeds clears the open cell draft (`committed`), so the draft also holds the buttons back.
    fn evidence_review_available(&self) -> bool {
        self.feature_edit_supported(Feature::Evidence) && self.review_wait_reason().is_none()
    }

    /// Why a review mark has to wait, in the words the window shows; `None` when it may be sent. Nothing is cleared
    /// while it waits: the mark, the figure and an open cell draft stay as they are.
    fn review_wait_reason(&self) -> Option<&'static str> {
        if self.dataset_edit.is_some() {
            Some("자료 값 편집을 닫은 뒤 검토 표시를 바꿀 수 있습니다")
        } else {
            figure_wait_reason(self.requests_on_their_way())
        }
    }

    /// Sends one review change against the revision the figure on screen was read at. If the figure has changed
    /// since, the host refuses the change (RevisionConflict) and nothing is applied.
    fn apply_evidence_review(&mut self, operation: Operation) {
        if !self.evidence_review_available() {
            return;
        }
        let Some(revision) = self.artifact.as_ref().map(|artifact| artifact.revision) else {
            return;
        };
        self.apply_operations_at(vec![operation], [Intent::Content].into_iter().collect(), revision);
    }

    /// The error-key switch of a plot panel. It is offered for a plot with error bars, or for one that shows the key already
    /// (so the key can be switched off). A plot without error bars has nothing to show.
    fn draw_plot_display(&mut self, ui: &mut Ui, panel: &FigurePanel, plot: &PlotSpec) {
        let mut shown = plot.display == Some(PlotDisplayMode::ErrorKey);
        let ready = self.plot_display_ready(plot);
        if ui.add_enabled(ready, egui::Checkbox::new(&mut shown, "오차 기준 표시")).changed() {
            self.invoke_ui("figure.plot_display", serde_json::json!({ "panel_id": panel.id, "on": shown }));
        }
    }

    /// Whether the error-key switch of this plot is offered now: the plot has error bars or already shows the key, the host
    /// accepts plot edits, and the window shows the open figure's current revision with nothing on its way. A switch that is
    /// not offered is disabled, so a click never ends in a refusal that the window does not show.
    pub(super) fn plot_display_ready(&self, plot: &PlotSpec) -> bool {
        let shown = plot.display == Some(PlotDisplayMode::ErrorKey);
        (plot.error.is_some() || shown)
            && self.feature_edit_supported(Feature::Plot)
            && self.figure_change_allowed()
            && self.shown_figure_revision().is_some()
    }

    /// The revision the window shows the open figure at, when the figure on screen, the open document and the read context
    /// all name the same figure at that revision. `None` while they disagree, for example while a commit or a read is on its way.
    pub(super) fn shown_figure_revision(&self) -> Option<u64> {
        let artifact = self.artifact.as_ref()?;
        let active = self.active.as_ref()?;
        let context = self.context.as_ref()?;
        let same_figure = artifact.id == active.id && context.artifact_id == artifact.id && artifact.kind == ArtifactKind::Figure;
        let same_revision = active.revision == artifact.revision && context.revision == artifact.revision;
        (same_figure && same_revision).then_some(artifact.revision)
    }

    /// Whether a change to the open figure may be sent now: no cell edit is open and no change, read or import is on its way.
    /// A change sent meanwhile would end the cell edit or be made against a figure the window no longer shows.
    fn figure_change_allowed(&self) -> bool {
        self.dataset_edit.is_none() && figure_wait_reason(self.requests_on_their_way()).is_none()
    }

    /// Sends the error-key mode of one plot panel as a design change against the revision the window shows. The change names
    /// the mode the window shows as `expected_display`, so the host refuses it if the plot has another mode by now. Nothing is
    /// sent when the window does not show the open figure's current revision. Returns whether a change was sent.
    pub(super) fn set_plot_display(&mut self, figure: &FigureDocument, panel_id: &str, error_key: bool) -> bool {
        let Some(revision) = self.shown_figure_revision() else {
            return false;
        };
        if !self.feature_edit_supported(Feature::Plot) || !self.figure_change_allowed() {
            return false;
        }
        let Some(operation) = plot_display_change(figure, panel_id, error_key) else {
            return false;
        };
        self.apply_operations_at(vec![operation], [Intent::Design].into_iter().collect(), revision);
        true
    }

    /// The held PDF evidence of the open figure, one set after another: where it was read, what it waits for or what the last
    /// attempt said, and what the user can do with it. The sets held for another figure are counted in one line instead.
    /// Nothing here writes to a figure or a document.
    pub(super) fn draw_held_pdf_evidence(&mut self, ui: &mut Ui) {
        let open = self.active.as_ref().map(|active| active.id.clone());
        let (mut retry, mut discard) = (None, None);
        let mut elsewhere = 0;
        for held in &self.held_pdf_evidence {
            if open.as_deref() != Some(held.target.artifact_id.as_str()) {
                elsewhere += held.records.len();
                continue;
            }
            let wait = self.held_pdf_wait(held);
            ui.label(
                RichText::new(format!("보류한 PDF 근거 {}개 · {}쪽", held.records.len(), held.target.page_index.saturating_add(1)))
                    .color(theme::TEAL),
            );
            let line = match wait {
                Some(reason) => reason,
                None => held.message.as_str(),
            };
            if !line.is_empty() {
                ui.label(RichText::new(line).color(theme::MUTED));
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(wait.is_none(), egui::Button::new("다시 넣기")).clicked() {
                    retry = Some(held.id.clone());
                }
                if ui.button("복사").clicked() {
                    ui.ctx().copy_text(held_pdf_copy_text(held));
                }
                if ui.add_enabled(held.ticket.is_none(), egui::Button::new("버리기")).clicked() {
                    discard = Some(held.id.clone());
                }
            });
            ui.add_space(4.0);
        }
        if elsewhere > 0 {
            ui.label(RichText::new(format!("다른 그림에 보류한 PDF 근거 {elsewhere}개가 있습니다. 그 그림을 열어 다시 넣거나 버리세요")).color(theme::MUTED));
        }
        if let Some(id) = retry {
            self.retry_held_pdf_evidence(&id);
        }
        if let Some(id) = discard {
            self.discard_held_pdf_evidence(&id);
        }
    }

    fn draw_diagram_tools(&mut self, ui: &mut Ui, panel: &FigurePanel, diagram: &Diagram) {
        let supported = self.feature_edit_supported(Feature::Diagram);
        let has_room = free_diagram_node_bounds(panel.bounds.size(), &diagram.nodes).is_some();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(supported && has_room, egui::Button::new("+ 도형"))
                .clicked()
            {
                self.add_figure_node(&panel.id, NodeKind::Rectangle);
            }
            if ui
                .add_enabled(supported && has_room, egui::Button::new("+ 원"))
                .clicked()
            {
                self.add_figure_node(&panel.id, NodeKind::Ellipse);
            }
        });
        if diagram.nodes.len() < 2 {
            return;
        }
        // The pickers keep node ids; drop a pick that belongs to another diagram.
        let known = |id: &Option<String>| {
            id.as_ref()
                .is_some_and(|id| diagram.nodes.iter().any(|node| &node.id == id))
        };
        if !known(&self.connect_from) {
            self.connect_from = None;
        }
        if !known(&self.connect_to) {
            self.connect_to = None;
        }
        let title_of = |id: &Option<String>| {
            id.as_ref()
                .and_then(|id| diagram.nodes.iter().enumerate().find(|(_, node)| &node.id == id))
                .map(|(index, node)| node_title(index, node))
                .unwrap_or_default()
        };
        // Combo boxes never wrap, so the row is sized to fit: two pickers, the arrow and the button.
        let combo_width = ((ui.available_width() - 70.0 - 4.0 * ui.spacing().item_spacing.x) / 2.0)
            .clamp(56.0, 140.0);
        ui.horizontal(|ui| {
            for (salt, choice) in [("connect-from", true), ("connect-to", false)] {
                let current = if choice { self.connect_from.clone() } else { self.connect_to.clone() };
                egui::ComboBox::from_id_salt(salt)
                    .width(combo_width)
                    .selected_text(title_of(&current))
                    .show_ui(ui, |ui| {
                        for (index, node) in diagram.nodes.iter().enumerate() {
                            let slot = if choice { &mut self.connect_from } else { &mut self.connect_to };
                            ui.selectable_value(slot, Some(node.id.clone()), node_title(index, node));
                        }
                    });
                if choice {
                    ui.label("→");
                }
            }
            let (from, to) = (self.connect_from.clone(), self.connect_to.clone());
            let linked = |from: &str, to: &str| {
                diagram
                    .edges
                    .iter()
                    .any(|edge| edge.from == from && edge.to == to)
            };
            let ready = match (&from, &to) {
                (Some(from), Some(to)) => from != to && !linked(from, to),
                _ => false,
            };
            if ui.add_enabled(supported && ready, egui::Button::new("연결")).clicked() {
                if let (Some(from), Some(to)) = (from, to) {
                    self.connect_figure_nodes(&panel.id, &from, &to);
                }
            }
        });
    }

    fn draw_datasets(&mut self, ui: &mut Ui, figure: &FigureDocument) {
        let can_edit = self.feature_edit_supported(Feature::Dataset);
        for dataset in &figure.datasets {
            egui::CollapsingHeader::new(format!(
                "{} · {}행",
                dataset_origin_label(&dataset.origin),
                dataset.rows.len()
            ))
            .id_salt(("dataset", dataset.id.as_str()))
            .show(ui, |ui| {
                ScrollArea::horizontal()
                    .id_salt(("dataset-scroll", dataset.id.as_str()))
                    .show(ui, |ui| {
                        Grid::new(("dataset-grid", dataset.id.as_str()))
                            .striped(true)
                            .show(ui, |ui| {
                                for column in &dataset.columns {
                                    let title = column
                                        .unit
                                        .as_ref()
                                        .map(|unit| format!("{} ({unit})", column.name))
                                        .unwrap_or_else(|| column.name.clone());
                                    ui.label(title);
                                }
                                ui.end_row();
                                for row in &dataset.rows {
                                    for column in &dataset.columns {
                                        let Some(cell) = row.cells.get(&column.id) else {
                                            ui.label("—");
                                            continue;
                                        };
                                        let color = match cell.state {
                                            designllm_contracts::CellState::Valid => theme::INK,
                                            _ => theme::RED,
                                        };
                                        let text = RichText::new(&cell.original).color(color);
                                        if !can_edit {
                                            ui.label(text);
                                        } else if ui.selectable_label(false, text).clicked() {
                                            self.dataset_edit = Some(DatasetCellEdit {
                                                dataset_id: dataset.id.clone(),
                                                version_hash: dataset.version_hash.clone(),
                                                row_id: row.id.clone(),
                                                column_id: column.id.clone(),
                                                original: cell.original.clone(),
                                                draft: cell.original.clone(),
                                            });
                                        }
                                    }
                                    ui.end_row();
                                }
                            });
                    });
            });
        }
        self.draw_dataset_cell_editor(ui, figure, can_edit);
    }

    fn draw_dataset_cell_editor(&mut self, ui: &mut Ui, figure: &FigureDocument, can_edit: bool) {
        let Some(mut edit) = self.dataset_edit.clone() else {
            return;
        };
        let current = figure.datasets.iter().any(|dataset| {
            dataset.id == edit.dataset_id && dataset.version_hash == edit.version_hash
        });
        if !current || !can_edit {
            // The value is not dropped silently: a changed value is kept as a copy-only draft, and the window says so.
            self.dataset_edit = None;
            if edit.draft != edit.original {
                let reason = if current { "자료 값을 지금은 적용할 수 없어 편집을 닫았습니다" } else { "자료가 다른 편집으로 바뀌었습니다" };
                self.keep_cell_as_copy(&edit);
                self.set_error(format!("{reason}. 편집하던 값은 초안 목록에 남겼습니다"));
            }
            return;
        }
        let mut apply = false;
        let mut close = false;
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("닫기").clicked() {
                    close = true;
                }
                if ui
                    .add_enabled(edit.draft != edit.original, egui::Button::new("값 적용"))
                    .clicked()
                {
                    apply = true;
                }
                let width = ui.available_width().max(48.0);
                ui.add(
                    TextEdit::singleline(&mut edit.draft)
                        .id(Id::new((
                            "dataset-cell",
                            edit.dataset_id.as_str(),
                            edit.row_id.as_str(),
                            edit.column_id.as_str(),
                        )))
                        .desired_width(width),
                );
            });
        });
        if close {
            self.dataset_edit = None;
        } else if apply {
            self.apply_dataset_cell(edit);
        } else {
            self.dataset_edit = Some(edit);
        }
    }

    pub(super) fn draw_figure_form(&mut self, ctx: &Context) {
        let Some(form) = self.figure_form else {
            return;
        };
        let title = match form {
            FigureForm::Chart => "차트 추가",
            FigureForm::Evidence => "근거 추가",
        };
        let mut open = true;
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_width(380.0);
                match form {
                    FigureForm::Chart => self.draw_chart_form(ui),
                    FigureForm::Evidence => self.draw_evidence_form(ui),
                }
            });
        if !open {
            self.figure_form = None;
            self.csv_draft = None;
        }
    }

    fn draw_evidence_form(&mut self, ui: &mut Ui) {
        let focus = std::mem::take(&mut self.focus_popup_field);
        ui.horizontal(|ui| {
            ui.label("쪽");
            ui.add(TextEdit::singleline(&mut self.pdf_page).desired_width(48.0));
        });
        let page_ok = self.pdf_page.trim().parse::<u32>().is_ok_and(|page| page > 0);
        let wait = figure_wait_reason(self.requests_on_their_way());
        let ready = page_ok && !self.pdf_path.trim().is_empty() && wait.is_none();
        if field_with_button(ui, &mut self.pdf_path, "PDF 파일 경로", "근거 추가", ready, focus) {
            self.import_pdf_evidence();
        }
        if let Some(reason) = wait {
            ui.label(RichText::new(reason).color(theme::MUTED));
        }
    }

    /// The chart window: the file path, then the declaration of the table's columns, then the chart.
    fn draw_chart_form(&mut self, ui: &mut Ui) {
        match self.csv_draft.as_ref().map(|draft| draft.step) {
            None => {
                let focus = std::mem::take(&mut self.focus_popup_field);
                let ready = !self.csv_path.trim().is_empty();
                if field_with_button(ui, &mut self.csv_path, "CSV 또는 TSV 파일 경로", "가져오기", ready, focus) {
                    self.import_csv_source();
                }
            }
            Some(CsvStep::Columns) => self.draw_columns_step(ui),
            Some(CsvStep::Chart) => self.draw_chart_step(ui),
        }
    }

    /// One row per header column: its type and, for a numeric column, its unit. The rows start from
    /// the types the import inferred; the confirmed declaration is what the table is read with.
    fn draw_columns_step(&mut self, ui: &mut Ui) {
        let Some(mut draft) = self.csv_draft.take() else {
            return;
        };
        ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
            Grid::new("csv-columns").num_columns(3).spacing([10.0, 8.0]).show(ui, |ui| {
                for (index, column) in draft.columns.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.set_width(120.0);
                        ui.add(egui::Label::new(column.name.as_str()).truncate());
                    });
                    egui::ComboBox::from_id_salt(("csv-column-type", index))
                        .width(84.0)
                        .selected_text(column_type_label(&column.data_type))
                        .show_ui(ui, |ui| {
                            for (data_type, label) in COLUMN_TYPES {
                                ui.selectable_value(&mut column.data_type, data_type.to_owned(), label);
                            }
                        });
                    // A unit belongs to numeric columns only; it goes away with the type.
                    let numeric = column.data_type == "numeric";
                    if !numeric {
                        column.unit.clear();
                    }
                    ui.add_enabled(
                        numeric,
                        TextEdit::singleline(&mut column.unit)
                            .hint_text(if numeric { "단위" } else { "" })
                            .desired_width(72.0),
                    );
                    ui.end_row();
                }
            });
        });
        ui.add_space(4.0);
        let waiting = self
            .pending
            .values()
            .any(|pending| matches!(pending.action, PendingAction::CsvDeclaredImport));
        let confirmed = ui.add_enabled(!waiting, egui::Button::new("확인")).clicked();
        let request = confirmed.then(|| draft.declaration_request());
        self.csv_draft = Some(draft);
        if let Some(request) = request {
            self.call("source.import_csv", request, PendingAction::CsvDeclaredImport);
        }
    }

    fn draw_chart_step(&mut self, ui: &mut Ui) {
        let Some(mut draft) = self.csv_draft.take() else {
            return;
        };
        let held = self.figure_document().and_then(|figure| {
            figure
                .datasets
                .into_iter()
                .find(|dataset| dataset.id == draft.dataset.id)
        });
        let dataset_ref = DatasetRef {
            dataset_id: draft.dataset.id.clone(),
            version_hash: held
                .as_ref()
                .map_or_else(|| draft.dataset.version_hash.clone(), |dataset| dataset.version_hash.clone()),
        };
        let before = (draft.plot_kind.clone(), draft.x_column.clone(), draft.y_column.clone());
        Grid::new("chart-form").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
            ui.label("종류");
            let kind_label = PLOT_KINDS
                .iter()
                .find(|(kind, _)| *kind == draft.plot_kind)
                .map_or("", |(_, label)| *label);
            egui::ComboBox::from_id_salt("chart-kind")
                .selected_text(kind_label)
                .show_ui(ui, |ui| {
                    for (kind, label) in PLOT_KINDS {
                        ui.selectable_value(&mut draft.plot_kind, kind.to_owned(), label);
                    }
                });
            ui.end_row();

            ui.label("X축");
            ui.horizontal(|ui| {
                let columns = x_columns(&draft.dataset, &draft.plot_kind)
                    .into_iter()
                    .map(|column| (column.id.clone(), column.name.clone()))
                    .collect::<Vec<_>>();
                let selected = columns
                    .iter()
                    .find(|(id, _)| *id == draft.x_column)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_default();
                egui::ComboBox::from_id_salt("chart-x")
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for (id, name) in columns {
                            ui.selectable_value(&mut draft.x_column, id, name);
                        }
                    });
                if draft.x_is_numeric() {
                    ui.add(TextEdit::singleline(&mut draft.x_unit).hint_text("단위").desired_width(72.0));
                }
            });
            ui.end_row();

            ui.label("Y축");
            ui.horizontal(|ui| {
                let columns = y_columns(&draft.dataset, &draft.x_column)
                    .into_iter()
                    .map(|column| (column.id.clone(), column.name.clone()))
                    .collect::<Vec<_>>();
                let selected = columns
                    .iter()
                    .find(|(id, _)| *id == draft.y_column)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_default();
                egui::ComboBox::from_id_salt("chart-y")
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for (id, name) in columns {
                            ui.selectable_value(&mut draft.y_column, id, name);
                        }
                    });
                ui.add(TextEdit::singleline(&mut draft.y_unit).hint_text("단위").desired_width(72.0));
            });
            ui.end_row();
        });
        if (before.0.as_str(), before.1.as_str()) != (draft.plot_kind.as_str(), draft.x_column.as_str()) {
            draft.repair_columns();
        }
        draft.sync_units(&before.1, &before.2);
        if let Some(reason) = draft.blocker() {
            ui.label(RichText::new(reason).color(theme::MUTED));
        }
        let can_plot = draft.plot_spec(dataset_ref).is_some();
        let (mut add_chart, mut add_data, mut regroup) = (false, false, false);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            add_chart = ui.add_enabled(can_plot, egui::Button::new("차트 추가")).clicked();
            add_data = ui
                .add_enabled(held.is_none(), egui::Button::new("자료만 추가"))
                .clicked();
            // A table the figure already holds keeps its declared types; only a new one can be re-declared.
            if held.is_none() {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    regroup = ui.button("열 형식").clicked();
                });
            }
        });
        if regroup {
            draft.reopen_declaration();
        }
        self.csv_draft = Some(draft);
        if add_chart {
            self.add_csv_data(true);
        } else if add_data {
            self.add_csv_data(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::{CellState, DataCell, DataRow, Rect, Size, EMU_PER_MM};

    fn figure(size: Size, panels: usize) -> FigureDocument {
        let mut figure = FigureDocument::new(size);
        for _ in 0..panels {
            let panel = new_diagram_panel(&figure);
            let operations = panel_additions(&figure, panel);
            apply_layout(&mut figure, &operations);
        }
        figure
    }

    /// Applies only what the layout rules decide (AddPanel and ArrangePanels), like the engine would.
    fn apply_layout(figure: &mut FigureDocument, operations: &[Operation]) {
        for operation in operations {
            match operation {
                Operation::AddPanel { index, panel } => figure.panels.insert(*index as usize, panel.clone()),
                Operation::ArrangePanels { bounds } => {
                    for panel in &mut figure.panels {
                        if let Some(rect) = bounds.get(&panel.id) {
                            panel.bounds = *rect;
                        }
                    }
                }
                other => panic!("unexpected operation {other:?}"),
            }
        }
    }

    fn overlaps(a: Rect, b: Rect) -> bool {
        a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
    }

    fn table(columns: &[(&str, &str)]) -> Dataset {
        Dataset {
            id: "data-1".into(),
            version_hash: "hash-1".into(),
            original_asset_hash: "a".repeat(64),
            origin: "user_experimental".into(),
            columns: columns
                .iter()
                .map(|(id, data_type)| DataColumn {
                    id: (*id).into(),
                    name: (*id).into(),
                    data_type: (*data_type).into(),
                    unit: None,
                })
                .collect(),
            rows: vec![DataRow {
                id: "r1".into(),
                cells: columns
                    .iter()
                    .map(|(id, _)| {
                        ((*id).to_owned(), DataCell { original: "1".into(), parsed_decimal: Some("1".into()), state: CellState::Valid })
                    })
                    .collect(),
            }],
            derived_from: None,
            transforms: Vec::new(),
        }
    }

    fn reference() -> DatasetRef {
        DatasetRef { dataset_id: "data-1".into(), version_hash: "hash-1".into() }
    }

    #[test]
    fn panels_are_lettered_like_a_multi_panel_figure() {
        assert_eq!(panel_label(0), "(a)");
        assert_eq!(panel_label(25), "(z)");
        assert_eq!(panel_label(26), "(27)");
    }

    #[test]
    fn a_wide_diagram_with_blocked_grid_slots_uses_the_remaining_space() {
        let mut figure = figure(Size { width: 180 * EMU_PER_MM, height: 48 * EMU_PER_MM }, 1);
        let id = figure.panels[0].id.clone();
        let Operation::AddDiagramNode { mut node, .. } = node_addition(&figure, &id, NodeKind::Rectangle).unwrap() else { panic!() };
        let size = figure.panels[0].bounds.size();
        node.bounds = Rect { x: 0, y: 0, width: 100 * EMU_PER_MM, height: size.height };
        let occupied = node.bounds;
        let PanelBody::Diagram(diagram) = &mut figure.panels[0].body else { panic!() };
        diagram.nodes.push(node);
        let Operation::AddDiagramNode { node, .. } = node_addition(&figure, &id, NodeKind::Rectangle).expect("room on the right") else { panic!() };
        assert!(node.bounds.x >= occupied.width && node.bounds.x + node.bounds.width <= size.width);
    }

    #[test]
    fn adding_panels_one_by_one_never_overlaps_and_keeps_every_panel_inside_the_figure() {
        for size in [Size { width: 170 * EMU_PER_MM, height: 120 * EMU_PER_MM }, Size::a4(), Size::slide()] {
            let mut figure = figure(size, 0);
            for count in 1..=9 {
                let operations = panel_additions(&figure, new_diagram_panel(&figure));
                assert!(matches!(operations[0], Operation::AddPanel { index, .. } if index as usize == count - 1));
                assert_eq!(operations.len(), if count == 1 { 1 } else { 2 });
                apply_layout(&mut figure, &operations);
                assert_eq!(figure.panels.len(), count);
                for (index, panel) in figure.panels.iter().enumerate() {
                    let b = panel.bounds;
                    assert!(b.x >= 0 && b.y >= 0 && b.width > 0 && b.height > 0);
                    assert!(b.x + b.width <= size.width && b.y + b.height <= size.height);
                    for other in &figure.panels[index + 1..] {
                        assert!(!overlaps(b, other.bounds), "{count} panels overlap on {size:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_new_panel_takes_the_last_slot_and_existing_panels_are_moved_to_theirs() {
        let size = Size { width: 170 * EMU_PER_MM, height: 120 * EMU_PER_MM };
        let existing = figure(size, 2);
        let operations = panel_additions(&existing, new_diagram_panel(&existing));
        let Operation::AddPanel { panel, .. } = &operations[0] else { panic!("first operation adds the panel") };
        assert_eq!(panel.bounds, tiled_panel_bounds(size, 2, 3));
        let Operation::ArrangePanels { bounds } = &operations[1] else { panic!("second operation arranges") };
        for (index, existing_panel) in existing.panels.iter().enumerate() {
            assert_eq!(bounds[&existing_panel.id], tiled_panel_bounds(size, index, 3));
        }
    }

    #[test]
    fn the_first_panel_of_a_figure_needs_no_rearranging() {
        let empty = figure(Size::a4(), 0);
        let operations = panel_additions(&empty, new_diagram_panel(&empty));
        assert_eq!(operations.len(), 1);
    }

    #[test]
    fn plot_kinds_accept_the_x_columns_the_figure_engine_accepts() {
        assert!(x_allowed("bar", "category") && !x_allowed("bar", "numeric") && !x_allowed("bar", "text"));
        assert!(x_allowed("scatter", "numeric") && !x_allowed("scatter", "category"));
        assert!(x_allowed("line", "numeric") && x_allowed("line", "category") && !x_allowed("line", "text"));
        assert!(!x_allowed("pie", "category"));
    }

    #[test]
    fn a_chart_draft_picks_valid_columns_and_repairs_them_when_the_kind_changes() {
        let dataset = table(&[("id", "category"), ("dose", "numeric"), ("response", "numeric"), ("n", "count")]);
        let mut draft = CsvImportDraft::new(dataset);
        assert_eq!(draft.plot_kind, "line");
        assert_eq!((draft.x_column.as_str(), draft.y_column.as_str()), ("id", "dose"));
        draft.plot_kind = "scatter".into();
        draft.repair_columns();
        assert_eq!((draft.x_column.as_str(), draft.y_column.as_str()), ("dose", "response"));
        draft.plot_kind = "bar".into();
        draft.repair_columns();
        assert_eq!(
            (draft.x_column.as_str(), draft.y_column.as_str()),
            ("id", "response"),
            "a still-valid y column is kept"
        );
    }

    #[test]
    fn a_table_without_a_usable_x_column_says_so_and_makes_no_chart() {
        // A text column cannot be an x axis; the user declares it a category in the column step.
        let mut labeled = CsvImportDraft::new(table(&[("group", "text"), ("value", "numeric")]));
        labeled.plot_kind = "bar".into();
        labeled.repair_columns();
        assert!(labeled.x_column.is_empty());
        assert!(labeled.blocker().is_some());
        labeled.y_unit = "mg".into();
        assert!(labeled.plot_spec(reference()).is_none());

        let counts_only = CsvImportDraft::new(table(&[("id", "category"), ("n", "count")]));
        assert!(counts_only.y_column.is_empty(), "counts are not plotted as values");
        assert!(counts_only.blocker().is_some());
    }

    #[test]
    fn a_chart_needs_the_axis_units_the_engine_requires() {
        let mut draft = CsvImportDraft::new(table(&[("id", "category"), ("value", "numeric")]));
        draft.plot_kind = "bar".into();
        draft.repair_columns();
        assert!(draft.plot_spec(reference()).is_none(), "the y unit is required");
        draft.y_unit = "  mg/L ".into();
        let plot = draft.plot_spec(reference()).expect("category x needs no x unit");
        assert_eq!(plot.kind, "bar");
        assert_eq!(plot.y_axis.unit.as_deref(), Some("mg/L"));
        assert_eq!(plot.x_axis.unit, None);
        assert_eq!((plot.x_column.as_str(), plot.y_columns.as_slice()), ("id", ["value".to_owned()].as_slice()));

        let mut numeric_x = CsvImportDraft::new(table(&[("dose", "numeric"), ("response", "numeric")]));
        numeric_x.plot_kind = "scatter".into();
        numeric_x.repair_columns();
        numeric_x.y_unit = "%".into();
        assert!(numeric_x.plot_spec(reference()).is_none(), "a numeric x axis needs its own unit");
        numeric_x.x_unit = "mg".into();
        let plot = numeric_x.plot_spec(reference()).expect("both units present");
        assert_eq!(plot.x_axis.unit.as_deref(), Some("mg"));
        assert_eq!(plot.x_axis.label, "dose");
        assert_eq!(plot.y_axis.label, "response");
        assert_eq!(plot.dataset_ref, reference());
    }

    #[test]
    fn a_table_read_with_inferred_types_asks_for_declarations_that_start_from_them() {
        let draft = CsvImportDraft::to_declare(table(&[("id", "category"), ("dose", "numeric"), ("group", "text")]));
        assert_eq!(draft.step, CsvStep::Columns);
        let declared: Vec<(&str, &str, &str)> = draft
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.data_type.as_str(), column.unit.as_str()))
            .collect();
        assert_eq!(declared, [("id", "category", ""), ("dose", "numeric", ""), ("group", "text", "")]);
        // The chart is not described until the columns are declared.
        assert!(draft.x_column.is_empty() && draft.y_column.is_empty());
    }

    #[test]
    fn the_declaration_request_names_every_column_once_and_sends_a_unit_only_where_the_host_accepts_one() {
        let mut draft = CsvImportDraft::to_declare(table(&[("group", "text"), ("value", "numeric"), ("n", "count")]));
        draft.columns[0].data_type = "category".into();
        draft.columns[1].unit = "  mg/L ".into();
        draft.columns[2].unit = "kg".into(); // typed before the type changed: never sent
        let request = draft.declaration_request();
        assert_eq!(request["sha256"], "a".repeat(64));
        assert_eq!(
            request["columns"],
            serde_json::json!([
                { "name": "group", "data_type": "category" },
                { "name": "value", "data_type": "numeric", "unit": "mg/L" },
                { "name": "n", "data_type": "count" },
            ])
        );

        // A numeric column whose unit was left empty sends none: the host refuses an empty unit.
        draft.columns[1].unit = "   ".into();
        assert_eq!(
            draft.declaration_request()["columns"][1],
            serde_json::json!({ "name": "value", "data_type": "numeric" })
        );
    }

    #[test]
    fn the_declared_table_starts_the_chart_with_its_units_and_can_be_declared_again() {
        let mut draft = CsvImportDraft::to_declare(table(&[("group", "text"), ("value", "numeric")]));
        let mut declared = table(&[("group", "category"), ("value", "numeric")]);
        declared.columns[1].unit = Some("mg/L".into());
        draft.adopt(declared);
        assert_eq!(draft.step, CsvStep::Chart);
        assert_eq!((draft.x_column.as_str(), draft.y_column.as_str()), ("group", "value"));
        assert_eq!(draft.y_unit, "mg/L", "the declared unit is offered as the axis unit");
        draft.plot_kind = "bar".into();
        draft.repair_columns();
        assert!(draft.plot_spec(reference()).is_some(), "category on x, a numeric column with its unit on y");

        draft.reopen_declaration();
        assert_eq!(draft.step, CsvStep::Columns);
        let declared: Vec<(&str, &str, &str)> = draft
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.data_type.as_str(), column.unit.as_str()))
            .collect();
        assert_eq!(declared, [("group", "category", ""), ("value", "numeric", "mg/L")]);
    }

    #[test]
    fn switching_an_axis_to_another_column_offers_that_columns_unit_and_keeps_typed_units_otherwise() {
        let mut dataset = table(&[("id", "category"), ("a", "numeric"), ("b", "numeric")]);
        dataset.columns[1].unit = Some("mg".into());
        dataset.columns[2].unit = Some("%".into());
        let mut draft = CsvImportDraft::new(dataset);
        assert_eq!((draft.y_column.as_str(), draft.y_unit.as_str()), ("a", "mg"));

        draft.y_unit = "g".into(); // typed by hand while the column stays
        draft.sync_units("id", "a");
        assert_eq!(draft.y_unit, "g");

        let before = draft.y_column.clone();
        draft.y_column = "b".into();
        draft.sync_units("id", &before);
        assert_eq!(draft.y_unit, "%");
    }

    #[test]
    fn clickable_regions_are_the_panels_then_their_nodes_in_figure_coordinates() {
        let mut figure = figure(Size::a4(), 2);
        let node = Node {
            id: "node-1".into(),
            kind: NodeKind::Rectangle,
            bounds: Rect { x: 100, y: 200, width: 300, height: 400 },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: BTreeMap::new(),
            ext: Default::default(),
        };
        let panel_origin = figure.panels[1].bounds;
        if let PanelBody::Diagram(diagram) = &mut figure.panels[1].body {
            diagram.nodes.push(node);
        }
        let regions = figure_regions(&figure);
        let ids: Vec<&str> = regions.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, [figure.panels[0].id.as_str(), figure.panels[1].id.as_str(), "node-1"]);
        assert_eq!(regions[0].1, figure.panels[0].bounds);
        assert_eq!(
            regions[2].1,
            Rect { x: panel_origin.x + 100, y: panel_origin.y + 200, width: 300, height: 400 },
            "a node is placed relative to its panel and clicked at its figure position"
        );
    }

    #[test]
    fn selections_resolve_to_the_panel_that_owns_them() {
        let mut figure = figure(Size::a4(), 2);
        let node = Node {
            id: "node-1".into(),
            kind: NodeKind::Rectangle,
            bounds: Rect { x: 0, y: 0, width: 1, height: 1 },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: BTreeMap::new(),
            ext: Default::default(),
        };
        if let PanelBody::Diagram(diagram) = &mut figure.panels[1].body {
            diagram.nodes.push(node);
        }
        let second = figure.panels[1].id.clone();
        assert_eq!(owning_panel(&figure, &second).map(|p| p.id.clone()), Some(second.clone()));
        assert_eq!(owning_panel(&figure, "node-1").map(|p| p.id.clone()), Some(second));
        assert!(owning_panel(&figure, "missing").is_none());
    }

    #[test]
    fn a_cell_value_for_a_table_that_changed_under_its_editor_is_kept_as_a_copy_and_the_user_is_told() {
        use crate::{app::drafts::Body, rpc::RpcWorker};
        let (rpc, _replies, _work) = RpcWorker::detached();
        let mut app = DesktopApp::new(rpc, "작업공간".into(), None, None);
        let mut doc = figure(Size::a4(), 0);
        doc.datasets.push(table(&[("id", "text"), ("value", "number")]));
        app.dataset_edit = Some(DatasetCellEdit {
            dataset_id: "data-1".into(),
            version_hash: "hash-before".into(),
            row_id: "r1".into(),
            column_id: "value".into(),
            original: "1".into(),
            draft: "2".into(),
        });
        let ctx = Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.draw_dataset_cell_editor(ui, &doc, true));
        });
        assert!(app.dataset_edit.is_none(), "the editor closes");
        let kept = app.drafts.recovered();
        assert_eq!(kept.len(), 1, "the value is kept as a copy");
        assert!(matches!(&kept[0].body, Body::Cell { draft, .. } if draft == "2"));
        assert!(
            app.notices.current(Instant::now()).is_some_and(|notice| notice.text.contains("초안 목록에 남겼습니다")),
            "the user is told where the value went"
        );
    }
}

#[cfg(test)]
#[path = "figure_evidence_review_tests.rs"]
mod evidence_review_tests;

#[cfg(test)]
#[path = "figure_evidence_reimport_tests.rs"]
mod evidence_reimport_tests;
