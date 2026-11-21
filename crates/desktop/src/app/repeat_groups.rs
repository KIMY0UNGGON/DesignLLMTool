//! Repeat and conditional groups in the window (D21). Each command reads the stored document, builds the change set of one
//! step (a new repeat group, its column bindings, a show condition) and sends it as one change set. The engine decides what
//! the change may do: the row expansion, the refusals and the limits (`canvas/src/canvas/repeat.rs`). A refusal reaches the
//! window as the engine's answer, and the window shows the document the engine returns; it does not expand a group itself.

use designllm_contracts::{
    new_id, ArtifactBody, ArtifactKind, ConditionOp, LayoutSpec, Node, NodeExt, NodeKind, Operation, Page, PageRole, RepeatBinding,
    RepeatSpec, ShowCondition, Size, Style, repeat_groups::REPEAT_MAX_COLUMNS,
};
use eframe::egui::{self, Id, Ui};
use serde_json::{json, Value};

use super::{doc_tree::find_node_in_node, properties::{row, section_head}, DesktopApp};
use crate::{theme, widgets};
use crate::commands::CommandSpec;

/// The change set of "반복 만들기": the node moves to a new component page, and an empty row group that will hold the
/// repeat takes its place on the page it was on. The group repeats the component page once per data row of `table_id`.
pub(super) fn create_operations(
    node: &Node,
    page_id: &str,
    page_count: u32,
    page_size: Size,
    table_id: &str,
    component_page_id: &str,
    group_id: &str,
) -> Vec<Operation> {
    let component = Page::new(component_page_id.to_owned(), page_size);
    let group = Node {
        id: group_id.to_owned(),
        kind: NodeKind::Group,
        bounds: node.bounds,
        text: String::new(),
        style: Style::default(),
        asset: None,
        children: Vec::new(),
        layout: Some(LayoutSpec { mode: "row".into(), ..LayoutSpec::default() }),
        interactions: Vec::new(),
        variants: Default::default(),
        ext: NodeExt::default(),
    };
    vec![
        Operation::InsertPage { index: page_count, page: component },
        Operation::SetPageRole { page_id: component_page_id.to_owned(), role: Some(PageRole::Component) },
        Operation::MoveNode { node_id: node.id.clone(), page_id: component_page_id.to_owned(), parent_id: None, index: 0 },
        Operation::AddNode { page_id: page_id.to_owned(), parent_id: None, node: group },
        Operation::SetGroupRepeat {
            node_id: group_id.to_owned(),
            expected: None,
            repeat: Some(RepeatSpec { table_id: table_id.to_owned(), component_page_id: component_page_id.to_owned(), bindings: Vec::new() }),
        },
    ]
}

/// A non-empty text argument.
fn required_text<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key).and_then(Value::as_str).filter(|text| !text.is_empty()).ok_or_else(|| format!("`{key}` 값이 필요합니다"))
}

/// A non-empty text argument that may be absent.
fn optional_text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|text| !text.is_empty())
}

impl DesktopApp {
    /// Runs one of the five repeat-group commands. `None` for any other command, so `dispatch` asks the next module.
    pub(super) fn dispatch_repeat_groups(&mut self, spec: &CommandSpec, args: &Value) -> Option<Result<(), String>> {
        let result = match spec.id {
            "repeat.create" => self.repeat_create(args),
            "repeat.bind" => self.repeat_bind(args),
            "repeat.clear" => self.repeat_clear(args),
            "repeat.condition_set" => self.repeat_condition_set(args),
            "repeat.condition_clear" => self.repeat_condition_clear(args),
            _ => return None,
        };
        Some(result)
    }

    /// "반복 만들기": the named top-level node becomes the component of a new repeat group over `table_id`.
    fn repeat_create(&mut self, args: &Value) -> Result<(), String> {
        let node_id = required_text(args, "node_id")?;
        let table_id = required_text(args, "table_id")?;
        let component_page_id = optional_text(args, "component_page_id").map(str::to_owned).unwrap_or_else(new_id);
        let group_id = optional_text(args, "group_id").map(str::to_owned).unwrap_or_else(new_id);
        let operations = {
            let pages = self.canvas_pages()?;
            let (page_count, page) = (pages.len() as u32, pages.iter().find(|page| page.nodes.iter().any(|top| top.id == node_id)));
            let page = page.ok_or("최상위 항목을 고르세요")?;
            let node = page.nodes.iter().find(|top| top.id == node_id).ok_or("최상위 항목을 고르세요")?;
            create_operations(node, &page.id, page_count, page.size, table_id, &component_page_id, &group_id)
        };
        self.kind_change(operations)
    }

