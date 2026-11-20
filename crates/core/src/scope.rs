//! Target scope of a job with `target_ids` (CCR-B). Such a job may change only its targets, what
//! lies inside them (every element nested in a target), and what the same job created. Operations
//! on the document as a whole (insert a page or a panel, the figure caption) are outside any
//! target. A job without target_ids works on the whole artifact.
//!
//! Ids are read from the artifact JSON: every string under an `id` key is an element id, and an
//! element contains every id nested inside it. Targets that are not element ids (web file paths,
//! `style:<name>`) match literally.
use crate::errors::{err, ErrExt};
use crate::pipeline::op_targets;
use crate::{store, Workspace};
use designllm_contracts::*;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Every `id` string in a JSON tree.
pub(crate) fn ids_in(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(m) => {
            if let Some(Value::String(id)) = m.get("id") { out.insert(id.clone()); }
            for v in m.values() { ids_in(v, out); }
        }
        Value::Array(items) => for v in items { ids_in(v, out); },
        _ => {}
    }
}

/// The ids inside every element of `doc` whose id is one of `roots` (the roots included).
pub(crate) fn inside(doc: &Value, roots: &BTreeSet<String>, out: &mut BTreeSet<String>) {
    match doc {
        Value::Object(m) => {
            if m.get("id").and_then(Value::as_str).is_some_and(|id| roots.contains(id)) { ids_in(doc, out); return; }
            for v in m.values() { inside(v, roots, out); }
        }
        Value::Array(items) => for v in items { inside(v, roots, out); },
        _ => {}
    }
}

fn include_canvas_page_memos(artifact: &Artifact, page_roots: &BTreeSet<String>, allowed: &mut BTreeSet<String>) {
    let ArtifactBody::Canvas(document) = &artifact.body else { return };
    for memo in document.board_memos.iter().filter(|memo| page_roots.contains(&memo.page_id)) {
        allowed.insert(memo.id.clone());
    }
}

fn include_canvas_page_sections(artifact: &Artifact, page_roots: &BTreeSet<String>, allowed: &mut BTreeSet<String>) {
    let ArtifactBody::Canvas(document) = &artifact.body else { return };
    for section in document.page_sections.iter().filter(|section| page_roots.contains(&section.start_page_id)) {
        allowed.insert(section.id.clone());
    }
}

/// The closed set of `roots` in `artifact`: the roots, everything inside them, and the memos and section markers of canvas
/// pages among them. Job scopes and job reservations (`Workspace::job_holds`) both compute it here.
pub(crate) fn closed_ids(artifact: &Artifact, roots: &BTreeSet<String>) -> DltResult<BTreeSet<String>> {
    let mut allowed = roots.clone();
    inside(&json_of(artifact)?, roots, &mut allowed);
    include_canvas_page_memos(artifact, roots, &mut allowed);
    include_canvas_page_sections(artifact, roots, &mut allowed);
    Ok(allowed)
}

/// What one operation needs in scope and what it creates. `None`: the document as a whole.
fn op_scope(op: &Operation, existing: &BTreeSet<String>) -> Option<(Vec<String>, BTreeSet<String>)> {
    use Operation::*;
    let made = |v: Result<Value, serde_json::Error>| { let mut s = BTreeSet::new(); if let Ok(v) = v { ids_in(&v, &mut s); } s };
    // Put operations upsert by id: an id the document has is changed, a new one is created.
    let upsert = |id: &String| if existing.contains(id) { (vec![id.clone()], BTreeSet::new()) } else { (vec![], BTreeSet::from([id.clone()])) };
    Some(match op {
        AddNode { page_id, parent_id, node } => {
            let mut addressed = vec![parent_id.clone().unwrap_or_else(|| page_id.clone())];
            let mut sources = Vec::new();
            copied_source_ids(node, &mut sources);
            for source in sources { if !addressed.contains(&source) { addressed.push(source); } }
            let mut created = BTreeSet::new();
            added_node_ids(node, &mut created);
            (addressed, created)
        }
        AddDiagramNode { panel_id, node, .. } => (vec![panel_id.clone()], made(serde_json::to_value(node))),
        ConnectDiagram { panel_id, edge } => (vec![panel_id.clone(), edge.from.clone(), edge.to.clone()], BTreeSet::from([edge.id.clone()])),
        InsertFlowBlock { section_id, block, .. } => (vec![section_id.clone()], made(serde_json::to_value(block))),
        MoveNode { node_id, page_id, parent_id, .. } => (vec![node_id.clone(), parent_id.clone().unwrap_or_else(|| page_id.clone())], BTreeSet::new()),
        SetCanvasTableColumnWidths { table_id, .. } => (vec![table_id.clone()], BTreeSet::new()),
        SetCanvasTableStructure { table_id, expected, table } => {
            let mut targets = vec![table_id.clone()];
            let mut created = BTreeSet::new();
            for row in &expected.rows {
                targets.push(row.id.clone());
                targets.extend(row.cell_ids.iter().cloned());
            }
            for row in &table.rows {
                if existing.contains(&row.id) { targets.push(row.id.clone()); } else { created.insert(row.id.clone()); }
                for cell_id in &row.cell_ids {
                    if existing.contains(cell_id) { targets.push(cell_id.clone()); } else { created.insert(cell_id.clone()); }
                }
            }
            (targets, created)
        }
        SetCanvasBoardMemos { page_id, expected, memos } => {
            let mut targets = vec![page_id.clone()];
            let mut created = BTreeSet::new();
            targets.extend(expected.iter().map(|memo| memo.id.clone()));
            for memo in memos {
                if existing.contains(&memo.id) { targets.push(memo.id.clone()); } else { created.insert(memo.id.clone()); }
            }
            (targets, created)
        }
        SetCanvasPageGuides { page_id, .. } => (vec![page_id.clone()], BTreeSet::new()),
        // Whole document (2026-11-18 decision; op_targets still reserves its ids).
        SetCanvasPageSections { .. } => return None,
        SetPageSize { page_id, .. } | RenamePage { page_id, .. } => (vec![page_id.clone()], BTreeSet::new()),
        SetPageRole { page_id, .. } => (vec![page_id.clone()], BTreeSet::new()),
        InstantiateComponent { page_id, parent_id, instance_id, component_page_id, .. } => {
            let mut addressed = vec![component_page_id.clone(), parent_id.clone().unwrap_or_else(|| page_id.clone())];
            if !addressed.contains(page_id) { addressed.push(page_id.clone()); }
            let created = if existing.contains(instance_id) { BTreeSet::new() } else { BTreeSet::from([instance_id.clone()]) };
            (addressed, created)
        }
        DetachInstance { instance_id } => (vec![instance_id.clone()], BTreeSet::new()),
        ReorderPages { page_ids } => (page_ids.clone(), BTreeSet::new()),
        ReorderPanels { panel_ids } => (panel_ids.clone(), BTreeSet::new()),
        PutDataset { dataset } => upsert(&dataset.id),
        PutEvidence { evidence } => upsert(&evidence.id),
        // Whole-document operations: no target. InsertPage and AddPanel keep it by the 2026-11-18 decision (pinned by
        // tests/job_targets_canvas_figure.rs). SetCaption has no target in op_targets either.
        InsertPage { .. } | AddPanel { .. } | SetCaption { .. } => return None,
        // A theme change restyles every slide: a whole-document change. op_targets still reserves "theme".
        SetTheme { .. } => return None,
        // Each of these addresses what op_targets lists and creates nothing. Listed, not `_`: an Operation added to the
        // enum must be placed here and in op_targets (both matches are exhaustive).
        ReplaceText { .. } | SetTextStyleRange { .. } | SetText { .. } | SetGeometry { .. } | RemoveNode { .. }
        | SetLayout { .. } | SetInteraction { .. } | SetInteractions { .. } | SetVariantStyle { .. }
        | ReplaceImageAsset { .. } | SetCanvasImageCrop { .. } | SetStyle { .. } | SetNamedStyle { .. }
        | SetParagraphFormat { .. } | SetPageSetup { .. } | SetTableDimensions { .. } | RemovePage { .. }
        | SetPageNotes { .. } | RemovePanel { .. } | SetPlot { .. } | SetPlotDisplay { .. } | SetAxisLabel { .. }
        | SetAxisTicks { .. } | SetPanelLabel { .. } | SetDiagramProvenance { .. } | SetFigureImageTransform { .. }
        | SetFigureImageCalibration { .. } | SetCommonLegend { .. } | SetFigureVisualReview { .. }
        | ArrangePanels { .. } | UpdateFigureLink { .. } | ReplaceWebSource { .. } | MappedWebEdit { .. }
        | SetZOrder { .. } | SetNodeAppearance { .. } | SetNodeInfo { .. } | SetNodeList { .. } | SetNodePath { .. }
        | BindToken { .. } | UnbindToken { .. } | PutToken { .. } | RemoveToken { .. } | SetTokenMode { .. }
        // SetGroupRepeat: the group, and the table and component page of the new and the current spec (op_targets).
        | SetGroupRepeat { .. } | SetGroupCondition { .. } => {
            let targets = op_targets(op);
            if targets.is_empty() { return None; }
            (targets, BTreeSet::new())
        }
    })
}

