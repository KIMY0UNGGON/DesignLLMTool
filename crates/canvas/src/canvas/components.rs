//! Local Canvas component instances. Children are materialized with parent-relative bounds so the
//! instance Group's frame controls placement without rewriting source geometry.

use super::{guard, model, ops, repeat};
use crate::common::{blocking, invalid, valid_id, MAX_NODES, MAX_PAGES};
use designllm_contracts::{
    BindTarget, CanvasDocument, CanvasTable, Diagnostic, DltError, DltResult, ErrorCode, InstanceRef, Interaction, Node, NodeExt, NodeKind,
    NodeOverride, Operation, PageRole, Rect, StyleField, TokenBinding, TokenRef,
};
use std::collections::{BTreeMap, BTreeSet};

const COMPONENT_INSTANCE_TABLE_STRUCTURE_MESSAGE: &str = "edit the component source page to change table rows or cells in an instance";
const COMPONENT_INSTANCE_TABLE_WIDTH_MESSAGE: &str = "set table column widths on the component source page; instance-specific widths are not supported";

// shortcut: instances inherit table structure; add persisted table overrides before enabling local row/cell edits.
fn component_instance_table_structure_error(node_id: &str) -> DltError {
    let mut error = unsupported("COMPONENT_INSTANCE_TABLE_STRUCTURE_UNSUPPORTED", COMPONENT_INSTANCE_TABLE_STRUCTURE_MESSAGE);
    error.affected_ids.push(node_id.to_string());
    if let Some(diagnostic) = error.diagnostics.last_mut() { diagnostic.node_id = Some(node_id.to_string()); }
    error
}

fn component_instance_table_width_error(node_id: &str) -> DltError {
    let mut error = unsupported("COMPONENT_INSTANCE_TABLE_WIDTH_UNSUPPORTED", COMPONENT_INSTANCE_TABLE_WIDTH_MESSAGE);
    error.affected_ids.push(node_id.to_string());
    if let Some(diagnostic) = error.diagnostics.last_mut() { diagnostic.node_id = Some(node_id.to_string()); }
    error
}

fn component_table_override_delete_error(source_cell_id: &str, instance_ids: &[String]) -> DltError {
    let message = format!("source table cell {source_cell_id} has a local override; detach the affected component instances before deleting it");
    let mut error = DltError::new(ErrorCode::ContentLocked, message.clone());
    error.affected_ids.push(source_cell_id.to_string());
    error.affected_ids.extend(instance_ids.iter().map(|instance_id| format!("{instance_id}::{source_cell_id}")));
    error.diagnostics.push(Diagnostic {
        code: "COMPONENT_TABLE_OVERRIDE_DELETE_BLOCKED".into(), message, node_id: Some(source_cell_id.to_string()), blocking: true, params: Default::default(),
    });
    error
}


pub(crate) fn set_page_role(doc: &mut CanvasDocument, page_id: &str, role: Option<PageRole>) -> DltResult<Vec<String>> {
    let page = doc.pages.iter().find(|page| page.id == page_id).ok_or_else(|| not_found("page", page_id))?;
    if role == Some(PageRole::Component) {
        if model::flatten(&page.nodes).iter().any(|node| node.ext.instance.is_some()) {
            return Err(unsupported("NESTED_INSTANCE_UNSUPPORTED", "a component source cannot contain an instance"));
        }
    } else if doc.pages.iter().flat_map(|page| model::flatten(&page.nodes)).any(|node| {
        node.ext.instance.as_ref().is_some_and(|instance| instance.component_page_id == page_id)
    }) {
        return Err(invalid("COMPONENT_IN_USE", format!("page {page_id} is used by a component instance")));
    }
    doc.pages.iter_mut().find(|page| page.id == page_id).expect("page was checked above").role = role;
    Ok(vec![page_id.to_string()])
}

pub(crate) fn instantiate(
    doc: &mut CanvasDocument,
    page_id: &str,
    parent_id: Option<&str>,
    instance_id: &str,
    component_page_id: &str,
    bounds: Rect,
) -> DltResult<Vec<String>> {
    if !valid_id(instance_id) || instance_id.contains("::") {
        return Err(invalid("COMPONENT_INSTANCE_ID", "instance id is invalid or uses the reserved '::' separator"));
    }
    let target_index = doc.pages.iter().position(|page| page.id == page_id).ok_or_else(|| not_found("page", page_id))?;
    let source_index = doc.pages.iter().position(|page| page.id == component_page_id).ok_or_else(|| not_found("component page", component_page_id))?;
    if target_index == source_index || doc.pages[target_index].role.is_some() {
        return Err(invalid("COMPONENT_TARGET_PAGE", "an instance must be placed on a regular page"));
    }
    let source = &doc.pages[source_index];
    if source.role != Some(PageRole::Component) {
        return Err(invalid("COMPONENT_SOURCE_PAGE", format!("page {component_page_id} is not a component source")));
    }
    if model::flatten(&source.nodes).iter().any(|node| node.ext.instance.is_some()) {
        return Err(unsupported("NESTED_INSTANCE_UNSUPPORTED", "component sources cannot contain instances"));
    }
    model::validate_bounds(&bounds, NodeKind::Group)?;

    let parent_depth = match parent_id {
        Some(parent_id) => {
            let parent = model::find_node(&doc.pages[target_index].nodes, parent_id).ok_or_else(|| not_found("node", parent_id))?;
            if !matches!(parent.kind, NodeKind::Group | NodeKind::Rectangle) {
                return Err(invalid("NODE_CHILDREN", "only group and rectangle nodes can contain an instance"));
            }
            ensure_unlocked_ancestry(&doc.pages[target_index].nodes, parent_id)?;
            if has_instance_ancestor(&doc.pages[target_index].nodes, parent_id) {
                return Err(unsupported("NESTED_INSTANCE_UNSUPPORTED", "an instance cannot be placed inside another instance"));
            }
            depth_of(&doc.pages[target_index].nodes, parent_id).unwrap_or(0) + 1
        }
        None => 0,
    };

    let existing: BTreeSet<String> = model::all_ids(doc).into_iter().collect();
    if existing.contains(instance_id) {
        return Err(duplicate_id(instance_id));
    }

    let derived_ids = derived_id_map(instance_id, &source.nodes)?;
    let mut new_ids = BTreeSet::from([instance_id.to_string()]);
    for id in derived_ids.values() {
        if existing.contains(id) || !new_ids.insert(id.clone()) {
            return Err(duplicate_id(&id));
        }
    }
    if existing.len() + new_ids.len() > MAX_NODES + MAX_PAGES {
        return Err(invalid("CANVAS_TOO_MANY_NODES", "component instance would exceed the node limit"));
    }

    let overrides = BTreeMap::new();
    let children = source.nodes.iter().map(|node| materialize_node(node, &derived_ids, &overrides)).collect();
    let group = Node {
        id: instance_id.to_string(), kind: NodeKind::Group, bounds, text: String::new(), style: Default::default(), asset: None,
        children, layout: None, interactions: Vec::new(), variants: BTreeMap::new(),
        ext: NodeExt { instance: Some(InstanceRef { component_page_id: component_page_id.to_string(), overrides: BTreeMap::new() }), ..Default::default() },
    };
    let mut diagnostics = Vec::new();
    model::validate_node(&group, parent_depth, &mut diagnostics);
    if let Some(diagnostic) = diagnostics.into_iter().find(|diagnostic| diagnostic.blocking) {
        return Err(DltError::invalid(diagnostic.message));
    }

    let bindings: Vec<TokenBinding> = doc.bindings.iter().filter_map(|binding| {
        derived_ids.get(binding.node_id.as_str()).map(|id| TokenBinding { node_id: id.clone(), target: binding.target.clone(), token: binding.token.clone() })
    }).collect();
    let mut changed = vec![instance_id.to_string()];
    changed.extend(derived_ids.values().cloned());
    match parent_id {
        Some(parent_id) => model::find_node_mut(&mut doc.pages[target_index].nodes, parent_id).expect("parent was checked above").children.push(group),
        None => doc.pages[target_index].nodes.push(group),
    }
    doc.bindings.extend(bindings);
    Ok(changed)
}

pub(crate) fn canonicalize_instance_add(
    doc: &CanvasDocument,
    page_id: &str,
    instance: &Node,
) -> DltResult<Option<(Node, Vec<TokenBinding>)>> {
    let Some(reference) = &instance.ext.instance else { return Ok(None) };
    if instance.ext.table.is_some() {
        return Err(unsupported("COMPONENT_INSTANCE_TABLE_ROOT_UNSUPPORTED", "table metadata belongs to a source child, not the component instance root"));
    }
    if instance.kind != NodeKind::Group || !valid_id(&instance.id) || instance.id.contains("::") {
        return Err(invalid("COMPONENT_INSTANCE_ID", "a duplicated component instance must be a Group with a non-reserved id"));
    }
    let target = doc.pages.iter().find(|page| page.id == page_id).ok_or_else(|| not_found("page", page_id))?;
    if target.role.is_some() { return Err(invalid("COMPONENT_TARGET_PAGE", "an instance must be placed on a regular page")); }
    let source = doc.pages.iter().find(|page| page.id == reference.component_page_id).ok_or_else(|| not_found("component page", &reference.component_page_id))?;
    if source.role != Some(PageRole::Component) || model::flatten(&source.nodes).iter().any(|node| node.ext.instance.is_some()) {
        return Err(unsupported("NESTED_INSTANCE_UNSUPPORTED", "only a local, non-nested component instance can be duplicated"));
    }
    let ids = derived_id_map(&instance.id, &source.nodes)?;
    let existing: BTreeSet<String> = model::all_ids(doc).into_iter().collect();
    if existing.contains(&instance.id) { return Err(duplicate_id(&instance.id)); }
    if let Some(id) = ids.values().find(|id| existing.contains(*id)) { return Err(duplicate_id(id)); }
    let mut old_to_new = BTreeMap::new();
    map_copied_children(&source.nodes, &instance.children, &ids, &mut old_to_new)?;
    let children = source.nodes.iter().map(|node| materialize_node(node, &ids, &reference.overrides)).collect();
    let mut interactions = instance.interactions.clone();
    for interaction in &mut interactions {
        match interaction {
            Interaction::ToggleVisibility { node_id } | Interaction::SetVariant { node_id, .. } => {
                if let Some(mapped) = old_to_new.get(node_id) { node_id.clone_from(mapped); }
            }
            _ => {}
        }
    }
    let canonical = Node {
        id: instance.id.clone(), kind: NodeKind::Group, bounds: instance.bounds, text: instance.text.clone(), style: instance.style.clone(),
        asset: instance.asset.clone(), children, layout: instance.layout.clone(), interactions, variants: instance.variants.clone(), ext: instance.ext.clone(),
    };
    let bindings = doc.bindings.iter().filter_map(|binding| {
        let override_value = reference.overrides.get(&binding.node_id);
        if override_value.is_some_and(|over| binding_is_overridden(&binding.target, over)) { return None }
        ids.get(&binding.node_id).map(|id| TokenBinding { node_id: id.clone(), target: binding.target.clone(), token: binding.token.clone() })
    }).collect();
    Ok(Some((canonical, bindings)))
}

fn map_copied_children(
    source: &[Node], copied: &[Node], ids: &BTreeMap<String, String>, old_to_new: &mut BTreeMap<String, String>,
) -> DltResult<()> {
    if source.len() != copied.len() {
        return Err(invalid("COMPONENT_INSTANCE_STALE", "a duplicated instance has a different child structure from its source"));
    }
    for (source, copied) in source.iter().zip(copied) {
        let id = ids.get(&source.id).expect("source id map was built from the source tree");
        if old_to_new.insert(copied.id.clone(), id.clone()).is_some() {
            return Err(duplicate_id(&copied.id));
        }
        map_copied_children(&source.children, &copied.children, ids, old_to_new)?;
    }
    Ok(())
}