    /// Sets the column bindings of a repeat group. The group's stored spec is the `expected` one.
    fn repeat_bind(&mut self, args: &Value) -> Result<(), String> {
        let group_id = required_text(args, "group_id")?.to_owned();
        let bindings: Vec<RepeatBinding> = serde_json::from_value(args.get("bindings").cloned().unwrap_or(Value::Null))
            .map_err(|error| format!("bindings 형식이 잘못되었습니다: {error}"))?;
        let stored = self.stored_repeat(&group_id)?;
        let repeat = RepeatSpec { bindings, ..stored.clone() };
        self.kind_change(vec![Operation::SetGroupRepeat { node_id: group_id, expected: Some(stored), repeat: Some(repeat) }])
    }

    /// Removes the repeat of a group; the rows it expanded to go with it.
    fn repeat_clear(&mut self, args: &Value) -> Result<(), String> {
        let group_id = required_text(args, "group_id")?.to_owned();
        let stored = self.stored_repeat(&group_id)?;
        self.kind_change(vec![Operation::SetGroupRepeat { node_id: group_id, expected: Some(stored), repeat: None }])
    }

    /// Shows a repeat component's group only for the rows whose column passes the comparison.
    fn repeat_condition_set(&mut self, args: &Value) -> Result<(), String> {
        let node_id = required_text(args, "node_id")?.to_owned();
        let column = args.get("column").and_then(Value::as_u64).and_then(|column| u32::try_from(column).ok()).ok_or("`column` 값이 필요합니다")?;
        let op: ConditionOp = serde_json::from_value(args.get("op").cloned().unwrap_or(Value::Null)).map_err(|_| "`op` 값이 잘못되었습니다")?;
        // An empty value is a value (`equals ""` matches an empty cell); the engine judges it as the MCP tool's change would be.
        let value = args.get("value").and_then(Value::as_str).map(str::to_owned);
        let stored = self.stored_node(&node_id)?.ext.condition.clone();
        let condition = ShowCondition { column, op, value };
        self.kind_change(vec![Operation::SetGroupCondition { node_id, expected: stored, condition: Some(condition) }])
    }

    /// Removes the show condition of a group.
    fn repeat_condition_clear(&mut self, args: &Value) -> Result<(), String> {
        let node_id = required_text(args, "node_id")?.to_owned();
        let stored = self.stored_node(&node_id)?.ext.condition.clone().ok_or("조건이 없습니다")?;
        self.kind_change(vec![Operation::SetGroupCondition { node_id, expected: Some(stored), condition: None }])
    }

