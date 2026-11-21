//! Formula nodes of a figure diagram (task 18, 2d): the command `figure.add_equation`, the source check the window runs before a
//! formula is sent (`text.set` on a formula node too), and the formula section of the figure panel. A source the grammar refuses
//! is shown inline and nothing is sent. The grammar is contracts' `figure_math::parse`; this module does not parse on its own.

use designllm_contracts::{figure_math, Diagram, FigureDocument, MathSyntax, Node, NodeKind, Operation, PanelBody};
use eframe::egui::{RichText, Ui};
use serde_json::{json, Value};

use super::{figure_r07::draft_id, figure_tools, DesktopApp};
use crate::{
    commands::CommandSpec,
    theme,
    widgets::{self, DialogButton},
};

/// A count with a comma between each group of three digits, as the window writes limits ("1,024").
fn grouped(count: usize) -> String {
    let digits = count.to_string();
    let mut text = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            text.push(',');
        }
        text.push(digit);
    }
    text
}

/// What the window shows for a source the grammar refuses, or `None` when the grammar reads it.
pub(super) fn source_refusal(source: &str) -> Option<String> {
    if source.trim().is_empty() {
        return Some("수식을 적어 주세요".into());
    }
    let error = figure_math::parse(source).err()?;
    let first = error.diagnostics.first();
    let words = match first.map(|diagnostic| diagnostic.code.as_str()) {
        Some(figure_math::FIGURE_MATH_LIMIT) => format!(
            "수식이 너무 깁니다 ({}바이트, 토큰 {}개, 중첩 {}단계 이내)",
            grouped(figure_math::MATH_MAX_BYTES),
            grouped(figure_math::MATH_MAX_TOKENS),
            figure_math::MATH_MAX_DEPTH
        ),
        _ => "지원하지 않는 수식 문법입니다".to_owned(),
    };
    Some(match first.and_then(|diagnostic| diagnostic.params.get("byte")).and_then(Value::as_u64) {
        Some(byte) => format!("{words} (바이트 {byte})"),
        None => words,
    })
}

/// The figure the window holds, borrowed.
fn shown_figure(artifact: Option<&designllm_contracts::Artifact>) -> Option<&FigureDocument> {
    match artifact.map(|artifact| &artifact.body) {
        Some(designllm_contracts::ArtifactBody::Figure(figure)) => Some(figure),
        _ => None,
    }
}

/// The formula node named `node_id` in a diagram of the figure.
fn formula_node<'a>(figure: &'a FigureDocument, node_id: &str) -> Option<&'a Node> {
    figure
        .panels
        .iter()
        .filter_map(|panel| match &panel.body {
            PanelBody::Diagram(Diagram { nodes, .. }) => Some(nodes),
            _ => None,
        })
        .flatten()
        .find(|node| node.id == node_id && node.ext.math.is_some())
}

/// What the formula section keeps between frames: the text it was based on and what the user has typed.
#[derive(Clone)]
pub(super) struct EquationDraft {
    pub(super) base: String,
    pub(super) text: String,
}

impl DesktopApp {
    /// Runs a figure formula command; `None` for an id this module does not serve.
    pub(super) fn dispatch_figure_math(&mut self, spec: &CommandSpec, args: &Value) -> Option<Result<(), String>> {
        match spec.id {
            "figure.add_equation" => Some(self.equation_add(args)),
            _ => None,
        }
    }

    /// Refuses a text change to a formula node whose new source the grammar does not read. Other text passes.
    pub(super) fn equation_check(&self, node_id: &str, source: &str) -> Result<(), String> {
        match shown_figure(self.artifact.as_ref()).and_then(|figure| formula_node(figure, node_id)) {
            Some(_) => source_refusal(source).map_or(Ok(()), Err),
            None => Ok(()),
        }
    }

