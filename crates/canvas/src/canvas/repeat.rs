//! Canvas repeat and conditional groups (D21). A repeat group is an empty Group with a layout and `NodeExt.repeat`: the
//! engine gives it one instance of the component page per data row of a Canvas table (the header row is not a data row),
//! with the instance id `<group id>-<row id>`. A bound text node shows the row's cell through `NodeOverride.text`; a
//! Group of the component with `NodeExt.condition` is hidden in the rows where the condition is false, through
//! `NodeOverride.hidden`. The instances are ordinary component instances: `components::refresh_instances` materializes
//! their children, so a copy is made by the same code as any instance. Expansion runs at the end of a change that reaches
//! a repeat source (`needs_expansion`), so a table or component edit expands again in the same commit.
//!
//! Limits recorded for review: an override is kept by instance id, so a row removed and a new row with the same id added in
//! one change inherit the old row's style and unbound-text overrides. The expansion limit (10,000 nodes per group) is above
//! the document limit (`MAX_NODES` + `MAX_PAGES` = 5,500), which is reached first.

use super::{components, model, ops};
use crate::common::{blocking, fail, valid_id};
use designllm_contracts::repeat_groups::{
    CANVAS_REPEAT_BINDING, CANVAS_REPEAT_COLUMN, CANVAS_REPEAT_CONDITION, CANVAS_REPEAT_CONDITION_SCOPE, CANVAS_REPEAT_EXPECTED,
    CANVAS_REPEAT_FLATTENED, CANVAS_REPEAT_GROUP_NOT_EMPTY, CANVAS_REPEAT_LIMIT, CANVAS_REPEAT_NESTED, CANVAS_REPEAT_STALE, CANVAS_REPEAT_TABLE,
    CANVAS_REPEAT_TARGET, REPEAT_MAX_CELL_BYTES, REPEAT_MAX_COLUMNS, REPEAT_MAX_EXPANDED_NODES, REPEAT_MAX_ROWS,
};
use designllm_contracts::{
    CanvasDocument, ConditionOp, Diagnostic, DltError, DltResult, ErrorCode, InstanceRef, Interaction, Node, NodeExt, NodeKind, NodeOverride, Operation,
    Page, PageRole, Rect, RepeatSpec, ShowCondition,
};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

/// Diagnostic codes that component instances and repeat groups share (components.rs reads them from here).
pub(super) const INSTANCE_CHILD_READ_ONLY: &str = "INSTANCE_CHILD_READ_ONLY";
pub(super) const COMPONENT_OVERRIDE_HIDDEN: &str = "COMPONENT_OVERRIDE_HIDDEN";

/// A refusal found while planning one repeat group.
struct Problem {
    error: ErrorCode,
    code: &'static str,
    message: String,
    node_id: String,
}

impl Problem {
    fn new(error: ErrorCode, code: &'static str, message: impl Into<String>, node_id: &str) -> Self {
        Self { error, code, message: message.into(), node_id: node_id.to_string() }
    }
    fn into_error(self) -> DltError {
        model::fail_with(self.error, self.code, self.message, Some(&self.node_id))
    }
    fn into_diagnostic(self) -> Diagnostic {
        blocking(self.code, self.message, Some(self.node_id))
    }
}

fn page_of<'a>(doc: &'a CanvasDocument, id: &str) -> Option<&'a Page> {
    doc.pages.iter().find(|page| model::find_node(&page.nodes, id).is_some())
}

/// Every repeat group: its page index and its id, in document order.
fn repeat_groups(doc: &CanvasDocument) -> Vec<(usize, String)> {
    doc.pages.iter().enumerate()
        .flat_map(|(index, page)| model::flatten(&page.nodes).into_iter().filter(|node| node.ext.repeat.is_some()).map(move |node| (index, node.id.clone())))
        .collect()
}

pub(crate) fn has_repeat_groups(doc: &CanvasDocument) -> bool {
    doc.pages.iter().any(|page| model::flatten(&page.nodes).iter().any(|node| node.ext.repeat.is_some()))
}

/// The ids of the instances repeat groups own (their direct children).
pub(super) fn repeat_instance_ids(doc: &CanvasDocument) -> BTreeSet<String> {
    doc.pages.iter().flat_map(|page| model::flatten(&page.nodes))
        .filter(|node| node.ext.repeat.is_some())
        .flat_map(|group| group.children.iter().map(|child| child.id.clone()))
        .collect()
}