    fn canvas_pages(&self) -> Result<&[Page], String> {
        match self.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::Canvas(document)) => Ok(&document.pages),
            _ => Err("반복 그룹은 Canvas에서만 사용할 수 있습니다".into()),
        }
    }

    fn stored_node(&self, node_id: &str) -> Result<&Node, String> {
        self.canvas_pages()?
            .iter()
            .find_map(|page| page.nodes.iter().find_map(|top| find_node_in_node(top, node_id)))
            .ok_or_else(|| "노드를 찾지 못했습니다".into())
    }

    fn stored_repeat(&self, group_id: &str) -> Result<RepeatSpec, String> {
        self.stored_node(group_id)?.ext.repeat.clone().ok_or_else(|| "반복 그룹이 아닙니다".into())
    }

    /// The page that holds the node, at any depth.
    fn page_of_node(&self, node_id: &str) -> Option<&Page> {
        self.canvas_pages().ok()?.iter().find(|page| page.nodes.iter().any(|top| find_node_in_node(top, node_id).is_some()))
    }

    /// What the properties panel shows for the selection: decided from the stored document and the selected node only.
    pub(super) fn repeat_panel(&self) -> RepeatPanel {
        let canvas = self.active.as_ref().is_some_and(|active| active.kind == ArtifactKind::Canvas);
        if !self.screen_is_editor() || !canvas || self.selected_nodes.len() > 1 {
            return RepeatPanel::Nothing;
        }
        let Some(node_id) = self.selected_node.as_deref() else { return RepeatPanel::Nothing };
        let (Some(page), Ok(node)) = (self.page_of_node(node_id), self.stored_node(node_id)) else { return RepeatPanel::Nothing };
        if node.ext.repeat.is_some() {
            return RepeatPanel::Repeat;
        }
        if node.ext.instance.is_some() {
            return RepeatPanel::Nothing;
        }
        if page.role == Some(PageRole::Component) {
            return if node.kind == NodeKind::Group { RepeatPanel::Condition } else { RepeatPanel::Nothing };
        }
        // The new group takes the node's place at the top of its page; which tables may feed it is the engine's answer.
        let top_level = page.nodes.iter().any(|top| top.id == node_id);
        if top_level && node.ext.table.is_none() && self.data_tables().iter().any(|(id, _)| id != node_id) {
            RepeatPanel::Create
        } else {
            RepeatPanel::Nothing
        }
    }

    /// The tables "반복 만들기" offers as (id, name): every table of the document outside the component pages and the
    /// instances, in page order. A table without a name is "표 N", N counting these tables.
    pub(super) fn data_tables(&self) -> Vec<(String, String)> {
        fn collect<'a>(nodes: &'a [Node], out: &mut Vec<&'a Node>) {
            for node in nodes.iter().filter(|node| node.ext.instance.is_none()) {
                if node.ext.table.is_some() {
                    out.push(node);
                }
                collect(&node.children, out);
            }
        }
        let mut tables = Vec::new();
        for page in self.canvas_pages().unwrap_or_default().iter().filter(|page| page.role != Some(PageRole::Component)) {
            collect(&page.nodes, &mut tables);
        }
        tables
            .into_iter()
            .enumerate()
            .map(|(index, node)| (node.id.clone(), node.ext.name.clone().unwrap_or_else(|| format!("표 {}", index + 1))))
            .collect()
    }

    /// The nodes of a component page a binding may name, as (id, text). Which of them take a column is the engine's answer.
    fn component_sources(&self, page_id: &str) -> Vec<(String, String)> {
        fn collect(nodes: &[Node], out: &mut Vec<(String, String)>) {
            for node in nodes {
                out.push((node.id.clone(), node.text.clone()));
                collect(&node.children, out);
            }
        }
        let mut out = Vec::new();
        if let Some(page) = self.canvas_pages().unwrap_or_default().iter().find(|page| page.id == page_id) {
            collect(&page.nodes, &mut out);
        }
        out
    }

    /// The key of a draft the panel keeps for `node_id` between frames. It names the document and the node, not the revision:
    /// a change of another node leaves the draft alone. A draft ends when the stored value becomes the draft's (the change was
    /// applied) or the edit is undone; a refused change leaves it to correct.
    pub(super) fn draft_key(&self, what: &str, node_id: &str) -> Id {
        let document = self.artifact.as_ref().map(|artifact| artifact.id.as_str());
        Id::new((what, document, node_id))
    }

    /// The panel's repeat controls. Returns the command a button sent; the caller runs it after the panel is drawn.
    pub(super) fn draw_repeat_controls(&self, ui: &mut Ui, palette: &theme::Palette, first: &mut bool) -> Option<(&'static str, Value)> {
        let node_id = self.selected_node.clone()?;
        match self.repeat_panel() {
            RepeatPanel::Nothing => None,
            RepeatPanel::Create => self.draw_repeat_create(ui, palette, first, &node_id),
            RepeatPanel::Repeat => self.draw_repeat_group(ui, palette, first, &node_id),
            RepeatPanel::Condition => self.draw_repeat_condition(ui, palette, first, &node_id),
        }
    }

    fn draw_repeat_create(&self, ui: &mut Ui, palette: &theme::Palette, first: &mut bool, node_id: &str) -> Option<(&'static str, Value)> {
        let tables: Vec<(String, String)> = self.data_tables().into_iter().filter(|(id, _)| id != node_id).collect();
        let (first_table, _) = tables.first()?.clone();
        section_head(ui, palette, first, "반복", None);
        let key = self.draft_key("repeat-create-table", node_id);
        let mut table = ui.data(|data| data.get_temp::<String>(key)).filter(|id| tables.iter().any(|(table, _)| table == id)).unwrap_or(first_table);
        let label = tables.iter().find(|(id, _)| *id == table).map(|(_, name)| name.clone()).unwrap_or_default();
        let names: Vec<&str> = tables.iter().map(|(_, name)| name.as_str()).collect();
        let current = tables.iter().position(|(id, _)| *id == table);
        row(ui, palette, Some("데이터 표"), |ui| {
            let width = ui.available_width();
            if let Some(index) = widgets::select_menu(ui, palette, &label, &names, current, width) {
                table = tables[index].0.clone();
            }
        });
        ui.data_mut(|data| data.insert_temp(key, table.clone()));
        let enabled = self.command_enabled_for_current_kind("repeat.create");
        let clicked = row(ui, palette, Some(""), |ui| ui.add_enabled_ui(enabled, |ui| widgets::labeled_button(ui, palette, None, "반복 만들기", true)).inner.clicked());
        clicked.then(|| ("repeat.create", create_args(node_id, &table, &new_id(), &new_id())))
    }

    fn draw_repeat_group(&self, ui: &mut Ui, palette: &theme::Palette, first: &mut bool, node_id: &str) -> Option<(&'static str, Value)> {
        let spec = self.stored_node(node_id).ok()?.ext.repeat.clone()?;
        let candidates = self.component_sources(&spec.component_page_id);
        section_head(ui, palette, first, "반복", None);
        let key = self.draft_key("repeat-bindings", node_id);
        let stored: Vec<(String, u32)> = spec.bindings.iter().map(|binding| (binding.source_node_id.clone(), binding.column)).collect();
        let mut draft = ui.data(|data| data.get_temp::<Draft<Vec<(String, u32)>>>(key)).unwrap_or_else(|| Draft { base: stored.clone(), value: stored.clone() });
        let mut remove = None;
        // A binding row is named by its source's text (its id when it has none), cut to the label column.
        let source_name = |id: &str| candidates.iter().find(|(candidate, _)| candidate == id).map_or(id, |(_, text)| if text.is_empty() { id } else { text.as_str() }).to_owned();
        // A row's stepper is named by the group, its source and, among the rows with that source, its place, so that no two
        // steppers share an id, two rows with the same source included. A row above it that is removed does not move it unless
        // both have the same source: then each later row of that source takes the place of the one before it. Bindings carry no
        // id of their own.
        let keys: Vec<(String, usize)> = draft
            .value
            .iter()
            .enumerate()
            .map(|(index, (source, _))| (source.clone(), draft.value[..index].iter().filter(|(other, _)| other == source).count()))
            .collect();
        for (index, ((source, column), key)) in draft.value.iter_mut().zip(keys).enumerate() {
            let shown = widgets::truncate_to_width(ui, &source_name(source), theme::LABEL_COLUMN - 8.0, palette.ink_2);
            row(ui, palette, Some(shown.as_str()), |ui| {
                widgets::number_stepper(ui, palette, (node_id, key), column, 0..=LAST_COLUMN, "열 ");
                if widgets::labeled_button(ui, palette, None, "삭제", false).clicked() {
                    remove = Some(index);
                }
            });
        }
        if let Some(index) = remove {
            draft.value.remove(index);
        }
        let add_key = self.draft_key("repeat-add-source", node_id);
        let mut add = ui
            .data(|data| data.get_temp::<(String, u32)>(add_key))
            .unwrap_or_else(|| (candidates.first().map(|(id, _)| id.clone()).unwrap_or_default(), 0));
        let text_or_id = |id: &String, text: &String| if text.is_empty() { id.clone() } else { text.clone() };
        let source_label = candidates.iter().find(|(id, _)| *id == add.0).map(|(id, text)| text_or_id(id, text)).unwrap_or_default();
        let mut action = None;
        if !candidates.is_empty() {
            let names: Vec<String> = candidates.iter().map(|(id, text)| text_or_id(id, text)).collect();
            let options: Vec<&str> = names.iter().map(String::as_str).collect();
            let current = candidates.iter().position(|(id, _)| *id == add.0);
            // The source takes the whole row and its column the next one: side by side they do not fit a narrow panel.
            row(ui, palette, Some("연결"), |ui| {
                let width = ui.available_width();
                if let Some(index) = widgets::select_menu(ui, palette, &source_label, &options, current, width) {
                    add.0 = candidates[index].0.clone();
                }
            });
            row(ui, palette, Some("열"), |ui| {
                widgets::number_stepper(ui, palette, (node_id, "repeat-add-column"), &mut add.1, 0..=LAST_COLUMN, "");
            });
            if row(ui, palette, Some(""), |ui| widgets::labeled_button(ui, palette, None, "연결 추가", false).clicked()) {
                draft.value.push(add.clone());
            }
            ui.data_mut(|data| data.insert_temp(add_key, add.clone()));
        }
        let bind = self.command_enabled_for_current_kind("repeat.bind");
        let clear = self.command_enabled_for_current_kind("repeat.clear");
        // Three buttons do not fit one row of a narrow panel: the draft's two, then the group's own.
        let (apply, revert) = row(ui, palette, Some(""), |ui| {
            (
                ui.add_enabled_ui(bind, |ui| widgets::labeled_button(ui, palette, None, "연결 적용", true)).inner.clicked(),
                widgets::labeled_button(ui, palette, None, "되돌리기", false).clicked(),
            )
        });
        let unbind = row(ui, palette, Some(""), |ui| ui.add_enabled_ui(clear, |ui| widgets::labeled_button(ui, palette, None, "반복 해제", false)).inner.clicked());
        // The draft stays after "연결 적용": a refusal leaves it to correct. Once the stored bindings are the draft's, the change
        // was applied and the draft ends. An edit not stored yet is kept even when another change moved the document.
        if revert {
            ui.data_mut(|data| data.remove::<Draft<Vec<(String, u32)>>>(key));
        } else {
            if apply {
                action = Some(("repeat.bind", bind_args(node_id, &draft.value)));
            }
            if draft.value != stored && draft.value != draft.base {
                if stored != draft.base {
                    stale_notice(ui, palette);
                }
                ui.data_mut(|data| data.insert_temp(key, draft));
            } else {
                ui.data_mut(|data| data.remove::<Draft<Vec<(String, u32)>>>(key));
            }
        }
        if unbind {
            action = Some(("repeat.clear", json!({ "group_id": node_id })));
        }
        action
    }

    fn draw_repeat_condition(&self, ui: &mut Ui, palette: &theme::Palette, first: &mut bool, node_id: &str) -> Option<(&'static str, Value)> {
        let stored = self.stored_node(node_id).ok()?.ext.condition.as_ref().map(form_of);
        section_head(ui, palette, first, "조건", None);
        let key = self.draft_key("repeat-condition", node_id);
        let mut draft = ui.data(|data| data.get_temp::<ConditionDraft>(key)).unwrap_or_else(|| ConditionDraft {
            base: stored.clone(),
            form: form_or_blank(&stored),
            clearing: false,
        });
        row(ui, palette, Some("열"), |ui| {
            widgets::number_stepper(ui, palette, (node_id, "repeat-condition-column"), &mut draft.form.column, 0..=LAST_COLUMN, "");
        });
        let op_label = OPS.iter().find(|(name, _)| *name == draft.form.op).map_or("", |(_, label)| label);
        let op_labels: Vec<&str> = OPS.iter().map(|(_, label)| *label).collect();
        let op_current = OPS.iter().position(|(name, _)| *name == draft.form.op);
        row(ui, palette, Some("비교"), |ui| {
            let width = ui.available_width();
            if let Some(index) = widgets::select_menu(ui, palette, op_label, &op_labels, op_current, width) {
                draft.form.op = OPS[index].0.to_owned();
            }
        });
        if op_needs_value(&draft.form.op) {
            row(ui, palette, Some("값"), |ui| {
                let width = ui.available_width();
                widgets::edit_line(ui, palette, &mut draft.form.value, widgets::Field { width, ..Default::default() });
            });
        }
        let set = self.command_enabled_for_current_kind("repeat.condition_set");
        let clear = self.command_enabled_for_current_kind("repeat.condition_clear");
        let (apply, unset) = row(ui, palette, Some(""), |ui| {
            (
                ui.add_enabled_ui(set, |ui| widgets::labeled_button(ui, palette, None, "조건 설정", true)).inner.clicked(),
                stored.is_some() && ui.add_enabled_ui(clear, |ui| widgets::labeled_button(ui, palette, None, "조건 해제", false)).inner.clicked(),
            )
        });
        let mut action = None;
        if unset {
            draft.clearing = true;
            action = Some(("repeat.condition_clear", json!({ "node_id": node_id })));
        } else if apply {
            draft.clearing = false;
            action = Some(("repeat.condition_set", condition_args(node_id, draft.form.column, &draft.form.op, &draft.form.value)));
        }
        // The draft ends when the stored condition is the one it asks for (or, after "조건 해제", none). A refused change keeps it.
        let settled = if draft.clearing { stored.is_none() } else { stored.as_ref().is_some_and(|saved| comparable(saved) == comparable(&draft.form)) };
        let edited = draft.clearing || comparable(&draft.form) != comparable(&form_or_blank(&draft.base));
        if settled || !edited {
            ui.data_mut(|data| data.remove::<ConditionDraft>(key));
        } else {
            if stored != draft.base {
                stale_notice(ui, palette);
            }
            ui.data_mut(|data| data.insert_temp(key, draft));
        }
        action
    }
}

