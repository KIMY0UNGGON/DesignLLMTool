//! R07 figure editing on the desktop: the origin and evidence of a diagram element, the common legend and the visual review
//! of a figure, and the crop, rotation and calibration of an image panel. Each control sends one of the figure commands
//! (`figure.diagram_provenance`, `figure.common_legend`, `figure.visual_review`, `figure.image_transform`,
//! `figure.image_calibration`). A command reads the current values from the figure and names them as `expected`, so the
//! engine refuses a change whose base has moved. The checks here repeat the engine's own rules (figures `ops.rs` and
//! `validate.rs`): an invalid value is refused inline, and nothing is sent.

use std::hash::Hash;

use designllm_contracts::{
    ArtifactBody, Calibration, CommonLegend, CommonLegendItem, CommonLegendPlacement, Diagnostic, Diagram, Evidence,
    FigureDocument, FigureImage, FigurePanel, Id, Operation, PanelBody, PixelRect, VisualReview, VisualReviewState,
};
use designllm_contracts::figure_rules::{
    DECIMAL_MAX_DIGITS, DIAGRAM_ORIGINS, IMAGE_ROTATION_LIMIT_MILLIDEG, MODEL_PROPOSAL_ORIGIN, PAPER_EVIDENCE_ORIGIN,
    USER_AUTHORED_ORIGIN,
};
use eframe::egui::{self, ComboBox, RichText, TextEdit, Ui};
use serde_json::{json, Value};

use super::{figure_tools::owning_panel, messages::diagnostic_message, truncate, DesktopApp};
use crate::{commands::CommandSpec, theme};

/// The origins a diagram element may take, in the order the window offers them. The codes are the contract's
/// (`figure_rules`); this table adds only the labels.
const ORIGINS: [(&str, &str); 3] = [
    (PAPER_EVIDENCE_ORIGIN, "논문 근거"),
    (USER_AUTHORED_ORIGIN, "직접 작성"),
    (MODEL_PROPOSAL_ORIGIN, "모델 제안"),
];
const PLACEMENTS: [(CommonLegendPlacement, &str); 3] =
    [(CommonLegendPlacement::Bottom, "아래"), (CommonLegendPlacement::Top, "위"), (CommonLegendPlacement::Right, "오른쪽")];
const REVIEW_STATES: [(VisualReviewState, &str); 3] =
    [(VisualReviewState::Pending, "검토 대기"), (VisualReviewState::Approved, "승인"), (VisualReviewState::NeedsRevision, "수정 필요")];
const NO_CHANGE: &str = "바뀐 내용이 없습니다";

/// The change of one R07 control: `Ok(None)` when it changes nothing, `Err` with the words for input the engine would refuse.
type Change = Result<Option<Operation>, String>;

/// The provenance change of diagram element `element_id` of panel `panel_id`. `origin` None clears a node's origin; a connector
/// always keeps one. `evidence_ids` must be evidence the figure holds.
pub(super) fn provenance_change(
    figure: &FigureDocument,
    panel_id: &str,
    element_id: &str,
    origin: Option<&str>,
    evidence_ids: &[Id],
) -> Change {
    let panel = figure.panels.iter().find(|panel| panel.id == panel_id).ok_or("패널을 찾지 못했습니다")?;
    let PanelBody::Diagram(diagram) = &panel.body else {
        return Err("구조도 패널이 아닙니다".into());
    };
    let (connector, expected_origin, expected_evidence_ids) =
        element_provenance(diagram, element_id).ok_or("도형이나 연결선을 찾지 못했습니다")?;
    match origin {
        Some(value) if !DIAGRAM_ORIGINS.contains(&value) => return Err("출처를 고르세요".into()),
        None if connector => return Err("연결선에는 출처가 있어야 합니다".into()),
        _ => {}
    }
    let evidence = distinct(evidence_ids);
    if evidence.iter().any(|id| !figure.evidence.iter().any(|item| &item.id == id)) {
        return Err("근거를 찾지 못했습니다".into());
    }
    if origin == Some(PAPER_EVIDENCE_ORIGIN) && evidence.is_empty() {
        return Err("논문 근거는 근거를 하나 이상 골라야 합니다".into());
    }
    if origin == expected_origin.as_deref() && evidence == expected_evidence_ids {
        return Ok(None);
    }
    Ok(Some(Operation::SetDiagramProvenance {
        panel_id: panel.id.clone(),
        element_id: element_id.to_owned(),
        expected_origin,
        expected_evidence_ids,
        origin: origin.map(str::to_owned),
        evidence_ids: evidence,
    }))
}

/// The common legend as it should be after the change (`None` removes it).
pub(super) fn legend_change(figure: &FigureDocument, legend: Option<CommonLegend>) -> Change {
    let legend = legend.map(checked_legend).transpose()?;
    if legend == figure.common_legend {
        return Ok(None);
    }
    Ok(Some(Operation::SetCommonLegend { expected: figure.common_legend.clone(), legend }))
}