fn condition_problem(condition: &ShowCondition, node_id: &str) -> Option<Problem> {
    let needs_value = matches!(condition.op, ConditionOp::Equals | ConditionOp::NotEquals);
    if needs_value != condition.value.is_some() {
        return Some(Problem::new(ErrorCode::InvalidArgument, CANVAS_REPEAT_CONDITION, "equals and not_equals need a value; is_empty and is_not_empty take none", node_id));
    }
    if condition.value.as_ref().is_some_and(|value| value.len() > REPEAT_MAX_CELL_BYTES) {
        return Some(Problem::new(ErrorCode::InvalidArgument, CANVAS_REPEAT_LIMIT, format!("a condition value may have at most {REPEAT_MAX_CELL_BYTES} bytes"), node_id));
    }
    None
}

fn passes(condition: &ShowCondition, cell: &str) -> bool {
    match condition.op {
        ConditionOp::Equals => condition.value.as_deref() == Some(cell),
        ConditionOp::NotEquals => condition.value.as_deref() != Some(cell),
        ConditionOp::IsEmpty => cell.is_empty(),
        ConditionOp::IsNotEmpty => !cell.is_empty(),
    }
}

/// The instance roots (without children) the group expands to. Style and text overrides of an instance that already
/// exists are kept; bound texts and condition results are written again from the row.
fn plan(doc: &CanvasDocument, page: &Page, group: &Node) -> Result<Vec<Node>, Problem> {
    let spec: &RepeatSpec = group.ext.repeat.as_ref().expect("a repeat group");
    let target = |message: &str| Problem::new(ErrorCode::InvalidArgument, CANVAS_REPEAT_TARGET, message, &group.id);
    if group.kind != NodeKind::Group || group.ext.instance.is_some() || group.ext.table.is_some() {
        return Err(target("a repeat group is a Group that is neither an instance nor a table"));
    }
    if !group.layout.as_ref().is_some_and(|layout| matches!(layout.mode.as_str(), "row" | "column" | "grid")) {
        return Err(target("a repeat group needs a row, column or grid layout to place its instances"));
    }
    let nested = |message: &str| Problem::new(ErrorCode::UnsupportedOperation, CANVAS_REPEAT_NESTED, message, &group.id);
    if page.role.is_some() || components::is_instance_subtree(doc, &group.id) {
        return Err(nested("a repeat group must be on a regular page, outside any component instance"));
    }
    let source = doc.pages.iter().find(|candidate| candidate.id == spec.component_page_id)
        .filter(|candidate| candidate.role == Some(PageRole::Component))
        .ok_or_else(|| Problem::new(ErrorCode::InvalidArgument, "COMPONENT_SOURCE_PAGE", format!("page {} is not a component source", spec.component_page_id), &group.id))?;
    let source_nodes = model::flatten(&source.nodes);
    if source_nodes.iter().any(|node| node.ext.instance.is_some()) {
        return Err(Problem::new(ErrorCode::UnsupportedOperation, "NESTED_INSTANCE_UNSUPPORTED", "component sources cannot contain instances", &group.id));
    }
    if source_nodes.iter().any(|node| node.ext.repeat.is_some()) {
        return Err(nested("a repeated component cannot contain a repeat group"));
    }
    let table_problem = |message: &str| Problem::new(ErrorCode::InvalidArgument, CANVAS_REPEAT_TABLE, message, &group.id);
    let table_page = page_of(doc, &spec.table_id).ok_or_else(|| table_problem("the data table does not exist"))?;
    let table_node = model::find_node(&table_page.nodes, &spec.table_id).expect("page_of found the table");
    let table = table_node.ext.table.as_ref().ok_or_else(|| table_problem("the data source must be a Canvas table"))?;
    if table_page.id == source.id {
        return Err(table_problem("the data table cannot be in the component page it repeats"));
    }
    if components::is_instance_subtree(doc, &spec.table_id) {
        return Err(table_problem("the data table cannot be inside a component instance"));
    }
    if !table.cell_spans.is_empty() {
        return Err(table_problem("a table with merged cells cannot be a data source"));
    }
    let limit = |message: String| Problem::new(ErrorCode::InvalidArgument, CANVAS_REPEAT_LIMIT, message, &group.id);
    let columns = table.rows.first().map_or(0, |row| row.cell_ids.len());
    if columns > REPEAT_MAX_COLUMNS {
        return Err(limit(format!("a data table may have at most {REPEAT_MAX_COLUMNS} columns")));
    }
    let rows: Vec<_> = table.rows.iter().filter(|row| table.header_row_id.as_deref() != Some(row.id.as_str())).collect();
    if rows.len() > REPEAT_MAX_ROWS {
        return Err(limit(format!("a data table may have at most {REPEAT_MAX_ROWS} data rows")));
    }
    if rows.len().saturating_mul(source_nodes.len() + 1) > REPEAT_MAX_EXPANDED_NODES {
        return Err(limit(format!("a repeat group may expand to at most {REPEAT_MAX_EXPANDED_NODES} nodes")));
    }
    let column = |index: u32, node_id: &str| -> Result<usize, Problem> {
        let index = index as usize;
        if index >= columns {
            return Err(Problem::new(ErrorCode::InvalidArgument, CANVAS_REPEAT_COLUMN, format!("column {index} is outside the table's {columns} columns"), node_id));
        }
        Ok(index)
    };
    let mut bound = BTreeMap::new();
    for binding in &spec.bindings {
        let binding_problem = |message: &str| Problem::new(ErrorCode::InvalidArgument, CANVAS_REPEAT_BINDING, message, &group.id);
        let node = model::find_node(&source.nodes, &binding.source_node_id).ok_or_else(|| binding_problem("a binding names a node the component page does not have"))?;
        if !matches!(node.kind, NodeKind::Text | NodeKind::Rectangle | NodeKind::Ellipse) {
            return Err(binding_problem("a binding must name a text, rectangle or ellipse node"));
        }
        if bound.insert(binding.source_node_id.as_str(), column(binding.column, &group.id)?).is_some() {
            return Err(binding_problem("a component node can be bound to one column only"));
        }
    }
    let mut conditions = Vec::new();
    for node in source_nodes.iter().filter_map(|node| node.ext.condition.as_ref().map(|condition| (node, condition))) {
        if let Some(problem) = condition_problem(node.1, &node.0.id) { return Err(problem); }
        conditions.push((node.0.id.as_str(), column(node.1.column, &node.0.id)?, node.1));
    }
    let width = source.nodes.iter().map(|node| node.bounds.x.saturating_add(node.bounds.width)).max().unwrap_or(1).max(1);
    let height = source.nodes.iter().map(|node| node.bounds.y.saturating_add(node.bounds.height)).max().unwrap_or(1).max(1);
    let cell_texts: BTreeMap<&str, &str> = table_node.children.iter().map(|cell| (cell.id.as_str(), cell.text.as_str())).collect();
    let mut roots = Vec::with_capacity(rows.len());
    for row in rows {
        let mut cells = Vec::with_capacity(columns);
        for cell_id in &row.cell_ids {
            let text = cell_texts.get(cell_id.as_str()).copied().ok_or_else(|| table_problem("a table cell node is missing"))?;
            if text.len() > REPEAT_MAX_CELL_BYTES {
                return Err(limit(format!("a data cell may have at most {REPEAT_MAX_CELL_BYTES} bytes")));
            }
            cells.push(text);
        }
        // A row shorter than the first one (a tampered file) has no cell for the column: refused, never indexed.
        let cell = |index: usize, node_id: &str| cells.get(index).copied().ok_or_else(|| {
            Problem::new(ErrorCode::InvalidArgument, CANVAS_REPEAT_BINDING, format!("row {} has no cell in column {index}", row.id), node_id)
        });
        let id = format!("{}-{}", group.id, row.id);
        if !valid_id(&id) || id.contains("::") {
            return Err(Problem::new(ErrorCode::InvalidArgument, "COMPONENT_INSTANCE_ID", format!("instance id {id} is invalid"), &group.id));
        }
        let mut overrides: BTreeMap<String, NodeOverride> = group.children.iter().find(|child| child.id == id)
            .and_then(|child| child.ext.instance.as_ref()).map(|instance| instance.overrides.clone()).unwrap_or_default();
        if overrides.values().any(|over| over.hidden == Some(false)) {
            return Err(Problem::new(ErrorCode::InvalidArgument, COMPONENT_OVERRIDE_HIDDEN, "a hidden override is only ever true", &id));
        }
        for over in overrides.values_mut() { over.hidden = None; }
        for (source_id, index) in &bound {
            overrides.entry((*source_id).to_string()).or_default().text = Some(cell(*index, &group.id)?.to_string());
        }
        for (source_id, index, condition) in &conditions {
            if !passes(condition, cell(*index, *source_id)?) {
                overrides.entry((*source_id).to_string()).or_default().hidden = Some(true);
            }
        }
        overrides.retain(|_, over| *over != NodeOverride::default());
        roots.push(Node {
            id, kind: NodeKind::Group, bounds: Rect { x: 0, y: 0, width, height }, text: String::new(), style: Default::default(), asset: None,
            children: Vec::new(), layout: None, interactions: Vec::new(), variants: BTreeMap::new(),
            ext: NodeExt { instance: Some(InstanceRef { component_page_id: spec.component_page_id.clone(), overrides }), ..Default::default() },
        });
    }
    Ok(roots)
}