/// What the properties panel offers for the selected node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RepeatPanel {
    Nothing,
    /// A top-level node of a regular page while the document has a table (`data_tables`): "반복 만들기".
    Create,
    /// A repeat group: column bindings, "연결 적용", "반복 해제".
    Repeat,
    /// A group of a repeat component page: the show condition.
    Condition,
}

/// A panel edit the stored document does not hold yet. `base` is the stored value when the edit began, `value` is what the
/// panel shows. The draft ends once the stored value is `value` (the change was applied) or the edit is undone.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Draft<T> {
    pub(super) base: T,
    pub(super) value: T,
}

/// A show condition as the panel edits it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ConditionForm {
    pub(super) column: u32,
    pub(super) op: String,
    pub(super) value: String,
}

/// The condition draft. `clearing` is set when "조건 해제" was sent: the draft ends when the stored condition is gone, and stays
/// while the engine refuses the clear.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ConditionDraft {
    pub(super) base: Option<ConditionForm>,
    pub(super) form: ConditionForm,
    pub(super) clearing: bool,
}

fn form_of(condition: &ShowCondition) -> ConditionForm {
    ConditionForm { column: condition.column, op: op_name(condition.op).to_owned(), value: condition.value.clone().unwrap_or_default() }
}

/// The form the panel shows for a stored condition, or the blank form for none.
fn form_or_blank(form: &Option<ConditionForm>) -> ConditionForm {
    form.clone().unwrap_or_else(|| ConditionForm { column: 0, op: OPS[0].0.to_owned(), value: String::new() })
}