pub(crate) fn preflight_table_operations(doc: &CanvasDocument, operations: &[Operation]) -> DltResult<()> {
    use Operation::*;
    let mut source_pages: BTreeSet<String> = doc.pages.iter().filter(|page| page.role == Some(PageRole::Component)).map(|page| page.id.clone()).collect();
    let mut node_pages: BTreeMap<String, String> = BTreeMap::new();
    let mut instances: Vec<(String, String)> = Vec::new();
    let mut instance_overrides: Vec<(String, String, BTreeSet<String>)> = Vec::new();
    for page in &doc.pages {
        for node in model::flatten(&page.nodes) {
            node_pages.insert(node.id.clone(), page.id.clone());
            if let Some(instance) = &node.ext.instance {
                instances.push((node.id.clone(), instance.component_page_id.clone()));
                instance_overrides.push((node.id.clone(), instance.component_page_id.clone(), instance.overrides.keys().cloned().collect()));
            }
        }
    }
    for operation in operations {
        match operation {
            SetPageRole { page_id, role: Some(PageRole::Component) } => { source_pages.insert(page_id.clone()); }
            InstantiateComponent { instance_id, component_page_id, .. } => {
                source_pages.insert(component_page_id.clone());
                instances.push((instance_id.clone(), component_page_id.clone()));
                instance_overrides.push((instance_id.clone(), component_page_id.clone(), BTreeSet::new()));
            }
            AddNode { page_id, node, .. } => {
                for child in model::flatten(std::slice::from_ref(node)) {
                    node_pages.insert(child.id.clone(), page_id.clone());
                    if let Some(instance) = &child.ext.instance {
                        instances.push((child.id.clone(), instance.component_page_id.clone()));
                        source_pages.insert(instance.component_page_id.clone());
                        instance_overrides.push((child.id.clone(), instance.component_page_id.clone(), instance.overrides.keys().cloned().collect()));
                    }
                }
            }
            InsertPage { page, .. } => {
                if page.role == Some(PageRole::Component) { source_pages.insert(page.id.clone()); }
                for child in model::flatten(&page.nodes) {
                    node_pages.insert(child.id.clone(), page.id.clone());
                    if let Some(instance) = &child.ext.instance {
                        instances.push((child.id.clone(), instance.component_page_id.clone()));
                        source_pages.insert(instance.component_page_id.clone());
                        instance_overrides.push((child.id.clone(), instance.component_page_id.clone(), instance.overrides.keys().cloned().collect()));
                    }
                }
            }
            _ => {}
        }
    }
    let mut table_cells: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for page in doc.pages.iter().filter(|page| source_pages.contains(&page.id)) {
        for node in model::flatten(&page.nodes) {
            if let Some(table) = &node.ext.table {
                table_cells.entry(page.id.clone()).or_default().extend(table.rows.iter().flat_map(|row| row.cell_ids.iter().cloned()));
            }
        }
    }
    let mut removed_source_nodes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for operation in operations {
        let (node_id, destination_page) = match operation {
            RemoveNode { node_id } => (node_id.as_str(), None),
            MoveNode { node_id, page_id, .. } => (node_id.as_str(), Some(page_id.as_str())),
            _ => continue,
        };
        let Some(source_page_id) = node_pages.get(node_id).filter(|page_id| source_pages.contains(*page_id)) else { continue };
        if destination_page.is_some_and(|page_id| page_id == source_page_id) { continue; }
        if let Some(node) = model::find_node(doc.pages.iter().find(|page| page.id == *source_page_id).map(|page| page.nodes.as_slice()).unwrap_or_default(), node_id) {
            removed_source_nodes.entry(source_page_id.clone()).or_default().extend(model::flatten(std::slice::from_ref(node)).into_iter().map(|node| node.id.clone()));
        }
    }
    for (source_page_id, removed_nodes) in removed_source_nodes {
        let Some(cells) = table_cells.get(&source_page_id) else { continue };
        for source_cell_id in cells.intersection(&removed_nodes) {
            let owners: Vec<String> = instance_overrides.iter().filter_map(|(instance_id, component_page_id, override_ids)| {
                (component_page_id == &source_page_id && override_ids.contains(source_cell_id)).then(|| instance_id.clone())
            }).collect();
            if !owners.is_empty() { return Err(component_table_override_delete_error(source_cell_id, &owners)); }
        }
    }
    let mut component_table_sources = BTreeSet::new();
    for operation in operations {
        let table_id = match operation {
            SetCanvasTableStructure { table_id, .. } | SetCanvasTableColumnWidths { table_id, .. } => table_id,
            _ => continue,
        };
        if is_instance_subtree(doc, table_id) || instances.iter().any(|(instance_id, source_page_id)| {
            let prefix = format!("{instance_id}::");
            table_id.strip_prefix(&prefix).is_some_and(|source_id| {
                doc.pages.iter().find(|page| page.id == *source_page_id)
                    .and_then(|page| model::find_node(&page.nodes, source_id))
                    .is_some_and(|node| node.ext.table.is_some())
            })
        }) {
            return Err(match operation {
                SetCanvasTableColumnWidths { .. } => component_instance_table_width_error(table_id),
                _ => component_instance_table_structure_error(table_id),
            });
        }
        if let SetCanvasTableStructure { expected, table, .. } = operation {
            if let Some(page_id) = node_pages.get(table_id).filter(|page_id| source_pages.contains(*page_id)) {
                let before = model::covered_table_cell_ids(expected).map_err(|problem| invalid("TABLE_CELL_SPANS", problem))?;
                let after = model::covered_table_cell_ids(table).map_err(|problem| invalid("TABLE_CELL_SPANS", problem))?;
                let newly_covered: BTreeSet<&str> = after.difference(&before).copied().collect();
                if instance_overrides.iter().any(|(_, component_page_id, overrides)| {
                    component_page_id == page_id && newly_covered.iter().any(|cell_id| overrides.contains(*cell_id))
                }) {
                    return Err(invalid("TABLE_SPAN_COMPONENT_OVERRIDE", "a source table cell with a local instance override cannot be covered by a merged cell"));
                }
                component_table_sources.insert(page_id.clone());
            }
        }
    }
    if !component_table_sources.is_empty() {
        for operation in operations {
            let mut targets = Vec::new();
            match operation {
                SetText { node_id, .. } => targets.push(node_id.as_str()),
                ReplaceText { range, .. } => targets.push(range.paragraph_id.as_str()),
                SetStyle { node_ids, .. } => targets.extend(node_ids.iter().map(String::as_str)),
                DetachInstance { instance_id } => {
                    if instances.iter().any(|(id, _)| id == instance_id) {
                        return Err(unsupported("COMPONENT_TABLE_BATCH_ORDER_UNSUPPORTED", "edit or detach component instances in a separate change after changing a source table structure"));
                    }
                }
                _ => {}
            }
            if targets.iter().any(|target| instances.iter().any(|(instance_id, _)| {
                target.strip_prefix(&format!("{instance_id}::")).is_some()
            })) {
                return Err(unsupported("COMPONENT_TABLE_BATCH_ORDER_UNSUPPORTED", "edit component instance children in a separate change after changing a source table structure"));
            }
        }
    }
    Ok(())
}

pub(crate) fn guard_operation(doc: &CanvasDocument, operation: &designllm_contracts::Operation) -> DltResult<()> {
    use designllm_contracts::Operation::*;
    let (ids, allows_override): (Vec<&str>, bool) = match operation {
        UpdateFigureLink { link } => (vec![link.target_node_id.as_str()], false),
        ReplaceText { range, .. } => (vec![range.paragraph_id.as_str()], true),
        SetText { node_id, .. } => (vec![node_id], true),
        ReplaceImageAsset { node_id, .. } => {
            guard_image_replacement_target(doc, node_id)?;
            (vec![node_id.as_str()], false)
        }
        SetCanvasImageCrop { node_id, .. } => {
            guard_image_crop_target(doc, node_id)?;
            (vec![node_id.as_str()], false)
        }
        SetStyle { node_ids, .. } => (node_ids.iter().map(String::as_str).collect(), true),
        SetGeometry { node_id, .. } | RemoveNode { node_id } | SetLayout { node_id, .. }
        | SetInteraction { node_id, .. } | SetInteractions { node_id, .. } | SetVariantStyle { node_id, .. }
        | SetZOrder { node_id, .. } | SetNodeList { node_id, .. } | DetachInstance { instance_id: node_id } => (vec![node_id], false),
        SetNodeAppearance { node_ids, .. } | SetNodeInfo { node_ids, .. } => (node_ids.iter().map(String::as_str).collect(), false),
        MoveNode { node_id, parent_id, .. } => {
            if parent_id.as_deref().is_some_and(|parent| is_instance_subtree(doc, parent)) {
                return Err(unsupported(repeat::INSTANCE_CHILD_READ_ONLY, "nodes cannot be moved inside a materialized component instance"));
            }
            (std::iter::once(node_id.as_str()).chain(parent_id.as_deref()).collect(), false)
        }
        AddNode { parent_id, .. } => {
            if parent_id.as_deref().is_some_and(|parent| is_instance_subtree(doc, parent)) {
                return Err(unsupported(repeat::INSTANCE_CHILD_READ_ONLY, "nodes cannot be inserted inside a materialized component instance"));
            }
            (parent_id.as_deref().into_iter().collect(), false)
        }
        InsertPage { .. } => (Vec::new(), false),
        SetCanvasTableStructure { table_id, .. } => {
            if is_instance_subtree(doc, table_id) {
                return Err(component_instance_table_structure_error(table_id));
            }
            (Vec::new(), false)
        }
        SetCanvasTableColumnWidths { table_id, .. } => {
            if is_instance_subtree(doc, table_id) {
                return Err(component_instance_table_width_error(table_id));
            }
            (Vec::new(), false)
        }
        BindToken { node_ids, .. } | UnbindToken { node_ids, .. } => (node_ids.iter().map(String::as_str).collect(), false),
        _ => (Vec::new(), false),
    };
    if !allows_override {
        if let Some(id) = ids.into_iter().find(|id| is_materialized_child(doc, id)) {
            let message = format!("component child {id} is read-only until an explicit override is applied");
            let mut error = DltError::unsupported(&message);
            error.diagnostics.push(Diagnostic { code: repeat::INSTANCE_CHILD_READ_ONLY.into(), message, node_id: Some(id.to_string()), blocking: true, params: Default::default() });
            error.affected_ids.push(id.to_string());
            return Err(error);
        }
    }
    Ok(())
}

pub(crate) fn guard_image_replacement_target(doc: &CanvasDocument, node_id: &str) -> DltResult<()> {
    if is_instance_subtree(doc, node_id) {
        return Err(unsupported("IMAGE_REPLACE_INSTANCE_UNSUPPORTED", "images inside a component instance cannot be replaced yet"));
    }
    if doc.pages.iter().any(|page| page.role == Some(PageRole::Component) && model::find_node(&page.nodes, node_id).is_some()) {
        return Err(unsupported("IMAGE_REPLACE_COMPONENT_SOURCE_UNSUPPORTED", "images in a component source cannot be replaced yet"));
    }
    Ok(())
}

pub(crate) fn guard_image_crop_target(doc: &CanvasDocument, node_id: &str) -> DltResult<()> {
    if is_instance_subtree(doc, node_id) {
        return Err(unsupported("IMAGE_CROP_INSTANCE_UNSUPPORTED", "images inside a component instance cannot be cropped yet"));
    }
    if doc.pages.iter().any(|page| page.role == Some(PageRole::Component) && model::find_node(&page.nodes, node_id).is_some()) {
        return Err(unsupported("IMAGE_CROP_COMPONENT_SOURCE_UNSUPPORTED", "images in a component source cannot be cropped yet"));
    }
    Ok(())
}

pub(crate) fn operation_touches_source(doc: &CanvasDocument, operation: &designllm_contracts::Operation) -> bool {
    use designllm_contracts::Operation::*;
    let in_source = |id: &str| doc.pages.iter().any(|page| page.role == Some(PageRole::Component) && model::find_node(&page.nodes, id).is_some());
    let source_page = |id: &str| doc.pages.iter().any(|page| page.id == id && page.role == Some(PageRole::Component));
    match operation {
        AddNode { page_id, .. } => source_page(page_id),
        InsertPage { page, .. } => page.role == Some(PageRole::Component),
        SetPageRole { role: Some(PageRole::Component), .. } => true,
        MoveNode { node_id, page_id, parent_id, .. } => {
            in_source(node_id) || source_page(page_id) || parent_id.as_deref().is_some_and(|id| in_source(id))
        }
        other => ops::writes_matching_node(doc, other, in_source),
    }
}

pub(crate) fn operation_needs_fresh_instance(doc: &CanvasDocument, operation: &Operation) -> bool {
    match operation {
        Operation::SetText { node_id, .. } => is_materialized_child(doc, node_id),
        Operation::ReplaceText { range, .. } => is_materialized_child(doc, &range.paragraph_id),
        Operation::ReplaceImageAsset { node_id, .. } => is_materialized_child(doc, node_id),
        Operation::SetStyle { node_ids, .. } => node_ids.iter().any(|id| is_materialized_child(doc, id)),
        Operation::DetachInstance { instance_id } => model::find_node_in_doc(doc, instance_id).is_some_and(|node| node.ext.instance.is_some()),
        _ => false,
    }
}

pub(super) struct InstanceTokenState {
    instance_id: String,
    node_id: String,
    style: designllm_contracts::Style,
    layout: Option<serde_json::Value>,
    bindings: Vec<TokenBinding>,
}