/// Same instance root, children aside.
fn same_root(planned: &Node, stored: &Node) -> bool {
    planned.id == stored.id && planned.kind == stored.kind && planned.bounds == stored.bounds && planned.text == stored.text
        && planned.style == stored.style && planned.asset == stored.asset && planned.layout.is_none() && stored.layout.is_none()
        && planned.interactions == stored.interactions && planned.variants == stored.variants && planned.ext == stored.ext
}

/// Expands every repeat group again and materializes the instances. Returns the changed ids and the refresh diagnostics.
pub(crate) fn expand(doc: &mut CanvasDocument) -> DltResult<(Vec<String>, Vec<Diagnostic>)> {
    let groups = repeat_groups(doc);
    if groups.is_empty() { return Ok((Vec::new(), Vec::new())); }
    let mut changed = BTreeSet::new();
    for (page_index, group_id) in groups {
        let group = model::find_node(&doc.pages[page_index].nodes, &group_id).expect("listed above");
        let roots = plan(doc, &doc.pages[page_index], group).map_err(Problem::into_error)?;
        let unchanged = roots.len() == group.children.len() && roots.iter().zip(&group.children).all(|(planned, stored)| same_root(planned, stored));
        if unchanged { continue; }
        ops::ensure_unlocked(group)?;
        // Only instances of this group's own component are replaced; any other child was put there by a change and
        // would be dropped, so the change is refused instead (R09).
        let component = &group.ext.repeat.as_ref().expect("a repeat group").component_page_id;
        if let Some(foreign) = group.children.iter().find(|stored| {
            !stored.id.starts_with(&format!("{group_id}-")) || stored.ext.instance.as_ref().is_none_or(|instance| &instance.component_page_id != component)
        }) {
            return Err(model::fail_with(ErrorCode::InvalidArgument, CANVAS_REPEAT_GROUP_NOT_EMPTY, format!("node {} is not an instance this repeat group made", foreign.id), Some(&group_id)));
        }
        let mut removed = BTreeSet::new();
        for stored in &group.children {
            match roots.iter().find(|planned| planned.id == stored.id) {
                Some(planned) if same_root(planned, stored) => {}
                Some(_) => { changed.insert(stored.id.clone()); }
                None => model::subtree_ids(stored, &mut removed),
            }
        }
        changed.insert(group_id.clone());
        changed.extend(roots.iter().filter(|planned| !group.children.iter().any(|stored| stored.id == planned.id)).map(|planned| planned.id.clone()));
        let group = model::find_node_mut(&mut doc.pages[page_index].nodes, &group_id).expect("listed above");
        let mut previous = std::mem::take(&mut group.children);
        group.children = roots.into_iter().map(|mut root| {
            if let Some(index) = previous.iter().position(|stored| stored.id == root.id) {
                root.children = std::mem::take(&mut previous[index].children);
            }
            root
        }).collect();
        doc.bindings.retain(|binding| !removed.contains(&binding.node_id));
        changed.extend(removed);
    }
    let (ids, diagnostics) = components::refresh_instances(doc)?;
    changed.extend(ids);
    Ok((changed.into_iter().collect(), diagnostics))
}