/// A form as the engine judges it: an operator that takes no value ignores the value the panel still holds.
fn comparable(form: &ConditionForm) -> ConditionForm {
    let value = if op_needs_value(&form.op) { form.value.clone() } else { String::new() };
    ConditionForm { column: form.column, op: form.op.clone(), value }
}

/// One line under the controls: the stored value moved while the panel held an edit, and the edit stays as it is.
fn stale_notice(ui: &mut Ui, palette: &theme::Palette) {
    row(ui, palette, Some(""), |ui| {
        ui.label(egui::RichText::new("저장된 값이 바뀌었습니다. 편집 중인 값은 유지했습니다").font(theme::display_font(false, 12.0)).color(palette.ink_2));
    });
}

/// The last column number the panel's fields take (the table limit of `contracts::repeat_groups`).
const LAST_COLUMN: u32 = REPEAT_MAX_COLUMNS as u32 - 1;

/// The condition operators in the panel's order, with the labels it shows.
const OPS: [(&str, &str); 4] = [("equals", "같음"), ("not_equals", "다름"), ("is_empty", "비어 있음"), ("is_not_empty", "비어 있지 않음")];

fn op_name(op: ConditionOp) -> &'static str {
    match op {
        ConditionOp::Equals => "equals",
        ConditionOp::NotEquals => "not_equals",
        ConditionOp::IsEmpty => "is_empty",
        ConditionOp::IsNotEmpty => "is_not_empty",
    }
}

/// Equals and not_equals take a value; the empty checks take none (the engine refuses a value they do not use).
fn op_needs_value(op: &str) -> bool {
    matches!(op, "equals" | "not_equals")
}

/// Arguments of `repeat.create` from the panel: the node, its table, and the two ids the new page and group take.
pub(super) fn create_args(node_id: &str, table_id: &str, component_page_id: &str, group_id: &str) -> Value {
    json!({ "node_id": node_id, "table_id": table_id, "component_page_id": component_page_id, "group_id": group_id })
}

/// Arguments of `repeat.bind` for the group's column bindings as the panel edited them.
pub(super) fn bind_args(group_id: &str, bindings: &[(String, u32)]) -> Value {
    let bindings: Vec<Value> = bindings.iter().map(|(source, column)| json!({ "source_node_id": source, "column": column })).collect();
    json!({ "group_id": group_id, "bindings": bindings })
}

/// Arguments of `repeat.condition_set`; the value is sent only for the operators that compare it.
pub(super) fn condition_args(node_id: &str, column: u32, op: &str, value: &str) -> Value {
    let mut args = json!({ "node_id": node_id, "column": column, "op": op });
    if op_needs_value(op) {
        args["value"] = json!(value);
    }
    args
}