/// The visual review as it should be after the change (`None` removes it). The engine records the approval digest itself.
pub(super) fn review_change(figure: &FigureDocument, review: Option<VisualReview>) -> Change {
    let review = review.map(|review| VisualReview {
        state: review.state,
        note: review.note.filter(|note| !note.trim().is_empty()),
        approved_digest: None,
    });
    if review.is_none() && figure.visual_review.is_none() {
        return Ok(None);
    }
    Ok(Some(Operation::SetFigureVisualReview { expected: figure.visual_review.clone(), review }))
}

/// The crop and rotation of image panel `panel_id` after the change.
pub(super) fn transform_change(figure: &FigureDocument, panel_id: &str, crop: Option<PixelRect>, rotation_millidegrees: i32) -> Change {
    let (panel, image) = image_panel(figure, panel_id)?;
    if let Some(rect) = crop {
        if !inside_image(rect, image) {
            return Err("자르기 영역은 그림 안에 있어야 하며 비어 있으면 안 됩니다".into());
        }
    }
    if rotation_millidegrees.unsigned_abs() >= IMAGE_ROTATION_LIMIT_MILLIDEG.unsigned_abs() {
        return Err("회전은 -360도보다 크고 360도보다 작아야 합니다".into());
    }
    if crop == image.crop_pixel_rect && rotation_millidegrees == image.rotation_millidegrees {
        return Ok(None);
    }
    Ok(Some(Operation::SetFigureImageTransform {
        panel_id: panel.id.clone(),
        expected_asset_sha256: image.asset.sha256.clone(),
        expected_crop_pixel_rect: image.crop_pixel_rect,
        expected_rotation_millidegrees: image.rotation_millidegrees,
        crop_pixel_rect: crop,
        rotation_millidegrees,
    }))
}

/// The measurement calibration of image panel `panel_id` after the change (`None` removes it).
pub(super) fn calibration_change(figure: &FigureDocument, panel_id: &str, calibration: Option<Calibration>) -> Change {
    let (panel, image) = image_panel(figure, panel_id)?;
    let calibration = calibration.map(|value| checked_calibration(figure, image, value)).transpose()?;
    if calibration == image.calibration {
        return Ok(None);
    }
    Ok(Some(Operation::SetFigureImageCalibration {
        panel_id: panel.id.clone(),
        expected_asset_sha256: image.asset.sha256.clone(),
        expected_calibration: image.calibration.clone(),
        calibration,
    }))
}

/// The words for an approval the figure has outgrown: an approved review while the window's figure reports
/// `VISUAL_REVIEW_STALE`. `None` for every other review.
pub(super) fn review_status_message(review: Option<&VisualReview>, diagnostics: &[Diagnostic]) -> Option<&'static str> {
    let review = review?;
    let stale = review.state == VisualReviewState::Approved && diagnostics.iter().any(|diagnostic| diagnostic.code == designllm_contracts::diag_codes::VISUAL_REVIEW_STALE);
    stale.then(|| diagnostic_message(designllm_contracts::diag_codes::VISUAL_REVIEW_STALE)).flatten()
}

/// A rotation typed in degrees (at most three decimals), as millidegrees within the engine's bound.
pub(super) fn millidegrees(text: &str) -> Result<i32, String> {
    let text = text.trim();
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = body.split_once('.').unwrap_or((body, ""));
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if (whole.is_empty() && fraction.is_empty()) || whole.len() > 3 || fraction.len() > 3 || !digits(whole) || !digits(fraction) {
        return Err("회전은 도 단위 숫자로 적어 주세요. 소수는 셋째 자리까지입니다".into());
    }
    let whole: i32 = whole.parse().unwrap_or(0);
    let fraction: i32 = format!("{fraction:0<3}").parse().unwrap_or(0);
    let magnitude = whole * 1000 + fraction;
    if magnitude >= IMAGE_ROTATION_LIMIT_MILLIDEG {
        return Err("회전은 -360도보다 크고 360도보다 작아야 합니다".into());
    }
    Ok(if negative { -magnitude } else { magnitude })
}

/// A diagram element's origin and evidence now, and whether it is a connector (a connector always has an origin).
fn element_provenance(diagram: &Diagram, element_id: &str) -> Option<(bool, Option<String>, Vec<Id>)> {
    if let Some(edge) = diagram.edges.iter().find(|edge| edge.id == element_id) {
        return Some((true, Some(edge.origin.clone()), edge.evidence_ids.clone()));
    }
    diagram.nodes.iter().any(|node| node.id == element_id).then(|| {
        let origin = diagram.node_origins.get(element_id).cloned();
        let evidence = diagram.evidence_ids.get(element_id).cloned().unwrap_or_default();
        (false, origin, evidence)
    })
}

fn image_panel<'a>(figure: &'a FigureDocument, panel_id: &str) -> Result<(&'a FigurePanel, &'a FigureImage), String> {
    let panel = figure.panels.iter().find(|panel| panel.id == panel_id).ok_or("패널을 찾지 못했습니다")?;
    match &panel.body {
        PanelBody::Image(image) => Ok((panel, image)),
        _ => Err("그림 패널을 고르세요".into()),
    }
}