/// Blocking diagnostics for repeat groups and conditions: the planning rules, a stored expansion that differs from
/// expanding again (CANVAS_REPEAT_STALE), and the condition scope.
pub(crate) fn validate_document(doc: &CanvasDocument) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for (page_index, group_id) in repeat_groups(doc) {
        let page = &doc.pages[page_index];
        let group = model::find_node(&page.nodes, &group_id).expect("listed above");
        match plan(doc, page, group) {
            Err(problem) => out.push(problem.into_diagnostic()),
            Ok(roots) => {
                if roots.len() != group.children.len() || !roots.iter().zip(&group.children).all(|(planned, stored)| same_root(planned, stored)) {
                    out.push(blocking(CANVAS_REPEAT_STALE, "the stored instances of this repeat group differ from its table and component", Some(group_id.clone())));
                }
            }
        }
    }
    let repeat_instances = repeat_instance_ids(doc);
    let mut condition_pages = BTreeSet::new();
    for page in &doc.pages {
        for node in model::flatten(&page.nodes) {
            let Some(condition) = &node.ext.condition else { continue };
            if page.role != Some(PageRole::Component) {
                out.push(blocking(CANVAS_REPEAT_CONDITION_SCOPE, "a show condition belongs to a group in a repeated component page", Some(node.id.clone())));
            } else if node.kind != NodeKind::Group {
                out.push(blocking(CANVAS_REPEAT_CONDITION_SCOPE, "a show condition belongs to a Group", Some(node.id.clone())));
            }
            if let Some(problem) = condition_problem(condition, &node.id) { out.push(problem.into_diagnostic()); }
            condition_pages.insert(page.id.as_str());
        }
    }
    for node in doc.pages.iter().flat_map(|page| model::flatten(&page.nodes)) {
        let Some(instance) = &node.ext.instance else { continue };
        if condition_pages.contains(instance.component_page_id.as_str()) && !repeat_instances.contains(&node.id) {
            out.push(blocking(CANVAS_REPEAT_CONDITION_SCOPE, "a component page with show conditions is used by repeat groups only", Some(node.id.clone())));
        }
    }
    out
}