pub(super) fn token_states_before(doc: &CanvasDocument, operation: &Operation) -> Vec<InstanceTokenState> {
    let targets: BTreeSet<String> = match operation {
        Operation::PutToken { token_id, .. } => doc.bindings.iter()
            .filter(|binding| binding.token.source == designllm_contracts::TokenSource::Local && binding.token.token_id == *token_id)
            .map(|binding| binding.node_id.clone()).collect(),
        Operation::SetTokenMode { .. } => doc.bindings.iter()
            .filter(|binding| binding.token.source == designllm_contracts::TokenSource::Local)
            .map(|binding| binding.node_id.clone()).collect(),
        _ => BTreeSet::new(),
    };
    targets.into_iter().filter_map(|node_id| {
        if !is_materialized_child(doc, &node_id) { return None }
        let (instance_id, _) = instance_source(doc, &node_id)?;
        let node = model::find_node_in_doc(doc, &node_id)?;
        Some(InstanceTokenState {
            instance_id, node_id: node_id.clone(), style: node.style.clone(),
            layout: serde_json::to_value(&node.layout).ok(),
            bindings: doc.bindings.iter().filter(|binding| binding.node_id == node_id).cloned().collect(),
        })
    }).collect()
}

pub(super) fn check_token_state_after(doc: &CanvasDocument, before: Vec<InstanceTokenState>) -> DltResult<()> {
    for state in before {
        let Some(node) = model::find_node_in_doc(doc, &state.node_id) else { continue };
        let layout = serde_json::to_value(&node.layout).ok();
        let bindings: Vec<TokenBinding> = doc.bindings.iter().filter(|binding| binding.node_id == state.node_id).cloned().collect();
        if state.style != node.style || state.layout != layout || state.bindings != bindings {
            let root = model::find_node_in_doc(doc, &state.instance_id).expect("instance root remains during a token operation");
            ops::ensure_unlocked(root)?;
            ensure_unlocked_ancestry(&root.children, &state.node_id)?;
        }
    }
    Ok(())
}

pub(crate) fn ensure_unlocked_materialized_updates(doc: &CanvasDocument, changed_ids: &[String]) -> DltResult<()> {
    for node_id in changed_ids {
        let Some((instance_id, _)) = instance_source(doc, node_id) else { continue };
        let root = model::find_node_in_doc(doc, &instance_id).expect("materialized node has an instance root");
        ops::ensure_unlocked(root)?;
        ensure_unlocked_ancestry(&root.children, node_id)?;
    }
    Ok(())
}

pub(crate) fn preflight_source_binding_change(
    doc: &CanvasDocument,
    source_node_ids: &[String],
    target: &BindTarget,
    token: Option<&TokenRef>,
) -> DltResult<()> {
    for source_node_id in source_node_ids {
        let Some(source_page) = doc.pages.iter().find(|page| {
            page.role == Some(PageRole::Component) && model::find_node(&page.nodes, source_node_id).is_some()
        }) else { continue };
        for instance in doc.pages.iter().flat_map(|page| model::flatten(&page.nodes)).filter(|node| {
            node.ext.instance.as_ref().is_some_and(|reference| reference.component_page_id == source_page.id)
        }) {
            let derived_id = format!("{}::{source_node_id}", instance.id);
            if model::find_node(&instance.children, &derived_id).is_none() { continue; }
            let existing = doc.bindings.iter().find(|binding| binding.node_id == derived_id && binding.target == *target);
            let changing = match token {
                Some(token) => existing.is_none_or(|binding| binding.token != *token),
                None => existing.is_some(),
            };
            if changing { ensure_unlocked_materialized_updates(doc, &[derived_id])?; }
        }
    }
    Ok(())
}

pub(crate) fn capture_override(doc: &mut CanvasDocument, operation: &Operation) -> DltResult<Vec<String>> {
    match operation {
        Operation::SetText { node_id, .. } => capture_text_override(doc, node_id),
        Operation::ReplaceText { range, .. } => capture_text_override(doc, &range.paragraph_id),
        Operation::SetStyle { node_ids, patch } => {
            let mut changed = Vec::new();
            for node_id in node_ids {
                if let Some((instance_id, source_id)) = instance_source(doc, node_id) {
                    let root = model::find_node_in_doc(&*doc, &instance_id).expect("instance exists");
                    ops::ensure_unlocked(root)?;
                    ensure_unlocked_ancestry(&root.children, node_id)?;
                    let source_node = source_node(doc, &instance_id, &source_id).expect("source node exists");
                    guard::check_patch_kind(patch, source_node.kind, node_id)?;
                    let previous = root.ext.instance.as_ref().and_then(|reference| reference.overrides.get(&source_id))
                        .and_then(|over| over.style.as_ref()).cloned().unwrap_or_default();
                    let merged = ops::merge_patch(&previous, patch)?;
                    let mut effective = source_node.style.clone();
                    merged.apply(&mut effective);
                    model::validate_style(&effective)?;
                    guard::check_style_kind(&effective, source_node.kind, node_id)?;
                    let root = model::find_node_in_doc_mut(doc, &instance_id).expect("instance exists");
                    let over = root.ext.instance.as_mut().expect("instance root").overrides.entry(source_id).or_default();
                    over.style = Some(merged);
                    changed.push(instance_id);
                }
            }
            Ok(changed)
        }
        _ => Ok(Vec::new()),
    }
}

pub(crate) fn detach(doc: &mut CanvasDocument, instance_id: &str) -> DltResult<Vec<String>> {
    let root = model::find_node_in_doc_mut(doc, instance_id).ok_or_else(|| not_found("node", instance_id))?;
    if root.ext.instance.is_none() { return Err(invalid("COMPONENT_INSTANCE", format!("node {instance_id} is not a component instance"))); }
    ops::ensure_unlocked(root)?;
    root.ext.instance = None;
    Ok(vec![instance_id.to_string()])
}

fn capture_text_override(doc: &mut CanvasDocument, node_id: &str) -> DltResult<Vec<String>> {
    let Some((instance_id, source_id)) = instance_source(doc, node_id) else { return Ok(Vec::new()) };
    let root = model::find_node_in_doc(doc, &instance_id).expect("instance exists");
    ops::ensure_unlocked(root)?;
    ensure_unlocked_ancestry(&root.children, node_id)?;
    let instance_nodes = model::find_node_in_doc(doc, node_id).expect("materialized child exists");
    ops::ensure_unlocked(instance_nodes)?;
    let text = instance_nodes.text.clone();
    let root = model::find_node_in_doc_mut(doc, &instance_id).expect("instance exists");
    root.ext.instance.as_mut().expect("instance root").overrides.entry(source_id).or_default().text = Some(text);
    Ok(vec![instance_id.to_string()])
}

pub(super) fn instance_source(doc: &CanvasDocument, derived_id: &str) -> Option<(String, String)> {
    for instance in doc.pages.iter().flat_map(|page| model::flatten(&page.nodes)).filter(|node| node.ext.instance.is_some()) {
        let prefix = format!("{}::", instance.id);
        let Some(source_id) = derived_id.strip_prefix(&prefix) else { continue };
        let reference = instance.ext.instance.as_ref()?;
        let source = doc.pages.iter().find(|page| page.id == reference.component_page_id)?;
        if model::find_node(&source.nodes, source_id).is_some() && model::find_node(&instance.children, derived_id).is_some() {
            return Some((instance.id.clone(), source_id.to_string()));
        }
    }
    None
}

fn source_node<'a>(doc: &'a CanvasDocument, instance_id: &str, source_id: &str) -> Option<&'a Node> {
    let instance = model::find_node_in_doc(doc, instance_id)?;
    let source_page_id = &instance.ext.instance.as_ref()?.component_page_id;
    let source_page = doc.pages.iter().find(|page| &page.id == source_page_id)?;
    model::find_node(&source_page.nodes, source_id)
}

fn binding_is_overridden(target: &BindTarget, over: &NodeOverride) -> bool {
    let Some(patch) = &over.style else { return false };
    let (field, set) = match target {
        BindTarget::StyleColor => (StyleField::Color, patch.color.is_some()),
        BindTarget::StyleFill => (StyleField::Fill, patch.fill.is_some()),
        BindTarget::StyleStroke => (StyleField::Stroke, patch.stroke.is_some()),
        BindTarget::StyleFontFamily => (StyleField::FontFamily, patch.font_family.is_some()),
        BindTarget::StyleFontSize => (StyleField::FontSize, patch.font_size.is_some()),
        BindTarget::StyleCornerRadius => (StyleField::CornerRadius, patch.corner_radius.is_some()),
        _ => return false,
    };
    set || patch.clear.contains(&field)
}

pub(super) fn derived_id_map(instance_id: &str, source_nodes: &[Node]) -> DltResult<BTreeMap<String, String>> {
    let mut ids = BTreeMap::new();
    for node in model::flatten(source_nodes) {
        let mut source_ids = vec![node.id.as_str()];
        if let Some(table) = &node.ext.table { source_ids.extend(table.rows.iter().map(|row| row.id.as_str())); }
        for source_id in source_ids {
            let id = format!("{instance_id}::{source_id}");
            if !valid_id(&id) {
                return Err(invalid("COMPONENT_DERIVED_ID", format!("derived component id for {source_id} exceeds the identifier limit")));
            }
            if ids.insert(source_id.to_string(), id).is_some() { return Err(duplicate_id(source_id)); }
        }
    }
    Ok(ids)
}

/// The ext of a materialized copy. A copy carries no repeat spec or show condition (D21): those belong to the source.
fn materialized_ext(source: &Node, ids: &BTreeMap<String, String>) -> NodeExt {
    let mut ext = source.ext.clone();
    ext.instance = None;
    ext.repeat = None;
    ext.condition = None;
    if let Some(table) = &mut ext.table {
        for row in &mut table.rows {
            row.id = ids.get(&row.id).expect("every source row id was mapped").clone();
            for cell_id in &mut row.cell_ids {
                *cell_id = ids.get(cell_id).expect("every source cell id was mapped").clone();
            }
        }
        table.cell_spans = table.cell_spans.iter().map(|(cell_id, span)| {
            (ids.get(cell_id).expect("every source table span anchor was mapped").clone(), *span)
        }).collect();
        if let Some(header_row_id) = &mut table.header_row_id {
            *header_row_id = ids.get(header_row_id).expect("every source header row id was mapped").clone();
        }
    }
    ext
}

/// `materialized_ext` plus the condition result: the copy is hidden when the source is or the override hides it (D21), so
/// a user's hidden flag on the source and the repeat condition do not mix.
fn copy_ext(source: &Node, ids: &BTreeMap<String, String>, over: Option<&NodeOverride>) -> NodeExt {
    let mut ext = materialized_ext(source, ids);
    if over.and_then(|over| over.hidden) == Some(true) { ext.hidden = Some(true); }
    ext
}

fn table_ids(table: &CanvasTable, out: &mut BTreeSet<String>) {
    for row in &table.rows {
        out.insert(row.id.clone());
        out.extend(row.cell_ids.iter().cloned());
    }
}

fn node_tree_ids(nodes: &[Node]) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for node in model::flatten(nodes) {
        ids.insert(node.id.clone());
        if let Some(table) = &node.ext.table { table_ids(table, &mut ids); }
    }
    ids
}