/// The outcome of checking a change set against a job's scope.
#[derive(Debug, Default)]
pub(crate) struct ScopeCheck {
    /// Ids the operations address outside the scope, in operation order, each once.
    pub outside: Vec<String>,
    /// Operations that change the document as a whole.
    pub whole_document: Vec<String>,
    /// Ids the operations create (they join the job's scope).
    pub created: BTreeSet<String>,
}

/// `allowed` grows with what earlier operations of the same change set created.
#[cfg(test)]
pub(crate) fn check(ops: &[Operation], allowed: &BTreeSet<String>, existing: &BTreeSet<String>) -> ScopeCheck {
    check_with_component_children(ops, allowed, existing, &BTreeMap::new())
}

fn check_with_component_children(
    ops: &[Operation],
    allowed: &BTreeSet<String>,
    existing: &BTreeSet<String>,
    component_children: &BTreeMap<String, BTreeSet<String>>,
) -> ScopeCheck {
    let mut allowed = allowed.clone();
    let mut out = ScopeCheck::default();
    for op in ops {
        match op_scope(op, existing) {
            None => out.whole_document.push(op.name().to_string()),
            Some((addressed, mut created)) => {
                for id in addressed {
                    if !allowed.contains(&id) && !out.outside.contains(&id) { out.outside.push(id); }
                }
                let instance_id = match op {
                    Operation::InstantiateComponent { instance_id, .. } => Some(instance_id.as_str()),
                    Operation::AddNode { node, .. } if node.ext.instance.is_some() => Some(node.id.as_str()),
                    _ => None,
                };
                if let Some(instance_id) = instance_id {
                    if let Some(children) = component_children.get(instance_id) { created.extend(children.iter().cloned()); }
                }
                allowed.extend(created.iter().cloned());
                out.created.extend(created);
            }
        }
    }
    out
}

fn json_of(artifact: &Artifact) -> DltResult<Value> { serde_json::to_value(artifact).map_err(|e| err(ErrorCode::Internal, e.to_string())) }

fn planned_component_children(
    artifact: &Artifact,
    ops: &[Operation],
    existing: &BTreeSet<String>,
) -> DltResult<BTreeMap<String, BTreeSet<String>>> {
    let mut planned = BTreeSet::new();
    let mut component_children = BTreeMap::new();
    let mut component_pages: BTreeSet<String> = match &artifact.body {
        ArtifactBody::Canvas(document) => document.pages.iter().filter(|page| page.role == Some(PageRole::Component)).map(|page| page.id.clone()).collect(),
        _ => BTreeSet::new(),
    };
    for operation in ops {
        if let Operation::SetPageRole { page_id, role: Some(PageRole::Component) } = operation {
            component_pages.insert(page_id.clone());
        }
    }
    for operation in ops {
        let instance = match operation {
            Operation::InstantiateComponent { instance_id, component_page_id, .. } => Some((instance_id.as_str(), component_page_id.as_str())),
            Operation::AddNode { node, .. } => node.ext.instance.as_ref().map(|reference| (node.id.as_str(), reference.component_page_id.as_str())),
            _ => None,
        };
        let Some((instance_id, component_page_id)) = instance else { continue };
        if existing.contains(instance_id) || !planned.insert(instance_id.to_string()) {
            return Err(err(ErrorCode::AlreadyExists, format!("component instance id {instance_id} already exists")).ids([instance_id.to_string()]));
        }
        let derived = component_child_ids(artifact, component_page_id, instance_id, &component_pages);
        if let Some(collision) = derived.iter().find(|id| existing.contains(*id)) {
            return Err(err(ErrorCode::AlreadyExists, format!("component child id {collision} already exists")).ids([collision.clone()]));
        }
        component_children.insert(instance_id.to_string(), derived);
    }
    Ok(component_children)
}