/// The non-blocking export diagnostic: exports draw the expanded nodes only, the data binding is not kept.
pub(crate) fn export_diagnostics(doc: &CanvasDocument) -> Vec<Diagnostic> {
    let bound = doc.pages.iter().flat_map(|page| model::flatten(&page.nodes)).any(|node| node.ext.repeat.is_some() || node.ext.condition.is_some());
    if !bound { return Vec::new(); }
    vec![Diagnostic {
        code: CANVAS_REPEAT_FLATTENED.into(),
        message: "repeat groups and show conditions are exported as their expanded nodes; the table binding is not kept".into(),
        node_id: None, blocking: false, params: Default::default(),
    }]
}

/// Whether a change can alter what a repeat group expands to, so `expand` runs only then. A plan reads the group (its spec,
/// kind, layout and page), the data table and its cells, and the component page (its nodes, bounds, conditions and role).
/// Edits that reach none of them leave every expansion as it is; component source edits that do reach the copies are
/// materialized by `components::refresh_instances` as for any instance.
pub(crate) fn needs_expansion(doc: &CanvasDocument, operations: &[Operation]) -> bool {
    use Operation::*;
    let carries_repeat = |nodes: Vec<&Node>| nodes.into_iter().any(|node| node.ext.repeat.is_some() || node.ext.condition.is_some());
    let groups = repeat_groups(doc);
    if groups.is_empty() {
        return operations.iter().any(|operation| match operation {
            SetGroupRepeat { .. } => true,
            AddNode { node, .. } => carries_repeat(model::flatten(std::slice::from_ref(node))),
            InsertPage { page, .. } => carries_repeat(model::flatten(&page.nodes)),
            _ => false,
        });
    }
    let mut sources: BTreeSet<&str> = BTreeSet::new();
    let mut component_pages = BTreeSet::new();
    for (page_index, group_id) in &groups {
        let page = &doc.pages[*page_index];
        sources.insert(page.id.as_str());
        let group = model::find_node(&page.nodes, group_id).expect("listed above");
        sources.insert(group.id.as_str());
        let spec = group.ext.repeat.as_ref().expect("a repeat group");
        sources.insert(spec.table_id.as_str());
        sources.insert(spec.component_page_id.as_str());
        component_pages.insert(spec.component_page_id.as_str());
        if let Some(table_page) = page_of(doc, &spec.table_id) {
            sources.insert(table_page.id.as_str());
            if let Some(table) = model::find_node(&table_page.nodes, &spec.table_id) {
                sources.extend(table.children.iter().map(|cell| cell.id.as_str()));
            }
        }
        if let Some(source) = doc.pages.iter().find(|page| page.id == spec.component_page_id) {
            sources.extend(model::flatten(&source.nodes).into_iter().map(|node| node.id.as_str()));
        }
    }
    let subtree_has_source = |id: &str| sources.contains(id) || model::find_node_in_doc(doc, id).is_some_and(|node|
        model::flatten(&node.children).iter().any(|child| sources.contains(child.id.as_str())));
    operations.iter().any(|operation| match operation {
        SetGroupRepeat { .. } | SetGroupCondition { .. } | SetPageRole { .. } => true,
        AddNode { page_id, parent_id, node } => {
            component_pages.contains(page_id.as_str()) || parent_id.as_deref().is_some_and(|id| sources.contains(id))
                || carries_repeat(model::flatten(std::slice::from_ref(node)))
        }
        InsertPage { page, .. } => carries_repeat(model::flatten(&page.nodes)),
        RemovePage { page_id } => sources.contains(page_id.as_str()),
        RemoveNode { node_id } => subtree_has_source(node_id),
        MoveNode { page_id, parent_id, node_id, .. } => {
            component_pages.contains(page_id.as_str()) || subtree_has_source(node_id) || parent_id.as_deref().is_some_and(|id| sources.contains(id))
        }
        InstantiateComponent { parent_id, .. } => parent_id.as_deref().is_some_and(|id| sources.contains(id)),
        other => ops::writes_matching_node(doc, other, |id| sources.contains(id)),
    })
}