pub(crate) fn refresh_instances(doc: &mut CanvasDocument) -> DltResult<(Vec<String>, Vec<Diagnostic>)> {
    let instances: Vec<(String, String)> = doc.pages.iter().flat_map(|page| model::flatten(&page.nodes)).filter_map(|node| {
        node.ext.instance.as_ref().map(|reference| (node.id.clone(), reference.component_page_id.clone()))
    }).collect();
    let mut changed = BTreeSet::new();
    let mut warnings = Vec::new();
    // Every id of the document, computed once and kept current as instance children are replaced below.
    let mut occupied: BTreeSet<String> = model::all_ids(doc).into_iter().collect();
    for (instance_id, component_page_id) in instances {
        let source_index = doc.pages.iter().position(|page| page.id == component_page_id).ok_or_else(|| invalid("COMPONENT_SOURCE_PAGE", "component source page was removed while an instance still refers to it"))?;
        if doc.pages[source_index].role != Some(PageRole::Component) {
            return Err(invalid("COMPONENT_SOURCE_PAGE", "component source page is no longer marked as a component"));
        }
        if model::flatten(&doc.pages[source_index].nodes).iter().any(|node| node.ext.instance.is_some()) {
            return Err(unsupported("NESTED_INSTANCE_UNSUPPORTED", "component sources cannot contain instances"));
        }
        let (ids, next_children, removed_overrides, next_bindings) = {
            let source_nodes = &doc.pages[source_index].nodes;
            let ids = derived_id_map(&instance_id, source_nodes)?;
            let instance = model::find_node_in_doc(doc, &instance_id).ok_or_else(|| not_found("instance", &instance_id))?;
            let overrides = &instance.ext.instance.as_ref().expect("instance id list was collected from instance nodes").overrides;
            let removed: Vec<String> = overrides.keys().filter(|id| !ids.contains_key(*id)).cloned().collect();
            let next: Vec<Node> = source_nodes.iter().map(|node| materialize_node(node, &ids, overrides)).collect();
            let bindings: Vec<TokenBinding> = doc.bindings.iter().filter_map(|binding| {
                let override_value = overrides.get(&binding.node_id);
                if override_value.is_some_and(|over| binding_is_overridden(&binding.target, over)) { return None }
                ids.get(&binding.node_id).map(|id| TokenBinding { node_id: id.clone(), target: binding.target.clone(), token: binding.token.clone() })
            }).collect();
            (ids, next, removed, bindings)
        };
        let instance = model::find_node_in_doc(doc, &instance_id).ok_or_else(|| not_found("instance", &instance_id))?;
        let previous_ids = node_tree_ids(&instance.children);
        for id in ids.values() {
            if occupied.contains(id) && !previous_ids.contains(id) { return Err(duplicate_id(id)); }
        }
        let mut node_changes = BTreeSet::new();
        let mut lock_sensitive = BTreeSet::new();
        let mut lock_only_changes = BTreeSet::new();
        collect_node_changes(&next_children, &instance.children, &instance_id, &mut node_changes, &mut lock_sensitive, &mut lock_only_changes);
        let old_ids = previous_ids;
        let next_ids: BTreeSet<String> = ids.values().cloned().collect();
        let derived_ids: BTreeSet<String> = old_ids.union(&next_ids).cloned().collect();
        let mut binding_changes = BTreeSet::new();
        for id in &derived_ids {
            let old: Vec<&TokenBinding> = doc.bindings.iter().filter(|binding| &binding.node_id == id).collect();
            let next: Vec<&TokenBinding> = next_bindings.iter().filter(|binding| &binding.node_id == id).collect();
            if old != next { binding_changes.insert(id.clone()); }
        }
        if node_changes.is_empty() && binding_changes.is_empty() && removed_overrides.is_empty() {
            continue;
        }
        for id in lock_sensitive.union(&binding_changes) {
            if let Some(instance) = model::find_node_in_doc(doc, &instance_id) { ensure_unlocked_ancestry(&instance.children, id)?; }
            ensure_unlocked_ancestry(&next_children, id)?;
        }
        for id in &lock_only_changes {
            if let Some(instance) = model::find_node_in_doc(doc, &instance_id) { ensure_unlocked_ancestors(&instance.children, id)?; }
            ensure_unlocked_ancestors(&next_children, id)?;
        }
        let node_tree_changed = !node_changes.is_empty();
        let bindings_changed = !binding_changes.is_empty();
        let removed_override_ids = !removed_overrides.is_empty();
        if removed_override_ids { changed.insert(instance_id.clone()); }
        changed.extend(node_changes);
        changed.extend(binding_changes);
        if node_tree_changed || bindings_changed || !lock_only_changes.is_empty() {
            let target = model::find_node_in_doc_mut(doc, &instance_id).expect("instance remains in the candidate");
            ops::ensure_unlocked(target)?;
            if node_tree_changed {
                target.children = next_children;
                for id in &old_ids { occupied.remove(id); }
                occupied.extend(next_ids.iter().cloned());
            }
        }
        if removed_override_ids {
            let target = model::find_node_in_doc_mut(doc, &instance_id).expect("instance remains in the candidate");
            let overrides = &mut target.ext.instance.as_mut().expect("instance root").overrides;
            for source_id in &removed_overrides { overrides.remove(source_id); }
            warnings.extend(removed_overrides.into_iter().map(|source_id| Diagnostic {
                code: "INSTANCE_OVERRIDE_REMOVED".into(), message: format!("override for removed component node {source_id} was removed"),
                node_id: Some(instance_id.clone()), blocking: false, params: Default::default(),
            }));
        }
        doc.bindings.retain(|binding| !derived_ids.contains(&binding.node_id));
        doc.bindings.extend(next_bindings);
    }
    Ok((changed.into_iter().collect(), warnings))
}

pub(crate) fn validate_document(doc: &CanvasDocument) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let repeat_instance_ids = repeat::repeat_instance_ids(doc);
    for page in &doc.pages {
        for node in model::flatten(&page.nodes) {
            if node.ext.image_crop_pixel_rect.is_some() {
                if page.role == Some(PageRole::Component) {
                    diagnostics.push(blocking("IMAGE_CROP_COMPONENT_SOURCE_UNSUPPORTED", "images in a component source cannot carry crop metadata yet", Some(node.id.clone())));
                } else if is_instance_subtree(doc, &node.id) {
                    diagnostics.push(blocking("IMAGE_CROP_INSTANCE_UNSUPPORTED", "images inside a component instance cannot carry crop metadata yet", Some(node.id.clone())));
                }
            }
        }
        if page.role == Some(PageRole::Component) && model::flatten(&page.nodes).iter().any(|node| node.ext.instance.is_some()) {
            diagnostics.push(blocking("NESTED_INSTANCE_UNSUPPORTED", "a component source cannot contain an instance", Some(page.id.clone())));
        }
        for instance in model::flatten(&page.nodes).into_iter().filter(|node| node.ext.instance.is_some()) {
            let reference = instance.ext.instance.as_ref().expect("filtered above");
            if instance.ext.table.is_some() {
                diagnostics.push(blocking("COMPONENT_INSTANCE_TABLE_ROOT_UNSUPPORTED", "table metadata belongs to a source child, not the component instance root", Some(instance.id.clone())));
                continue;
            }
            let Some(source) = doc.pages.iter().find(|source| source.id == reference.component_page_id) else {
                diagnostics.push(blocking("COMPONENT_SOURCE_PAGE", "component source page does not exist", Some(instance.id.clone())));
                continue;
            };
            if page.role.is_some() || source.role != Some(PageRole::Component) || instance.kind != NodeKind::Group || source.id == page.id
                || instance.layout.as_ref().is_some_and(|layout| layout.mode != "absolute")
                || !valid_id(&instance.id) || instance.id.contains("::")
            {
                diagnostics.push(blocking("COMPONENT_INSTANCE_INVALID", "component instance must be a Group on a regular page and refer to a component page", Some(instance.id.clone())));
                continue;
            }
            let covered_source_cells: BTreeSet<&str> = model::flatten(&source.nodes).into_iter().filter_map(|node| node.ext.table.as_ref())
                .filter_map(|table| model::covered_table_cell_ids(table).ok())
                .flatten()
                .collect();
            let mut override_invalid = false;
            let repeat_instance = repeat_instance_ids.contains(instance.id.as_str());
            for (source_id, override_value) in &reference.overrides {
                // Only a repeat expansion writes a hidden override, and only `true` (D21); `false` would be a second way to
                // say "not hidden".
                if override_value.hidden == Some(false) || (override_value.hidden.is_some() && !repeat_instance) {
                    diagnostics.push(blocking(repeat::COMPONENT_OVERRIDE_HIDDEN, "only a repeat group's condition sets a hidden override, and only to true", Some(instance.id.clone())));
                    override_invalid = true;
                    continue;
                }
                if covered_source_cells.contains(source_id.as_str()) {
                    diagnostics.push(blocking("COMPONENT_SPAN_OVERRIDE", "a covered table cell cannot retain a component instance override", Some(instance.id.clone())));
                    override_invalid = true;
                    continue;
                }
                let Some(source_node) = model::find_node(&source.nodes, source_id) else {
                    diagnostics.push(blocking("COMPONENT_OVERRIDE_SOURCE", "instance override refers to a missing source node", Some(instance.id.clone())));
                    override_invalid = true;
                    continue;
                };
                if override_value.text.as_ref().is_some_and(|text| {
                    text.len() > crate::common::MAX_TEXT_BYTES || !matches!(source_node.kind, NodeKind::Text | NodeKind::Rectangle | NodeKind::Ellipse)
                }) {
                    diagnostics.push(blocking("COMPONENT_OVERRIDE_TEXT", "text override is too long or unsupported for its source node", Some(instance.id.clone())));
                    override_invalid = true;
                }
                if let Some(patch) = &override_value.style {
                    let mut effective = source_node.style.clone();
                    patch.apply(&mut effective);
                    if model::validate_patch(patch).is_err() || model::validate_style(&effective).is_err()
                        || guard::check_patch_kind(patch, source_node.kind, &source_node.id).is_err()
                        || guard::check_style_kind(&effective, source_node.kind, &source_node.id).is_err()
                    {
                        diagnostics.push(blocking("COMPONENT_OVERRIDE_STYLE", "style override is invalid or unsupported for its source node", Some(instance.id.clone())));
                        override_invalid = true;
                    }
                }
            }
            if override_invalid {
                continue;
            }
            let ids = match derived_id_map(&instance.id, &source.nodes) {
                Ok(ids) => ids,
                Err(error) => {
                    diagnostics.push(blocking("COMPONENT_DERIVED_ID", error.message, Some(instance.id.clone())));
                    continue;
                }
            };
            if instance.children.len() != source.nodes.len() || !source.nodes.iter().zip(&instance.children).all(|(source, clone)| nodes_match(source, clone, &ids, &reference.overrides)) {
                diagnostics.push(blocking("COMPONENT_INSTANCE_STALE", "materialized component children do not match their source", Some(instance.id.clone())));
            }
        }
    }
    diagnostics
}

fn nodes_match(source: &Node, actual: &Node, ids: &BTreeMap<String, String>, overrides: &BTreeMap<String, NodeOverride>) -> bool {
    shallow_nodes_match(source, actual, ids, overrides)
        && source.children.len() == actual.children.len()
        && source.children.iter().zip(&actual.children).all(|(source, actual)| nodes_match(source, actual, ids, overrides))
}

fn shallow_nodes_match(source: &Node, actual: &Node, ids: &BTreeMap<String, String>, overrides: &BTreeMap<String, NodeOverride>) -> bool {
    let interactions: Vec<Interaction> = source.interactions.iter().map(|interaction| match interaction {
        Interaction::ToggleVisibility { node_id } => Interaction::ToggleVisibility { node_id: ids.get(node_id).cloned().unwrap_or_else(|| node_id.clone()) },
        Interaction::SetVariant { node_id, variant } => Interaction::SetVariant { node_id: ids.get(node_id).cloned().unwrap_or_else(|| node_id.clone()), variant: variant.clone() },
        other => other.clone(),
    }).collect();
    let override_value = overrides.get(&source.id);
    let ext = copy_ext(source, ids, override_value);
    let text = override_value.and_then(|over| over.text.as_ref()).unwrap_or(&source.text);
    let mut style = source.style.clone();
    if let Some(patch) = override_value.and_then(|over| over.style.as_ref()) { patch.apply(&mut style); }
    ids.get(&source.id).is_some_and(|id| id == &actual.id)
        && source.kind == actual.kind && source.bounds == actual.bounds && text == &actual.text && style == actual.style
        && source.asset == actual.asset && serde_json::to_value(&source.layout).ok() == serde_json::to_value(&actual.layout).ok()
        && interactions == actual.interactions && source.variants == actual.variants && ext == actual.ext
}

fn collect_node_changes(
    next: &[Node], current: &[Node], parent_id: &str, changed: &mut BTreeSet<String>,
    lock_sensitive: &mut BTreeSet<String>, lock_only_changes: &mut BTreeSet<String>,
) {
    let expected: Vec<String> = next.iter().map(|node| node.id.clone()).collect();
    let actual: Vec<String> = current.iter().map(|node| node.id.clone()).collect();
    let expected_set: BTreeSet<&str> = expected.iter().map(String::as_str).collect();
    if expected != actual {
        changed.insert(parent_id.to_string());
        lock_sensitive.insert(parent_id.to_string());
    }
    for node in next {
        let Some(current) = current.iter().find(|candidate| candidate.id == node.id) else {
            collect_added_subtree(node, changed, lock_sensitive);
            continue;
        };
        if !same_materialized_node(node, current) {
            changed.insert(node.id.clone());
        }
        if node.ext.table != current.ext.table {
            let structure_changed = node.ext.table.as_ref().map(|table| (&table.rows, &table.header_row_id))
                != current.ext.table.as_ref().map(|table| (&table.rows, &table.header_row_id));
            let rows_changed = node.ext.table.as_ref().map(|table| &table.rows) != current.ext.table.as_ref().map(|table| &table.rows);
            let widths_changed = node.ext.table.as_ref().map(|table| &table.column_widths) != current.ext.table.as_ref().map(|table| &table.column_widths);
            let spans_changed = node.ext.table.as_ref().map(|table| &table.cell_spans) != current.ext.table.as_ref().map(|table| &table.cell_spans);
            for table in [node.ext.table.as_ref(), current.ext.table.as_ref()].into_iter().flatten() {
                if structure_changed || spans_changed {
                    table_ids(table, changed);
                } else if widths_changed {
                    changed.extend(table.rows.iter().flat_map(|row| row.cell_ids.iter().cloned()));
                }
            }
            if rows_changed || widths_changed || spans_changed {
                lock_sensitive.insert(node.id.clone());
                for table in [node.ext.table.as_ref(), current.ext.table.as_ref()].into_iter().flatten() {
                    lock_sensitive.extend(table.rows.iter().flat_map(|row| row.cell_ids.iter().cloned()));
                }
            }
        }
        if !same_node_except_lock(node, current) {
            lock_sensitive.insert(node.id.clone());
        } else if node.ext.locked != current.ext.locked {
            lock_only_changes.insert(node.id.clone());
        }
        collect_node_changes(&node.children, &current.children, &node.id, changed, lock_sensitive, lock_only_changes);
    }
    for node in current {
        if !expected_set.contains(node.id.as_str()) {
            collect_removed_subtree(node, changed, lock_sensitive);
        }
    }
}