impl Workspace {
    /// The ids a job with target_ids may change in `artifact` (see `holds_in`). None: no target_ids.
    pub(crate) fn job_scope_ids(&self, job: &store::JobRow, artifact: &Artifact) -> DltResult<Option<BTreeSet<String>>> {
        if job.request.target_ids.is_empty() { return Ok(None); }
        let targets: BTreeSet<String> = job.request.target_ids.iter().cloned().collect();
        Ok(Some(self.holds_in(Some(&job.job_id), &targets, artifact)?))
    }

    /// What a reservation holds in `artifact`: its nodes and what lies inside them (`closed_ids`). A job's reservation also
    /// holds what its own commits created. The job's scope and its reservation are this one set, and a user's reservation
    /// uses it too, so a change is busy on exactly the ids the holder may not change.
    pub(crate) fn holds_in(&self, job_id: Option<&str>, roots: &BTreeSet<String>, artifact: &Artifact) -> DltResult<BTreeSet<String>> {
        let Some(job_id) = job_id else { return closed_ids(artifact, roots) };
        let mut all = roots.clone();
        all.extend(store::job_created_ids(&self.conn, job_id)?);
        closed_ids(artifact, &all)
    }

    /// What a user's reservation of `ids` holds on the current revision (`holds_in`), for `targets.reserve`.
    pub(crate) fn reservation_holds(&self, artifact_id: &str, ids: &[Id]) -> DltResult<BTreeSet<String>> {
        let (_, _, artifact, _) = self.load_revision(artifact_id, None)?;
        let roots: BTreeSet<String> = ids.iter().cloned().collect();
        self.holds_in(None, &roots, &artifact)
    }

    /// Recomputes the holds of every reservation on `artifact_id` after a commit moved the artifact to a new revision (so a
    /// new descendant of a reserved node is held too). If the artifact cannot be read, the holds fall back to the nodes
    /// themselves and the error is returned.
    pub(crate) fn refresh_holds(&mut self, artifact_id: &str) -> DltResult<()> {
        let reservations = self.views.reservations_on(artifact_id);
        if reservations.is_empty() { return Ok(()); }
        let computed = self.load_revision(artifact_id, None).and_then(|(_, _, artifact, _)| {
            reservations.iter()
                .map(|r| Ok((r.reservation_id.clone(), self.holds_in(r.job_id.as_deref(), &r.node_ids, &artifact)?)))
                .collect::<DltResult<Vec<_>>>()
        });
        match computed {
            Ok(held) => {
                for (reservation_id, held) in held { self.views.set_held(&reservation_id, held); }
                Ok(())
            }
            Err(error) => {
                for r in reservations { self.views.set_held(&r.reservation_id, r.node_ids); }
                Err(error)
            }
        }
    }

    /// The holds every reservation on `artifact_id` would have in `candidate`, the revision a write would commit, by
    /// reservation id. The post-check of a write counts them besides the current holds: ids the write newly places inside a
    /// held node.
    pub(crate) fn candidate_holds(&self, artifact_id: &str, candidate: &Artifact) -> DltResult<BTreeMap<String, BTreeSet<String>>> {
        self.views.reservations_on(artifact_id).iter()
            .map(|r| Ok((r.reservation_id.clone(), self.holds_in(r.job_id.as_deref(), &r.node_ids, candidate)?)))
            .collect()
    }

    /// The TARGET_BUSY check of a write that changes `changed` on `artifact_id`, against what the other reservations hold now
    /// and on `candidate`, the revision the write would commit. `actor` and `job` name the writer, whose own reservation passes
    /// (`Views::busy`). An empty actor and no job own nothing: every reservation counts.
    pub(crate) fn check_changed(&mut self, artifact_id: &str, changed: &BTreeSet<String>, actor: &str, job: Option<&str>, candidate: &Artifact) -> DltResult<()> {
        let extra = self.candidate_holds(artifact_id, candidate)?;
        match self.views.busy_with(artifact_id, changed, actor, job, &extra) {
            Some((ids, owner)) => Err(err(ErrorCode::TargetBusy, format!("changed targets are reserved by {owner:?}")).ids(ids)),
            None => Ok(()),
        }
    }

    /// The artifact a stored snapshot of `artifact_id` holds (an earlier revision the undo of a change restores).
    pub(crate) fn load_snapshot(&self, artifact_id: &str, snapshot: &str) -> DltResult<Artifact> {
        let row = store::artifact_row(&self.conn, artifact_id)?;
        let engine = self.engine_by_id(&row.engine_id)?;
        self.load(&*engine, snapshot, row.current_revision)
    }

    /// A change set of a job with target_ids: CONTENT_LOCKED (the outside ids in `affected_ids`)
    /// unless every operation stays in scope. Returns the ids the operations create.
    pub(crate) fn check_job_scope(&self, job: &store::JobRow, base: &Artifact, ops: &[Operation]) -> DltResult<BTreeSet<String>> {
        let Some(allowed) = self.job_scope_ids(job, base)? else { return Ok(BTreeSet::new()) };
        let mut existing = BTreeSet::new();
        ids_in(&json_of(base)?, &mut existing);
        let component_children = planned_component_children(base, ops, &existing)?;
        let result = check_with_component_children(ops, &allowed, &existing, &component_children);
        if result.outside.is_empty() && result.whole_document.is_empty() { return Ok(result.created); }
        let mut parts = Vec::new();
        if !result.outside.is_empty() { parts.push(format!("outside its targets: {}", result.outside.join(", "))); }
        if !result.whole_document.is_empty() { parts.push(format!("operations on the whole document: {}", result.whole_document.join(", "))); }
        let mut ids = result.outside;
        if !result.whole_document.is_empty() { ids.push(base.id.clone()); }
        Err(err(ErrorCode::ContentLocked, format!("job {} may change only its target_ids, what lies inside them and what it created; nothing was applied ({})", job.job_id, parts.join("; "))).ids(ids))
    }