/// Strip copied row trees before preflight and ID checks. Some callers rename only the outer group,
/// leaving its original row IDs in the payload; in that case the document identifies their owner.
fn strip_pasted_rows(doc: &CanvasDocument, node: &mut Node) -> DltResult<()> {
    let Some(spec) = &node.ext.repeat else {
        for child in &mut node.children { strip_pasted_rows(doc, child)?; }
        return Ok(());
    };
    let Some(first) = node.children.first() else { return Ok(()) };
    let mut prefix = format!("{}-", node.id);
    let original = doc.pages.iter().flat_map(|page| model::flatten(&page.nodes))
        .filter(|group| group.ext.repeat.as_ref() == Some(spec))
        .filter(|group| group.children.iter().any(|row| row.id == first.id)
            || (!first.id.starts_with(&prefix) && first.id.strip_prefix(&group.id).is_some_and(|suffix| suffix.starts_with('-'))))
        .max_by_key(|group| group.id.len());
    if let Some(group) = original {
        prefix = format!("{}-", group.id);
    } else if !first.id.starts_with(&prefix) {
        // A paste into another document may keep the copied row IDs but rename the group.
        if let Some(table) = model::find_node_in_doc(doc, &spec.table_id).and_then(|node| node.ext.table.as_ref()) {
            let rows: BTreeSet<&str> = table.rows.iter().map(|row| row.id.as_str()).collect();
            if let Some(old_prefix) = first.id.match_indices('-').map(|(i, _)| &first.id[..=i]).find(|prefix|
                node.children.iter().all(|row| row.id.strip_prefix(*prefix).is_some_and(|id| rows.contains(id))))
            {
                prefix = old_prefix.to_owned();
            }
        }
    }
    let stored_rows: BTreeSet<&str> = original.into_iter().flat_map(|group| &group.children).map(|row| row.id.as_str()).collect();
    let mut seen = BTreeSet::new();
    for row in &node.children {
        let belongs = row.id.strip_prefix(&prefix).is_some_and(valid_id) && !row.id.contains("::")
            && (stored_rows.contains(row.id.as_str()) || model::find_node_in_doc(doc, &row.id).is_none());
        let copied_prefix = format!("{}::", row.id);
        let instance_matches = row.ext.instance.as_ref().is_some_and(|reference|
            reference.component_page_id == spec.component_page_id && spec.bindings.iter().all(|binding|
                reference.overrides.get(&binding.source_node_id).is_some_and(|value| value.text.is_some())));
        if !belongs || !seen.insert(row.id.as_str()) || !instance_matches || row.kind != NodeKind::Group
            || !row.text.is_empty() || row.asset.is_some() || row.layout.is_some() || row.ext.set_fields() != ["instance"]
            || row.style != Default::default() || !row.interactions.is_empty() || !row.variants.is_empty()
            || model::flatten(&row.children).iter().any(|child| !child.id.starts_with(&copied_prefix))
        {
            return Err(model::fail_with(ErrorCode::InvalidArgument, CANVAS_REPEAT_GROUP_NOT_EMPTY,
                format!("node {} is not a generated row of the pasted repeat group", row.id), Some(&node.id)));
        }
    }
    for interaction in &mut node.interactions {
        let target = match interaction {
            Interaction::ToggleVisibility { node_id } | Interaction::SetVariant { node_id, .. } => node_id,
            _ => continue,
        };
        if let Some(suffix) = target.strip_prefix(&prefix) {
            if model::find_node(&node.children, target).is_some() {
                *target = format!("{}-{suffix}", node.id);
            }
        }
    }
    node.children.clear();
    Ok(())
}