fn collect_added_subtree(node: &Node, changed: &mut BTreeSet<String>, lock_sensitive: &mut BTreeSet<String>) {
    changed.insert(node.id.clone());
    lock_sensitive.insert(node.id.clone());
    if let Some(table) = &node.ext.table { table_ids(table, changed); }
    for child in &node.children { collect_added_subtree(child, changed, lock_sensitive); }
}

fn collect_removed_subtree(node: &Node, changed: &mut BTreeSet<String>, lock_sensitive: &mut BTreeSet<String>) {
    changed.insert(node.id.clone());
    lock_sensitive.insert(node.id.clone());
    if let Some(table) = &node.ext.table {
        table_ids(table, changed);
        lock_sensitive.extend(table.rows.iter().flat_map(|row| row.cell_ids.iter().cloned()));
    }
    for child in &node.children { collect_removed_subtree(child, changed, lock_sensitive); }
}

fn same_materialized_node(left: &Node, right: &Node) -> bool {
    left.id == right.id && left.kind == right.kind && left.bounds == right.bounds && left.text == right.text && left.style == right.style
        && left.asset == right.asset && serde_json::to_value(&left.layout).ok() == serde_json::to_value(&right.layout).ok()
        && left.interactions == right.interactions && left.variants == right.variants && left.ext == right.ext
}

fn same_node_except_lock(left: &Node, right: &Node) -> bool {
    let mut left_ext = left.ext.clone();
    let mut right_ext = right.ext.clone();
    left_ext.locked = None;
    right_ext.locked = None;
    left.id == right.id && left.kind == right.kind && left.bounds == right.bounds && left.text == right.text && left.style == right.style
        && left.asset == right.asset && serde_json::to_value(&left.layout).ok() == serde_json::to_value(&right.layout).ok()
        && left.interactions == right.interactions && left.variants == right.variants && left_ext == right_ext
}

fn is_materialized_child(doc: &CanvasDocument, id: &str) -> bool {
    doc.pages.iter().any(|page| instance_child(&page.nodes, id, false))
}

pub(super) fn is_instance_subtree(doc: &CanvasDocument, id: &str) -> bool {
    doc.pages.iter().flat_map(|page| model::flatten(&page.nodes)).any(|node| {
        node.ext.instance.is_some() && (node.id == id || model::find_node(&node.children, id).is_some())
    })
}

fn instance_child(nodes: &[Node], id: &str, in_instance: bool) -> bool {
    nodes.iter().any(|node| {
        if node.id == id { return in_instance; }
        instance_child(&node.children, id, in_instance || node.ext.instance.is_some())
    })
}

pub(super) fn materialize_node(source: &Node, ids: &BTreeMap<String, String>, overrides: &BTreeMap<String, NodeOverride>) -> Node {
    let interactions = source.interactions.iter().map(|interaction| match interaction {
        Interaction::ToggleVisibility { node_id } => Interaction::ToggleVisibility { node_id: ids.get(node_id.as_str()).cloned().unwrap_or_else(|| node_id.clone()) },
        Interaction::SetVariant { node_id, variant } => Interaction::SetVariant { node_id: ids.get(node_id.as_str()).cloned().unwrap_or_else(|| node_id.clone()), variant: variant.clone() },
        other => other.clone(),
    }).collect();
    let override_value = overrides.get(&source.id);
    let text = override_value.and_then(|over| over.text.clone()).unwrap_or_else(|| source.text.clone());
    let mut style = source.style.clone();
    if let Some(patch) = override_value.and_then(|over| over.style.as_ref()) { patch.apply(&mut style); }
    Node {
        id: ids.get(source.id.as_str()).expect("every source node was mapped").clone(), kind: source.kind, bounds: source.bounds,
        text, style, asset: source.asset.clone(),
        children: source.children.iter().map(|child| materialize_node(child, ids, overrides)).collect(), layout: source.layout.clone(),
        interactions, variants: source.variants.clone(), ext: copy_ext(source, ids, override_value),
    }
}