    /// Checks engine-expanded effects after prepare_change. New descendants are admitted only under
    /// explicit target/job-created roots; an unlisted component instance never joins scope implicitly.
    pub(crate) fn check_job_scope_affected(
        &self,
        job: &store::JobRow,
        base: &Artifact,
        candidate: &Artifact,
        created: &BTreeSet<String>,
        changed_ids: &[String],
    ) -> DltResult<()> {
        let Some(mut allowed) = self.job_scope_ids(job, base)? else { return Ok(()) };
        allowed.extend(created.iter().cloned());
        let mut roots: BTreeSet<String> = job.request.target_ids.iter().cloned().collect();
        roots.extend(store::job_created_ids(&self.conn, &job.job_id)?);
        roots.extend(created.iter().cloned());
        allowed.extend(closed_ids(candidate, &roots)?);
        let outside: Vec<String> = changed_ids.iter().filter(|id| !allowed.contains(*id)).cloned().collect();
        if outside.is_empty() { return Ok(()) }
        Err(err(
            ErrorCode::ContentLocked,
            format!("job {} would change materialized targets outside its target_ids; nothing was applied", job.job_id),
        ).ids(outside))
    }

    /// Undo/redo by a job with target_ids: its own commits, or a change whose ids are in its scope.
    pub(crate) fn check_job_history(&self, job: &JobRef, artifact_id: &str, seq: i64, changed_ids: &[String]) -> DltResult<()> {
        let row = store::job_row(&self.conn, &job.job_id)?;
        if row.request.target_ids.is_empty() || store::undo_entry_job(&self.conn, artifact_id, seq)?.as_deref() == Some(job.job_id.as_str()) { return Ok(()); }
        let art = store::artifact_row(&self.conn, artifact_id)?;
        let engine = self.engine_by_id(&art.engine_id)?;
        let current = self.load(&*engine, &art.current_snapshot, art.current_revision)?;
        let allowed = self.job_scope_ids(&row, &current)?.unwrap_or_default();
        let outside: Vec<String> = changed_ids.iter().filter(|id| !allowed.contains(*id)).cloned().collect();
        if outside.is_empty() { return Ok(()); }
        Err(err(ErrorCode::ContentLocked, format!("job {} may take back only its own changes or changes inside its target_ids", job.job_id)).ids(outside))
    }
}

fn component_child_ids(
    artifact: &Artifact,
    component_page_id: &str,
    instance_id: &str,
    component_pages: &BTreeSet<String>,
) -> BTreeSet<String> {
    fn add(instance_id: &str, node: &Node, out: &mut BTreeSet<String>) {
        out.insert(format!("{instance_id}::{}", node.id));
        if let Some(table) = &node.ext.table {
            out.extend(table.rows.iter().map(|row| format!("{instance_id}::{}", row.id)));
        }
        for child in &node.children { add(instance_id, child, out); }
    }
    let ArtifactBody::Canvas(document) = &artifact.body else { return BTreeSet::new() };
    let Some(source) = document.pages.iter().find(|page| page.id == component_page_id && component_pages.contains(&page.id)) else {
        return BTreeSet::new();
    };
    if source.nodes.iter().any(contains_component_instance) { return BTreeSet::new(); }
    let mut ids = BTreeSet::new();
    for node in &source.nodes { add(instance_id, node, &mut ids); }
    ids
}

fn contains_component_instance(node: &Node) -> bool {
    node.ext.instance.is_some() || node.children.iter().any(contains_component_instance)
}

// Copied instance and repeat children are generated again. Their incoming IDs confer no job scope.
fn added_node_ids(node: &Node, out: &mut BTreeSet<String>) {
    out.insert(node.id.clone());
    if node.ext.repeat.is_some() || node.ext.instance.is_some() { return; }
    if let Some(table) = &node.ext.table { out.extend(table.rows.iter().map(|row| row.id.clone())); }
    for child in &node.children { added_node_ids(child, out); }
}