pub(super) fn prepare_pastes<'a>(doc: &CanvasDocument, operations: &'a [Operation]) -> DltResult<Cow<'a, [Operation]>> {
    fn carries_rows(nodes: &[Node]) -> bool {
        nodes.iter().any(|node| !node.children.is_empty() && (node.ext.repeat.is_some() || carries_rows(&node.children)))
    }
    let mut result = Cow::Borrowed(operations);
    for (index, operation) in operations.iter().enumerate() {
        let nodes = match operation {
            Operation::AddNode { node, .. } => std::slice::from_ref(node),
            Operation::InsertPage { page, .. } => &page.nodes,
            _ => continue,
        };
        if !carries_rows(nodes) { continue; }
        match &mut result.to_mut()[index] {
            Operation::AddNode { node, .. } => strip_pasted_rows(doc, node)?,
            Operation::InsertPage { page, .. } => for node in &mut page.nodes { strip_pasted_rows(doc, node)?; },
            _ => unreachable!(),
        }
    }
    Ok(result)
}

fn read_only(id: &str, message: &str) -> DltError {
    let mut error = fail(ErrorCode::UnsupportedOperation, INSTANCE_CHILD_READ_ONLY, message);
    error.affected_ids.push(id.to_string());
    if let Some(diagnostic) = error.diagnostics.last_mut() { diagnostic.node_id = Some(id.to_string()); }
    error
}

/// Refuses edits a repeat expansion would overwrite: the instances it owns, children added to a repeat group, and the text
/// of a bound node's copy (the row value is its text). Style and unbound text edits of a copy stay overrides.
pub(crate) fn guard_operation(doc: &CanvasDocument, operation: &Operation) -> DltResult<()> {
    use Operation::*;
    let groups: BTreeSet<String> = repeat_groups(doc).into_iter().map(|(_, id)| id).collect();
    if groups.is_empty() && !matches!(operation, SetGroupRepeat { .. } | SetGroupCondition { .. }) { return Ok(()); }
    let instances = repeat_instance_ids(doc);
    let parent = match operation {
        AddNode { parent_id, .. } | MoveNode { parent_id, .. } | InstantiateComponent { parent_id, .. } => parent_id.as_deref(),
        _ => None,
    };
    if let Some(parent) = parent.filter(|parent| groups.contains(*parent)) {
        return Err(read_only(parent, "a repeat group's children come from its table; nodes cannot be added to it"));
    }
    for target in ops::node_targets(operation) {
        if instances.contains(target) {
            return Err(read_only(target, "an instance of a repeat group is made from its table and cannot be edited directly"));
        }
        if matches!(operation, SetGroupRepeat { .. } | SetGroupCondition { .. }) && components::is_instance_subtree(doc, target) {
            return Err(read_only(target, "a node inside a component instance cannot take a repeat spec or a condition"));
        }
        if matches!(operation, SetText { .. } | ReplaceText { .. }) {
            if let Some((instance_id, source_id)) = components::instance_source(doc, target).filter(|(instance_id, _)| instances.contains(instance_id)) {
                let group = doc.pages.iter().flat_map(|page| model::flatten(&page.nodes))
                    .find(|node| node.ext.repeat.is_some() && node.children.iter().any(|child| child.id == instance_id));
                if group.and_then(|group| group.ext.repeat.as_ref()).is_some_and(|spec| spec.bindings.iter().any(|binding| binding.source_node_id == source_id)) {
                    return Err(read_only(target, "the text of a bound node comes from the table row"));
                }
            }
        }
    }
    Ok(())
}