    /// Adds a formula node (`NodeKind::Text` with `ext.math`) to a diagram: the panel named by `panel_id`, else the diagram that owns
    /// the selection, else the first diagram. The node is placed the way a new box is.
    fn equation_add(&mut self, args: &Value) -> Result<(), String> {
        let source = args.get("source").and_then(Value::as_str).unwrap_or_default();
        if let Some(message) = source_refusal(source) {
            return Err(message);
        }
        let figure = self.figure_document().ok_or("논문 그림이 열려 있지 않습니다")?;
        let diagram_panel = |panel: &designllm_contracts::FigurePanel| matches!(panel.body, PanelBody::Diagram(_));
        let panel_id = match args.get("panel_id").and_then(Value::as_str) {
            Some(id) => {
                let panel = figure.panels.iter().find(|panel| panel.id == id).ok_or("패널을 찾지 못했습니다")?;
                if !diagram_panel(panel) {
                    return Err("구조도 패널이 아닙니다".into());
                }
                panel.id.clone()
            }
            None => self
                .selected_node
                .as_deref()
                .and_then(|id| figure_tools::owning_panel(&figure, id))
                .filter(|panel| diagram_panel(panel))
                .or_else(|| figure.panels.iter().find(|panel| diagram_panel(panel)))
                .map(|panel| panel.id.clone())
                .ok_or("구조도 패널이 없습니다")?,
        };
        let mut operation = figure_tools::node_addition(&figure, &panel_id, NodeKind::Text).ok_or("이 패널에는 더 넣을 자리가 없습니다")?;
        if let Operation::AddDiagramNode { node, .. } = &mut operation {
            node.text = source.to_owned();
            node.ext.math = Some(MathSyntax::TexSubset);
        }
        self.kind_change(vec![operation])
    }

    /// The formula section of the figure panel: the source of the selected formula, else the field that adds a formula to a diagram.
    pub(super) fn draw_equation(&mut self, ui: &mut Ui, figure: &FigureDocument) {
        let ready = self.shown_figure_revision().is_some() && self.dataset_edit.is_none();
        let figure_id = self.artifact.as_ref().map_or_else(String::new, |artifact| artifact.id.clone());
        let selected = self.selected_node.clone().and_then(|id| formula_node(figure, &id).map(|node| (node.id.clone(), node.text.clone())));
        match selected {
            Some((node_id, base)) => {
                ui.separator();
                self.draw_equation_edit(ui, &figure_id, &node_id, &base, ready);
            }
            None if figure.panels.iter().any(|panel| matches!(panel.body, PanelBody::Diagram(_))) => {
                ui.separator();
                self.draw_equation_add(ui, &figure_id, ready);
            }
            None => {}
        }
    }

    fn draw_equation_edit(&mut self, ui: &mut Ui, figure_id: &str, node_id: &str, base: &str, ready: bool) {
        let palette = theme::palette(ui.ctx());
        let key = draft_id("figure-math-edit", figure_id, node_id);
        let mut draft = ui
            .data(|data| data.get_temp::<EquationDraft>(key))
            .filter(|stored| stored.base == base)
            .unwrap_or_else(|| EquationDraft { base: base.to_owned(), text: base.to_owned() });
        ui.label(RichText::new("수식").strong());
        let width = ui.available_width();
        widgets::edit_line(ui, &palette, &mut draft.text, widgets::Field { width, ..Default::default() });
        let changed = draft.text != draft.base;
        let refusal = if changed { source_refusal(&draft.text) } else { None };
        ui.horizontal_wrapped(|ui| {
            if widgets::dialog_button(ui, &palette, "수식 적용", DialogButton::Plain, ready && changed && refusal.is_none()).clicked() {
                self.invoke_ui("text.set", json!({ "node_id": node_id, "text": draft.text.clone() }));
            }
            if let Some(message) = &refusal {
                ui.label(RichText::new(message).color(theme::RED));
            }
        });
        ui.data_mut(|data| data.insert_temp(key, draft));
    }

    fn draw_equation_add(&mut self, ui: &mut Ui, figure_id: &str, ready: bool) {
        let palette = theme::palette(ui.ctx());
        let key = draft_id("figure-math-add", figure_id, ());
        let mut draft = ui
            .data(|data| data.get_temp::<EquationDraft>(key))
            .unwrap_or_else(|| EquationDraft { base: String::new(), text: String::new() });
        ui.label(RichText::new("수식 추가").strong());
        let width = ui.available_width();
        widgets::edit_line(ui, &palette, &mut draft.text, widgets::Field { width, ..Default::default() });
        let refusal = if draft.text.is_empty() { None } else { source_refusal(&draft.text) };
        ui.horizontal_wrapped(|ui| {
            if widgets::dialog_button(ui, &palette, "수식 넣기", DialogButton::Plain, ready && !draft.text.trim().is_empty() && refusal.is_none()).clicked() {
                let source = draft.text.clone();
                match self.invoke("figure.add_equation", json!({ "source": source })) {
                    Ok(()) => draft.text.clear(),
                    Err(message) => self.set_error(message),
                }
            }
            if let Some(message) = &refusal {
                ui.label(RichText::new(message).color(theme::RED));
            }
        });
        ui.data_mut(|data| data.insert_temp(key, draft));
    }
}