pub(crate) fn copied_source_ids(node: &Node, out: &mut Vec<String>) {
    if let Some(spec) = &node.ext.repeat {
        out.extend([spec.table_id.clone(), spec.component_page_id.clone()]);
        return;
    }
    if let Some(instance) = &node.ext.instance { out.push(instance.component_page_id.clone()); }
    for child in &node.children { copied_source_ids(child, out); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn set(ids: &[&str]) -> BTreeSet<String> { ids.iter().map(|s| s.to_string()).collect() }

    #[test]
    fn image_asset_replacement_requires_the_target_node_in_scope() {
        let replace = Operation::ReplaceImageAsset {
            node_id: "image".into(), expected_asset_sha256: "old".into(), asset_sha256: "new".into(),
        };
        let existing = set(&["image", "other"]);
        assert!(check(&[replace.clone()], &set(&["image"]), &existing).outside.is_empty());
        assert_eq!(check(&[replace], &set(&["other"]), &existing).outside, ["image"]);
    }

    #[test]
    fn a_target_contains_every_element_nested_in_it() {
        let doc = json!({ "id": "doc", "body": { "type": "canvas", "document": { "pages": [{ "id": "p", "nodes": [
            { "id": "card", "children": [{ "id": "title", "children": [] }, { "id": "icon", "children": [] }] },
            { "id": "other", "children": [] },
        ] }] } } });
        let mut out = BTreeSet::new();
        inside(&doc, &set(&["card"]), &mut out);
        assert_eq!(out, set(&["card", "title", "icon"]));
        let mut all = BTreeSet::new();
        inside(&doc, &set(&["doc"]), &mut all);
        assert_eq!(all, set(&["doc", "p", "card", "title", "icon", "other"]), "the artifact id targets everything");
    }

    #[test]
    fn operations_need_their_existing_ids_in_scope_and_their_new_ids_join_it() {
        let allowed = set(&["card", "title"]);
        let existing = set(&["p", "card", "title", "other", "d1"]);
        let node = |id: &str| Node { id: id.into(), kind: NodeKind::Text, bounds: Rect::default(), text: String::new(), style: Style::default(), asset: None, children: vec![], layout: None, interactions: vec![], variants: Default::default(), ext: NodeExt::default() };
        let ops = vec![
            Operation::SetText { node_id: "title".into(), text: "x".into() },
            Operation::AddNode { page_id: "p".into(), parent_id: Some("card".into()), node: node("new") },
            Operation::SetText { node_id: "new".into(), text: "y".into() },
            Operation::SetText { node_id: "other".into(), text: "z".into() },
            Operation::AddNode { page_id: "p".into(), parent_id: None, node: node("loose") },
            Operation::InsertPage { index: 1, page: Page::new("p2", Size::slide()) },
            Operation::SetNamedStyle { name: "Body".into(), patch: StylePatch::default(), clear_overrides: false },
        ];
        let found = check(&ops, &allowed, &existing);
        assert_eq!(found.outside, ["other", "p", "style:Body"]);
        assert_eq!(found.whole_document, ["insert_page"]);
        assert_eq!(found.created, set(&["new", "loose"]));
        // A dataset the figure does not have yet is created; an existing one must be in scope.
        let dataset = |id: &str| Dataset { id: id.into(), version_hash: "v".into(), original_asset_hash: String::new(), origin: "user".into(), columns: vec![], rows: vec![], derived_from: None, transforms: vec![] };
        let puts = check(&[Operation::PutDataset { dataset: dataset("d2") }, Operation::PutDataset { dataset: dataset("d1") }], &allowed, &existing);
        assert_eq!((puts.outside, puts.created), (vec!["d1".to_string()], set(&["d2"])));
    }

    #[test]
    fn a_pasted_repeat_addresses_its_sources_and_creates_only_the_new_group() {
        let paste: Operation = serde_json::from_value(json!({
            "op": "add_node", "page_id": "page", "node": {
                "id": "copy", "kind": "group", "bounds": Rect::default(),
                "repeat": { "table_id": "data", "component_page_id": "card" },
                "children": [{ "id": "list-row", "kind": "group", "bounds": Rect::default(),
                    "instance": { "component_page_id": "card" } }]
            }
        })).unwrap();
        let existing = set(&["page", "card", "data", "list-row"]);
        let result = check(std::slice::from_ref(&paste), &set(&["page"]), &existing);
        assert_eq!(result.outside, ["data", "card"]);
        assert_eq!(result.created, set(&["copy"]));
        assert_eq!(op_targets(&paste), ["page", "data", "card", "copy"]);
    }

    #[test]
    fn set_node_list_is_content_and_scoped_to_its_node() {
        let allowed = set(&["card", "title"]);
        let existing = set(&["p", "card", "title", "other"]);
        let inside = Operation::SetNodeList { node_id: "title".into(), list: Some(TextListKind::Bullet) };
        assert_eq!(inside.intent(), Intent::Content);
        let accepted = check(&[inside], &allowed, &existing);
        assert!(accepted.outside.is_empty(), "a list edit inside the target stays in scope");

        let outside = Operation::SetNodeList { node_id: "other".into(), list: None };
        let refused = check(&[outside], &allowed, &existing);
        assert_eq!(refused.outside, ["other"]);
        assert!(refused.whole_document.is_empty(), "list edits are node scoped, not document wide");
    }

    /// A job scoped to a repeat group and its table cannot repeat a component page outside its scope, and clearing a repeat
    /// needs the table and the component it repeats now. An `outside` id makes `check_job_scope` refuse with CONTENT_LOCKED.
    #[test]
    fn set_group_repeat_needs_the_group_its_table_and_its_component_page_in_scope() {
        let existing = set(&["page", "list", "table", "card", "other-card"]);
        let allowed = set(&["list", "table", "card"]);
        let spec = |component: &str| RepeatSpec { table_id: "table".into(), component_page_id: component.into(), bindings: vec![] };
        let inside = Operation::SetGroupRepeat { node_id: "list".into(), expected: None, repeat: Some(spec("card")) };
        assert!(check(&[inside], &allowed, &existing).outside.is_empty());
        let foreign = Operation::SetGroupRepeat { node_id: "list".into(), expected: None, repeat: Some(spec("other-card")) };
        assert_eq!(check(&[foreign], &allowed, &existing).outside, ["other-card"]);
        let clear = Operation::SetGroupRepeat { node_id: "list".into(), expected: Some(spec("other-card")), repeat: None };
        assert_eq!(check(&[clear], &allowed, &existing).outside, ["other-card"]);
        let group_only = set(&["list"]);
        let condition = Operation::SetGroupCondition { node_id: "list".into(), expected: None, condition: None };
        assert!(check(&[condition], &group_only, &existing).outside.is_empty());
        let reserved: BTreeSet<String> = op_targets(&Operation::SetGroupRepeat { node_id: "list".into(), expected: None, repeat: Some(spec("card")) }).into_iter().collect();
        assert!(reserved.contains("card"), "the component page is reserved too");
    }

    #[test]
    fn set_interactions_is_content_and_scoped_to_its_node() {
        let allowed = set(&["card", "title"]);
        let existing = set(&["p", "card", "title", "other"]);
        let inside = Operation::SetInteractions {
            node_id: "title".into(),
            interactions: vec![Interaction::OpenUrl { url: "https://example.com/".into() }],
        };
        assert_eq!(inside.intent(), Intent::Content);
        let accepted = check(&[inside], &allowed, &existing);
        assert!(accepted.outside.is_empty(), "an interaction edit inside the target stays in scope");

        let outside = Operation::SetInteractions { node_id: "other".into(), interactions: vec![] };
        let refused = check(&[outside], &allowed, &existing);
        assert_eq!(refused.outside, ["other"]);
        assert!(refused.whole_document.is_empty(), "interaction edits are node scoped, not document wide");
    }

    #[test]
    fn canvas_table_structure_scopes_old_ids_and_creates_only_new_row_and_cell_ids_in_either_order() {
        let old = CanvasTable {
            rows: vec![
                CanvasTableRow { id: "r1".into(), cell_ids: vec!["c11".into(), "c12".into()] },
                CanvasTableRow { id: "r2".into(), cell_ids: vec!["c21".into(), "c22".into()] },
            ],
            header_row_id: Some("r1".into()),
            column_widths: None,
            cell_spans: Default::default(),
        };
        let new = CanvasTable {
            rows: vec![
                old.rows[0].clone(), old.rows[1].clone(),
                CanvasTableRow { id: "r3".into(), cell_ids: vec!["c31".into(), "c32".into()] },
            ],
            header_row_id: old.header_row_id.clone(),
            column_widths: None,
            cell_spans: Default::default(),
        };
        let expected_scope = set(&["table", "r1", "r2", "c11", "c12", "c21", "c22"]);
        let existing = set(&["page", "table", "r1", "r2", "c11", "c12", "c21", "c22", "external"]);
        let cell = |id: &str| Node {
            id: id.into(), kind: NodeKind::Rectangle, bounds: Rect::default(), text: String::new(), style: Style::default(),
            asset: None, children: vec![], layout: None, interactions: vec![], variants: Default::default(), ext: NodeExt::default(),
        };
        let set_structure = Operation::SetCanvasTableStructure { table_id: "table".into(), expected: old, table: new.clone() };
        let add_first = vec![
            set_structure.clone(),
            Operation::AddNode { page_id: "page".into(), parent_id: Some("table".into()), node: cell("c31") },
            Operation::AddNode { page_id: "page".into(), parent_id: Some("table".into()), node: cell("c32") },
        ];
        let add_last = vec![
            Operation::AddNode { page_id: "page".into(), parent_id: Some("table".into()), node: cell("c31") },
            Operation::AddNode { page_id: "page".into(), parent_id: Some("table".into()), node: cell("c32") },
            set_structure,
        ];
        for operations in [add_first, add_last] {
            let result = check(&operations, &expected_scope, &existing);
            assert!(result.outside.is_empty(), "a table-targeted job can add rows regardless of operation order: {result:?}");
            assert_eq!(result.created, set(&["r3", "c31", "c32"]));
        }

        let collision = CanvasTable {
            rows: vec![
                new.rows[0].clone(), new.rows[1].clone(),
                CanvasTableRow { id: "external".into(), cell_ids: vec!["c31".into(), "c32".into()] },
            ],
            header_row_id: Some("r1".into()),
            column_widths: None,
            cell_spans: Default::default(),
        };
        let collision = Operation::SetCanvasTableStructure { table_id: "table".into(), expected: new.clone(), table: collision };
        let denied = check(&[collision], &expected_scope.union(&set(&["c31", "c32", "r3"])).cloned().collect(), &existing);
        assert_eq!(denied.outside, ["external"], "an existing id outside the table scope is never reclassified as newly created");
        assert!(!denied.created.contains("external"));

        let old_cell = Operation::SetCanvasTableStructure { table_id: "table".into(), expected: new.clone(), table: new };
        let partial_scope = set(&["table", "r1", "r2", "r3", "c11", "c12", "c21", "c31", "c32"]);
        let denied = check(&[old_cell], &partial_scope, &existing.union(&set(&["r3", "c31", "c32"])).cloned().collect());
        assert_eq!(denied.outside, ["c22"], "existing old cell ids remain scope-checked");
    }

    #[test]
    fn custom_table_width_operation_is_design_scoped_to_its_table() {
        let operation = Operation::SetCanvasTableColumnWidths {
            table_id: "table".into(), expected: None, column_widths: Some(vec![1, 2]),
        };
        assert_eq!(operation.intent(), Intent::Design);
        let existing = set(&["page", "table", "r1", "r2", "c11", "c12", "c21", "c22", "other"]);
        let allowed = check(std::slice::from_ref(&operation), &set(&["table"]), &existing);
        assert!(allowed.outside.is_empty() && allowed.whole_document.is_empty());
        let cell_only = check(std::slice::from_ref(&operation), &set(&["c11"]), &existing);
        assert_eq!(cell_only.outside, ["table"], "a cell-only job cannot resize its owning table");
        let refused = check(std::slice::from_ref(&operation), &set(&["other"]), &existing);
        assert_eq!(refused.outside, ["table"]);
    }

    #[test]
    fn canvas_image_crop_is_design_scoped_to_its_image_target() {
        let operation = Operation::SetCanvasImageCrop {
            node_id: "image".into(), expected_asset_sha256: "sha".into(), crop_pixel_rect: None,
        };
        assert_eq!(operation.intent(), Intent::Design);
        assert_eq!(op_targets(&operation), vec!["image".to_string()]);
        let existing = set(&["image", "other"]);
        assert!(check(std::slice::from_ref(&operation), &set(&["image"]), &existing).outside.is_empty());
        assert_eq!(check(std::slice::from_ref(&operation), &set(&["other"]), &existing).outside, ["image"]);
    }

    #[test]
    fn canvas_board_memo_snapshot_scopes_page_and_joins_new_memo_ids() {
        let memo = CanvasBoardMemo {
            kind: CanvasBoardMemoKind::Memo,
            id: "memo-1".into(), page_id: "page".into(),
            bounds: Rect { x: -100, y: 200, width: 300, height: 200 },
            text: "note".into(), color: "#FFF2CC".into(),
        };
        let create = Operation::SetCanvasBoardMemos {
            page_id: "page".into(), expected: Vec::new(), memos: vec![memo.clone()],
        };
        assert_eq!(op_targets(&create), vec!["page".to_string(), "memo-1".to_string()]);
        let created = check(&[create.clone()], &set(&["page"]), &set(&["page"]));
        assert!(created.outside.is_empty());
        assert_eq!(created.created, set(&["memo-1"]));
        assert_eq!(check(&[create], &set(&["other-page"]), &set(&["page"])).outside, ["page"]);
    }

    #[test]
    fn page_scoped_board_memo_updates_include_only_that_pages_existing_memo_ids() {
        let memo_a = CanvasBoardMemo {
            kind: CanvasBoardMemoKind::Memo,
            id: "memo-a".into(), page_id: "page-a".into(),
            bounds: Rect { x: 0, y: 0, width: 300, height: 200 }, text: "A".into(), color: "#FFF2CC".into(),
        };
        let memo_b = CanvasBoardMemo {
            kind: CanvasBoardMemoKind::Memo,
            id: "memo-b".into(), page_id: "page-b".into(),
            bounds: Rect { x: 0, y: 0, width: 300, height: 200 }, text: "B".into(), color: "#DDEEFF".into(),
        };
        let artifact = Artifact::new(ArtifactKind::Canvas, "board", "engine", ArtifactBody::Canvas(CanvasDocument {
            pages: vec![
                Page::new("page-a", Size::slide()),
                Page::new("page-b", Size::slide()),
            ],
            board_memos: vec![memo_a.clone(), memo_b],
            ..Default::default()
        }));
        let roots = set(&["page-a"]);
        let mut allowed = roots.clone();
        let document_json = json_of(&artifact).unwrap();
        inside(&document_json, &roots, &mut allowed);
        include_canvas_page_memos(&artifact, &roots, &mut allowed);
        assert!(allowed.contains("memo-a"));
        assert!(!allowed.contains("memo-b"), "a page target does not expand to another page's memo");

        let mut updated = memo_a.clone();
        updated.text = "edited".into();
        let operation = Operation::SetCanvasBoardMemos {
            page_id: "page-a".into(), expected: vec![memo_a], memos: vec![updated],
        };
        let mut existing = BTreeSet::new();
        ids_in(&document_json, &mut existing);
        let accepted = check(std::slice::from_ref(&operation), &allowed, &existing);
        assert!(accepted.outside.is_empty(), "a page-scoped job may edit that page's existing memo");
        let refused = check(&[operation], &set(&["page-b"]), &existing);
        assert!(refused.outside.contains(&"page-a".to_string()));
        assert!(refused.outside.contains(&"memo-a".to_string()));
    }

    #[test]
    fn canvas_page_sections_are_document_scoped_and_section_ids_follow_their_start_pages() {
        let section_a = CanvasPageSection { id: "section-a".into(), name: "Part A".into(), start_page_id: "page-a".into() };
        let section_b = CanvasPageSection { id: "section-b".into(), name: "Part B".into(), start_page_id: "page-b".into() };
        let artifact = Artifact::new(ArtifactKind::Canvas, "sections", "engine", ArtifactBody::Canvas(CanvasDocument {
            pages: vec![
                Page::new("page-a", Size::slide()),
                Page::new("page-b", Size::slide()),
            ],
            page_sections: vec![section_a.clone(), section_b.clone()],
            ..Default::default()
        }));
        let roots = set(&["page-a"]);
        let mut allowed = roots.clone();
        let document_json = json_of(&artifact).unwrap();
        inside(&document_json, &roots, &mut allowed);
        include_canvas_page_sections(&artifact, &roots, &mut allowed);
        assert!(allowed.contains("section-a"));
        assert!(!allowed.contains("section-b"), "a page target does not expand to another page's section marker");

        let operation = Operation::SetCanvasPageSections { expected: vec![section_a], sections: vec![section_b] };
        assert_eq!(op_targets(&operation), vec!["section-a".to_string(), "page-a".to_string(), "section-b".to_string(), "page-b".to_string()]);
        let check = check(&[operation], &roots, &ids_in_artifact(&artifact));
        assert_eq!(check.whole_document, ["set_canvas_page_sections"], "page-scoped jobs cannot alter section boundaries");
    }

    #[test]
    fn every_id_the_scope_check_addresses_is_reserved() {
        // 14f: the reservation (op_targets) holds every id op_scope addresses. These are the operations whose reservation
        // used to miss their page, component page or panel ids.
        let node = |id: &str| Node {
            id: id.into(), kind: NodeKind::Text, bounds: Rect::default(), text: String::new(), style: Style::default(), asset: None,
            children: vec![], layout: None, interactions: vec![], variants: Default::default(), ext: NodeExt::default(),
        };
        let instance = Node {
            kind: NodeKind::Group,
            ext: NodeExt { instance: Some(InstanceRef { component_page_id: "component".into(), overrides: Default::default() }), ..Default::default() },
            ..node("copy")
        };
        let operations = [
            Operation::AddNode { page_id: "page".into(), parent_id: None, node: instance },
            Operation::AddNode { page_id: "page".into(), parent_id: Some("parent".into()), node: node("plain") },
            Operation::MoveNode { node_id: "moved".into(), page_id: "page".into(), parent_id: None, index: 0 },
            Operation::ReorderPages { page_ids: vec!["page".into(), "other".into()] },
            Operation::ReorderPanels { panel_ids: vec!["panel".into(), "other".into()] },
            Operation::SetPageSize { page_id: "page".into(), size: Size::slide() },
            Operation::RenamePage { page_id: "page".into(), name: None },
        ];
        let existing = set(&["page", "parent", "moved", "other", "panel", "component"]);
        for operation in &operations {
            let (addressed, _created) = op_scope(operation, &existing).expect("a scoped operation");
            let reserved: BTreeSet<String> = op_targets(operation).into_iter().collect();
            assert!(addressed.iter().all(|id| reserved.contains(id)), "{operation:?}: scope addresses {addressed:?}, reservation holds {reserved:?}");
        }
    }

    #[test]
    fn copied_component_add_scopes_its_source_and_created_derived_children() {
        let mut source_node = Node { id: "source-text".into(), kind: NodeKind::Text, bounds: Rect { x: 0, y: 0, width: 100, height: 40 }, text: "source".into(), style: Style::default(), asset: None, children: vec![], layout: None, interactions: vec![], variants: Default::default(), ext: NodeExt::default() };
        source_node.text = "source".into();
        let artifact = Artifact::new(ArtifactKind::Canvas, "scope", "engine", ArtifactBody::Canvas(CanvasDocument {
            pages: vec![
                Page::new("target", Size::slide()),
                Page { nodes: vec![source_node], ..Page::new("source-page", Size::slide()) },
            ],
            ..Default::default()
        }));
        let instance = Node {
            id: "copy".into(), kind: NodeKind::Group, bounds: Rect { x: 0, y: 0, width: 500, height: 200 }, text: String::new(),
            style: Style::default(), asset: None,
            children: vec![Node { id: "temporary-child".into(), kind: NodeKind::Text, bounds: Rect { x: 0, y: 0, width: 100, height: 40 }, text: "stale".into(), style: Style::default(), asset: None, children: vec![], layout: None, interactions: vec![], variants: Default::default(), ext: NodeExt::default() }],
            layout: None, interactions: vec![], variants: Default::default(),
            ext: NodeExt { instance: Some(InstanceRef { component_page_id: "source-page".into(), overrides: Default::default() }), ..Default::default() },
        };
        let operations = [
            Operation::SetPageRole { page_id: "source-page".into(), role: Some(PageRole::Component) },
            Operation::AddNode { page_id: "target".into(), parent_id: None, node: instance },
            Operation::SetText { node_id: "copy::source-text".into(), text: "override".into() },
        ];
        let existing = ids_in_artifact(&artifact);
        let allowed = set(&["target", "source-page"]);
        let extra = BTreeMap::from([("copy".to_string(), set(&["copy::source-text"]))]);
        let result = check_with_component_children(&operations, &allowed, &existing, &extra);
        assert!(result.outside.is_empty() && result.whole_document.is_empty(), "new derived ids join the scope: {result:?}");
        assert_eq!(result.created, set(&["copy", "copy::source-text"]));

        let no_role = [
            Operation::AddNode { page_id: "target".into(), parent_id: None, node: Node {
                id: "copy-2".into(), kind: NodeKind::Group, bounds: Rect { x: 0, y: 0, width: 500, height: 200 }, text: String::new(),
                style: Style::default(), asset: None, children: vec![Node { id: "stale".into(), kind: NodeKind::Text, bounds: Rect { x: 0, y: 0, width: 100, height: 40 }, text: String::new(), style: Style::default(), asset: None, children: vec![], layout: None, interactions: vec![], variants: Default::default(), ext: NodeExt::default() }],
                layout: None, interactions: vec![], variants: Default::default(),
                ext: NodeExt { instance: Some(InstanceRef { component_page_id: "source-page".into(), overrides: Default::default() }), ..Default::default() },
            } },
            Operation::SetText { node_id: "copy-2::source-text".into(), text: "override".into() },
        ];
        let no_source_role = check_with_component_children(&no_role, &allowed, &existing, &BTreeMap::new());
        assert_eq!(no_source_role.outside, ["copy-2::source-text"], "without a source role in the base or this change, derived IDs are not granted");

        let outside_source = check_with_component_children(&operations, &set(&["target"]), &existing, &extra);
        assert_eq!(outside_source.outside, ["source-page"]);
    }

    #[test]
    fn component_child_id_collision_is_refused_during_scope_preflight() {
        let source = Node { id: "source-text".into(), kind: NodeKind::Text, bounds: Rect { x: 0, y: 0, width: 100, height: 40 }, text: String::new(), style: Style::default(), asset: None, children: vec![], layout: None, interactions: vec![], variants: Default::default(), ext: NodeExt::default() };
        let outside = Node { id: "copy::source-text".into(), ..source.clone() };
        let artifact = Artifact::new(ArtifactKind::Canvas, "collision", "engine", ArtifactBody::Canvas(CanvasDocument {
            pages: vec![
                Page { nodes: vec![outside], ..Page::new("target", Size::slide()) },
                Page { nodes: vec![source], role: Some(PageRole::Component), ..Page::new("source-page", Size::slide()) },
            ],
            ..Default::default()
        }));
        let existing = ids_in_artifact(&artifact);
        let operation = Operation::InstantiateComponent {
            page_id: "target".into(), parent_id: None, instance_id: "copy".into(), component_page_id: "source-page".into(),
            bounds: Rect { x: 0, y: 0, width: 500, height: 200 },
        };
        let error = planned_component_children(&artifact, &[operation], &existing).unwrap_err();
        assert_eq!((error.code, error.affected_ids), (ErrorCode::AlreadyExists, vec!["copy::source-text".to_string()]));
    }

    #[test]
    fn planned_component_table_rows_are_scoped_and_row_id_collisions_are_refused() {
        let cell = Node { id: "source-cell".into(), kind: NodeKind::Rectangle, bounds: Rect::default(), text: String::new(), style: Style::default(), asset: None, children: vec![], layout: None, interactions: vec![], variants: Default::default(), ext: NodeExt::default() };
        let table = Node {
            id: "source-table".into(), kind: NodeKind::Group, bounds: Rect::default(), text: String::new(), style: Style::default(), asset: None,
            children: vec![cell], layout: None, interactions: vec![], variants: Default::default(),
            ext: NodeExt { table: Some(CanvasTable { rows: vec![CanvasTableRow { id: "source-row".into(), cell_ids: vec!["source-cell".into()] }], header_row_id: None, column_widths: None, cell_spans: Default::default() }), ..Default::default() },
        };
        let artifact = Artifact::new(ArtifactKind::Canvas, "table-scope", "engine", ArtifactBody::Canvas(CanvasDocument {
            pages: vec![
                Page::new("target", Size::slide()),
                Page { nodes: vec![table], role: Some(PageRole::Component), ..Page::new("source-page", Size::slide()) },
            ],
            ..Default::default()
        }));
        let derived = component_child_ids(&artifact, "source-page", "copy", &BTreeSet::from(["source-page".into()]));
        assert_eq!(derived, set(&["copy::source-table", "copy::source-cell", "copy::source-row"]));

        let mut collision = artifact.clone();
        let ArtifactBody::Canvas(doc) = &mut collision.body else { panic!("Canvas body") };
        doc.pages[0].nodes.push(Node { id: "copy::source-row".into(), kind: NodeKind::Text, bounds: Rect::default(), text: String::new(), style: Style::default(), asset: None, children: vec![], layout: None, interactions: vec![], variants: Default::default(), ext: NodeExt::default() });
        let existing = ids_in_artifact(&collision);
        let operation = Operation::InstantiateComponent {
            page_id: "target".into(), parent_id: None, instance_id: "copy".into(), component_page_id: "source-page".into(),
            bounds: Rect { x: 0, y: 0, width: 500, height: 200 },
        };
        let error = planned_component_children(&collision, &[operation], &existing).unwrap_err();
        assert_eq!((error.code, error.affected_ids), (ErrorCode::AlreadyExists, vec!["copy::source-row".to_string()]));
    }

    fn ids_in_artifact(artifact: &Artifact) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        ids_in(&serde_json::to_value(artifact).unwrap(), &mut ids);
        ids
    }
}