fn expected_conflict(node_id: &str, what: &str) -> DltError {
    model::fail_with(ErrorCode::RevisionConflict, CANVAS_REPEAT_EXPECTED, format!("the {what} of {node_id} is not the expected one"), Some(node_id))
}

/// SetGroupRepeat. A new or changed spec starts from no instances; the expansion at the end of the change fills them.
pub(crate) fn set_group_repeat(doc: &mut CanvasDocument, node_id: &str, expected: Option<&RepeatSpec>, repeat: Option<&RepeatSpec>) -> DltResult<Vec<String>> {
    let node = model::find_node_in_doc(doc, node_id).ok_or_else(|| components::not_found("node", node_id))?;
    ops::ensure_unlocked(node)?;
    if node.ext.repeat.as_ref() != expected { return Err(expected_conflict(node_id, "repeat spec")); }
    if node.kind != NodeKind::Group {
        return Err(model::fail_with(ErrorCode::InvalidArgument, CANVAS_REPEAT_TARGET, "only a Group can repeat a component", Some(node_id)));
    }
    if expected.is_none() && repeat.is_some() && !node.children.is_empty() {
        return Err(model::fail_with(ErrorCode::InvalidArgument, CANVAS_REPEAT_GROUP_NOT_EMPTY, "a repeat group must be empty; its children come from the table", Some(node_id)));
    }
    if expected == repeat { return Ok(Vec::new()); }
    let mut removed = BTreeSet::new();
    for child in &node.children { model::subtree_ids(child, &mut removed); }
    let node = model::find_node_in_doc_mut(doc, node_id).expect("found above");
    node.ext.repeat = repeat.cloned();
    node.children.clear();
    doc.bindings.retain(|binding| !removed.contains(&binding.node_id));
    let mut changed = vec![node_id.to_string()];
    changed.extend(removed);
    Ok(changed)
}

/// SetGroupCondition: a Group of a component page that no ordinary instance uses.
pub(crate) fn set_group_condition(doc: &mut CanvasDocument, node_id: &str, expected: Option<&ShowCondition>, condition: Option<&ShowCondition>) -> DltResult<Vec<String>> {
    let page = page_of(doc, node_id).ok_or_else(|| components::not_found("node", node_id))?;
    let node = model::find_node(&page.nodes, node_id).expect("page_of found the node");
    ops::ensure_unlocked(node)?;
    if node.ext.condition.as_ref() != expected { return Err(expected_conflict(node_id, "show condition")); }
    if let Some(condition) = condition {
        let scope = |message: &str| model::fail_with(ErrorCode::InvalidArgument, CANVAS_REPEAT_CONDITION_SCOPE, message, Some(node_id));
        if node.kind != NodeKind::Group { return Err(scope("a show condition belongs to a Group")); }
        if page.role != Some(PageRole::Component) { return Err(scope("a show condition belongs to a group in a repeated component page")); }
        let repeat_instances = repeat_instance_ids(doc);
        if doc.pages.iter().flat_map(|page| model::flatten(&page.nodes)).any(|other| {
            other.ext.instance.as_ref().is_some_and(|instance| instance.component_page_id == page.id) && !repeat_instances.contains(&other.id)
        }) {
            return Err(scope("this component page has ordinary instances; a show condition is for repeat groups only"));
        }
        if let Some(problem) = condition_problem(condition, node_id) { return Err(problem.into_error()); }
    }
    if expected == condition { return Ok(Vec::new()); }
    model::find_node_in_doc_mut(doc, node_id).expect("found above").ext.condition = condition.cloned();
    Ok(vec![node_id.to_string()])
}

/// The feature key condition: a repeat spec, a condition, or a hidden override anywhere.
pub(crate) fn uses_repeat_groups(node: &Node) -> bool {
    node.ext.repeat.is_some() || node.ext.condition.is_some()
        || node.ext.instance.as_ref().is_some_and(|instance| instance.overrides.values().any(|over| over.hidden.is_some()))
}