/// Whether a crop lies inside the image and is not empty (validate.rs, IMAGE_CROP_INVALID).
fn inside_image(rect: PixelRect, image: &FigureImage) -> bool {
    rect.width > 0
        && rect.height > 0
        && rect.x.checked_add(rect.width).is_some_and(|right| right <= image.pixel_width)
        && rect.y.checked_add(rect.height).is_some_and(|bottom| bottom <= image.pixel_height)
}

fn checked_legend(mut legend: CommonLegend) -> Result<CommonLegend, String> {
    if legend.items.is_empty() {
        return Err("범례에는 항목이 하나 이상 있어야 합니다".into());
    }
    if legend.items.iter().any(|item| item.label.trim().is_empty()) {
        return Err("범례 항목의 이름을 적어 주세요".into());
    }
    if legend.items.iter().any(|item| !valid_colour(&item.color)) {
        return Err("색은 #RRGGBB 또는 #RRGGBBAA 형식이어야 합니다".into());
    }
    legend.title = legend.title.filter(|title| !title.trim().is_empty());
    Ok(legend)
}

/// `#RRGGBB` or `#RRGGBBAA` (figures `validate.rs`, valid_color).
fn valid_colour(value: &str) -> bool {
    value.strip_prefix('#').is_some_and(|hex| (hex.len() == 6 || hex.len() == 8) && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn checked_calibration(figure: &FigureDocument, image: &FigureImage, value: Calibration) -> Result<Calibration, String> {
    if image.origin == "generated_illustration" {
        return Err("생성 삽화에는 측정 보정을 둘 수 없습니다".into());
    }
    let length = value.length.trim();
    let pixel_span = value.pixel_span.trim();
    if !positive_decimal(length) || !positive_decimal(pixel_span) {
        return Err("길이와 픽셀 간격은 0보다 큰 십진수여야 합니다".into());
    }
    let unit = value.unit.trim();
    if unit.is_empty() {
        return Err("단위를 적어 주세요".into());
    }
    if let Some(id) = &value.evidence_id {
        if !figure.evidence.iter().any(|item| &item.id == id) {
            return Err("근거를 찾지 못했습니다".into());
        }
    }
    Ok(Calibration {
        length: length.to_owned(),
        unit: unit.to_owned(),
        pixel_span: pixel_span.to_owned(),
        evidence_id: value.evidence_id,
    })
}

/// A decimal the engine reads as positive (figures `decimal.rs`, and CALIBRATION_INVALID): an optional plus sign, ASCII digits
/// with at most one point, at most `DECIMAL_MAX_DIGITS` digits, and some digit above zero.
fn positive_decimal(text: &str) -> bool {
    let body = text.strip_prefix('+').unwrap_or(text);
    let (whole, fraction) = body.split_once('.').unwrap_or((body, ""));
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    digits(whole) && digits(fraction) && whole.len() + fraction.len() <= DECIMAL_MAX_DIGITS && body.bytes().any(|byte| matches!(byte, b'1'..=b'9'))
}

/// The ids in their first order, without repeats.
fn distinct(ids: &[Id]) -> Vec<Id> {
    let mut kept: Vec<Id> = Vec::with_capacity(ids.len());
    for id in ids {
        if !kept.contains(id) {
            kept.push(id.clone());
        }
    }
    kept
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn string_list(args: &Value, key: &str) -> Result<Vec<Id>, String> {
    let values = args.get(key).and_then(Value::as_array).ok_or_else(|| format!("{key}는 문자열 배열이어야 합니다"))?;
    values
        .iter()
        .map(|value| value.as_str().map(str::to_owned).ok_or_else(|| "근거 번호는 문자열이어야 합니다".to_owned()))
        .collect()
}

/// The panel of a diagram element: the one the arguments name, else the one that holds the element.
fn diagram_owner(figure: &FigureDocument, args: &Value, element_id: &str) -> Result<String, String> {
    if let Some(id) = str_arg(args, "panel_id") {
        return Ok(id.to_owned());
    }
    owning_panel(figure, element_id).map(|panel| panel.id.clone()).ok_or_else(|| "도형을 찾지 못했습니다".to_owned())
}

fn legend_from_json(value: &Value) -> Result<CommonLegend, String> {
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .ok_or("범례 항목을 적어 주세요")?
        .iter()
        .map(|item| -> Result<CommonLegendItem, String> {
            let label = str_arg(item, "label").ok_or("범례 항목의 이름을 적어 주세요")?;
            let color = str_arg(item, "color").ok_or("범례 항목의 색을 적어 주세요")?;
            Ok(CommonLegendItem { label: label.to_owned(), color: color.to_owned() })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let placement = match value.get("placement") {
        Some(name) => serde_json::from_value(name.clone()).map_err(|_| "범례 위치를 고르세요".to_owned())?,
        None => CommonLegendPlacement::default(),
    };
    Ok(CommonLegend { items, placement, title: str_arg(value, "title").map(str::to_owned) })
}

fn review_from_json(value: &Value) -> Result<VisualReview, String> {
    let state = serde_json::from_value(value.get("state").cloned().unwrap_or(Value::Null)).map_err(|_| "검토 상태를 고르세요".to_owned())?;
    Ok(VisualReview { state, note: str_arg(value, "note").map(str::to_owned), approved_digest: None })
}

fn crop_from_json(value: &Value) -> Result<PixelRect, String> {
    let pixels = |key: &str| {
        value.get(key).and_then(Value::as_u64).and_then(|number| u32::try_from(number).ok()).ok_or("자르기 영역의 값을 확인해 주세요")
    };
    Ok(PixelRect { x: pixels("x")?, y: pixels("y")?, width: pixels("width")?, height: pixels("height")? })
}

fn calibration_from_json(value: &Value) -> Result<Calibration, String> {
    let text = |key: &str| str_arg(value, key).map(str::to_owned).ok_or("측정 보정 값을 적어 주세요");
    Ok(Calibration {
        length: text("length")?,
        unit: text("unit")?,
        pixel_span: text("pixel_span")?,
        evidence_id: str_arg(value, "evidence_id").map(str::to_owned),
    })
}

/// A legend, review, crop or calibration as the command arguments name it.
fn legend_value(legend: &CommonLegend) -> Value {
    let items: Vec<Value> = legend.items.iter().map(|item| json!({ "label": item.label, "color": item.color })).collect();
    json!({ "items": items, "placement": placement_name(legend.placement), "title": legend.title })
}

fn review_value(review: &VisualReview) -> Value {
    json!({ "state": state_name(review.state), "note": review.note })
}

fn crop_value(rect: PixelRect) -> Value {
    json!({ "x": rect.x, "y": rect.y, "width": rect.width, "height": rect.height })
}

fn calibration_value(calibration: &Calibration) -> Value {
    json!({
        "length": calibration.length,
        "unit": calibration.unit,
        "pixel_span": calibration.pixel_span,
        "evidence_id": calibration.evidence_id,
    })
}

fn placement_name(placement: CommonLegendPlacement) -> &'static str {
    match placement {
        CommonLegendPlacement::Bottom => "bottom",
        CommonLegendPlacement::Top => "top",
        CommonLegendPlacement::Right => "right",
    }
}

fn state_name(state: VisualReviewState) -> &'static str {
    match state {
        VisualReviewState::Pending => "pending",
        VisualReviewState::Approved => "approved",
        VisualReviewState::NeedsRevision => "needs_revision",
    }
}

fn label_of<T: Copy + PartialEq>(table: &[(T, &'static str)], value: T) -> &'static str {
    table.iter().find(|(known, _)| *known == value).map_or("", |(_, label)| *label)
}

fn origin_label(origin: Option<&str>) -> &'static str {
    match origin {
        None => "출처 없음",
        Some(value) => ORIGINS.iter().find(|(known, _)| *known == value).map_or("", |(_, label)| *label),
    }
}

fn evidence_label(item: &Evidence) -> String {
    let page = item.physical_page_index.map_or_else(|| "원문".to_owned(), |index| format!("{}쪽", index.saturating_add(1)));
    format!("{page} · {}", truncate(&item.text, 48))
}

fn evidence_text(figure: &FigureDocument, id: Option<&str>) -> String {
    id.and_then(|id| figure.evidence.iter().find(|item| item.id == id)).map_or_else(|| "근거 없음".to_owned(), evidence_label)
}

fn pixel(text: &str) -> Result<u32, String> {
    text.trim().parse::<u32>().map_err(|_| "자르기 값은 0 이상의 정수여야 합니다".to_owned())
}

/// The crop and rotation a transform draft names, as the operation takes them.
fn transform_request(draft: &TransformDraft) -> Result<(Option<PixelRect>, i32), String> {
    let crop = if draft.crop_on {
        Some(PixelRect { x: pixel(&draft.x)?, y: pixel(&draft.y)?, width: pixel(&draft.width)?, height: pixel(&draft.height)? })
    } else {
        None
    };
    Ok((crop, millidegrees(&draft.rotation)?))
}

fn degrees_text(millidegrees: i32) -> String {
    let sign = if millidegrees < 0 { "-" } else { "" };
    let magnitude = millidegrees.unsigned_abs();
    let (whole, fraction) = (magnitude / 1000, magnitude % 1000);
    if fraction == 0 {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{fraction:03}").trim_end_matches('0').to_owned()
    }
}

/// The egui memory key of one R07 draft: the section, the figure, and the part of the figure it edits (`()` for the legend and the
/// review, which belong to the whole figure). The figure is the artifact id, which names one figure. The element and panel ids are
/// the author's and can repeat across figures, and two figures can share a base (both without a legend, for example), so a draft
/// keyed without the figure would be shown on the other figure.
pub(super) fn draft_id(section: &str, figure_id: &str, part: impl Hash) -> egui::Id {
    egui::Id::new((section, figure_id, part))
}

// What the user has typed for one section, kept in egui's temporary memory. `base` is the value the draft started from; a
// draft whose base is no longer the figure's value starts again.

#[derive(Clone, PartialEq)]
pub(super) struct ProvenanceDraft {
    base: (Option<String>, Vec<Id>),
    pub(super) origin: Option<String>,
    evidence: Vec<Id>,
}

impl ProvenanceDraft {
    fn of(origin: Option<String>, evidence: Vec<Id>) -> Self {
        Self { base: (origin.clone(), evidence.clone()), origin, evidence }
    }
}

#[derive(Clone, PartialEq)]
pub(super) struct LegendDraft {
    base: Option<CommonLegend>,
    placement: CommonLegendPlacement,
    title: String,
    pub(super) items: Vec<(String, String)>,
}

impl LegendDraft {
    pub(super) fn of(base: Option<&CommonLegend>) -> Self {
        Self {
            base: base.cloned(),
            placement: base.map_or(CommonLegendPlacement::default(), |legend| legend.placement),
            title: base.and_then(|legend| legend.title.clone()).unwrap_or_default(),
            items: base.map_or_else(Vec::new, |legend| legend.items.iter().map(|item| (item.label.clone(), item.color.clone())).collect()),
        }
    }

    fn legend(&self) -> CommonLegend {
        CommonLegend {
            items: self.items.iter().map(|(label, color)| CommonLegendItem { label: label.clone(), color: color.clone() }).collect(),
            placement: self.placement,
            title: Some(self.title.clone()),
        }
    }
}

#[derive(Clone, PartialEq)]
pub(super) struct ReviewDraft {
    base: Option<VisualReview>,
    pub(super) state: VisualReviewState,
    note: String,
}

impl ReviewDraft {
    pub(super) fn of(base: Option<&VisualReview>) -> Self {
        Self {
            base: base.cloned(),
            state: base.map_or(VisualReviewState::Pending, |review| review.state),
            note: base.and_then(|review| review.note.clone()).unwrap_or_default(),
        }
    }

    fn review(&self) -> VisualReview {
        VisualReview { state: self.state, note: Some(self.note.clone()), approved_digest: None }
    }
}

#[derive(Clone, PartialEq)]
pub(super) struct TransformDraft {
    base: (Option<PixelRect>, i32),
    crop_on: bool,
    x: String,
    y: String,
    width: String,
    height: String,
    pub(super) rotation: String,
}

impl TransformDraft {
    fn of(image: &FigureImage) -> Self {
        let crop = image.crop_pixel_rect;
        let (x, y, width, height) = match crop {
            Some(rect) => (rect.x, rect.y, rect.width, rect.height),
            None => (0, 0, image.pixel_width, image.pixel_height),
        };
        Self {
            base: (crop, image.rotation_millidegrees),
            crop_on: crop.is_some(),
            x: x.to_string(),
            y: y.to_string(),
            width: width.to_string(),
            height: height.to_string(),
            rotation: degrees_text(image.rotation_millidegrees),
        }
    }
}

#[derive(Clone, PartialEq)]
pub(super) struct CalibrationDraft {
    base: Option<Calibration>,
    pub(super) length: String,
    pixel_span: String,
    unit: String,
    evidence: Option<Id>,
}

impl CalibrationDraft {
    fn of(base: Option<&Calibration>) -> Self {
        Self {
            base: base.cloned(),
            length: base.map(|value| value.length.clone()).unwrap_or_default(),
            pixel_span: base.map(|value| value.pixel_span.clone()).unwrap_or_default(),
            unit: base.map(|value| value.unit.clone()).unwrap_or_default(),
            evidence: base.and_then(|value| value.evidence_id.clone()),
        }
    }

    fn calibration(&self) -> Calibration {
        Calibration {
            length: self.length.clone(),
            unit: self.unit.clone(),
            pixel_span: self.pixel_span.clone(),
            evidence_id: self.evidence.clone(),
        }
    }
}

impl DesktopApp {
    /// Runs one R07 figure command; `None` for an id this module does not serve.
    pub(super) fn dispatch_figure_r07(&mut self, spec: &CommandSpec, args: &Value) -> Option<Result<(), String>> {
        let result = match spec.id {
            "figure.diagram_provenance" => self.r07_provenance(args),
            "figure.common_legend" => self.r07_legend(args),
            "figure.visual_review" => self.r07_review(args),
            "figure.image_transform" => self.r07_transform(args),
            "figure.image_calibration" => self.r07_calibration(args),
            _ => return None,
        };
        Some(result)
    }

    /// The figure the window holds, borrowed: the change copies only what it names.
    fn r07_figure(&self) -> Result<&FigureDocument, String> {
        match self.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::Figure(figure)) => Ok(figure),
            _ => Err("논문 그림이 열려 있지 않습니다".into()),
        }
    }

    /// The id of the figure the window holds (its artifact id); empty when no artifact is open.
    fn r07_figure_id(&self) -> &str {
        self.artifact.as_ref().map_or("", |artifact| artifact.id.as_str())
    }

    /// The image panel a command means: the one it names, else the panel that owns the selection.
    fn r07_image_panel(&self, figure: &FigureDocument, args: &Value) -> Result<String, String> {
        if let Some(id) = str_arg(args, "panel_id") {
            return Ok(id.to_owned());
        }
        self.selected_node
            .as_deref()
            .and_then(|id| owning_panel(figure, id))
            .map(|panel| panel.id.clone())
            .ok_or_else(|| "그림 패널을 고르세요".to_owned())
    }

    /// The diagnostics of the figure revision the window shows; empty while the render of that revision is not in.
    fn r07_diagnostics(&self) -> &[Diagnostic] {
        match (self.render.as_ref(), self.active.as_ref()) {
            (Some(snapshot), Some(active)) if snapshot.artifact_id == active.id && snapshot.revision == active.revision => {
                snapshot.scene.diagnostics.as_slice()
            }
            _ => &[],
        }
    }

    /// Sends the change of an R07 command. A refusal of the input, or a change that changes nothing, sends nothing.
    fn r07_send(&mut self, change: Change) -> Result<(), String> {
        let operation = change?.ok_or_else(|| NO_CHANGE.to_owned())?;
        if self.context.is_none() {
            return Err("작업물을 아직 읽는 중입니다".into());
        }
        let intents = [operation.intent()].into_iter().collect();
        self.apply_operations(vec![operation], intents);
        Ok(())
    }

    fn r07_provenance(&mut self, args: &Value) -> Result<(), String> {
        let element = str_arg(args, "element_id").ok_or("도형을 고르세요")?;
        let figure = self.r07_figure()?;
        let panel = diagram_owner(figure, args, element)?;
        let evidence = string_list(args, "evidence_ids")?;
        let change = provenance_change(figure, &panel, element, str_arg(args, "origin"), &evidence);
        self.r07_send(change)
    }

    fn r07_legend(&mut self, args: &Value) -> Result<(), String> {
        let legend = match args.get("legend") {
            Some(Value::Null) => None,
            Some(value) => Some(legend_from_json(value)?),
            None => return Err("범례 내용이 없습니다".into()),
        };
        let change = legend_change(self.r07_figure()?, legend);
        self.r07_send(change)
    }

    fn r07_review(&mut self, args: &Value) -> Result<(), String> {
        let change = self.r07_review_change(args);
        self.r07_send(change)
    }

    /// The change `figure.visual_review` makes with `args`; the agent channel's review gate judges exactly this (49w).
    pub(super) fn r07_review_change(&self, args: &Value) -> Change {
        let review = match args.get("review") {
            Some(Value::Null) => None,
            Some(value) => Some(review_from_json(value)?),
            None => return Err("시각 검토 내용이 없습니다".into()),
        };
        review_change(self.r07_figure()?, review)
    }

    fn r07_transform(&mut self, args: &Value) -> Result<(), String> {
        let crop = match args.get("crop") {
            Some(Value::Null) => None,
            Some(value) => Some(crop_from_json(value)?),
            None => return Err("자르기 영역을 적어 주세요".into()),
        };
        let rotation = args
            .get("rotation_millidegrees")
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .ok_or("회전 각도를 적어 주세요")?;
        let figure = self.r07_figure()?;
        let panel = self.r07_image_panel(figure, args)?;
        let change = transform_change(figure, &panel, crop, rotation);
        self.r07_send(change)
    }

    fn r07_calibration(&mut self, args: &Value) -> Result<(), String> {
        let calibration = match args.get("calibration") {
            Some(Value::Null) => None,
            Some(value) => Some(calibration_from_json(value)?),
            None => return Err("측정 보정 내용이 없습니다".into()),
        };
        let figure = self.r07_figure()?;
        let panel = self.r07_image_panel(figure, args)?;
        let change = calibration_change(figure, &panel, calibration);
        self.r07_send(change)
    }

    /// The R07 sections of the figure properties panel. The selected diagram element gets its origin and evidence; the selected
    /// image panel gets its crop, rotation and calibration. The common legend and the visual review belong to the figure.
    pub(super) fn r07_draw_figure(&mut self, ui: &mut Ui, figure: &FigureDocument) {
        if let Some(selected) = self.selected_node.clone() {
            if let Some(panel) = owning_panel(figure, &selected) {
                match &panel.body {
                    PanelBody::Diagram(diagram) if panel.id != selected => {
                        ui.separator();
                        self.r07_draw_provenance(ui, figure, &panel.id, diagram, &selected);
                    }
                    PanelBody::Image(image) if panel.id == selected => {
                        ui.separator();
                        self.r07_draw_transform(ui, figure, &panel.id, image);
                        self.r07_draw_calibration(ui, figure, &panel.id, image);
                    }
                    _ => {}
                }
            }
        }
        ui.separator();
        self.r07_draw_legend(ui, figure);
        ui.separator();
        self.r07_draw_review(ui, figure);
    }

    /// One apply button of an R07 section. It is enabled while the change is valid and the window may send it. Input the engine
    /// would refuse shows its words beside the button once the user has changed something; nothing is sent for it.
    fn r07_control(&mut self, ui: &mut Ui, label: &str, command: &str, change: Change, args: Value, dirty: bool) {
        let ready = self.shown_figure_revision().is_some() && self.dataset_edit.is_none();
        let sendable = matches!(change, Ok(Some(_)));
        let refusal = match &change {
            Err(message) if dirty => Some(message.clone()),
            _ => None,
        };
        ui.horizontal(|ui| {
            if ui.add_enabled(ready && sendable, egui::Button::new(label)).clicked() {
                self.invoke_ui(command, args);
            }
            if let Some(message) = refusal {
                ui.label(RichText::new(message).color(theme::RED));
            }
        });
    }

    fn r07_draw_provenance(&mut self, ui: &mut Ui, figure: &FigureDocument, panel_id: &str, diagram: &Diagram, element_id: &str) {
        let Some((connector, origin, evidence)) = element_provenance(diagram, element_id) else {
            return;
        };
        let key = draft_id("r07-provenance", self.r07_figure_id(), element_id);
        let fresh = ProvenanceDraft::of(origin, evidence);
        let mut draft = ui
            .data(|data| data.get_temp::<ProvenanceDraft>(key))
            .filter(|stored| stored.base == fresh.base)
            .unwrap_or_else(|| fresh.clone());
        ui.label(RichText::new("출처와 근거").strong());
        ui.horizontal(|ui| {
            ComboBox::from_id_salt(("r07-origin", element_id))
                .selected_text(origin_label(draft.origin.as_deref()))
                .show_ui(ui, |ui| {
                    if !connector {
                        ui.selectable_value(&mut draft.origin, None, "출처 없음");
                    }
                    for (value, label) in ORIGINS {
                        ui.selectable_value(&mut draft.origin, Some(value.to_owned()), label);
                    }
                });
        });
        for item in &figure.evidence {
            let mut checked = draft.evidence.contains(&item.id);
            if ui.checkbox(&mut checked, evidence_label(item)).changed() {
                draft.evidence.retain(|id| id != &item.id);
                if checked {
                    draft.evidence.push(item.id.clone());
                }
            }
        }
        // The evidence goes in the order the figure lists it, so the same set always sends the same list.
        let ordered: Vec<Id> = figure.evidence.iter().filter(|item| draft.evidence.contains(&item.id)).map(|item| item.id.clone()).collect();
        let dirty = draft != fresh;
        let args = json!({ "panel_id": panel_id, "element_id": element_id, "origin": draft.origin, "evidence_ids": ordered });
        let change = if dirty { provenance_change(figure, panel_id, element_id, draft.origin.as_deref(), &ordered) } else { Ok(None) };
        self.r07_control(ui, "적용", "figure.diagram_provenance", change, args, dirty);
        ui.data_mut(|data| data.insert_temp(key, draft));
    }

    fn r07_draw_transform(&mut self, ui: &mut Ui, figure: &FigureDocument, panel_id: &str, image: &FigureImage) {
        let key = draft_id("r07-transform", self.r07_figure_id(), panel_id);
        let fresh = TransformDraft::of(image);
        let mut draft = ui
            .data(|data| data.get_temp::<TransformDraft>(key))
            .filter(|stored| stored.base == fresh.base)
            .unwrap_or_else(|| fresh.clone());
        ui.label(RichText::new("그림 자르기·회전").strong());
        ui.horizontal(|ui| {
            ui.checkbox(&mut draft.crop_on, "자르기");
            let on = draft.crop_on;
            for (label, field) in [("x", &mut draft.x), ("y", &mut draft.y), ("너비", &mut draft.width), ("높이", &mut draft.height)] {
                ui.label(label);
                ui.add_enabled(on, TextEdit::singleline(field).desired_width(56.0));
            }
        });
        ui.horizontal(|ui| {
            ui.label("회전 (도)");
            ui.add(TextEdit::singleline(&mut draft.rotation).desired_width(72.0));
        });
        let dirty = draft != fresh;
        let request = transform_request(&draft);
        let change = match &request {
            Ok((crop, rotation)) if dirty => transform_change(figure, panel_id, *crop, *rotation),
            Ok(_) => Ok(None),
            Err(message) => Err(message.clone()),
        };
        let args = match &request {
            Ok((crop, rotation)) => {
                json!({ "panel_id": panel_id, "crop": (*crop).map_or(Value::Null, crop_value), "rotation_millidegrees": rotation })
            }
            Err(_) => Value::Null,
        };
        self.r07_control(ui, "적용", "figure.image_transform", change, args, dirty);
        ui.data_mut(|data| data.insert_temp(key, draft));
    }

    fn r07_draw_calibration(&mut self, ui: &mut Ui, figure: &FigureDocument, panel_id: &str, image: &FigureImage) {
        let key = draft_id("r07-calibration", self.r07_figure_id(), panel_id);
        let fresh = CalibrationDraft::of(image.calibration.as_ref());
        let mut draft = ui
            .data(|data| data.get_temp::<CalibrationDraft>(key))
            .filter(|stored| stored.base == fresh.base)
            .unwrap_or_else(|| fresh.clone());
        ui.label(RichText::new("측정 보정").strong());
        ui.horizontal(|ui| {
            ui.add(TextEdit::singleline(&mut draft.length).hint_text("길이").desired_width(72.0));
            ui.add(TextEdit::singleline(&mut draft.pixel_span).hint_text("픽셀 간격").desired_width(84.0));
            ui.add(TextEdit::singleline(&mut draft.unit).hint_text("단위").desired_width(56.0));
        });
        ui.horizontal(|ui| {
            ComboBox::from_id_salt(("r07-calibration-evidence", panel_id))
                .selected_text(evidence_text(figure, draft.evidence.as_deref()))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut draft.evidence, None, "근거 없음");
                    for item in &figure.evidence {
                        ui.selectable_value(&mut draft.evidence, Some(item.id.clone()), evidence_label(item));
                    }
                });
        });
        let dirty = draft != fresh;
        let calibration = draft.calibration();
        let args = json!({ "panel_id": panel_id, "calibration": calibration_value(&calibration) });
        let change = if dirty { calibration_change(figure, panel_id, Some(calibration)) } else { Ok(None) };
        self.r07_control(ui, "적용", "figure.image_calibration", change, args, dirty);
        if image.calibration.is_some() {
            let change = calibration_change(figure, panel_id, None);
            self.r07_control(ui, "보정 지우기", "figure.image_calibration", change, json!({ "panel_id": panel_id, "calibration": null }), true);
        }
        ui.data_mut(|data| data.insert_temp(key, draft));
    }

    fn r07_draw_legend(&mut self, ui: &mut Ui, figure: &FigureDocument) {
        let key = draft_id("r07-legend", self.r07_figure_id(), ());
        let fresh = LegendDraft::of(figure.common_legend.as_ref());
        let mut draft = ui
            .data(|data| data.get_temp::<LegendDraft>(key))
            .filter(|stored| stored.base == fresh.base)
            .unwrap_or_else(|| fresh.clone());
        ui.label(RichText::new("공통 범례").strong());
        ui.horizontal(|ui| {
            ComboBox::from_id_salt("r07-legend-placement")
                .selected_text(label_of(&PLACEMENTS, draft.placement))
                .show_ui(ui, |ui| {
                    for (value, label) in PLACEMENTS {
                        ui.selectable_value(&mut draft.placement, value, label);
                    }
                });
            ui.add(TextEdit::singleline(&mut draft.title).hint_text("제목").desired_width(160.0));
        });
        let mut remove_row = None;
        for (index, (label, color)) in draft.items.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(TextEdit::singleline(label).desired_width(140.0));
                ui.add(TextEdit::singleline(color).desired_width(96.0));
                if ui.small_button("삭제").clicked() {
                    remove_row = Some(index);
                }
            });
        }
        if let Some(index) = remove_row {
            draft.items.remove(index);
        }
        if ui.button("항목 추가").clicked() {
            draft.items.push((String::new(), "#1F4E79".to_owned()));
        }
        let dirty = draft != fresh;
        let legend = draft.legend();
        let args = json!({ "legend": legend_value(&legend) });
        let change = if dirty { legend_change(figure, Some(legend)) } else { Ok(None) };
        self.r07_control(ui, "적용", "figure.common_legend", change, args, dirty);
        if figure.common_legend.is_some() {
            let change = legend_change(figure, None);
            self.r07_control(ui, "범례 지우기", "figure.common_legend", change, json!({ "legend": null }), true);
        }
        ui.data_mut(|data| data.insert_temp(key, draft));
    }

    fn r07_draw_review(&mut self, ui: &mut Ui, figure: &FigureDocument) {
        let key = draft_id("r07-review", self.r07_figure_id(), ());
        let fresh = ReviewDraft::of(figure.visual_review.as_ref());
        let mut draft = ui
            .data(|data| data.get_temp::<ReviewDraft>(key))
            .filter(|stored| stored.base == fresh.base)
            .unwrap_or_else(|| fresh.clone());
        ui.label(RichText::new("시각 검토").strong());
        let stale = review_status_message(figure.visual_review.as_ref(), self.r07_diagnostics());
        if let Some(message) = stale {
            ui.label(RichText::new(message).color(theme::RED));
        }
        ui.horizontal(|ui| {
            ComboBox::from_id_salt("r07-review-state")
                .selected_text(label_of(&REVIEW_STATES, draft.state))
                .show_ui(ui, |ui| {
                    for (value, label) in REVIEW_STATES {
                        ui.selectable_value(&mut draft.state, value, label);
                    }
                });
            ui.add(TextEdit::singleline(&mut draft.note).hint_text("메모").desired_width(200.0));
        });
        // A stale approval may be given again: the new approval records the figure as it is now.
        let dirty = draft != fresh || stale.is_some();
        let review = draft.review();
        let args = json!({ "review": review_value(&review) });
        let change = if dirty { review_change(figure, Some(review)) } else { Ok(None) };
        self.r07_control(ui, "저장", "figure.visual_review", change, args, dirty);
        if figure.visual_review.is_some() {
            let change = review_change(figure, None);
            self.r07_control(ui, "검토 지우기", "figure.visual_review", change, json!({ "review": null }), true);
        }
        ui.data_mut(|data| data.insert_temp(key, draft));
    }
}

#[cfg(test)]
mod origin_tests {
    use super::{DIAGRAM_ORIGINS, ORIGINS};

    /// The window offers exactly the contract's origin codes, in the contract's order.
    #[test]
    fn the_origin_choices_are_the_contract_origins_in_order() {
        let offered: Vec<&str> = ORIGINS.iter().map(|(code, _)| *code).collect();
        assert_eq!(offered, DIAGRAM_ORIGINS);
    }
}