fn ensure_unlocked_ancestry(nodes: &[Node], id: &str) -> DltResult<bool> {
    for node in nodes {
        if node.id == id {
            ops::ensure_unlocked(node)?;
            return Ok(true);
        }
        if ensure_unlocked_ancestry(&node.children, id)? {
            ops::ensure_unlocked(node)?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn ensure_unlocked_ancestors(nodes: &[Node], id: &str) -> DltResult<bool> {
    for node in nodes {
        if node.id == id { return Ok(true); }
        if ensure_unlocked_ancestors(&node.children, id)? {
            ops::ensure_unlocked(node)?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn has_instance_ancestor(nodes: &[Node], id: &str) -> bool {
    nodes.iter().any(|node| {
        node.ext.instance.is_some() && (node.id == id || model::find_node(&node.children, id).is_some())
            || has_instance_ancestor(&node.children, id)
    })
}

fn depth_of(nodes: &[Node], id: &str) -> Option<usize> {
    for node in nodes {
        if node.id == id { return Some(0); }
        if let Some(depth) = depth_of(&node.children, id) { return Some(depth + 1); }
    }
    None
}

pub(super) fn not_found(kind: &str, id: &str) -> DltError {
    let mut error = DltError::new(ErrorCode::NotFound, format!("{kind} {id} does not exist"));
    error.affected_ids.push(id.to_string());
    error
}

pub(super) fn duplicate_id(id: &str) -> DltError {
    let mut error = DltError::new(ErrorCode::AlreadyExists, format!("identifier {id} already exists"));
    error.affected_ids.push(id.to_string());
    error.diagnostics.push(Diagnostic { code: "DUPLICATE_ID".into(), message: error.message.clone(), node_id: Some(id.to_string()), blocking: true, params: Default::default() });
    error
}

fn unsupported(code: &str, message: &str) -> DltError {
    let mut error = DltError::unsupported(message);
    error.diagnostics.push(Diagnostic { code: code.into(), message: message.to_string(), node_id: None, blocking: true, params: Default::default() });
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::tokens;
    use designllm_contracts::{AssetRef, AssetStore, BindTarget, CanvasTable, CanvasTableRow, LayoutSpec, Operation, Size, Style, StylePatch, TokenBinding, TokenDef, TokenMode, TokenRef, TokenSource, TokenValues};

    struct NoAssets;
    impl AssetStore for NoAssets {
        fn put(&self, _bytes: &[u8], _mime: &str) -> DltResult<AssetRef> { Err(DltError::unsupported("not used")) }
        fn read(&self, _asset: &AssetRef) -> DltResult<Vec<u8>> { Err(DltError::unsupported("not used")) }
        fn lookup(&self, _hash: &str) -> DltResult<AssetRef> { Err(DltError::unsupported("not used")) }
    }

    fn node(id: &str, kind: NodeKind, bounds: Rect, children: Vec<Node>) -> Node {
        Node { id: id.into(), kind, bounds, text: String::new(), style: Style::default(), asset: None, children, layout: None,
            interactions: Vec::new(), variants: BTreeMap::new(), ext: NodeExt::default() }
    }

    fn document() -> CanvasDocument {
        let mut text = node("source-text", NodeKind::Text, Rect { x: 25, y: 35, width: 900, height: 300 }, vec![]);
        text.text = "component".into();
        let mut source_group = node("source-group", NodeKind::Group, Rect { x: 15, y: 20, width: 1200, height: 700 }, vec![text]);
        source_group.interactions = vec![Interaction::ToggleVisibility { node_id: "source-text".into() }];
        CanvasDocument {
            pages: vec![
                designllm_contracts::Page::new("regular", Size::slide()),
                designllm_contracts::Page {
                    nodes: vec![source_group],
                    ..designllm_contracts::Page::new("component", Size::slide())
                },
            ],
            ..Default::default()
        }
    }

    fn table_group(id: &str, cell_id: &str) -> Node {
        let mut cell = node(cell_id, NodeKind::Rectangle, Rect { x: 0, y: 0, width: 200, height: 100 }, vec![]);
        cell.layout = Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() });
        let mut group = node(id, NodeKind::Group, Rect { x: 0, y: 0, width: 200, height: 100 }, vec![cell]);
        group.layout = Some(LayoutSpec { mode: "grid".into(), height_mode: "hug".into(), grid_columns: Some(1), gap: 0, row_gap: Some(0), ..Default::default() });
        group.ext.table = Some(CanvasTable {
            rows: vec![CanvasTableRow { id: format!("{id}-row"), cell_ids: vec![cell_id.into()] }],
            header_row_id: None,
            column_widths: None,
            cell_spans: Default::default(),
        });
        group
    }

    fn table_cell(id: &str, text: &str) -> Node {
        let mut cell = node(id, NodeKind::Rectangle, Rect { x: 0, y: 0, width: 200, height: 100 }, vec![]);
        cell.text = text.into();
        cell.layout = Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() });
        cell
    }

    fn two_column_table(id: &str, first_cell_id: &str, second_cell_id: &str) -> Node {
        let mut group = node(
            id,
            NodeKind::Group,
            Rect { x: 0, y: 0, width: 400, height: 100 },
            vec![table_cell(first_cell_id, "first"), table_cell(second_cell_id, "second")],
        );
        group.layout = Some(LayoutSpec { mode: "grid".into(), height_mode: "hug".into(), grid_columns: Some(2), gap: 0, row_gap: Some(0), ..Default::default() });
        group.ext.table = Some(CanvasTable {
            rows: vec![CanvasTableRow { id: format!("{id}-row"), cell_ids: vec![first_cell_id.into(), second_cell_id.into()] }],
            header_row_id: None,
            column_widths: None,
            cell_spans: Default::default(),
        });
        group
    }

    fn table_change(node: &Node) -> Operation {
        let expected = node.ext.table.clone().expect("table metadata");
        let mut table = expected.clone();
        table.header_row_id = Some(table.rows[0].id.clone());
        Operation::SetCanvasTableStructure { table_id: node.id.clone(), expected, table }
    }

    fn bound_component(source: TokenSource, light: &str, dark: &str) -> CanvasDocument {
        let mut doc = document();
        doc.pages[1].nodes[0].children[0].style.fill = light.to_string();
        if source == TokenSource::Local {
            doc.tokens.insert("fill".into(), TokenDef { name: "Fill".into(), values: TokenValues::Color { light: light.into(), dark: dark.into() } });
        }
        doc.bindings.push(TokenBinding {
            node_id: "source-text".into(), target: BindTarget::StyleFill,
            token: TokenRef { source, token_id: "fill".into() },
        });
        set_page_role(&mut doc, "component", Some(PageRole::Component)).unwrap();
        instantiate(&mut doc, "regular", None, "instance", "component", Rect { x: 300, y: 200, width: 4000, height: 3000 }).unwrap();
        doc
    }

    #[test]
    fn component_tables_materialize_and_copy_with_derived_row_and_cell_ids() {
        let mut doc = document();
        doc.pages[1].nodes[0].children.push(table_group("source-table", "source-cell"));
        set_page_role(&mut doc, "component", Some(PageRole::Component)).unwrap();
        instantiate(&mut doc, "regular", None, "instance", "component", Rect { x: 0, y: 0, width: 1000, height: 1000 }).unwrap();
        let table = model::find_node_in_doc(&doc, "instance::source-table").unwrap();
        let metadata = table.ext.table.as_ref().unwrap();
        assert_eq!(metadata.rows[0].id, "instance::source-table-row");
        assert_eq!(metadata.rows[0].cell_ids, ["instance::source-cell"]);
        assert_eq!(table.children[0].id, "instance::source-cell");
        assert!(validate_document(&doc).is_empty());

        let mut invalid_root = doc.clone();
        model::find_node_in_doc_mut(&mut invalid_root, "instance").unwrap().ext.table = Some(CanvasTable {
            rows: vec![CanvasTableRow { id: "instance-root-row".into(), cell_ids: vec!["instance::source-group".into()] }],
            header_row_id: None,
            column_widths: None,
            cell_spans: Default::default(),
        });
        assert!(validate_document(&invalid_root).iter().any(|diagnostic| diagnostic.code == "COMPONENT_INSTANCE_TABLE_ROOT_UNSUPPORTED"));

        let mut duplicate = model::find_node_in_doc(&doc, "instance").unwrap().clone();
        duplicate.id = "copy".into();
        let mut links = Vec::new();
        ops::apply(&mut doc, &mut links, &NoAssets, &[Operation::AddNode { page_id: "regular".into(), parent_id: None, node: duplicate }]).unwrap();
        let copied = model::find_node_in_doc(&doc, "copy::source-table").unwrap();
        assert_eq!(copied.ext.table.as_ref().unwrap().rows[0].id, "copy::source-table-row");
        assert_eq!(copied.ext.table.as_ref().unwrap().rows[0].cell_ids, ["copy::source-cell"]);
        assert!(validate_document(&doc).is_empty(), "copied component tables retain unique remapped identifiers");
    }

    #[test]
    fn table_span_anchor_ids_are_derived_with_component_cells() {
        use designllm_contracts::CanvasTableSpan;

        let mut source = table_group("source-table", "source-cell-1");
        source.children.push(table_cell("source-cell-2", ""));
        source.layout.as_mut().unwrap().grid_columns = Some(2);
        let table = source.ext.table.as_mut().unwrap();
        table.rows[0].cell_ids.push("source-cell-2".into());
        table.cell_spans.insert("source-cell-1".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        let ids = derived_id_map("instance", std::slice::from_ref(&source)).expect("source IDs map");

        let materialized = materialized_ext(&source, &ids).table.expect("table metadata");
        assert_eq!(materialized.cell_spans.get("instance::source-cell-1"), Some(&CanvasTableSpan { row_span: 1, column_span: 2 }));
        assert!(materialized.cell_spans.get("source-cell-1").is_none(), "span keys refer to materialized cell IDs");
    }

    #[test]
    fn source_merge_refuses_to_hide_an_instance_cell_override() {
        use designllm_contracts::CanvasTableSpan;

        let mut doc = document();
        let mut source_table = table_group("source-table", "source-cell-1");
        source_table.children.push(table_cell("source-cell-2", ""));
        source_table.layout.as_mut().unwrap().grid_columns = Some(2);
        let source_metadata = source_table.ext.table.as_mut().unwrap();
        source_metadata.rows[0].cell_ids.push("source-cell-2".into());
        doc.pages[1].nodes.push(source_table);
        set_page_role(&mut doc, "component", Some(PageRole::Component)).unwrap();
        instantiate(&mut doc, "regular", None, "instance", "component", Rect { x: 0, y: 0, width: 4000, height: 3000 }).unwrap();
        ops::apply(&mut doc, &mut Vec::new(), &NoAssets, &[Operation::SetText { node_id: "instance::source-cell-2".into(), text: "local text".into() }]).unwrap();

        let expected = model::find_node_in_doc(&doc, "source-table").unwrap().ext.table.clone().unwrap();
        let mut merged = expected.clone();
        merged.cell_spans.insert("source-cell-1".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        let error = preflight_table_operations(&doc, &[Operation::SetCanvasTableStructure { table_id: "source-table".into(), expected, table: merged }])
            .expect_err("a local override stays visible until the instance is detached");
        assert!(error.diagnostics.iter().any(|diagnostic| diagnostic.code == "TABLE_SPAN_COMPONENT_OVERRIDE"));
    }

    #[test]
    fn source_table_structure_refresh_preserves_cell_overrides_and_changed_ids() {
        let mut doc = document();
        doc.pages[1].nodes[0].children.push(table_group("source-table", "source-cell"));
        set_page_role(&mut doc, "component", Some(PageRole::Component)).unwrap();
        instantiate(&mut doc, "regular", None, "instance", "component", Rect { x: 0, y: 0, width: 1000, height: 1000 }).unwrap();
        let mut links = Vec::new();
        ops::apply(&mut doc, &mut links, &NoAssets, &[
            Operation::SetText { node_id: "instance::source-cell".into(), text: "local text".into() },
            Operation::SetStyle { node_ids: vec!["instance::source-cell".into()], patch: StylePatch { fill: Some("#123456".into()), ..Default::default() } },
        ]).unwrap();

        let source_table = model::find_node_in_doc(&doc, "source-table").unwrap();
        let expected = source_table.ext.table.clone().unwrap();
        let mut next = expected.clone();
        next.rows.push(CanvasTableRow { id: "source-row-2".into(), cell_ids: vec!["source-cell-2".into()] });
        let operations = [
            Operation::SetCanvasTableStructure { table_id: "source-table".into(), expected, table: next },
            Operation::SetText { node_id: "source-cell".into(), text: "updated source".into() },
            Operation::SetStyle { node_ids: vec!["source-cell".into()], patch: StylePatch { fill: Some("#ABCDEF".into()), ..Default::default() } },
            Operation::AddNode { page_id: "component".into(), parent_id: Some("source-table".into()), node: table_cell("source-cell-2", "new source cell") },
        ];
        let (changed, diagnostics) = ops::apply(&mut doc, &mut links, &NoAssets, &operations).unwrap();
        assert!(diagnostics.is_empty());
        assert!(changed.contains(&"instance::source-table".into()));
        assert!(changed.contains(&"instance::source-row-2".into()));
        assert!(changed.contains(&"instance::source-cell-2".into()));
        let instance_table = model::find_node_in_doc(&doc, "instance::source-table").unwrap();
        let metadata = instance_table.ext.table.as_ref().unwrap();
        assert_eq!(metadata.rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["instance::source-table-row", "instance::source-row-2"]);
        assert_eq!(metadata.rows[1].cell_ids, ["instance::source-cell-2"]);
        assert_eq!(instance_table.children[0].text, "local text");
        assert_eq!(instance_table.children[0].style.fill, "#123456");
        assert_eq!(instance_table.children[1].text, "new source cell");
        assert!(validate_document(&doc).is_empty());
    }

    #[test]
    fn locked_component_table_and_cells_refuse_source_structure_refresh_atomically() {
        let mut base = document();
        base.pages[1].nodes[0].children.push(table_group("source-table", "source-cell"));
        set_page_role(&mut base, "component", Some(PageRole::Component)).unwrap();
        instantiate(&mut base, "regular", None, "instance", "component", Rect { x: 0, y: 0, width: 1000, height: 1000 }).unwrap();
        let source = model::find_node_in_doc(&base, "source-table").unwrap();
        let expected = source.ext.table.clone().unwrap();
        let mut next = expected.clone();
        next.rows.push(CanvasTableRow { id: "source-row-2".into(), cell_ids: vec!["source-cell-2".into()] });
        let operations = [
            Operation::SetCanvasTableStructure { table_id: "source-table".into(), expected, table: next },
            Operation::AddNode { page_id: "component".into(), parent_id: Some("source-table".into()), node: table_cell("source-cell-2", "new") },
        ];

        for locked_id in ["instance::source-table", "instance::source-cell"] {
            let mut before = base.clone();
            model::find_node_in_doc_mut(&mut before, locked_id).unwrap().ext.locked = Some(true);
            let expected_before = serde_json::to_value(&before).unwrap();
            let mut candidate = before.clone();
            let error = ops::apply(&mut candidate, &mut Vec::new(), &NoAssets, &operations).unwrap_err();
            assert_eq!(error.code, ErrorCode::ContentLocked);
            assert_eq!(serde_json::to_value(&before).unwrap(), expected_before, "a rejected component table refresh does not mutate its base");
        }
    }

    #[test]
    fn component_table_widths_follow_source_and_locked_cells_block_refresh_atomically() {
        let mut doc = document();
        doc.pages[1].nodes[0].children.push(two_column_table("source-table", "source-cell-1", "source-cell-2"));
        set_page_role(&mut doc, "component", Some(PageRole::Component)).unwrap();
        instantiate(&mut doc, "regular", None, "instance", "component", Rect { x: 0, y: 0, width: 1000, height: 1000 }).unwrap();
        let mut links = Vec::new();
        let widths = Operation::SetCanvasTableColumnWidths { table_id: "source-table".into(), expected: None, column_widths: Some(vec![1, 2]) };
        let (changed, diagnostics) = ops::apply(&mut doc, &mut links, &NoAssets, std::slice::from_ref(&widths)).unwrap();
        assert!(diagnostics.is_empty());
        assert!(changed.contains(&"instance::source-table".into()));
        assert!(changed.contains(&"instance::source-cell-1".into()));
        assert!(changed.contains(&"instance::source-cell-2".into()));
        assert_eq!(model::find_node_in_doc(&doc, "instance::source-table").unwrap().ext.table.as_ref().unwrap().column_widths, Some(vec![1, 2]));
        assert!(validate_document(&doc).is_empty());

        let before = serde_json::to_value(&doc).unwrap();
        let direct_instance_edit = Operation::SetCanvasTableColumnWidths { table_id: "instance::source-table".into(), expected: Some(vec![1, 2]), column_widths: Some(vec![2, 1]) };
        let mut candidate = doc.clone();
        let error = ops::apply(&mut candidate, &mut Vec::new(), &NoAssets, &[direct_instance_edit]).unwrap_err();
        assert_eq!(error.diagnostics[0].code, "COMPONENT_INSTANCE_TABLE_WIDTH_UNSUPPORTED");
        assert_eq!(serde_json::to_value(&doc).unwrap(), before, "an instance cannot silently save a local width change");

        let mut locked = doc.clone();
        model::find_node_in_doc_mut(&mut locked, "instance::source-cell-1").unwrap().ext.locked = Some(true);
        let locked_before = serde_json::to_value(&locked).unwrap();
        let mut candidate = locked.clone();
        let source_width_update = Operation::SetCanvasTableColumnWidths {
            table_id: "source-table".into(), expected: Some(vec![1, 2]), column_widths: Some(vec![2, 1]),
        };
        let error = ops::apply(&mut candidate, &mut Vec::new(), &NoAssets, &[source_width_update]).unwrap_err();
        assert_eq!(error.code, ErrorCode::ContentLocked, "width propagation checks each derived table cell lock");
        assert_eq!(serde_json::to_value(&locked).unwrap(), locked_before, "a locked derived cell leaves the source and all instances unchanged");
    }

    #[test]
    fn deleting_an_overridden_source_table_cell_is_blocked_until_instance_detach() {
        let mut doc = document();
        let mut source_table = table_group("source-table", "source-cell");
        source_table.children.push(table_cell("source-cell-2", "source second"));
        source_table.ext.table.as_mut().unwrap().rows.push(CanvasTableRow { id: "source-row-2".into(), cell_ids: vec!["source-cell-2".into()] });
        doc.pages[1].nodes[0].children.push(source_table);
        set_page_role(&mut doc, "component", Some(PageRole::Component)).unwrap();
        instantiate(&mut doc, "regular", None, "instance-a", "component", Rect { x: 0, y: 0, width: 1000, height: 1000 }).unwrap();
        instantiate(&mut doc, "regular", None, "instance-b", "component", Rect { x: 0, y: 0, width: 1000, height: 1000 }).unwrap();
        let mut links = Vec::new();
        ops::apply(&mut doc, &mut links, &NoAssets, &[Operation::SetText { node_id: "instance-a::source-cell-2".into(), text: "kept local edit".into() }]).unwrap();

        let expected = model::find_node_in_doc(&doc, "source-table").unwrap().ext.table.clone().unwrap();
        let mut next = expected.clone();
        next.rows.pop();
        let operations = [
            Operation::SetCanvasTableStructure { table_id: "source-table".into(), expected, table: next },
            Operation::RemoveNode { node_id: "source-cell-2".into() },
        ];
        let before = serde_json::to_value(&doc).unwrap();
        let mut candidate = doc.clone();
        let error = ops::apply(&mut candidate, &mut Vec::new(), &NoAssets, &operations).unwrap_err();
        assert_eq!(error.code, ErrorCode::ContentLocked);
        assert_eq!(error.diagnostics[0].code, "COMPONENT_TABLE_OVERRIDE_DELETE_BLOCKED");
        assert_eq!(serde_json::to_value(&doc).unwrap(), before, "the source cell and every instance override stay intact on refusal");

        ops::apply(&mut doc, &mut links, &NoAssets, &[Operation::DetachInstance { instance_id: "instance-a".into() }]).unwrap();
        ops::apply(&mut doc, &mut links, &NoAssets, &operations).expect("detaching preserves the local edit and releases its source cell");
        assert_eq!(model::find_node_in_doc(&doc, "instance-a::source-cell-2").unwrap().text, "kept local edit");
        assert!(model::find_node_in_doc(&doc, "instance-b::source-cell-2").is_none(), "the other live instance follows the source deletion");
        assert!(validate_document(&doc).is_empty());
    }

    #[test]
    fn instance_table_structure_is_explicitly_refused_and_source_structure_batches_are_order_safe() {
        let mut source = document();
        source.pages[1].role = Some(PageRole::Component);
        let table = table_group("source-table", "source-cell");
        source.pages[1].nodes[0].children.push(table.clone());
        let op = table_change(&table);
        assert!(preflight_table_operations(&source, std::slice::from_ref(&op)).is_ok());

        let mut instance = document();
        instance.pages[1].nodes[0].children.push(table_group("source-table", "source-cell"));
        set_page_role(&mut instance, "component", Some(PageRole::Component)).unwrap();
        instantiate(&mut instance, "regular", None, "instance", "component", Rect { x: 0, y: 0, width: 1000, height: 1000 }).unwrap();
        let before = serde_json::to_value(&instance).unwrap();
        let table = model::find_node_in_doc(&instance, "instance::source-table").unwrap().clone();
        let op = table_change(&table);
        let error = preflight_table_operations(&instance, std::slice::from_ref(&op)).unwrap_err();
        assert_eq!(error.diagnostics[0].code, "COMPONENT_INSTANCE_TABLE_STRUCTURE_UNSUPPORTED");
        assert_eq!(serde_json::to_value(&instance).unwrap(), before);

        let before = serde_json::to_value(&instance).unwrap();
        let operations = [
            table_change(model::find_node_in_doc(&instance, "source-table").unwrap()),
            Operation::SetText { node_id: "instance::source-cell".into(), text: "same batch".into() },
        ];
        let error = preflight_table_operations(&instance, &operations).unwrap_err();
        assert_eq!(error.diagnostics[0].code, "COMPONENT_TABLE_BATCH_ORDER_UNSUPPORTED");
        assert_eq!(serde_json::to_value(&instance).unwrap(), before, "an ambiguous refresh and instance edit is rejected before mutation");
        let mut candidate = instance.clone();
        let error = ops::apply(&mut candidate, &mut Vec::new(), &NoAssets, &operations).unwrap_err();
        assert_eq!(error.diagnostics[0].code, "COMPONENT_TABLE_BATCH_ORDER_UNSUPPORTED");
        assert_eq!(serde_json::to_value(&candidate).unwrap(), before, "the public operation pipeline rejects the whole batch without mutation");
    }

    #[test]
    fn regular_table_and_separate_component_can_coexist_and_change_in_one_batch() {
        let mut doc = document();
        let table = table_group("regular-table", "regular-cell");
        let table_op = table_change(&table);
        doc.pages[0].nodes.push(table);
        set_page_role(&mut doc, "component", Some(PageRole::Component)).unwrap();
        instantiate(&mut doc, "regular", None, "instance", "component", Rect { x: 300, y: 200, width: 4000, height: 3000 }).unwrap();
        assert!(validate_document(&doc).is_empty(), "ordinary tables and separate components coexist");

        let operations = [Operation::SetText { node_id: "source-text".into(), text: "updated".into() }, table_op];
        let mut links = Vec::new();
        let (changed, diagnostics) = ops::apply(&mut doc, &mut links, &NoAssets, &operations).unwrap();
        assert!(diagnostics.is_empty());
        assert!(changed.iter().any(|id| id == "instance::source-text"));
        assert_eq!(model::find_node_in_doc(&doc, "instance::source-text").unwrap().text, "updated");
        assert_eq!(model::find_node_in_doc(&doc, "regular-table").unwrap().ext.table.as_ref().unwrap().header_row_id.as_deref(), Some("regular-table-row"));
        assert!(validate_document(&doc).is_empty());
    }

    #[test]
    fn instance_children_keep_parent_relative_geometry_and_remap_internal_interactions() {
        let mut doc = document();
        set_page_role(&mut doc, "component", Some(PageRole::Component)).unwrap();
        let changed = instantiate(&mut doc, "regular", None, "instance", "component", Rect { x: 300, y: 200, width: 4000, height: 3000 }).unwrap();
        let instance = &doc.pages[0].nodes[0];
        assert_eq!(instance.id, "instance");
        assert_eq!(instance.bounds, Rect { x: 300, y: 200, width: 4000, height: 3000 });
        assert_eq!(instance.children[0].id, "instance::source-group");
        assert_eq!(instance.children[0].bounds, Rect { x: 15, y: 20, width: 1200, height: 700 });
        assert_eq!(instance.children[0].children[0].id, "instance::source-text");
        assert_eq!(instance.children[0].children[0].bounds, Rect { x: 25, y: 35, width: 900, height: 300 });
        assert!(matches!(instance.children[0].interactions.as_slice(), [Interaction::ToggleVisibility { node_id }] if node_id == "instance::source-text"));
        assert!(matches!(changed.as_slice(), [group, first, second] if group == "instance" && first == "instance::source-group" && second == "instance::source-text"));
        assert_eq!(instance.ext.instance.as_ref().unwrap().component_page_id, "component");
        assert!(validate_document(&doc).is_empty());
        let mut edited = doc;
        edited.pages[0].nodes[0].children[0].children[0].text.push('!');
        assert!(validate_document(&edited).iter().any(|diagnostic| diagnostic.code == "COMPONENT_INSTANCE_STALE"));
    }

    #[test]
    fn id_collision_and_locked_parent_reject_without_mutating_document() {
        let mut collision = document();
        collision.pages[0].nodes.push(node("instance::source-group", NodeKind::Rectangle, Rect { x: 1, y: 2, width: 3, height: 4 }, vec![]));
        set_page_role(&mut collision, "component", Some(PageRole::Component)).unwrap();
        let before = serde_json::to_value(&collision).unwrap();
        let error = instantiate(&mut collision, "regular", None, "instance", "component", Rect { x: 1, y: 2, width: 3, height: 4 }).unwrap_err();
        assert_eq!(error.code, ErrorCode::AlreadyExists);
        assert_eq!(serde_json::to_value(&collision).unwrap(), before);

        let mut row_collision = document();
        let mut outside_table = table_group("outside-table", "outside-cell");
        outside_table.ext.table.as_mut().unwrap().rows[0].id = "instance::source-table-row".into();
        row_collision.pages[0].nodes.push(outside_table);
        row_collision.pages[1].nodes[0].children.push(table_group("source-table", "source-cell"));
        set_page_role(&mut row_collision, "component", Some(PageRole::Component)).unwrap();
        let before = serde_json::to_value(&row_collision).unwrap();
        let error = instantiate(&mut row_collision, "regular", None, "instance", "component", Rect { x: 1, y: 2, width: 3, height: 4 }).unwrap_err();
        assert_eq!(error.code, ErrorCode::AlreadyExists);
        assert_eq!(error.affected_ids, ["instance::source-table-row"]);
        assert_eq!(serde_json::to_value(&row_collision).unwrap(), before, "row ID collisions are rejected before adding an instance");

        let mut locked = document();
        let mut parent = node("locked-parent", NodeKind::Group, Rect { x: 0, y: 0, width: 100, height: 100 }, vec![]);
        parent.ext.locked = Some(true);
        locked.pages[0].nodes.push(parent);
        set_page_role(&mut locked, "component", Some(PageRole::Component)).unwrap();
        let before = serde_json::to_value(&locked).unwrap();
        let error = instantiate(&mut locked, "regular", Some("locked-parent"), "instance", "component", Rect { x: 1, y: 2, width: 3, height: 4 }).unwrap_err();
        assert_eq!(error.code, ErrorCode::ContentLocked);
        assert_eq!(serde_json::to_value(&locked).unwrap(), before);
    }

    #[test]
    fn new_instance_ids_reserve_double_colon() {
        let mut doc = document();
        set_page_role(&mut doc, "component", Some(PageRole::Component)).unwrap();
        assert_eq!(instantiate(&mut doc, "regular", None, "bad::id", "component", Rect { x: 0, y: 0, width: 10, height: 10 }).unwrap_err().code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn source_geometry_refresh_keeps_instance_frame_and_refuses_locked_propagation_atomically() {
        let mut doc = document();
        set_page_role(&mut doc, "component", Some(PageRole::Component)).unwrap();
        instantiate(&mut doc, "regular", None, "instance", "component", Rect { x: 300, y: 200, width: 4000, height: 3000 }).unwrap();
        let instance = &mut doc.pages[0].nodes[0];
        instance.bounds.x = 350;
        instance.bounds.y = 240;
        doc.pages[1].nodes[0].bounds.x = 45;
        doc.pages[1].nodes[0].children[0].bounds.y = 55;
        let (changed, diagnostics) = refresh_instances(&mut doc).unwrap();
        assert!(diagnostics.is_empty());
        let instance = &doc.pages[0].nodes[0];
        assert_eq!(instance.bounds, Rect { x: 350, y: 240, width: 4000, height: 3000 });
        assert_eq!(instance.children[0].bounds, Rect { x: 45, y: 20, width: 1200, height: 700 });
        assert_eq!(instance.children[0].children[0].bounds.y, 55);
        assert!(changed.contains(&"instance::source-group".to_string()));
        assert!(changed.contains(&"instance::source-text".to_string()));

        let mut locked_group = document();
        set_page_role(&mut locked_group, "component", Some(PageRole::Component)).unwrap();
        instantiate(&mut locked_group, "regular", None, "instance", "component", Rect { x: 300, y: 200, width: 4000, height: 3000 }).unwrap();
        locked_group.pages[0].nodes[0].ext.locked = Some(true);
        locked_group.pages[1].nodes[0].children[0].text = "changed".into();
        let before = locked_group.pages[0].nodes[0].children[0].children[0].text.clone();
        assert_eq!(refresh_instances(&mut locked_group).unwrap_err().code, ErrorCode::ContentLocked);
        assert_eq!(locked_group.pages[0].nodes[0].children[0].children[0].text, before);

        let mut locked_child = document();
        set_page_role(&mut locked_child, "component", Some(PageRole::Component)).unwrap();
        instantiate(&mut locked_child, "regular", None, "instance", "component", Rect { x: 300, y: 200, width: 4000, height: 3000 }).unwrap();
        locked_child.pages[0].nodes[0].children[0].children[0].ext.locked = Some(true);
        locked_child.pages[1].nodes[0].children[0].text = "changed".into();
        let before = locked_child.pages[0].nodes[0].children[0].children[0].text.clone();
        assert_eq!(refresh_instances(&mut locked_child).unwrap_err().code, ErrorCode::ContentLocked);
        assert_eq!(locked_child.pages[0].nodes[0].children[0].children[0].text, before);
    }

    #[test]
    fn text_and_style_overrides_survive_source_refresh_and_detach_stops_updates() {
        let mut doc = document();
        set_page_role(&mut doc, "component", Some(PageRole::Component)).unwrap();
        instantiate(&mut doc, "regular", None, "instance", "component", Rect { x: 300, y: 200, width: 4000, height: 3000 }).unwrap();
        let instance_text = model::find_node_in_doc_mut(&mut doc, "instance::source-text").unwrap();
        instance_text.text = "local copy".into();
        instance_text.style.font_size = 200_000;
        capture_override(&mut doc, &Operation::SetText { node_id: "instance::source-text".into(), text: "local copy".into() }).unwrap();
        capture_override(&mut doc, &Operation::SetStyle {
            node_ids: vec!["instance::source-text".into()], patch: StylePatch { font_size: Some(200_000), ..Default::default() },
        }).unwrap();
        let instance_text = model::find_node_in_doc_mut(&mut doc, "instance::source-text").unwrap();
        instance_text.style.fill = "#123456".into();
        capture_override(&mut doc, &Operation::SetStyle {
            node_ids: vec!["instance::source-text".into()], patch: StylePatch { fill: Some("#123456".into()), ..Default::default() },
        }).unwrap();

        let source_text = model::find_node_in_doc_mut(&mut doc, "source-text").unwrap();
        source_text.text = "updated source".into();
        source_text.style.font_size = 300_000;
        source_text.style.fill = "#ABCDEF".into();
        refresh_instances(&mut doc).unwrap();
        let instance_text = model::find_node(&doc.pages[0].nodes, "instance::source-text").unwrap();
        assert_eq!(instance_text.text, "local copy");
        assert_eq!(instance_text.style.font_size, 200_000);
        assert_eq!(instance_text.style.fill, "#123456");
        assert!(validate_document(&doc).is_empty());

        detach(&mut doc, "instance").unwrap();
        let source_text = model::find_node_in_doc_mut(&mut doc, "source-text").unwrap();
        source_text.text = "detached source edit".into();
        let (changed, diagnostics) = refresh_instances(&mut doc).unwrap();
        assert!(changed.is_empty() && diagnostics.is_empty());
        let detached = model::find_node(&doc.pages[0].nodes, "instance").unwrap();
        assert!(detached.ext.instance.is_none());
        let instance_text = model::find_node(&detached.children, "instance::source-text").unwrap();
        assert_eq!(instance_text.text, "local copy");
        assert_eq!(instance_text.style.font_size, 200_000);
    }

    #[test]
    fn same_change_set_refreshes_source_before_child_override_and_preserves_reverse_order() {
        let make_doc = || {
            let mut doc = document();
            set_page_role(&mut doc, "component", Some(PageRole::Component)).unwrap();
            instantiate(&mut doc, "regular", None, "instance", "component", Rect { x: 300, y: 200, width: 4000, height: 3000 }).unwrap();
            doc
        };
        let mut doc = make_doc();
        let operations = [
            Operation::SetText { node_id: "source-text".into(), text: "source first".into() },
            Operation::SetText { node_id: "instance::source-text".into(), text: "local second".into() },
            Operation::DetachInstance { instance_id: "instance".into() },
        ];
        let mut links = Vec::new();
        ops::apply(&mut doc, &mut links, &NoAssets, &operations).unwrap();
        let detached = model::find_node(&doc.pages[0].nodes, "instance").unwrap();
        assert!(detached.ext.instance.is_none());
        assert_eq!(model::find_node(&detached.children, "instance::source-text").unwrap().text, "local second");

        let mut doc = make_doc();
        let operations = [
            Operation::SetText { node_id: "instance::source-text".into(), text: "local first".into() },
            Operation::SetText { node_id: "source-text".into(), text: "source second".into() },
        ];
        let mut links = Vec::new();
        ops::apply(&mut doc, &mut links, &NoAssets, &operations).unwrap();
        let instance_text = model::find_node_in_doc(&doc, "instance::source-text").unwrap();
        assert_eq!(instance_text.text, "local first");
        assert!(validate_document(&doc).is_empty());
        ops::apply(&mut doc, &mut links, &NoAssets, &[Operation::DetachInstance { instance_id: "instance".into() }]).unwrap();
        assert!(model::find_node_in_doc(&doc, "instance").unwrap().ext.instance.is_none());
    }

    #[test]
    fn unchanged_token_values_do_not_let_binding_propagation_bypass_locked_instances_or_children() {
        let token = || TokenDef {
            name: "Transparent fill".into(),
            values: TokenValues::Color { light: "transparent".into(), dark: "transparent".into() },
        };
        let binding = |node_id: &str| TokenBinding {
            node_id: node_id.into(), target: BindTarget::StyleFill,
            token: TokenRef { source: TokenSource::Local, token_id: "fill".into() },
        };
        let mut base = document();
        base.tokens.insert("fill".into(), token());
        set_page_role(&mut base, "component", Some(PageRole::Component)).unwrap();
        instantiate(&mut base, "regular", None, "instance", "component", Rect { x: 300, y: 200, width: 4000, height: 3000 }).unwrap();
        base.pages[0].nodes[0].ext.locked = Some(true);
        let mut bind_candidate = base.clone();
        let bind = Operation::BindToken { node_ids: vec!["source-text".into()], target: BindTarget::StyleFill, token: binding("source-text").token };
        let mut links = Vec::new();
        assert_eq!(ops::apply(&mut bind_candidate, &mut links, &NoAssets, &[bind]).unwrap_err().code, ErrorCode::ContentLocked);
        assert!(!base.bindings.iter().any(|binding| binding.node_id == "instance::source-text"));

        let mut bound = base.clone();
        bound.bindings.push(binding("source-text"));
        bound.bindings.push(binding("instance::source-text"));
        let mut unbind_candidate = bound.clone();
        let unbind = Operation::UnbindToken { node_ids: vec!["source-text".into()], target: BindTarget::StyleFill };
        assert_eq!(ops::apply(&mut unbind_candidate, &mut links, &NoAssets, &[unbind]).unwrap_err().code, ErrorCode::ContentLocked);
        assert!(bound.bindings.iter().any(|binding| binding.node_id == "instance::source-text"));

        let mut locked_child = base.clone();
        locked_child.pages[0].nodes[0].ext.locked = None;
        model::find_node_in_doc_mut(&mut locked_child, "instance::source-text").unwrap().ext.locked = Some(true);
        let mut child_candidate = locked_child.clone();
        let bind = Operation::BindToken { node_ids: vec!["source-text".into()], target: BindTarget::StyleFill, token: binding("source-text").token };
        assert_eq!(ops::apply(&mut child_candidate, &mut links, &NoAssets, &[bind]).unwrap_err().code, ErrorCode::ContentLocked);
        assert!(!locked_child.bindings.iter().any(|binding| binding.node_id == "instance::source-text"));

        let mut token_update = base.clone();
        token_update.tokens.insert("fill".into(), TokenDef {
            name: "Fill".into(), values: TokenValues::Color { light: "#000000".into(), dark: "#000000".into() },
        });
        for id in ["source-text", "instance::source-text"] {
            model::find_node_in_doc_mut(&mut token_update, id).unwrap().style.fill = "#000000".into();
            token_update.bindings.push(binding(id));
        }
        let mut token_candidate = token_update.clone();
        let put = Operation::PutToken {
            token_id: "fill".into(), name: "Fill".into(),
            values: TokenValues::Color { light: "#FF0000".into(), dark: "#FF0000".into() },
        };
        assert_eq!(ops::apply(&mut token_candidate, &mut links, &NoAssets, &[put]).unwrap_err().code, ErrorCode::ContentLocked);
        assert_eq!(model::find_node_in_doc(&token_update, "instance::source-text").unwrap().style.fill, "#000000");
    }

    #[test]
    fn local_and_shared_token_updates_preflight_locked_instance_roots_and_children() {
        let operation = || Operation::SetTokenMode { mode: TokenMode::Dark };
        let mut links = Vec::new();
        for lock_root in [true, false] {
            let mut base = bound_component(TokenSource::Local, "#000000", "#FFFFFF");
            if lock_root { model::find_node_in_doc_mut(&mut base, "instance").unwrap().ext.locked = Some(true); }
            else { model::find_node_in_doc_mut(&mut base, "instance::source-text").unwrap().ext.locked = Some(true); }
            let mut candidate = base.clone();
            assert_eq!(ops::apply(&mut candidate, &mut links, &NoAssets, &[operation()]).unwrap_err().code, ErrorCode::ContentLocked);
            assert_eq!(model::find_node_in_doc(&candidate, "source-text").unwrap().style.fill, "#000000");
            assert_eq!(model::find_node_in_doc(&candidate, "instance::source-text").unwrap().style.fill, "#000000");
        }

        let mut local_update = bound_component(TokenSource::Local, "#000000", "#000000");
        model::find_node_in_doc_mut(&mut local_update, "instance::source-text").unwrap().ext.locked = Some(true);
        let mut candidate = local_update.clone();
        let put = Operation::PutToken {
            token_id: "fill".into(), name: "Fill".into(),
            values: TokenValues::Color { light: "#FF0000".into(), dark: "#FF0000".into() },
        };
        assert_eq!(ops::apply(&mut candidate, &mut links, &NoAssets, &[put]).unwrap_err().code, ErrorCode::ContentLocked);
        assert_eq!(model::find_node_in_doc(&candidate, "source-text").unwrap().style.fill, "#000000");
        assert_eq!(model::find_node_in_doc(&candidate, "instance::source-text").unwrap().style.fill, "#000000");

        let mut local_noop = bound_component(TokenSource::Local, "#000000", "#000000");
        model::find_node_in_doc_mut(&mut local_noop, "instance").unwrap().ext.locked = Some(true);
        ops::apply(&mut local_noop, &mut links, &NoAssets, &[operation()]).expect("same concrete values are allowed under a lock");
        assert_eq!(model::find_node_in_doc(&local_noop, "instance::source-text").unwrap().style.fill, "#000000");

        let shared_tokens = BTreeMap::from([("fill".into(), TokenDef {
            name: "Fill".into(), values: TokenValues::Color { light: "#FF0000".into(), dark: "#FF0000".into() },
        })]);
        for lock_root in [true, false] {
            let mut base = bound_component(TokenSource::Shared, "#000000", "#000000");
            if lock_root { model::find_node_in_doc_mut(&mut base, "instance").unwrap().ext.locked = Some(true); }
            else { model::find_node_in_doc_mut(&mut base, "instance::source-text").unwrap().ext.locked = Some(true); }
            let mut candidate = base.clone();
            assert_eq!(tokens::sync_shared_bindings(&mut candidate, &shared_tokens).unwrap_err().code, ErrorCode::ContentLocked);
            assert_eq!(model::find_node_in_doc(&candidate, "source-text").unwrap().style.fill, "#000000");
            assert_eq!(model::find_node_in_doc(&candidate, "instance::source-text").unwrap().style.fill, "#000000");
        }

        let mut shared_noop = bound_component(TokenSource::Shared, "#000000", "#000000");
        model::find_node_in_doc_mut(&mut shared_noop, "instance").unwrap().ext.locked = Some(true);
        let same_tokens = BTreeMap::from([("fill".into(), TokenDef {
            name: "Fill".into(), values: TokenValues::Color { light: "#000000".into(), dark: "#000000".into() },
        })]);
        assert!(tokens::sync_shared_bindings(&mut shared_noop, &same_tokens).unwrap().is_empty());
    }

    #[test]
    fn deleting_a_source_node_prunes_its_override_with_a_nonblocking_diagnostic() {
        let mut doc = document();
        doc.pages[1].nodes[0].interactions.clear();
        set_page_role(&mut doc, "component", Some(PageRole::Component)).unwrap();
        instantiate(&mut doc, "regular", None, "instance", "component", Rect { x: 300, y: 200, width: 4000, height: 3000 }).unwrap();
        model::find_node_in_doc_mut(&mut doc, "instance::source-text").unwrap().text = "local".into();
        capture_override(&mut doc, &Operation::SetText { node_id: "instance::source-text".into(), text: "local".into() }).unwrap();
        let mut links = Vec::new();
        let (changed, diagnostics) = ops::apply(&mut doc, &mut links, &NoAssets, &[Operation::RemoveNode { node_id: "source-text".into() }]).unwrap();
        assert!(changed.contains(&"instance".to_string()));
        assert!(changed.contains(&"instance::source-text".to_string()));
        assert!(diagnostics.iter().any(|diagnostic| diagnostic.code == "INSTANCE_OVERRIDE_REMOVED" && !diagnostic.blocking));
        let instance = model::find_node_in_doc(&doc, "instance").unwrap();
        assert!(instance.ext.instance.as_ref().unwrap().overrides.is_empty());
        assert!(model::find_node(&instance.children, "instance::source-text").is_none());
        assert!(validate_document(&doc).is_empty());
    }

    #[test]
    fn source_unlock_does_not_change_a_locked_instance_or_child() {
        let mut doc = document();
        doc.pages[1].nodes[0].children[0].ext.locked = Some(true);
        set_page_role(&mut doc, "component", Some(PageRole::Component)).unwrap();
        instantiate(&mut doc, "regular", None, "instance", "component", Rect { x: 300, y: 200, width: 4000, height: 3000 }).unwrap();
        doc.pages[0].nodes[0].ext.locked = Some(true);
        doc.pages[1].nodes[0].children[0].ext.locked = None;
        assert_eq!(refresh_instances(&mut doc).unwrap_err().code, ErrorCode::ContentLocked);
        assert_eq!(model::find_node_in_doc(&doc, "instance::source-text").unwrap().ext.locked, Some(true));
    }

    #[test]
    fn adding_a_copied_instance_rematerializes_ids_and_bindings() {
        let mut doc = document();
        set_page_role(&mut doc, "component", Some(PageRole::Component)).unwrap();
        doc.bindings.push(TokenBinding {
            node_id: "source-text".into(), target: BindTarget::StyleFill,
            token: TokenRef { source: TokenSource::Local, token_id: "fill".into() },
        });
        instantiate(&mut doc, "regular", None, "instance", "component", Rect { x: 300, y: 200, width: 4000, height: 3000 }).unwrap();
        model::find_node_in_doc_mut(&mut doc, "instance::source-text").unwrap().text = "local copy".into();
        capture_override(&mut doc, &Operation::SetText { node_id: "instance::source-text".into(), text: "local copy".into() }).unwrap();

        let mut duplicate = model::find_node_in_doc(&doc, "instance").unwrap().clone();
        duplicate.id = "copy".into();
        duplicate.children[0].id = "temporary-group".into();
        duplicate.children[0].children[0].id = "temporary-text".into();
        duplicate.interactions = vec![Interaction::ToggleVisibility { node_id: "temporary-text".into() }];
        let mut links = Vec::new();
        ops::apply(&mut doc, &mut links, &NoAssets, &[Operation::AddNode { page_id: "regular".into(), parent_id: None, node: duplicate }]).unwrap();
        let copy = model::find_node_in_doc(&doc, "copy").unwrap();
        assert_eq!(copy.children[0].id, "copy::source-group");
        assert_eq!(copy.children[0].children[0].id, "copy::source-text");
        assert_eq!(copy.children[0].children[0].text, "local copy");
        assert!(matches!(copy.interactions.as_slice(), [Interaction::ToggleVisibility { node_id }] if node_id == "copy::source-text"));
        assert!(doc.bindings.iter().any(|binding| binding.node_id == "copy::source-text"));
        assert!(validate_document(&doc).is_empty());
    }

    #[test]
    fn local_token_override_detaches_only_the_overridden_binding_and_survives_source_update() {
        let mut doc = bound_component(TokenSource::Local, "#000000", "#000000");
        let mut links = Vec::new();
        ops::apply(&mut doc, &mut links, &NoAssets, &[Operation::SetStyle {
            node_ids: vec!["instance::source-text".into()], patch: StylePatch { fill: Some("#123456".into()), ..Default::default() },
        }]).unwrap();
        assert!(doc.bindings.iter().any(|binding| binding.node_id == "source-text"));
        assert!(!doc.bindings.iter().any(|binding| binding.node_id == "instance::source-text"));
        ops::apply(&mut doc, &mut links, &NoAssets, &[Operation::PutToken {
            token_id: "fill".into(), name: "Fill".into(),
            values: TokenValues::Color { light: "#FF0000".into(), dark: "#FF0000".into() },
        }]).unwrap();
        assert_eq!(model::find_node_in_doc(&doc, "source-text").unwrap().style.fill, "#FF0000");
        assert_eq!(model::find_node_in_doc(&doc, "instance::source-text").unwrap().style.fill, "#123456");
        assert!(validate_document(&doc).is_empty());
    }
}
