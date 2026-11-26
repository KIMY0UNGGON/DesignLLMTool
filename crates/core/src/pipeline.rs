//! Change pipeline (CONTRACTS §3/§5/§7): shape -> receipt -> revision/dependency/job/intent ->
//! reservation -> engine prepare on a staging asset store -> structural + semantic digest checks
//! -> durable blobs -> one IMMEDIATE transaction (re-check revision/job/receipt; revision,
//! pointer, undo log, assets, receipt, event) -> COMMIT -> response.
use crate::blobs::{fire, is_hash, FaultPoint, StagingAssets};
use crate::errors::{err, integrity, invalid, is_recorded, sql, ErrExt};
use crate::receipts::{change_key, key_for, reject_duplicate_set, RequestKey};
use crate::routes::representation_key;
use crate::scope::copied_source_ids;
use crate::store::{self, StoredResult};
use crate::{asset_refs, parse, to_value, PdfCancel, PdfEditImporter, Workspace};
use designllm_contracts::*;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub struct PdfOpen {
    file: std::fs::File,
    title: String,
    actor: String,
    key: RequestKey,
    import: PdfEditImporter,
}

pub(crate) enum PreparedPdfOpen { Existing(Value), Pending(PdfOpen) }

pub(crate) fn is_pdf_path(path: &str) -> bool {
    std::path::Path::new(path).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

impl PdfOpen {
    pub fn run(&self, cancel: &PdfCancel) -> DltResult<Vec<u8>> {
        if cancel.is_canceled() { return Err(err(ErrorCode::Canceled, "PDF conversion was canceled")); }
        let bytes = crate::files::read_source(&self.file)?;
        (self.import)(&bytes, cancel)
    }
}

/// Per-revision snapshots above this size get a `snapshot_large` diagnostic.
const SNAPSHOT_WARN_BYTES: usize = 8 * 1024 * 1024;

/// `Revert`: history.undo_change of a change that later changes follow. It is recorded like a new
/// change in the undo log (so history.undo takes it back) and as an "undo" revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitKind { Change, Undo { seq: i64 }, Redo { seq: i64 }, Revert }
impl CommitKind {
    fn revision_kind(self) -> &'static str { match self { Self::Change => "change", Self::Undo { .. } | Self::Revert => "undo", Self::Redo { .. } => "redo" } }
    fn event(self) -> &'static str { match self { Self::Change => "artifact.changed", Self::Undo { .. } | Self::Revert => "artifact.undone", Self::Redo { .. } => "artifact.redone" } }
}

pub(crate) struct Candidate {
    pub key: RequestKey,
    pub artifact_id: String,
    pub actor: String,
    pub job: Option<JobRef>,
    /// Job recorded for audit only (proposal applied by a user).
    pub audit_job: Option<JobRef>,
    pub allowed: BTreeSet<Intent>,
    pub base_revision: Revision,
    pub before: String,
    pub after: String,
    pub after_bytes: Option<Vec<u8>>,
    pub title: String,
    pub changed_ids: Vec<Id>,
    pub intents: BTreeSet<Intent>,
    pub diagnostics: Vec<Diagnostic>,
    pub kind: CommitKind,
    pub staged: Vec<(AssetRef, Arc<Vec<u8>>)>,
    pub candidate: Option<Artifact>,
    pub design_system_revision_guard: Option<(String, Revision)>,
    /// history.undo_change: the request id of the change taken back (in the event detail).
    pub undone_change: Option<Id>,
    /// Ids a job with target_ids created in this change (recorded for its scope).
    pub created_ids: BTreeSet<String>,
    /// Persisted extension keys are merged in the same transaction as the snapshot.
    pub persisted_features: BTreeSet<String>,
    pub receipt_notes: Option<Vec<Value>>,
}

struct GroupUndoRow {
    artifact_id: String,
    seq: i64,
    before_snapshot: String,
    after_snapshot: String,
    intents: String,
    changed_ids: String,
    state: String,
}

pub(crate) enum Fail { New(DltError), Final(DltError) }

/// The job a request runs as: the connection's binding, else the body's `job_ref`. A connection the host fixed an identity for
/// (`caller`, task 81) without a binding cannot name a job in the body (83): it would pass that job's reservations and be
/// recorded as the job. Only in-process calls and the gui connection (no caller) keep the body `job_ref`.
pub(crate) fn effective_job(bound: Option<&JobRef>, body: Option<&JobRef>, caller: Option<&str>) -> DltResult<Option<JobRef>> {
    match (bound, body) {
        (Some(b), Some(j)) if b.job_id != j.job_id || b.attempt_id != j.attempt_id => Err(invalid("job_ref differs from the connection's job binding")),
        (Some(b), _) => Ok(Some(b.clone())),
        (None, Some(j)) if caller.is_some() => Err(invalid(format!("job_ref names job {}; only that job's own connection may act as it", j.job_id))),
        (None, j) => Ok(j.cloned()),
    }
}

/// Ids an operation addresses directly (reservation and CONTENT_LOCKED reporting). It holds every id op_scope addresses
/// (scope.rs test `every_id_the_scope_check_addresses_is_reserved`). The ids a change creates are here too; op_scope
/// treats those as created, not addressed (see the arms marked "created").
pub(crate) fn op_targets(op: &Operation) -> Vec<String> {
    use Operation::*;
    match op {
        ReplaceText { range, .. } | SetTextStyleRange { range, .. } => vec![range.paragraph_id.clone()],
        SetText { node_id, .. } | SetGeometry { node_id, .. } | RemoveNode { node_id } | SetLayout { node_id, .. }
        | SetInteraction { node_id, .. } | SetInteractions { node_id, .. } | SetVariantStyle { node_id, .. }
        | ReplaceImageAsset { node_id, .. } | SetCanvasImageCrop { node_id, .. } => vec![node_id.clone()],
        SetStyle { node_ids, .. } => node_ids.clone(),
        SetNamedStyle { name, .. } => vec![format!("style:{name}")],
        // Reserve the destination and copied sources, including the table and component of each repeat group.
        AddNode { page_id, parent_id, node } => {
            let mut ids = vec![parent_id.clone().unwrap_or_else(|| page_id.clone())];
            let mut sources = Vec::new();
            copied_source_ids(node, &mut sources);
            for source in sources { if !ids.contains(&source) { ids.push(source); } }
            ids.push(node.id.clone());
            ids
        }
        SetParagraphFormat { paragraph_id, .. } => vec![paragraph_id.clone()],
        SetPageSetup { section_id, .. } => vec![section_id.clone()],
        SetTableDimensions { table_id, .. } => vec![table_id.clone()],
        InsertFlowBlock { section_id, .. } => vec![section_id.clone()],
        // A reorder addresses the pages or panels it lists (op_scope: the same ids).
        ReorderPages { page_ids } => page_ids.clone(),
        ReorderPanels { panel_ids } => panel_ids.clone(),
        SetCaption { .. } => vec![],
        // created: page.id. op_scope treats InsertPage as a whole-document change (2026-11-18 decision, kept).
        InsertPage { page, .. } => vec![page.id.clone()],
        RemovePage { page_id } | SetPageNotes { page_id, .. } => vec![page_id.clone()],
        // created: the new memo ids (op_scope addresses only the memos that exist).
        SetCanvasBoardMemos { page_id, expected, memos } => std::iter::once(page_id.clone())
            .chain(expected.iter().chain(memos).map(|memo| memo.id.clone()))
            .collect(),
        SetCanvasPageGuides { page_id, .. } => vec![page_id.clone()],
        // op_scope treats this as a whole-document change (2026-11-18 decision, kept); the ids are reserved here.
        SetCanvasPageSections { expected, sections } => expected.iter().chain(sections)
            .flat_map(|section| [section.id.clone(), section.start_page_id.clone()])
            .collect(),
        // created: edge.id (op_scope does not address it).
        ConnectDiagram { panel_id, edge } => vec![panel_id.clone(), edge.id.clone(), edge.from.clone(), edge.to.clone()],
        // created: node.id (op_scope does not address it).
        AddDiagramNode { panel_id, node, .. } => vec![panel_id.clone(), node.id.clone()],
        // created: panel.id. op_scope treats AddPanel as a whole-document change (2026-11-18 decision, kept).
        AddPanel { panel, .. } => vec![panel.id.clone()],
        RemovePanel { panel_id } | SetPlot { panel_id, .. } | SetPlotDisplay { panel_id, .. } | SetAxisLabel { panel_id, .. } | SetAxisTicks { panel_id, .. } | SetPanelLabel { panel_id, .. } => vec![panel_id.clone()],
        // R07: the diagram element and its panel; the image panel; a whole-figure legend or review addresses no element.
        SetDiagramProvenance { panel_id, element_id, .. } => vec![panel_id.clone(), element_id.clone()],
        SetFigureImageTransform { panel_id, .. } | SetFigureImageCalibration { panel_id, .. } => vec![panel_id.clone()],
        SetCommonLegend { .. } | SetFigureVisualReview { .. } => vec![],
        // created when the id is new: an upsert (op_scope addresses an existing id and treats a new one as created).
        PutEvidence { evidence } => vec![evidence.id.clone()],
        PutDataset { dataset } => vec![dataset.id.clone()],
        ArrangePanels { bounds } => bounds.keys().cloned().collect(),
        UpdateFigureLink { link } => vec![link.target_node_id.clone()],
        ReplaceWebSource { path, .. } | MappedWebEdit { path, .. } => vec![path.clone()],
        // v1.2: the moved node and its new parent, or the page when the node moves to the page root (op_scope: the same ids).
        MoveNode { node_id, page_id, parent_id, .. } => vec![node_id.clone(), parent_id.clone().unwrap_or_else(|| page_id.clone())],
        SetZOrder { node_id, .. } => vec![node_id.clone()],
        SetNodeAppearance { node_ids, .. } | SetNodeInfo { node_ids, .. } => node_ids.clone(),
        SetNodeList { node_id, .. } => vec![node_id.clone()],
        SetNodePath { node_id, .. } => vec![node_id.clone()],
        SetCanvasTableColumnWidths { table_id, .. } => vec![table_id.clone()],
        // created: the new row and cell ids (op_scope addresses only the ones that exist).
        SetCanvasTableStructure { table_id, expected, table } => std::iter::once(table_id.clone())
            .chain(expected.rows.iter().chain(&table.rows).flat_map(|row| std::iter::once(row.id.clone()).chain(row.cell_ids.iter().cloned())))
            .collect(),
        BindToken { node_ids, .. } | UnbindToken { node_ids, .. } => node_ids.clone(),
        PutToken { .. } | RemoveToken { .. } | SetTokenMode { .. } => vec![],
        SetPageSize { page_id, .. } | RenamePage { page_id, .. } => vec![page_id.clone()],
        SetPageRole { page_id, .. } => vec![page_id.clone()],
        // created: instance_id (op_scope does not address it).
        InstantiateComponent { page_id, parent_id, instance_id, component_page_id, .. } => {
            let mut ids = vec![component_page_id.clone(), parent_id.clone().unwrap_or_else(|| page_id.clone()), instance_id.clone()];
            if !ids.contains(page_id) { ids.push(page_id.clone()); }
            ids
        }
        DetachInstance { instance_id } => vec![instance_id.clone()],
        // The group, then the data table and the component page of the new spec and of the current one (`expected`, which
        // the engine requires to equal the stored spec), as InstantiateComponent addresses its component page. op_scope
        // addresses the same ids, so a table or component outside the job scope is CONTENT_LOCKED.
        SetGroupRepeat { node_id, expected, repeat } => {
            let mut ids = vec![node_id.clone()];
            for spec in repeat.iter().chain(expected) {
                for id in [&spec.table_id, &spec.component_page_id] {
                    if !ids.contains(id) { ids.push(id.clone()); }
                }
            }
            ids
        }
        SetGroupCondition { node_id, .. } => vec![node_id.clone()],
        // The deck theme (op_scope: a whole-document change).
        SetTheme { .. } => vec!["theme".into()],
    }
}

fn collect_local_dependency_ids(value: &Value, ids: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            if let Some(id) = object.get("id").and_then(Value::as_str) { ids.insert(id.to_string()); }
            if let Some(styles) = object.get("styles").and_then(Value::as_object) {
                ids.extend(styles.keys().map(|name| format!("style:{name}")));
            }
            for value in object.values() { collect_local_dependency_ids(value, ids); }
        }
        Value::Array(values) => for value in values { collect_local_dependency_ids(value, ids); },
        _ => {}
    }
}

fn local_dependency_ids(artifact: &Artifact) -> DltResult<BTreeSet<String>> {
    let value = serde_json::to_value(artifact).map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
    let mut ids = BTreeSet::new();
    collect_local_dependency_ids(&value, &mut ids);
    Ok(ids)
}

fn check_job_read_dependencies(cs: &ChangeSet, artifact: &Artifact, revision: Revision) -> DltResult<()> {
    let local_ids = local_dependency_ids(artifact)?;
    for dependency in &cs.read_dependencies {
        if dependency.id != cs.artifact_id && !local_ids.contains(&dependency.id) {
            // Do not distinguish an external artifact from a missing local id.
            return Err(invalid("job read_dependencies may name only its artifact or local identifiers"));
        }
        if dependency.revision.is_some_and(|read| read != revision) {
            return Err(err(ErrorCode::RevisionConflict, format!("read dependency {} was read at another revision", dependency.id)).ids([dependency.id.clone()]));
        }
        if dependency.hash.is_some() { return Err(errors_unsupported_hash(&dependency.id)); }
    }
    Ok(())
}

fn blocking(diags: &[Diagnostic]) -> Vec<Diagnostic> { diags.iter().filter(|d| d.blocking).cloned().collect() }
fn diag_key(d: &Diagnostic) -> (String, Option<String>, String) { (d.code.clone(), d.node_id.clone(), d.message.clone()) }

fn persisted_features(engine: &dyn EngineAdapter, artifact: &Artifact) -> DltResult<BTreeSet<String>> {
    let supported = engine.supported_persisted_features();
    let mut used = engine.persisted_features(artifact)?;
    if artifact.design_system.is_some() || artifact.publishes_design_system {
        used.insert(design_policy::FEATURE_ARTIFACT_DESIGN_SYSTEM.into());
    }
    if let Some(unknown) = used.difference(&supported).next() {
        return Err(err(ErrorCode::SchemaUnsupported, format!("engine {} detected persisted feature {unknown:?} that it does not declare as supported", engine.id())));
    }
    Ok(used)
}

impl Workspace {
    /// Shape rules checked before receipts and before any engine (refusals are not stored). `at` is
    /// the pointer of the change set inside the request ("" or "/change_set").
    pub(crate) fn check_change_shape(&self, cs: &ChangeSet, at: &str) -> DltResult<()> {
        if cs.workspace_id != self.id { return Err(invalid(format!("workspace_id {:?} does not match this workspace", cs.workspace_id))); }
        if cs.artifact_id.trim().is_empty() { return Err(invalid("artifact_id is empty")); }
        if cs.allowed_intents.is_empty() { return Err(invalid("allowed_intents must not be empty")); }
        if cs.operations.is_empty() { return Err(invalid("operations must not be empty")); }
        crate::style_patches::check_style_patches(&cs.operations, at)
    }

    pub(crate) fn apply_changes(&mut self, params: Value, bound: Option<&JobRef>) -> DltResult<Value> {
        reject_duplicate_set(&params, "allowed_intents")?;
        let cs: ChangeSet = parse(params)?;
        self.check_change_shape(&cs, "")?;
        let job = effective_job(bound, cs.job_ref.as_ref(), self.caller.as_deref())?;
        let key = change_key("changes.apply", &cs, job.as_ref())?;
        to_value(&self.apply_keyed(&cs, key, job, None)?)
    }

    pub(crate) fn apply_keyed(&mut self, cs: &ChangeSet, key: RequestKey, job: Option<JobRef>, audit_job: Option<JobRef>) -> DltResult<ChangeResult> {
        self.apply_keyed_with_notes(cs, key, job, audit_job, None)
    }

    pub(crate) fn apply_keyed_with_notes(&mut self, cs: &ChangeSet, key: RequestKey, job: Option<JobRef>, audit_job: Option<JobRef>, notes: Option<Vec<Value>>) -> DltResult<ChangeResult> {
        if let Some(found) = store::find_receipt(&self.conn, &key)? {
            return found.and_then(|v| serde_json::from_value(v).map_err(|e| integrity(format!("stored change receipt is unreadable: {e}"))));
        }
        let staging = self.staging();
        let mut cand = match self.evaluate_change_as(cs, &key, job.as_ref(), audit_job.as_ref(), &staging) {
            Ok(c) => c,
            Err(e) => return Err(self.reject(&key, Some(&cs.artifact_id), e)),
        };
        cand.audit_job = audit_job;
        cand.receipt_notes = notes;
        self.commit_design_system_change(cand, &staging)
    }

    fn design_system_tokens(&self, reference: &DesignSystemRef) -> DltResult<BTreeMap<String, TokenDef>> {
        let (row, engine, artifact, _) = self.load_revision(&reference.artifact_id, Some(reference.revision))?;
        if !artifact.publishes_design_system {
            return Err(invalid(format!("artifact {} revision {} is not published as a design system", row.id, reference.revision)));
        }
        if artifact.design_system.is_some() {
            return Err(invalid("a design system cannot consume another design system"));
        }
        persisted_features(&*engine, &artifact)?;
        engine.design_tokens(&artifact)
    }

    fn current_design_system_consumers(&self, system_id: &str) -> DltResult<Vec<(store::ArtifactRow, Arc<dyn EngineAdapter>, Artifact)>> {
        let mut consumers = Vec::new();
        for row in store::artifact_rows(&self.conn)? {
            if row.id == system_id { continue; }
            let (current, engine, artifact, _) = self.load_revision(&row.id, None)?;
            if artifact.design_system.as_ref().is_some_and(|reference| reference.artifact_id == system_id) {
                persisted_features(&*engine, &artifact)?;
                consumers.push((current, engine, artifact));
            }
        }
        Ok(consumers)
    }

    fn shared_snapshot_integrity_error(row_id: &str, error: DltError) -> DltError {
        let mut wrapped = integrity(format!("artifact {row_id} shared design system snapshot is inconsistent: {}", error.message))
            .ids([row_id.to_owned()]);
        for id in error.affected_ids {
            if !wrapped.affected_ids.contains(&id) { wrapped.affected_ids.push(id); }
        }
        wrapped.current_revision = error.current_revision;
        wrapped.retryable = error.retryable;
        wrapped.diagnostics = error.diagnostics;
        wrapped
    }

    fn verify_shared_snapshot(
        &self,
        row: &store::ArtifactRow,
        engine: &dyn EngineAdapter,
        artifact: &Artifact,
    ) -> DltResult<()> {
        let Some(reference) = artifact.design_system.as_ref() else { return Ok(()) };
        let tokens = self.design_system_tokens(reference)?;
        let (_, hash) = Workspace::snapshot_of(engine, artifact)?;
        if hash != row.current_snapshot {
            return Err(integrity(format!("artifact {} shared design system snapshot is inconsistent", row.id)).ids([row.id.clone()]));
        }
        match engine.validate_shared_design_system_snapshot(artifact, &tokens) {
            Ok(()) => Ok(()),
            Err(error) if error.code == ErrorCode::UnsupportedOperation => Err(error),
            Err(error) => Err(Self::shared_snapshot_integrity_error(&row.id, error)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_external_candidate(
        &mut self,
        engine: &dyn EngineAdapter,
        row: &store::ArtifactRow,
        base: &Artifact,
        prepared: PreparedChange,
        key: &RequestKey,
        actor: &str,
        allowed: &BTreeSet<Intent>,
        intents: BTreeSet<Intent>,
        audit_job: Option<JobRef>,
        staging: &StagingAssets,
        force_free: bool,
    ) -> DltResult<Candidate> {
        let candidate = prepared.artifact;
        if candidate.id != base.id || candidate.kind != base.kind || candidate.revision != base.revision || candidate.engine_id != base.engine_id {
            return Err(err(ErrorCode::Internal, format!("engine {} changed artifact identity in design system synchronization", engine.id())));
        }
        if candidate.schema_version != SCHEMA_VERSION {
            return Err(err(ErrorCode::SchemaUnsupported, "candidate schema_version differs from the contract"));
        }
        let mut changed: BTreeSet<String> = prepared.changed_ids.into_iter().collect();
        if candidate.design_system != base.design_system { changed.insert("artifact:design_system".into()); }
        if candidate.publishes_design_system != base.publishes_design_system { changed.insert("artifact:publishes_design_system".into()); }
        let mut diagnostics = prepared.diagnostics;
        let refused = blocking(&diagnostics);
        if !refused.is_empty() {
            return Err(err(ErrorCode::UnsupportedOperation, "engine reported blocking diagnostics; nothing was applied").diags(refused).ids(changed.iter().cloned()));
        }
        let renderer = self.renderer.clone();
        let context = EngineContext { assets: staging, renderer: &*renderer };
        let before_diagnostics: BTreeSet<_> = engine.validate(base, &context).iter().filter(|d| d.blocking).map(diag_key).collect();
        let after_diagnostics = engine.validate(&candidate, &context);
        let introduced: Vec<Diagnostic> = after_diagnostics.iter().filter(|d| d.blocking && !before_diagnostics.contains(&diag_key(d))).cloned().collect();
        if !introduced.is_empty() {
            return Err(err(ErrorCode::UnsupportedOperation, "the design system change would introduce blocking validation diagnostics; nothing was applied").diags(introduced));
        }
        diagnostics.extend(after_diagnostics.into_iter().filter(|d| !d.blocking));
        let (before_digest, after_digest) = (engine.content_digest(base)?, engine.content_digest(&candidate)?);
        for (intent, before, after) in [(Intent::Content, &before_digest.content, &after_digest.content), (Intent::Data, &before_digest.data, &after_digest.data)] {
            if !allowed.contains(&intent) && before != after {
                return Err(err(ErrorCode::ContentLocked, format!("the design system change alters {intent:?}, which is not in allowed_intents")).ids(changed.iter().cloned()));
            }
        }
        // The post-check: the changed ids against the holds now and on the candidate (scope.rs `check_changed`).
        if force_free {
            self.check_changed(&row.id, &changed, "", None, &candidate)?;
        } else {
            self.check_changed(&row.id, &changed, actor, audit_job.as_ref().map(|job| job.job_id.as_str()), &candidate)?;
        }
        let mut refs = BTreeSet::new();
        asset_refs(&serde_json::to_value(&candidate).map_err(|e| err(ErrorCode::Internal, e.to_string()))?, &mut refs);
        let missing: Vec<String> = refs.into_iter().filter(|hash| !staging.contains(hash)).collect();
        if !missing.is_empty() { return Err(integrity("the design system candidate references assets that are not in this workspace").ids(missing)); }
        let (bytes, hash) = Workspace::snapshot_of(engine, &candidate)?;
        if bytes.len() > SNAPSHOT_WARN_BYTES {
            diagnostics.push(Diagnostic { code: "snapshot_large".into(), message: format!("revision snapshot is {} bytes; engines should keep binaries in assets and reference them", bytes.len()), node_id: None, blocking: false, params: Default::default() });
        }
        if hash == row.current_snapshot {
            diagnostics.push(Diagnostic { code: "no_effect".into(), message: "the design system request did not change the artifact".into(), node_id: None, blocking: false, params: Default::default() });
        }
        let persisted_features = persisted_features(engine, &candidate)?;
        Ok(Candidate {
            key: key.clone(), artifact_id: row.id.clone(), actor: actor.to_owned(), job: None, audit_job,
            allowed: allowed.clone(), base_revision: row.current_revision, before: row.current_snapshot.clone(),
            after: hash, after_bytes: Some(bytes), title: candidate.title.clone(), changed_ids: changed.into_iter().collect(),
            intents, diagnostics, kind: CommitKind::Change, staged: staging.take_staged(), candidate: None, design_system_revision_guard: None,
            undone_change: None, created_ids: BTreeSet::new(), persisted_features, receipt_notes: None,
        })
    }

    pub(crate) fn design_system_list(&self) -> DltResult<Value> {
        let rows = store::artifact_rows(&self.conn)?;
        let mut systems = Vec::new();
        for indexed in rows {
            if indexed.kind != ArtifactKind::Canvas { continue; }
            let (row, engine, artifact, _) = self.load_revision(&indexed.id, None)?;
            if !artifact.publishes_design_system { continue; }
            if artifact.design_system.is_some() {
                return Err(integrity(format!("published artifact {} also references another design system", row.id)).ids([row.id]));
            }
            engine.design_tokens(&artifact).map_err(|error| {
                integrity(format!("published artifact {} cannot provide valid design tokens: {}", row.id, error.message)).ids([row.id.clone()])
            })?;
            systems.push(DesignSystemSummary { artifact_id: row.id, title: artifact.title, revision: row.current_revision });
        }
        systems.sort_by(|left, right| left.artifact_id.cmp(&right.artifact_id));
        to_value(&DesignSystemListResult { systems })
    }

    pub(crate) fn design_system_publish(&mut self, params: Value) -> DltResult<Value> {
        let request: crate::routes::DesignSystemPublishParams = parse(params)?;
        if request.workspace_id != self.id { return Err(invalid(format!("workspace_id {:?} does not match this workspace", request.workspace_id))); }
        if request.allowed_intents.is_empty() { return Err(invalid("allowed_intents must not be empty")); }
        let key = key_for("design_system.publish", &request.request_id, &request)?;
        if let Some(found) = store::find_receipt(&self.conn, &key)? { return found; }
        if !request.allowed_intents.contains(&Intent::Design) {
            return Err(self.reject(&key, Some(&request.artifact_id), err(ErrorCode::ContentLocked, "design_system.publish requires design intent")));
        }
        let (row, engine, base, _) = self.load_revision(&request.artifact_id, None)?;
        if row.current_revision != request.expected_revision {
            return Err(self.reject(&key, Some(&request.artifact_id), err(ErrorCode::RevisionConflict, format!("expected_revision {} is not the current revision {}", request.expected_revision, row.current_revision)).at(row.current_revision)));
        }
        if let Err(error) = engine.design_tokens(&base) {
            return Err(self.reject(&key, Some(&request.artifact_id), error));
        }
        if !request.publish {
            match self.current_design_system_consumers(&request.artifact_id) {
                Ok(consumers) if !consumers.is_empty() => {
                    return Err(self.reject(&key, Some(&request.artifact_id), err(ErrorCode::ContentLocked, format!("design system {} is still referenced by {} artifact(s)", request.artifact_id, consumers.len())).ids(consumers.into_iter().map(|(row, _, _)| row.id))));
                }
                Ok(_) => {}
                Err(error) => return Err(self.reject(&key, Some(&request.artifact_id), error)),
            }
        }
        let mut next = base.clone();
        next.publishes_design_system = request.publish;
        let changed_ids = (base.publishes_design_system != request.publish).then(|| vec!["artifact:publishes_design_system".to_string()]).unwrap_or_default();
        let prepared = PreparedChange { artifact: next, changed_ids, diagnostics: Vec::new() };
        let staging = self.staging();
        let candidate = match self.prepare_external_candidate(
            &*engine, &row, &base, prepared, &key, &request.actor, &request.allowed_intents,
            BTreeSet::from([Intent::Design]), None, &staging, true,
        ) {
            Ok(candidate) => candidate,
            Err(error) => return Err(self.reject(&key, Some(&request.artifact_id), error)),
        };
        to_value(&self.commit(candidate)?)
    }

    pub(crate) fn design_system_set(&mut self, params: Value) -> DltResult<Value> {
        let request: crate::routes::DesignSystemSetParams = parse(params)?;
        if request.workspace_id != self.id { return Err(invalid(format!("workspace_id {:?} does not match this workspace", request.workspace_id))); }
        if request.allowed_intents.is_empty() { return Err(invalid("allowed_intents must not be empty")); }
        let key = key_for("design_system.set", &request.request_id, &request)?;
        if let Some(found) = store::find_receipt(&self.conn, &key)? { return found; }
        if !request.allowed_intents.contains(&Intent::Design) {
            return Err(self.reject(&key, Some(&request.artifact_id), err(ErrorCode::ContentLocked, "design_system.set requires design intent")));
        }
        if request.design_system.is_none() && request.expected_system_revision.is_some() {
            return Err(self.reject(&key, Some(&request.artifact_id), invalid("expected_system_revision requires design_system")));
        }
        if request.design_system.is_none() && request.direct_formatting.is_some() {
            return Err(self.reject(&key, Some(&request.artifact_id), invalid("direct_formatting requires design_system; a detach keeps the document as it is")));
        }
        let (row, engine, base, _) = self.load_revision(&request.artifact_id, None)?;
        if row.current_revision != request.expected_revision {
            return Err(self.reject(&key, Some(&request.artifact_id), err(ErrorCode::RevisionConflict, format!("expected_revision {} is not the current revision {}", request.expected_revision, row.current_revision)).at(row.current_revision)));
        }
        if base.publishes_design_system && request.design_system.is_some() {
            return Err(self.reject(&key, Some(&request.artifact_id), invalid("a design system publisher cannot consume another design system")));
        }
        // SPEC.md:81: a document with direct formatting states what the link does with it. Canvas and
        // WebCode bind tokens per node and keep their behaviour without the field.
        let canvas_family = matches!(base.kind, ArtifactKind::Canvas | ArtifactKind::WebCode);
        if request.design_system.is_some() && request.direct_formatting.is_none() && !canvas_family {
            return Err(self.reject(&key, Some(&request.artifact_id), invalid(format!(
                "design_system.set on a {:?} consumer needs direct_formatting (keep or clear): the published styles rank above its named styles, and the request must say whether its direct formatting stays",
                base.kind,
            ))));
        }
        let direct_formatting = request.direct_formatting.unwrap_or(design_policy::DirectFormattingPolicy::Keep);
        let (tokens, revision) = if let Some(system_id) = request.design_system.as_deref() {
            if system_id.trim().is_empty() || system_id == base.id {
                return Err(self.reject(&key, Some(&request.artifact_id), invalid("design_system must name another published artifact")));
            }
            let (system_row, system_engine, system, _) = match self.load_revision(system_id, None) {
                Ok(value) => value,
                Err(error) => return Err(self.reject(&key, Some(&request.artifact_id), error)),
            };
            if !system.publishes_design_system || system.design_system.is_some() {
                return Err(self.reject(&key, Some(&request.artifact_id), invalid(format!("artifact {system_id} is not a published design system"))));
            }
            if request.expected_system_revision.is_some_and(|expected| expected != system_row.current_revision) {
                return Err(self.reject(&key, Some(&request.artifact_id), err(
                    ErrorCode::RevisionConflict,
                    format!("expected design-system revision {:?} is not current revision {}", request.expected_system_revision, system_row.current_revision),
                ).at(system_row.current_revision).ids([system_id.to_owned()])));
            }
            let tokens = match system_engine.design_tokens(&system) {
                Ok(tokens) => tokens,
                Err(error) => return Err(self.reject(&key, Some(&request.artifact_id), error)),
            };
            (tokens, Some(system_row.current_revision))
        } else {
            (BTreeMap::new(), None)
        };
        let desired_reference = request.design_system.as_ref().zip(revision)
            .map(|(id, revision)| DesignSystemRef { artifact_id: id.clone(), revision });
        let staging = self.staging();
        let renderer = self.renderer.clone();
        let context = EngineContext { assets: &staging, renderer: &*renderer };
        let prepared = match engine.sync_design_system_in_context(&base, &tokens, desired_reference.as_ref(), direct_formatting, &context) {
            Ok(prepared) => prepared,
            Err(error) => return Err(self.reject(&key, Some(&request.artifact_id), error)),
        };
        let mut candidate = match self.prepare_external_candidate(
            &*engine, &row, &base, prepared, &key, &request.actor, &request.allowed_intents,
            BTreeSet::from([Intent::Design]), None, &staging, true,
        ) {
            Ok(candidate) => candidate,
            Err(error) => return Err(self.reject(&key, Some(&request.artifact_id), error)),
        };
        candidate.design_system_revision_guard = request.design_system.as_ref().zip(revision)
            .map(|(system_id, revision)| (system_id.clone(), revision));
        to_value(&self.commit(candidate)?)
    }

    fn expand_shared_design_system(&mut self, source: &Candidate, staging: &StagingAssets) -> DltResult<Option<Vec<Candidate>>> {
        let source_row = store::artifact_row(&self.conn, &source.artifact_id)?;
        if source_row.current_revision != source.base_revision || source_row.current_snapshot != source.before {
            return Err(err(ErrorCode::RevisionConflict, format!("artifact {} changed while shared tokens were being prepared", source.artifact_id)).at(source_row.current_revision));
        }
        let source_engine = self.engine_by_id(&source_row.engine_id)?;
        let before = self.load(&*source_engine, &source.before, source.base_revision)?;
        let loaded_after = if source.candidate.is_none() {
            Some(self.load(&*source_engine, &source.after, source.base_revision)?)
        } else {
            None
        };
        let after = source.candidate.as_ref().or(loaded_after.as_ref())
            .expect("source candidate or stored after snapshot is available");
        if !before.publishes_design_system || !after.publishes_design_system {
            return Ok(None);
        }
        if before.design_system.is_some() || after.design_system.is_some() {
            return Err(invalid("a design system publisher cannot consume another design system"));
        }
        let before_tokens = source_engine.design_tokens(&before)?;
        let after_tokens = source_engine.design_tokens(&after)?;
        if before_tokens == after_tokens { return Ok(None); }
        let consumers = self.current_design_system_consumers(&source.artifact_id)?;
        if consumers.is_empty() { return Ok(None); }
        if source.job.is_some() {
            let consumer_ids: Vec<String> = consumers.iter().map(|(row, _, _)| row.id.clone()).collect();
            return Err(err(
                ErrorCode::InvalidArgument,
                "a job-bound design-system change cannot propagate to consumers outside the job artifact scope",
            ).ids(consumer_ids));
        }

        let new_revision = source.base_revision + 1;
        let audit_job = source.job.clone().or_else(|| source.audit_job.clone());
        let renderer = self.renderer.clone();
        let mut candidates = Vec::with_capacity(consumers.len());
        for (row, engine, consumer) in consumers {
            let reference = consumer.design_system.as_ref()
                .ok_or_else(|| integrity("design system consumer list lost its reference"))?;
            if reference.artifact_id != source.artifact_id {
                return Err(integrity("design system consumer list contains another publisher").ids([consumer.id.clone()]));
            }
            self.verify_shared_snapshot(&row, &*engine, &consumer)?;
            let digest_before = engine.content_digest(&consumer)?;
            let mut desired_reference = reference.clone();
            desired_reference.revision = new_revision;
            // A publisher update keeps every consumer's direct formatting: the policy is not stored, and
            // `Clear` comes only from an explicit design_system.set.
            let context = EngineContext { assets: staging, renderer: &*renderer };
            let prepared = engine.sync_design_system_in_context(&consumer, &after_tokens, Some(&desired_reference), design_policy::DirectFormattingPolicy::Keep, &context)?;
            let digest_after = engine.content_digest(&prepared.artifact)?;
            if digest_before != digest_after {
                return Err(err(ErrorCode::UnsupportedOperation, format!("engine {} changed consumer content while synchronizing design tokens; the group was refused", engine.id())).ids([consumer.id.clone()]));
            }
            candidates.push(self.prepare_external_candidate(
                &*engine,
                &row,
                &consumer,
                prepared,
                &source.key,
                &source.actor,
                &source.allowed,
                BTreeSet::from([Intent::Design]),
                audit_job.clone(),
                staging,
                true,
            )?);
        }
        Ok(Some(candidates))
    }

    fn commit_design_system_change(&mut self, source: Candidate, staging: &StagingAssets) -> DltResult<ChangeResult> {
        let key = source.key.clone();
        let source_id = source.artifact_id.clone();
        let consumers = match self.expand_shared_design_system(&source, staging) {
            Ok(consumers) => consumers,
            Err(error) => return Err(self.reject(&key, Some(&source_id), error)),
        };
        let Some(mut members) = consumers else { return self.commit(source) };
        members.push(source);
        let group_id = new_id();
        self.commit_group(&key, &source_id, &group_id, true, members)
    }
    /// changes.validate: every apply check on a staging store; no blob, receipt, revision or event.
    pub(crate) fn validate_changes(&mut self, params: Value, bound: Option<&JobRef>) -> DltResult<Value> {
        reject_duplicate_set(&params, "allowed_intents")?;
        let cs: ChangeSet = parse(params)?;
        self.check_change_shape(&cs, "")?;
        let job = effective_job(bound, cs.job_ref.as_ref(), self.caller.as_deref())?;
        let key = change_key("changes.apply", &cs, job.as_ref())?;
        let staging = self.staging();
        let cand = self.evaluate_change(&cs, &key, job.as_ref(), &staging)?;
        to_value(&ChangeResult { request_id: key.request_id, artifact_id: cs.artifact_id.clone(), revision: cand.base_revision, changed_ids: cand.changed_ids, diagnostics: cand.diagnostics, group: None })
    }

    pub(crate) fn evaluate_change(&mut self, cs: &ChangeSet, key: &RequestKey, job: Option<&JobRef>, staging: &StagingAssets) -> DltResult<Candidate> {
        self.evaluate_change_as(cs, key, job, None, staging)
    }
    /// `author`: the job a user-applied proposal came from. Its target reservation lets the change
    /// through, because the change is that job's work.
    pub(crate) fn evaluate_change_as(&mut self, cs: &ChangeSet, key: &RequestKey, job: Option<&JobRef>, author: Option<&JobRef>, staging: &StagingAssets) -> DltResult<Candidate> {
        let row = store::artifact_row(&self.conn, &cs.artifact_id)?;
        let rev = row.current_revision;
        self.evaluate_change_at(cs, key, job, author, job, staging, &row, rev, &row.current_snapshot.clone(), false).map_err(|e| e.at(rev))
    }

    /// Read-only candidate for proposal previews: source scope applies for a job, but write intents,
    /// target scope and reservations do not.
    pub(crate) fn evaluate_preview(&mut self, cs: &ChangeSet, key: &RequestKey, staging: &StagingAssets, row: &store::ArtifactRow, rev: Revision, base_snapshot: &str, source_job: Option<&JobRef>) -> DltResult<Candidate> {
        self.evaluate_change_at(cs, key, None, None, source_job, staging, row, rev, base_snapshot, true)
    }

    /// Evaluate on an explicit base (current, or a proposal's historical base for previews).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn evaluate_change_at(&mut self, cs: &ChangeSet, key: &RequestKey, job: Option<&JobRef>, author: Option<&JobRef>, source_job: Option<&JobRef>, staging: &StagingAssets, row: &store::ArtifactRow, rev: Revision, base_snapshot: &str, preview: bool) -> DltResult<Candidate> {
        if cs.base_revision != rev {
            return Err(err(ErrorCode::RevisionConflict, format!("base_revision {} is not the current revision {rev}", cs.base_revision)));
        }
        let job_row = match job { Some(j) => Some(store::check_job_write(&self.conn, j, &cs.artifact_id, &cs.allowed_intents)?), None => None };
        if !preview && job_row.is_none() {
            for dep in &cs.read_dependencies {
                if dep.id == cs.artifact_id {
                    if dep.revision.is_some_and(|r| r != rev) { return Err(err(ErrorCode::RevisionConflict, format!("read dependency {} is not at revision {rev}", dep.id)).ids([dep.id.clone()])); }
                    if dep.hash.is_some() { return Err(errors_unsupported_hash(&dep.id)); }
                } else if store::artifact_exists(&self.conn, &dep.id)? {
                    let other = store::artifact_row(&self.conn, &dep.id)?;
                    if dep.hash.is_some() { return Err(errors_unsupported_hash(&dep.id)); }
                    if dep.revision.is_some_and(|r| r != other.current_revision) {
                        return Err(err(ErrorCode::RevisionConflict, format!("dependency artifact {} changed (now revision {})", dep.id, other.current_revision)).ids([dep.id.clone()]));
                    }
                } else {
                    // v1: node/style/dataset dependencies are satisfied only by exact base_revision.
                    if dep.revision.is_some_and(|r| r != cs.base_revision) { return Err(err(ErrorCode::RevisionConflict, format!("read dependency {} was read at another revision", dep.id)).ids([dep.id.clone()])); }
                    if dep.hash.is_some() { return Err(errors_unsupported_hash(&dep.id)); }
                }
            }
        }
        let preview_job_row = if job.is_none() {
            match source_job {
                Some(j) => Some(store::check_job_write(&self.conn, j, &cs.artifact_id, &BTreeSet::new())?),
                None => None,
            }
        } else { None };
        let source_job_row = job_row.as_ref().or(preview_job_row.as_ref());
        let pass = job.or(author).map(|j| j.job_id.as_str());
        let locked: Vec<String> = cs.operations.iter().filter(|op| !cs.allowed_intents.contains(&op.intent())).flat_map(op_targets).collect();
        if cs.operations.iter().any(|op| !cs.allowed_intents.contains(&op.intent())) {
            let intents: BTreeSet<Intent> = cs.operations.iter().map(Operation::intent).filter(|i| !cs.allowed_intents.contains(i)).collect();
            return Err(err(ErrorCode::ContentLocked, format!("operations carry {intents:?} which allowed_intents does not include; the whole change set is rejected")).ids(locked));
        }
        cs.operations.iter().try_for_each(Operation::check_new_interaction_urls)?;
        let targets: BTreeSet<String> = cs.operations.iter().flat_map(op_targets).collect();
        if let Some((ids, owner)) = if preview { None } else { self.views.busy(&cs.artifact_id, &targets, &cs.actor, pass) } {
            return Err(err(ErrorCode::TargetBusy, format!("targets are reserved by {owner:?} (composition, drag or an AI job in progress)")).ids(ids));
        }
        let engine = self.engine_by_id(&row.engine_id)?;
        let renderer = self.renderer.clone();
        let base = self.load(&*engine, base_snapshot, rev)?;
        let current_source_artifact = if source_job_row.is_some() && rev != row.current_revision {
            Some(self.load(&*engine, &row.current_snapshot, row.current_revision)?)
        } else { None };
        let source_artifact = current_source_artifact.as_ref().unwrap_or(&base);
        if job_row.is_some() {
            check_job_read_dependencies(cs, &base, rev)?;
        }
        let mut job_assets = source_job_row.map(|job_row| self.job_asset_scope_for(job_row, source_artifact)).transpose()?;
        if let (Some(source_job_row), Some(allowed_assets)) = (source_job_row, job_assets.as_ref()) {
            let mut requested_assets = BTreeSet::new();
            asset_refs(&serde_json::to_value(&cs.operations).map_err(|e| err(ErrorCode::Internal, e.to_string()))?, &mut requested_assets);
            for op in &cs.operations {
                if let Operation::ReplaceImageAsset { asset_sha256, .. } = op {
                    requested_assets.insert(asset_sha256.clone());
                }
            }
            if requested_assets.difference(allowed_assets).next().is_some() {
                return Err(invalid("job changes may use only assets in the target artifact or declared source_refs"));
            }
            for op in &cs.operations {
                let Operation::UpdateFigureLink { link } = op else { continue };
                let existing_pair = source_artifact.figure_links.iter().any(|current| {
                    current.figure_id == link.figure_id && current.figure_revision == link.figure_revision
                });
                let approved_figure = source_job_row.request.figure_refs.iter().any(|reference| {
                    reference.figure_id == link.figure_id && reference.figure_revision == link.figure_revision
                });
                if !existing_pair && !approved_figure {
                    return Err(invalid("job FigureLinks may use only a figure revision already linked in the target artifact or declared in figure_refs").ids([link.target_node_id.clone()]));
                }
                self.check_figure_link(link)?;
            }
        } else {
            for op in &cs.operations {
                if let Operation::UpdateFigureLink { link } = op { self.check_figure_link(link)?; }
            }
        }
        let created_ids = match &job_row { Some(r) => self.check_job_scope(r, &base, &cs.operations)?, None => BTreeSet::new() };
        let ctx = EngineContext { assets: staging, renderer: &*renderer };
        let mut prepared = engine.prepare_change(&base, &cs.operations, &ctx)?;
        if let Some(reference) = prepared.artifact.design_system.clone() {
            self.verify_shared_snapshot(row, &*engine, &base)?;
            let shared_tokens = self.design_system_tokens(&reference)?;
            let synced = engine.sync_design_system_in_context(&prepared.artifact, &shared_tokens, Some(&reference), design_policy::DirectFormattingPolicy::Keep, &ctx)?;
            prepared.artifact = synced.artifact;
            prepared.diagnostics.extend(synced.diagnostics);
            let mut changed: BTreeSet<String> = prepared.changed_ids.into_iter().collect();
            changed.extend(synced.changed_ids);
            prepared.changed_ids = changed.into_iter().collect();
        }
        if let Some(job) = &job_row {
            self.check_job_scope_affected(job, &base, &prepared.artifact, &created_ids, &prepared.changed_ids)?;
        }
        let candidate = prepared.artifact;
        if candidate.id != base.id || candidate.kind != base.kind || candidate.revision != base.revision || candidate.engine_id != base.engine_id {
            return Err(err(ErrorCode::Internal, format!("engine {} changed artifact identity (id/kind/revision/engine_id) in prepare_change", engine.id())));
        }
        if candidate.schema_version != SCHEMA_VERSION { return Err(err(ErrorCode::SchemaUnsupported, "candidate schema_version differs from the contract")); }
        let mut diagnostics = prepared.diagnostics.clone();
        let refused = blocking(&prepared.diagnostics);
        if !refused.is_empty() {
            return Err(err(ErrorCode::UnsupportedOperation, "engine reported blocking diagnostics; nothing was applied").diags(refused).ids(prepared.changed_ids.clone()));
        }
        let before: BTreeSet<_> = engine.validate(&base, &ctx).iter().filter(|d| d.blocking).map(diag_key).collect();
        let after = engine.validate(&candidate, &ctx);
        let introduced: Vec<Diagnostic> = after.iter().filter(|d| d.blocking && !before.contains(&diag_key(d))).cloned().collect();
        if !introduced.is_empty() {
            return Err(err(ErrorCode::UnsupportedOperation, "the change would introduce blocking validation diagnostics; nothing was applied").diags(introduced));
        }
        diagnostics.extend(after.into_iter().filter(|d| !d.blocking));
        let (d0, d1) = (engine.content_digest(&base)?, engine.content_digest(&candidate)?);
        for (intent, a, b) in [(Intent::Content, &d0.content, &d1.content), (Intent::Data, &d0.data, &d1.data)] {
            if !cs.allowed_intents.contains(&intent) && a != b {
                return Err(err(ErrorCode::ContentLocked, format!("engine {} reports a {intent:?} change but {intent:?} is not in allowed_intents", engine.id())).ids(prepared.changed_ids.clone()));
            }
        }
        let changed: BTreeSet<String> = prepared.changed_ids.iter().cloned().collect();
        // The post-check: the changed ids against the holds now and on the candidate (scope.rs `check_changed`).
        if !preview { self.check_changed(&cs.artifact_id, &changed, &cs.actor, pass, &candidate)?; }
        let mut refs = BTreeSet::new();
        asset_refs(&serde_json::to_value(&candidate).map_err(|e| err(ErrorCode::Internal, e.to_string()))?, &mut refs);
        let missing: Vec<String> = refs.iter().filter(|h| !staging.contains(h)).cloned().collect();
        if !missing.is_empty() { return Err(invalid("the change references assets that are not in this workspace; import them first").ids(missing)); }
        let staged = staging.take_staged();
        if let Some(allowed_assets) = job_assets.as_mut() {
            allowed_assets.extend(staged.iter().map(|(asset, _)| asset.sha256.clone()));
            if refs.difference(allowed_assets).next().is_some() {
                return Err(invalid("engine candidate references an asset outside the job's artifact and declared sources"));
            }
        }
        let (bytes, hash) = Workspace::snapshot_of(&*engine, &candidate)?;
        if bytes.len() > SNAPSHOT_WARN_BYTES {
            diagnostics.push(Diagnostic { code: "snapshot_large".into(), message: format!("revision snapshot is {} bytes; engines should keep binaries in assets and reference them", bytes.len()), node_id: None, blocking: false, params: Default::default() });
        }
        if hash == base_snapshot { diagnostics.push(Diagnostic { code: "no_effect".into(), message: "operations did not change the artifact".into(), node_id: None, blocking: false, params: Default::default() }); }
        let persisted_features = persisted_features(&*engine, &candidate)?;
        Ok(Candidate {
            key: key.clone(), artifact_id: cs.artifact_id.clone(), actor: cs.actor.clone(), job: job.cloned(), audit_job: None,
            allowed: cs.allowed_intents.clone(), base_revision: rev, before: base_snapshot.to_string(), after: hash, after_bytes: Some(bytes),
            title: candidate.title.clone(), changed_ids: prepared.changed_ids, intents: cs.operations.iter().map(Operation::intent).collect(),
            diagnostics, kind: CommitKind::Change, staged, candidate: Some(candidate), design_system_revision_guard: None, undone_change: None, created_ids, persisted_features, receipt_notes: None,
        })
    }

    /// The link names a render prepared by figure.render_link. Only the lookup is normalized; the
    /// engine receives the link exactly as sent.
    fn check_figure_link(&self, link: &FigureLink) -> DltResult<()> {
        let fig = store::artifact_row(&self.conn, &link.figure_id)?;
        if fig.kind != ArtifactKind::Figure { return Err(invalid(format!("{} is not a figure artifact", link.figure_id))); }
        if link.figure_revision > fig.current_revision { return Err(errors::not_found_rev(&link.figure_id, link.figure_revision)); }
        let representation = representation_key(&link.representation);
        if representation.is_empty() { return Err(invalid("figure link representation is empty").ids([link.target_node_id.clone()])); }
        if !is_hash(&link.render_hash) { return Err(invalid(format!("figure link render_hash must be 64 lowercase hex characters: {:?}", link.render_hash)).ids([link.target_node_id.clone()])); }
        let ok = self.conn.query_row(
            "SELECT 1 FROM figure_renders WHERE figure_id = ?1 AND figure_revision = ?2 AND representation = ?3 AND sha256 = ?4",
            params![link.figure_id, link.figure_revision as i64, representation, link.render_hash], |_| Ok(()),
        ).optional().map_err(sql)?;
        if ok.is_none() {
            return Err(invalid(format!("figure {} revision {} has no prepared {representation:?} render with hash {}; call figure.render_link first", link.figure_id, link.figure_revision, link.render_hash)).ids([link.figure_id.clone()]));
        }
        Ok(())
    }

    /// Records a deterministic rejection as the request's receipt (own transaction). If the
    /// receipt cannot be stored, the rejection is still returned and a replay re-evaluates.
    pub(crate) fn reject(&mut self, key: &RequestKey, artifact_id: Option<&str>, e: DltError) -> DltError {
        if !is_recorded(&e.code) { return e; }
        let stored = StoredResult::Rejected { error: e.clone() };
        let recorded = (|| -> DltResult<()> {
            let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
            store::insert_receipt(&tx, key, artifact_id, &stored)?;
            tx.commit().map_err(sql)
        })();
        let _ = recorded;
        e
    }

    pub(crate) fn commit(&mut self, cand: Candidate) -> DltResult<ChangeResult> {
        let outcome = commit_inner(&mut self.conn, &self.base, &self.hook, &cand);
        match outcome {
            Ok(result) => {
                self.refresh_holds(&cand.artifact_id)?;
                fire(&self.hook, FaultPoint::AfterCommit)?;
                Ok(result)
            }
            Err(Fail::Final(e)) => Err(e),
            Err(Fail::New(e)) => Err(self.reject(&cand.key, Some(&cand.artifact_id), e)),
        }
    }

    fn commit_group(
        &mut self,
        key: &RequestKey,
        source_id: &str,
        group_id: &str,
        create_history_group: bool,
        mut members: Vec<Candidate>,
    ) -> DltResult<ChangeResult> {
        members.sort_by(|left, right| left.artifact_id.cmp(&right.artifact_id));
        let outcome = commit_group_inner(&mut self.conn, &self.base, &self.hook, key, source_id, group_id, create_history_group, &members);
        match outcome {
            Ok(result) => {
                for member in &members { self.refresh_holds(&member.artifact_id)?; }
                fire(&self.hook, FaultPoint::AfterCommit)?;
                Ok(result)
            }
            Err(Fail::Final(error)) => Err(error),
            Err(Fail::New(error)) => Err(self.reject(key, Some(source_id), error)),
        }
    }

    // ------------------------------------------------------------------ undo / redo
    fn group_undo_rows(&self, group_id: &str) -> DltResult<Vec<GroupUndoRow>> {
        let expected: Option<i64> = self.conn.query_row(
            "SELECT member_count FROM history_groups WHERE group_id = ?1",
            params![group_id], |row| row.get(0),
        ).optional().map_err(sql)?;
        let expected = expected.ok_or_else(|| integrity(format!("history group {group_id} is missing")))?;
        if expected < 2 { return Err(integrity(format!("history group {group_id} has an invalid member count"))); }
        let mut statement = self.conn.prepare(
            "SELECT artifact_id,seq,before_snapshot,after_snapshot,intents,changed_ids,state \
             FROM undo_log WHERE group_id = ?1 ORDER BY artifact_id",
        ).map_err(sql)?;
        let rows = statement.query_map(params![group_id], |row| {
            Ok(GroupUndoRow {
                artifact_id: row.get(0)?,
                seq: row.get(1)?,
                before_snapshot: row.get(2)?,
                after_snapshot: row.get(3)?,
                intents: row.get(4)?,
                changed_ids: row.get(5)?,
                state: row.get(6)?,
            })
        }).map_err(sql)?;
        let members = rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)?;
        if members.len() as i64 != expected {
            return Err(integrity(format!("history group {group_id} expects {expected} members but has {}", members.len())));
        }
        Ok(members)
    }

    fn current_history_group(&self, artifact_id: &str, redo: bool) -> DltResult<Option<String>> {
        let query = if redo {
            "SELECT group_id FROM undo_log WHERE artifact_id = ?1 AND state = 'undone' ORDER BY seq ASC LIMIT 1"
        } else {
            "SELECT group_id FROM undo_log WHERE artifact_id = ?1 AND state = 'done' ORDER BY seq DESC LIMIT 1"
        };
        let group: Option<Option<String>> = self.conn.query_row(query, params![artifact_id], |row| row.get(0)).optional().map_err(sql)?;
        Ok(group.flatten())
    }

    fn change_history_group(&self, artifact_id: &str, change_id: &str) -> DltResult<Option<String>> {
        let group: Option<Option<String>> = self.conn.query_row(
            "SELECT u.group_id FROM undo_log u JOIN revisions r ON r.artifact_id = u.artifact_id AND r.revision = u.change_revision \
             WHERE u.artifact_id = ?1 AND r.request_id = ?2 LIMIT 1",
            params![artifact_id, change_id], |row| row.get(0),
        ).optional().map_err(sql)?;
        Ok(group.flatten())
    }

    fn evaluate_group_history(
        &mut self,
        req: &HistoryRequest,
        key: &RequestKey,
        group_id: &str,
        job: Option<&JobRef>,
        redo: bool,
    ) -> DltResult<Vec<Candidate>> {
        let members = self.group_undo_rows(group_id)?;
        if !members.iter().any(|member| member.artifact_id == req.artifact_id) {
            return Err(integrity(format!("history group {group_id} does not include requested artifact {}", req.artifact_id)));
        }
        if job.is_some() {
            let outside_scope: Vec<String> = members.iter().filter(|member| member.artifact_id != req.artifact_id).map(|member| member.artifact_id.clone()).collect();
            if !outside_scope.is_empty() {
                return Err(invalid("job-bound history cannot change group participants outside the job artifact scope").ids(outside_scope));
            }
        }
        let expected_state = if redo { "undone" } else { "done" };
        let mut candidates = Vec::with_capacity(members.len());
        for member in members {
            if member.state != expected_state {
                return Err(err(ErrorCode::RevisionConflict, format!("history group {group_id} is not uniformly {expected_state}")).ids([member.artifact_id]));
            }
            let row = store::artifact_row(&self.conn, &member.artifact_id)?;
            if member.artifact_id == req.artifact_id && req.expected_revision != row.current_revision {
                return Err(err(ErrorCode::RevisionConflict, format!("expected_revision {} is not the current revision {}", req.expected_revision, row.current_revision)).at(row.current_revision));
            }
            let changed_ids: Vec<String> = store::from_json(&member.changed_ids, "group undo changed_ids")?;
            let intents: BTreeSet<Intent> = store::from_json(&member.intents, "group undo intents")?;
            let missing: Vec<Intent> = intents.iter().copied().filter(|intent| !req.allowed_intents.contains(intent)).collect();
            if !missing.is_empty() {
                return Err(err(ErrorCode::ContentLocked, format!("group {group_id} would revert {missing:?}, which allowed_intents does not include")).ids(changed_ids));
            }
            let (target, kind) = if redo {
                let first_undone: Option<i64> = self.conn.query_row(
                    "SELECT MIN(seq) FROM undo_log WHERE artifact_id = ?1 AND state = 'undone'",
                    params![member.artifact_id], |row| row.get(0),
                ).map_err(sql)?;
                if first_undone != Some(member.seq) || row.current_snapshot != member.before_snapshot {
                    return Err(err(ErrorCode::RevisionConflict, format!("artifact {} has a later history change after group {group_id}", member.artifact_id)).at(row.current_revision));
                }
                (member.after_snapshot.clone(), CommitKind::Redo { seq: member.seq })
            } else {
                let latest_done: Option<i64> = self.conn.query_row(
                    "SELECT MAX(seq) FROM undo_log WHERE artifact_id = ?1 AND state = 'done'",
                    params![member.artifact_id], |row| row.get(0),
                ).map_err(sql)?;
                if latest_done != Some(member.seq) || row.current_snapshot != member.after_snapshot {
                    return Err(err(ErrorCode::RevisionConflict, format!("artifact {} has a later history change after group {group_id}", member.artifact_id)).at(row.current_revision));
                }
                (member.before_snapshot.clone(), CommitKind::Undo { seq: member.seq })
            };
            if let Some(job) = job.filter(|_| member.artifact_id == req.artifact_id) {
                store::check_job_write(&self.conn, job, &member.artifact_id, &req.allowed_intents)?;
                self.check_job_history(job, &member.artifact_id, member.seq, &changed_ids)?;
            }
            let ids: BTreeSet<String> = changed_ids.iter().cloned().collect();
            let engine = self.engine_by_id(&row.engine_id)?;
            let restored = self.load(&*engine, &target, row.current_revision)?;
            // The member's changed ids against the holds now and on the revision it restores. Another member is checked
            // as a whole: no writer owns a reservation there.
            if member.artifact_id == req.artifact_id {
                self.check_changed(&member.artifact_id, &ids, &req.actor, job.map(|job| job.job_id.as_str()), &restored)?;
            } else {
                self.check_changed(&member.artifact_id, &ids, "", None, &restored)?;
            }
            let persisted_features = persisted_features(&*engine, &restored)?;
            candidates.push(Candidate {
                key: key.clone(), artifact_id: member.artifact_id.clone(), actor: req.actor.clone(),
                job: if member.artifact_id == req.artifact_id { job.cloned() } else { None },
                audit_job: job.cloned(), allowed: req.allowed_intents.clone(), base_revision: row.current_revision,
                before: row.current_snapshot.clone(), after: target.to_owned(), after_bytes: None, title: restored.title.clone(),
                changed_ids, intents, diagnostics: Vec::new(), kind, staged: Vec::new(), candidate: None, design_system_revision_guard: None,
                undone_change: None, created_ids: BTreeSet::new(), persisted_features, receipt_notes: None,
            });
        }
        Ok(candidates)
    }

    fn evaluate_group_undo_change(
        &mut self,
        req: &UndoChangeRequest,
        change_id: &str,
        key: &RequestKey,
        group_id: &str,
        job: Option<&JobRef>,
    ) -> DltResult<Vec<Candidate>> {
        let members = self.group_undo_rows(group_id)?;
        if !members.iter().any(|member| member.artifact_id == req.artifact_id) {
            return Err(integrity(format!("history group {group_id} does not include requested artifact {}", req.artifact_id)));
        }
        if job.is_some() {
            let outside_scope: Vec<String> = members.iter().filter(|member| member.artifact_id != req.artifact_id).map(|member| member.artifact_id.clone()).collect();
            if !outside_scope.is_empty() {
                return Err(invalid("job-bound undo_change cannot change group participants outside the job artifact scope").ids(outside_scope));
            }
        }
        let mut candidates = Vec::with_capacity(members.len());
        for member in members {
            let row = store::artifact_row(&self.conn, &member.artifact_id)?;
            if member.artifact_id == req.artifact_id && req.expected_revision != row.current_revision {
                return Err(err(ErrorCode::RevisionConflict, format!("expected_revision {} is not the current revision {}", req.expected_revision, row.current_revision)).at(row.current_revision));
            }
            let mut member_req = req.clone();
            member_req.artifact_id = member.artifact_id.clone();
            member_req.expected_revision = row.current_revision;
            member_req.job_ref = None;
            let member_job = job.filter(|_| member.artifact_id == req.artifact_id);
            let mut candidate = self.evaluate_undo_change(&member_req, change_id, key, member_job)?;
            if member.artifact_id != req.artifact_id && self.views.reserved(&member.artifact_id) {
                // Another member is checked as a whole (no writer owns a reservation there), on the revision it restores.
                let ids: BTreeSet<String> = candidate.changed_ids.iter().cloned().collect();
                let loaded;
                let restored: &Artifact = match &candidate.candidate {
                    Some(artifact) => artifact,
                    None => {
                        loaded = self.load_snapshot(&member.artifact_id, &candidate.after)?;
                        &loaded
                    }
                };
                self.check_changed(&member.artifact_id, &ids, "", None, restored)?;
            }
            candidate.kind = CommitKind::Revert;
            candidate.undone_change = Some(change_id.to_owned());
            candidate.audit_job = job.cloned();
            candidates.push(candidate);
        }
        Ok(candidates)
    }
    pub(crate) fn history(&mut self, params: Value, redo: bool, bound: Option<&JobRef>) -> DltResult<Value> {
        let (req, key, job) = self.history_request(params, redo, bound)?;
        if let Some(found) = store::find_receipt(&self.conn, &key)? {
            return found;
        }
        let (group_id, candidates) = self.history_candidates(&req, &key, job.as_ref(), redo)?;
        let mut candidates = candidates.map_err(|error| self.reject(&key, Some(&req.artifact_id), error))?;
        if let Some(group_id) = group_id {
            return to_value(&self.commit_group(&key, &req.artifact_id, &group_id, false, candidates)?);
        }
        let cand = candidates.pop().ok_or_else(|| integrity("history has no change to commit"))?;
        let staging = self.staging();
        to_value(&self.commit_design_system_change(cand, &staging)?)
    }

    /// The request of `history.undo` or `history.redo`, its receipt key and the job it runs as.
    fn history_request(&self, params: Value, redo: bool, bound: Option<&JobRef>) -> DltResult<(HistoryRequest, RequestKey, Option<JobRef>)> {
        reject_duplicate_set(&params, "allowed_intents")?;
        let req: HistoryRequest = parse(params)?;
        if req.workspace_id != self.id { return Err(invalid(format!("workspace_id {:?} does not match this workspace", req.workspace_id))); }
        if req.allowed_intents.is_empty() { return Err(invalid("allowed_intents must not be empty")); }
        let job = effective_job(bound, req.job_ref.as_ref(), self.caller.as_deref())?;
        let mut keyed = req.clone();
        keyed.job_ref = job.clone();
        let method = if redo { "history.redo" } else { "history.undo" };
        let key = key_for(method, &req.request_id, &keyed)?;
        Ok((req, key, job))
    }

    /// What `history.undo` or `history.redo` would commit: the members of the history group it reverts (with the group id), or
    /// its one change. The outer error is the store's; the inner one is the refusal of the request, which is recorded.
    fn history_candidates(&mut self, req: &HistoryRequest, key: &RequestKey, job: Option<&JobRef>, redo: bool) -> DltResult<(Option<String>, DltResult<Vec<Candidate>>)> {
        Ok(match self.current_history_group(&req.artifact_id, redo)? {
            Some(group_id) => {
                let members = self.evaluate_group_history(req, key, &group_id, job, redo);
                (Some(group_id), members)
            }
            None => (None, self.evaluate_history(req, key, job, redo).map(|candidate| vec![candidate])),
        })
    }

    /// The (current, restored) artifact of each artifact that a `history.undo`, `history.redo` or `history.undo_change`
    /// request would change if it were dispatched now by a connection bound to `bound`. Nothing is committed and no receipt is
    /// recorded: the host judges the review gate on it before it dispatches the request (49w). Empty for any other method, for
    /// a request whose receipt would be replayed and for a request the core would refuse (the dispatch then refuses it the
    /// same way). An error only when a snapshot the request would move from or to cannot be read.
    pub fn history_preview(&mut self, method: &str, params: &Value, bound: Option<&JobRef>) -> DltResult<Vec<(Artifact, Artifact)>> {
        let refused_or = |candidates: DltResult<(Option<String>, DltResult<Vec<Candidate>>)>| {
            candidates.and_then(|(_, candidates)| candidates).unwrap_or_default()
        };
        let candidates = match method {
            "history.undo" | "history.redo" => {
                let redo = method == "history.redo";
                match self.history_request(params.clone(), redo, bound) {
                    Ok((req, key, job)) if store::find_receipt(&self.conn, &key)?.is_none() => refused_or(self.history_candidates(&req, &key, job.as_ref(), redo)),
                    _ => Vec::new(),
                }
            }
            "history.undo_change" => match self.undo_change_request(params.clone(), bound) {
                Ok((req, change_id, key, job)) if store::find_receipt(&self.conn, &key)?.is_none() => {
                    refused_or(self.undo_change_candidates(&req, &change_id, &key, job.as_ref()))
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        candidates.into_iter().map(|candidate| self.history_transition(candidate)).collect()
    }

    /// The artifact `candidate` moves from (the current snapshot) and the one it restores.
    fn history_transition(&self, candidate: Candidate) -> DltResult<(Artifact, Artifact)> {
        let row = store::artifact_row(&self.conn, &candidate.artifact_id)?;
        let engine = self.engine_by_id(&row.engine_id)?;
        let current = self.load(&*engine, &candidate.before, row.current_revision)?;
        let restored = match candidate.candidate {
            Some(artifact) => artifact,
            None => self.load(&*engine, &candidate.after, row.current_revision)?,
        };
        Ok((current, restored))
    }

    fn evaluate_history(&mut self, req: &HistoryRequest, key: &RequestKey, job: Option<&JobRef>, redo: bool) -> DltResult<Candidate> {
        let row = store::artifact_row(&self.conn, &req.artifact_id)?;
        let rev = row.current_revision;
        (|| {
            if req.expected_revision != rev { return Err(err(ErrorCode::RevisionConflict, format!("expected_revision {} is not the current revision {rev}", req.expected_revision))); }
            if let Some(j) = job { store::check_job_write(&self.conn, j, &req.artifact_id, &req.allowed_intents)?; }
            let sql_text = if redo {
                "SELECT seq, before_snapshot, after_snapshot, intents, changed_ids FROM undo_log WHERE artifact_id = ?1 AND state = 'undone' ORDER BY seq ASC LIMIT 1"
            } else {
                "SELECT seq, before_snapshot, after_snapshot, intents, changed_ids FROM undo_log WHERE artifact_id = ?1 AND state = 'done' ORDER BY seq DESC LIMIT 1"
            };
            let entry: Option<(i64, String, String, String, String)> = self.conn
                .query_row(sql_text, params![req.artifact_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .optional().map_err(sql)?;
            let (seq, before, after, intents, changed) = entry.ok_or_else(|| errors::not_found(if redo { "nothing to redo" } else { "nothing to undo" }))?;
            let intents: BTreeSet<Intent> = store::from_json(&intents, "undo intents")?;
            let changed_ids: Vec<String> = store::from_json(&changed, "undo changed_ids")?;
            let missing: Vec<Intent> = intents.iter().copied().filter(|i| !req.allowed_intents.contains(i)).collect();
            if !missing.is_empty() {
                return Err(err(ErrorCode::ContentLocked, format!("{} would revert {missing:?}, which allowed_intents does not include", if redo { "redo" } else { "undo" })).ids(changed_ids));
            }
            if let Some(j) = job { self.check_job_history(j, &req.artifact_id, seq, &changed_ids)?; }
            let ids: BTreeSet<String> = changed_ids.iter().cloned().collect();
            if let Some((hit, owner)) = self.views.busy(&req.artifact_id, &ids, &req.actor, job.map(|j| j.job_id.as_str())) {
                return Err(err(ErrorCode::TargetBusy, format!("targets are reserved by {owner:?}")).ids(hit));
            }
            let (expected_current, target, kind) = if redo { (before, after, CommitKind::Redo { seq }) } else { (after, before, CommitKind::Undo { seq }) };
            if row.current_snapshot != expected_current { return Err(integrity("undo log does not match the current snapshot")); }
            let engine = self.engine_by_id(&row.engine_id)?;
            let restored = self.load(&*engine, &target, rev)?;
            // The post-check on the revision the undo or redo would commit: ids it newly places inside a held node.
            self.check_changed(&req.artifact_id, &ids, &req.actor, job.map(|j| j.job_id.as_str()), &restored)?;
            let persisted_features = persisted_features(&*engine, &restored)?;
            Ok(Candidate {
                key: key.clone(), artifact_id: req.artifact_id.clone(), actor: req.actor.clone(), job: job.cloned(), audit_job: None,
                allowed: req.allowed_intents.clone(), base_revision: rev, before: row.current_snapshot.clone(), after: target, after_bytes: None,
                title: restored.title.clone(), changed_ids, intents, diagnostics: vec![], kind, staged: vec![], candidate: None, design_system_revision_guard: None, undone_change: None, created_ids: BTreeSet::new(), persisted_features, receipt_notes: None,
            })
        })().map_err(|e| e.at(rev))
    }

    /// history.undo_change: takes back one change batch, named by its request id. The latest change
    /// still in effect is undone exactly like history.undo (redo brings it back). An earlier one is
    /// taken back structurally (crate::revert) as a new undo-log entry, refused when a later change
    /// still in effect changed one of its ids or the same data.
    pub(crate) fn undo_change(&mut self, params: Value, bound: Option<&JobRef>) -> DltResult<Value> {
        let (req, change_id, key, job) = self.undo_change_request(params, bound)?;
        if let Some(found) = store::find_receipt(&self.conn, &key)? { return found; }
        let (group_id, candidates) = self.undo_change_candidates(&req, &change_id, &key, job.as_ref())?;
        let mut candidates = candidates.map_err(|error| self.reject(&key, Some(&req.artifact_id), error))?;
        if group_id.is_some() {
            let revert_group_id = new_id();
            return to_value(&self.commit_group(&key, &req.artifact_id, &revert_group_id, true, candidates)?);
        }
        let cand = candidates.pop().ok_or_else(|| integrity("undo_change has no change to commit"))?;
        let staging = self.staging();
        to_value(&self.commit_design_system_change(cand, &staging)?)
    }

    /// The request of `history.undo_change`, the normalized change id, its receipt key and the job it runs as.
    fn undo_change_request(&self, params: Value, bound: Option<&JobRef>) -> DltResult<(UndoChangeRequest, String, RequestKey, Option<JobRef>)> {
        reject_duplicate_set(&params, "allowed_intents")?;
        let req: UndoChangeRequest = parse(params)?;
        if req.workspace_id != self.id { return Err(invalid(format!("workspace_id {:?} does not match this workspace", req.workspace_id))); }
        if req.allowed_intents.is_empty() { return Err(invalid("allowed_intents must not be empty")); }
        let change_id = crate::receipts::normalize_request_id(&req.change_id).map_err(|_| invalid(format!("change_id must be the request id of a change: {:?}", req.change_id)))?;
        let job = effective_job(bound, req.job_ref.as_ref(), self.caller.as_deref())?;
        let mut keyed = req.clone();
        keyed.job_ref = job.clone();
        let key = key_for("history.undo_change", &req.request_id, &keyed)?;
        Ok((req, change_id, key, job))
    }

    /// What `history.undo_change` would commit, as [`Self::history_candidates`] for `history.undo`.
    fn undo_change_candidates(&mut self, req: &UndoChangeRequest, change_id: &str, key: &RequestKey, job: Option<&JobRef>) -> DltResult<(Option<String>, DltResult<Vec<Candidate>>)> {
        Ok(match self.change_history_group(&req.artifact_id, change_id)? {
            Some(group_id) => {
                let members = self.evaluate_group_undo_change(req, change_id, key, &group_id, job);
                (Some(group_id), members)
            }
            None => (None, self.evaluate_undo_change(req, change_id, key, job).map(|candidate| vec![candidate])),
        })
    }

    fn evaluate_undo_change(&mut self, req: &UndoChangeRequest, change_id: &str, key: &RequestKey, job: Option<&JobRef>) -> DltResult<Candidate> {
        let row = store::artifact_row(&self.conn, &req.artifact_id)?;
        let rev = row.current_revision;
        (|| {
            if req.expected_revision != rev { return Err(err(ErrorCode::RevisionConflict, format!("expected_revision {} is not the current revision {rev}", req.expected_revision))); }
            if let Some(j) = job { store::check_job_write(&self.conn, j, &req.artifact_id, &req.allowed_intents)?; }
            let entry: Option<(i64, String, String, String, String, String)> = self.conn.query_row(
                "SELECT u.seq, u.before_snapshot, u.after_snapshot, u.intents, u.changed_ids, u.state FROM undo_log u \
                 JOIN revisions r ON r.artifact_id = u.artifact_id AND r.revision = u.change_revision WHERE u.artifact_id = ?1 AND r.request_id = ?2",
                params![req.artifact_id, change_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            ).optional().map_err(sql)?;
            let gone = |why: &str| errors::not_found(format!("change {change_id} {why}")).ids([change_id.to_string()]);
            let (seq, before, after, intents, changed, state) = entry.ok_or_else(|| gone(&format!("is not in the undo history of artifact {}", req.artifact_id)))?;
            match state.as_str() {
                "done" => {}
                "undone" => return Err(gone("is already undone; history.redo brings it back")),
                _ => return Err(gone("was undone and a later edit discarded it")),
            }
            let intents: BTreeSet<Intent> = store::from_json(&intents, "undo intents")?;
            let changed_ids: Vec<String> = store::from_json(&changed, "undo changed_ids")?;
            let missing: Vec<Intent> = intents.iter().copied().filter(|i| !req.allowed_intents.contains(i)).collect();
            if !missing.is_empty() {
                return Err(err(ErrorCode::ContentLocked, format!("undoing change {change_id} would revert {missing:?}, which allowed_intents does not include")).ids(changed_ids));
            }
            if let Some(j) = job { self.check_job_history(j, &req.artifact_id, seq, &changed_ids)?; }
            // Later changes still in effect.
            let later: Vec<String> = {
                let mut st = self.conn.prepare("SELECT changed_ids FROM undo_log WHERE artifact_id = ?1 AND seq > ?2 AND state = 'done' ORDER BY seq").map_err(sql)?;
                let rows = st.query_map(params![req.artifact_id, seq], |r| r.get::<_, String>(0)).map_err(sql)?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)?
            };
            let ids: BTreeSet<String> = changed_ids.iter().cloned().collect();
            let mut touched = BTreeSet::new();
            for later_ids in &later {
                let later_ids: Vec<String> = store::from_json(later_ids, "undo changed_ids")?;
                touched.extend(later_ids.into_iter().filter(|id| ids.contains(id)));
            }
            if !touched.is_empty() {
                return Err(err(ErrorCode::RevisionConflict, format!("{} later change(s) still in effect changed the same ids; undo them first (history.undo) or edit by hand", later.len())).ids(touched));
            }
            if let Some((hit, owner)) = self.views.busy(&req.artifact_id, &ids, &req.actor, job.map(|j| j.job_id.as_str())) {
                return Err(err(ErrorCode::TargetBusy, format!("targets are reserved by {owner:?}")).ids(hit));
            }
            let engine = self.engine_by_id(&row.engine_id)?;
            let base = |kind: CommitKind, after: String, after_bytes: Option<Vec<u8>>, title: String, diagnostics: Vec<Diagnostic>, staged: Vec<(AssetRef, Arc<Vec<u8>>)>, candidate: Option<Artifact>, persisted_features: BTreeSet<String>| Candidate {
                key: key.clone(), artifact_id: req.artifact_id.clone(), actor: req.actor.clone(), job: job.cloned(), audit_job: None,
                allowed: req.allowed_intents.clone(), base_revision: rev, before: row.current_snapshot.clone(), after, after_bytes, title,
                changed_ids: changed_ids.clone(), intents: intents.clone(), diagnostics, kind, staged, candidate, design_system_revision_guard: None, undone_change: Some(change_id.to_string()), created_ids: BTreeSet::new(), persisted_features, receipt_notes: None,
            };
            if later.is_empty() {
                // The latest change still in effect: exactly history.undo.
                if row.current_snapshot != after { return Err(integrity("undo log does not match the current snapshot")); }
                let restored = self.load(&*engine, &before, rev)?;
                self.check_changed(&req.artifact_id, &ids, &req.actor, job.map(|j| j.job_id.as_str()), &restored)?;
                let persisted_features = persisted_features(&*engine, &restored)?;
                return Ok(base(CommitKind::Undo { seq }, before, None, restored.title, vec![], vec![], None, persisted_features));
            }
            let (b, a, c) = (self.load(&*engine, &before, rev)?, self.load(&*engine, &after, rev)?, self.load(&*engine, &row.current_snapshot, rev)?);
            let json_of = |artifact: &Artifact| serde_json::to_value(artifact).map_err(|e| err(ErrorCode::Internal, e.to_string()));
            let (mut bv, mut av, cv) = (json_of(&b)?, json_of(&a)?, json_of(&c)?);
            // Diagnostics describe the last edit, not the document: the result keeps the current ones.
            for v in [&mut bv, &mut av] { v["diagnostics"] = cv["diagnostics"].clone(); }
            let merged = crate::revert::revert(&bv, &av, &cv).map_err(|pointers| {
                let total = pointers.len();
                let listed: Vec<Diagnostic> = pointers.into_iter().take(crate::strict::MAX_LISTED)
                    .map(|p| Diagnostic { code: "undo_conflict".into(), message: if p.is_empty() { "(the whole document)".into() } else { p }, node_id: None, blocking: true, params: Default::default() }).collect();
                err(ErrorCode::RevisionConflict, format!("change {change_id} cannot be undone alone: later changes edited the same data at {total} place(s)")).diags(listed)
            })?;
            let candidate: Artifact = serde_json::from_value(merged)
                .map_err(|e| err(ErrorCode::RevisionConflict, format!("change {change_id} cannot be undone alone: the result is not a valid document ({e})")))?;
            if candidate.id != c.id || candidate.kind != c.kind || candidate.engine_id != c.engine_id || candidate.schema_version != c.schema_version {
                return Err(integrity("undo log snapshots disagree on the artifact identity"));
            }
            self.check_changed(&req.artifact_id, &ids, &req.actor, job.map(|j| j.job_id.as_str()), &candidate)?;
            let staging = self.staging();
            let renderer = self.renderer.clone();
            let ctx = EngineContext { assets: &staging, renderer: &*renderer };
            let known: BTreeSet<_> = engine.validate(&c, &ctx).iter().filter(|d| d.blocking).map(diag_key).collect();
            let found = engine.validate(&candidate, &ctx);
            let introduced: Vec<Diagnostic> = found.iter().filter(|d| d.blocking && !known.contains(&diag_key(d))).cloned().collect();
            if !introduced.is_empty() {
                return Err(err(ErrorCode::UnsupportedOperation, format!("undoing change {change_id} alone would leave blocking validation diagnostics; nothing was applied")).diags(introduced));
            }
            let (d0, d1) = (engine.content_digest(&c)?, engine.content_digest(&candidate)?);
            for (intent, x, y) in [(Intent::Content, &d0.content, &d1.content), (Intent::Data, &d0.data, &d1.data)] {
                if !req.allowed_intents.contains(&intent) && x != y {
                    return Err(err(ErrorCode::ContentLocked, format!("undoing change {change_id} changes {intent:?}, which allowed_intents does not include")).ids(changed_ids.clone()));
                }
            }
            let mut refs = BTreeSet::new();
            asset_refs(&serde_json::to_value(&candidate).map_err(|e| err(ErrorCode::Internal, e.to_string()))?, &mut refs);
            let missing: Vec<String> = refs.into_iter().filter(|h| !staging.contains(h)).collect();
            if !missing.is_empty() { return Err(integrity("the undone document references assets that are not in this workspace").ids(missing)); }
            let (bytes, hash) = Workspace::snapshot_of(&*engine, &candidate)?;
            let mut diagnostics: Vec<Diagnostic> = found.into_iter().filter(|d| !d.blocking).collect();
            if hash == row.current_snapshot { diagnostics.push(Diagnostic { code: "no_effect".into(), message: "the change was already taken back".into(), node_id: None, blocking: false, params: Default::default() }); }
            let title = candidate.title.clone();
            let persisted_features = persisted_features(&*engine, &candidate)?;
            Ok(base(CommitKind::Revert, hash, Some(bytes), title, diagnostics, staging.take_staged(), Some(candidate), persisted_features))
        })().map_err(|e| e.at(rev))
    }

    // ------------------------------------------------------------------ create / open
    pub(crate) fn create_artifact(&mut self, req: CreateArtifactRequest) -> DltResult<Value> {
        let key = key_for("artifact.create", &req.request_id, &req)?;
        if let Some(found) = store::find_receipt(&self.conn, &key)? { return found; }
        if req.options.title.chars().count() > 1024 { return Err(invalid("title is longer than 1024 characters")); }
        if let Some(size) = req.options.size { if !size.valid() { return Err(invalid("size must be positive and at most 10 m per side")); } }
        let engine = self.engine_by_kind(req.options.kind)?;
        let staging = self.staging();
        let renderer = self.renderer.clone();
        let artifact = engine.create(&req.options, &EngineContext { assets: &staging, renderer: &*renderer })?;
        self.commit_new(&*engine, key, &req.actor, artifact, req.options.kind, &staging, None)
    }

    pub(crate) fn open_artifact(&mut self, req: OpenRequest) -> DltResult<Value> {
        if is_pdf_path(&req.path) {
            return match self.prepare_pdf_open(req, self.agent_paths())? {
                PreparedPdfOpen::Existing(result) => Ok(result),
                PreparedPdfOpen::Pending(open) => {
                    let cancel = PdfCancel::new();
                    let converted = open.run(&cancel);
                    self.finish_pdf_open(open, converted, &cancel)
                }
            };
        }
        let key = key_for("artifact.open", &req.request_id, &req)?;
        if let Some(found) = store::find_receipt(&self.conn, &key)? { return found; }
        let (path, file) = crate::files::check_source_path(&req.path, self.agent_paths())?;
        let (kind, bytes) = crate::files::read_artifact_input(&path, file, req.kind)?;
        let engine = self.engine_by_kind(kind)?;
        let source = (path.to_string_lossy().into_owned(), sha256(&bytes));
        let staging = self.staging();
        let asset = staging.put(&bytes, &crate::files::mime_for(&path, &bytes))?;
        drop(bytes);
        let title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let renderer = self.renderer.clone();
        let artifact = engine.open(&OpenInput { kind, title, asset }, &EngineContext { assets: &staging, renderer: &*renderer })?;
        self.commit_new(&*engine, key, &req.actor, artifact, kind, &staging, Some(source))
    }

    pub(crate) fn prepare_pdf_open(&self, req: OpenRequest, agent_paths: bool) -> DltResult<PreparedPdfOpen> {
        let key = key_for("artifact.open", &req.request_id, &req)?;
        if let Some(found) = store::find_receipt(&self.conn, &key)? { return found.map(PreparedPdfOpen::Existing); }
        if req.kind.is_some_and(|kind| kind != ArtifactKind::Docx) {
            return Err(invalid("a PDF opens as an editable DOCX document"));
        }
        self.engine_by_kind(ArtifactKind::Docx)?;
        let import = self.pdf_edit_importer.ok_or_else(|| DltError::unsupported("PDF editing is unavailable in this host"))?;
        let (path, file) = crate::files::check_source_path(&req.path, agent_paths)?;
        let title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(PreparedPdfOpen::Pending(PdfOpen { file, title, actor: req.actor, key, import }))
    }

    pub fn finish_pdf_open(&mut self, open: PdfOpen, converted: DltResult<Vec<u8>>, cancel: &PdfCancel) -> DltResult<Value> {
        if let Some(found) = store::find_receipt(&self.conn, &open.key)? { return found; }
        let check_cancel = || if cancel.is_canceled() { Err(err(ErrorCode::Canceled, "PDF conversion was canceled")) } else { Ok(()) };
        check_cancel()?;
        let bytes = converted?;
        let kind = ArtifactKind::Docx;
        let engine = self.engine_by_kind(kind)?;
        let staging = self.staging();
        let asset = staging.put(&bytes, "application/vnd.openxmlformats-officedocument.wordprocessingml.document")?;
        drop(bytes);
        let renderer = self.renderer.clone();
        let mut artifact = engine.open(&OpenInput { kind, title: open.title, asset }, &EngineContext { assets: &staging, renderer: &*renderer })?;
        artifact.diagnostics.push(Diagnostic {
            code: designllm_contracts::PDF_EDIT_APPROXIMATE.into(),
            message: "PDF was converted to editable DOCX. Table recognition and pagination depend on the PDF; check them before exporting.".into(),
            node_id: None, blocking: false, params: Default::default(),
        });
        check_cancel()?;
        self.commit_new(&*engine, open.key, &open.actor, artifact, kind, &staging, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_new(&mut self, engine: &dyn EngineAdapter, key: RequestKey, actor: &str, artifact: Artifact, kind: ArtifactKind, staging: &StagingAssets, source: Option<(String, String)>) -> DltResult<Value> {
        let new = self.stage_new(engine, artifact, kind, staging)?;
        let artifact = &new.artifact;
        let summary = ArtifactSummary { id: artifact.id.clone(), kind, title: artifact.title.clone(), revision: 0, engine_id: artifact.engine_id.clone(), dirty: source.is_none(), modified_at_ms: None, active_jobs: vec![] };
        let result = to_value(&summary)?;
        let opened = source.is_some();
        {
            let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
            if let Some(found) = store::find_receipt(&tx, &key)? { return found; }
            insert_new(&tx, &new, kind, &key, actor, source.as_ref())?;
            store::merge_store_features(&tx, &new.persisted_features)?;
            store::insert_receipt(&tx, &key, Some(&artifact.id), &StoredResult::Committed { result: result.clone() })?;
            store::insert_event(&tx, Some(&artifact.id), Some(0), if opened { "artifact.opened" } else { "artifact.created" },
                &json!({ "request_id": key.request_id, "kind": kind, "title": artifact.title, "diagnostics": artifact.diagnostics.len(), "actor": actor }))?;
            fire(&self.hook, FaultPoint::TxnBeforeCommit)?;
            tx.commit().map_err(sql)?;
        }
        self.base.add_committed(&new.promoted);
        fire(&self.hook, FaultPoint::AfterCommit)?;
        Ok(result)
    }

    /// Checks an engine-made artifact (create/open), gives it revision 0 and makes its blobs durable.
    /// No row is written; `insert_new` does that inside the caller's transaction.
    pub(crate) fn stage_new(&mut self, engine: &dyn EngineAdapter, mut artifact: Artifact, kind: ArtifactKind, staging: &StagingAssets) -> DltResult<NewArtifact> {
        if artifact.kind != kind { return Err(err(ErrorCode::Internal, format!("engine {} returned {:?} for a {kind:?} request", engine.id(), artifact.kind))); }
        if artifact.schema_version != SCHEMA_VERSION { return Err(err(ErrorCode::SchemaUnsupported, "engine produced another contract schema_version")); }
        if artifact.engine_id != engine.id() { return Err(err(ErrorCode::Internal, format!("engine {} labeled the artifact with engine_id {:?}", engine.id(), artifact.engine_id))); }
        if artifact.id.trim().is_empty() { return Err(err(ErrorCode::Internal, "engine returned an empty artifact id")); }
        // A native project file saved from this workspace may carry an id that is still in use;
        // opening it again creates a new artifact instead of failing or aliasing the old one.
        if store::artifact_exists(&self.conn, &artifact.id)? {
            let previous = std::mem::replace(&mut artifact.id, new_id());
            artifact.diagnostics.push(Diagnostic {
                code: "artifact_id_reassigned".into(),
                message: format!("the file carries artifact id {previous}, which already exists in this workspace; it was opened as a new artifact"),
                node_id: None, blocking: false,
                params: Default::default(),
            });
        }
        artifact.revision = 0;
        let mut refs = BTreeSet::new();
        asset_refs(&serde_json::to_value(&artifact).map_err(|e| err(ErrorCode::Internal, e.to_string()))?, &mut refs);
        let missing: Vec<String> = refs.into_iter().filter(|h| !staging.contains(h)).collect();
        if !missing.is_empty() { return Err(err(ErrorCode::Internal, "engine produced references to assets it did not store").ids(missing)); }
        let persisted_features = persisted_features(engine, &artifact)?;
        let (bytes, hash) = Workspace::snapshot_of(engine, &artifact)?;
        let staged = staging.take_staged();
        let mut promoted = Vec::new();
        for (r, data) in &staged { self.base.blobs.put(data)?; promoted.push(r.clone()); }
        self.base.blobs.put(&bytes)?;
        Ok(NewArtifact { artifact, hash, promoted, persisted_features })
    }
}

/// An artifact at revision 0 whose blobs are durable (`Workspace::stage_new`).
pub(crate) struct NewArtifact { pub artifact: Artifact, pub hash: String, pub promoted: Vec<AssetRef>, pub persisted_features: BTreeSet<String> }

/// Rows of a new artifact: its assets, the artifact row and revision 0. `source` = opened file
/// (path, hash), which is also the saved state.
pub(crate) fn insert_new(tx: &rusqlite::Connection, new: &NewArtifact, kind: ArtifactKind, key: &RequestKey, actor: &str, source: Option<&(String, String)>) -> DltResult<()> {
    let (artifact, hash) = (&new.artifact, &new.hash);
    let opened = source.is_some();
    store::insert_assets(tx, &new.promoted, if opened { "open" } else { "create" })?;
    let (source_path, source_hash) = source.cloned().unzip();
    tx.execute(
        "INSERT INTO artifacts(artifact_id, kind, engine_id, title, origin, current_revision, current_snapshot, base_snapshot, source_path, source_hash, saved_revision, saved_path, saved_hash, saved_snapshot, created_seq) \
         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?6, ?7, ?8, ?9, ?7, ?8, ?10, (SELECT COALESCE(MAX(created_seq), 0) + 1 FROM artifacts))",
        params![artifact.id, store::kind_str(kind), artifact.engine_id, artifact.title, artifact.origin, hash, source_path, source_hash,
                if opened { Some(0i64) } else { None }, if opened { Some(hash.clone()) } else { None }],
    ).map_err(sql)?;
    tx.execute(
        "INSERT INTO revisions(artifact_id, revision, snapshot, kind, request_id, actor, job_id, attempt_id, intents, changed_ids) VALUES (?1, 0, ?2, ?3, ?4, ?5, NULL, NULL, '[]', '[]')",
        params![artifact.id, hash, if opened { "open" } else { "create" }, key.request_id, actor],
    ).map_err(sql)?;
    store::touch_artifact(tx, &artifact.id)
}

fn errors_unsupported_hash(id: &str) -> DltError {
    err(ErrorCode::UnsupportedOperation, format!("read dependency {id}: hash dependencies are not supported in v1; use revision")).ids([id.to_string()])
}

fn commit_inner(conn: &mut rusqlite::Connection, base: &crate::blobs::AssetBase, hook: &crate::blobs::SharedHook, c: &Candidate) -> Result<ChangeResult, Fail> {
    let mut promoted = Vec::new();
    for (r, bytes) in &c.staged { base.blobs.put(bytes).map_err(Fail::Final)?; promoted.push(r.clone()); }
    match &c.after_bytes {
        Some(bytes) => { base.blobs.put(bytes).map_err(Fail::Final)?; }
        None => { if !base.blobs.contains(&c.after) { return Err(Fail::Final(integrity("history target snapshot blob is missing").ids([c.after.clone()]))); } }
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|e| Fail::Final(sql(e)))?;
    if let Some(found) = store::find_receipt(&tx, &c.key).map_err(Fail::Final)? {
        return match found {
            Ok(v) => serde_json::from_value(v).map_err(|e| Fail::Final(integrity(format!("stored receipt is unreadable: {e}")))),
            Err(e) => Err(Fail::Final(e)),
        };
    }
    let row = store::artifact_row(&tx, &c.artifact_id).map_err(Fail::New)?;
    if row.current_revision != c.base_revision || row.current_snapshot != c.before {
        return Err(Fail::New(err(ErrorCode::RevisionConflict, format!("artifact {} is at revision {}; the request was prepared on {}", c.artifact_id, row.current_revision, c.base_revision)).at(row.current_revision)));
    }
    if let Some((system_id, expected_revision)) = &c.design_system_revision_guard {
        let system_row = store::artifact_row(&tx, system_id).map_err(Fail::New)?;
        if system_row.current_revision != *expected_revision {
            return Err(Fail::New(err(
                ErrorCode::RevisionConflict,
                format!("design system {system_id} changed during consumer setup (now revision {})", system_row.current_revision),
            ).at(system_row.current_revision).ids([system_id.clone()])));
        }
    }
    if let Some(j) = &c.job { store::check_job_commit(&tx, j, &c.artifact_id, &c.allowed).map_err(|e| Fail::New(e.at(row.current_revision)))?; }
    let fin = |e: DltError| Fail::Final(e);
    store::insert_assets(&tx, &promoted, "change").map_err(fin)?;
    let revision = row.current_revision + 1;
    let audit = c.job.as_ref().or(c.audit_job.as_ref());
    tx.execute(
        "INSERT INTO revisions(artifact_id, revision, snapshot, kind, request_id, actor, job_id, attempt_id, intents, changed_ids) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![c.artifact_id, revision as i64, c.after, c.kind.revision_kind(), c.key.request_id, c.actor,
                audit.map(|j| j.job_id.as_str()), audit.map(|j| j.attempt_id.as_str()),
                store::to_json(&c.intents).map_err(fin)?, store::to_json(&c.changed_ids).map_err(fin)?],
    ).map_err(|e| fin(sql(e)))?;
    tx.execute("UPDATE artifacts SET current_revision = ?2, current_snapshot = ?3, title = ?4 WHERE artifact_id = ?1", params![c.artifact_id, revision as i64, c.after, c.title]).map_err(|e| fin(sql(e)))?;
    store::touch_artifact(&tx, &c.artifact_id).map_err(fin)?;
    if let Some(j) = audit.filter(|_| !c.created_ids.is_empty()) { store::add_job_created_ids(&tx, &j.job_id, &c.created_ids).map_err(fin)?; }
    match c.kind {
        CommitKind::Change | CommitKind::Revert => {
            // A new ordinary edit ends the redo branch.
            tx.execute("UPDATE undo_log SET state = 'discarded' WHERE artifact_id = ?1 AND state = 'undone'", params![c.artifact_id]).map_err(|e| fin(sql(e)))?;
            let seq: i64 = tx.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM undo_log WHERE artifact_id = ?1", params![c.artifact_id], |r| r.get(0)).map_err(|e| fin(sql(e)))?;
            tx.execute(
                "INSERT INTO undo_log(artifact_id, seq, change_revision, before_snapshot, after_snapshot, intents, changed_ids, state) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'done')",
                params![c.artifact_id, seq, revision as i64, c.before, c.after, store::to_json(&c.intents).map_err(fin)?, store::to_json(&c.changed_ids).map_err(fin)?],
            ).map_err(|e| fin(sql(e)))?;
        }
        CommitKind::Undo { seq } | CommitKind::Redo { seq } => {
            let (from, to) = if matches!(c.kind, CommitKind::Undo { .. }) { ("done", "undone") } else { ("undone", "done") };
            let n = tx.execute("UPDATE undo_log SET state = ?3 WHERE artifact_id = ?1 AND seq = ?2 AND state = ?4", params![c.artifact_id, seq, to, from]).map_err(|e| fin(sql(e)))?;
            if n != 1 { return Err(Fail::New(err(ErrorCode::RevisionConflict, "history changed since the request was evaluated").at(row.current_revision))); }
        }
    }
    store::merge_store_features(&tx, &c.persisted_features).map_err(fin)?;
    let result = ChangeResult { request_id: c.key.request_id.clone(), artifact_id: c.artifact_id.clone(), revision, changed_ids: c.changed_ids.clone(), diagnostics: c.diagnostics.clone(), group: None };
    let mut value = serde_json::to_value(&result).map_err(|e| fin(err(ErrorCode::Internal, e.to_string())))?;
    if let Some(notes) = &c.receipt_notes { value["notes"] = json!(notes); }
    store::insert_receipt(&tx, &c.key, Some(&c.artifact_id), &StoredResult::Committed { result: value }).map_err(fin)?;
    let mut detail = json!({
        "request_id": c.key.request_id, "changed_ids": c.changed_ids, "actor": c.actor,
        "job_id": audit.map(|j| j.job_id.clone()), "attempt_id": audit.map(|j| j.attempt_id.clone()),
    });
    if let Some(undone) = &c.undone_change { detail["undone_change"] = json!(undone); }
    store::insert_event(&tx, Some(&c.artifact_id), Some(revision), c.kind.event(), &detail).map_err(fin)?;
    fire(hook, FaultPoint::TxnBeforeCommit).map_err(fin)?;
    tx.commit().map_err(|e| fin(sql(e)))?;
    base.add_committed(&promoted);
    Ok(result)
}

fn commit_group_inner(
    conn: &mut rusqlite::Connection,
    base: &crate::blobs::AssetBase,
    hook: &crate::blobs::SharedHook,
    key: &RequestKey,
    source_id: &str,
    group_id: &str,
    create_history_group: bool,
    members: &[Candidate],
) -> Result<ChangeResult, Fail> {
    if members.len() < 2 || members.iter().all(|member| member.artifact_id != source_id) {
        return Err(Fail::Final(integrity("a design-system group must include its source and at least one consumer")));
    }
    if members.iter().any(|member| member.key != *key) {
        return Err(Fail::Final(integrity("group members do not share one request receipt")));
    }
    let mut promoted = BTreeMap::<String, AssetRef>::new();
    for member in members {
        for (reference, bytes) in &member.staged {
            if !promoted.contains_key(&reference.sha256) {
                base.blobs.put(bytes).map_err(Fail::Final)?;
                promoted.insert(reference.sha256.clone(), reference.clone());
            }
        }
        match &member.after_bytes {
            Some(bytes) => { base.blobs.put(bytes).map_err(Fail::Final)?; }
            None => {
                if !base.blobs.contains(&member.after) {
                    return Err(Fail::Final(integrity("group history target snapshot blob is missing").ids([member.after.clone()])));
                }
            }
        }
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|error| Fail::Final(sql(error)))?;
    if let Some(found) = store::find_receipt(&tx, key).map_err(Fail::Final)? {
        return match found {
            Ok(value) => serde_json::from_value(value).map_err(|error| Fail::Final(integrity(format!("stored group receipt is unreadable: {error}")))),
            Err(error) => Err(Fail::Final(error)),
        };
    }
    let mut rows = BTreeMap::new();
    for member in members {
        let row = store::artifact_row(&tx, &member.artifact_id).map_err(Fail::New)?;
        if row.current_revision != member.base_revision || row.current_snapshot != member.before {
            return Err(Fail::New(err(
                ErrorCode::RevisionConflict,
                format!("artifact {} changed while the design-system group was being prepared", member.artifact_id),
            ).at(row.current_revision)));
        }
        if let Some(job) = member.job.as_ref() {
            if member.artifact_id != source_id {
                return Err(Fail::Final(integrity("a group consumer cannot carry the source job binding")));
            }
            store::check_job_commit(&tx, job, &member.artifact_id, &member.allowed)
                .map_err(|error| Fail::New(error.at(row.current_revision)))?;
        }
        rows.insert(member.artifact_id.clone(), row);
    }
    if create_history_group {
        tx.execute(
            "INSERT INTO history_groups(group_id, request_id, member_count) VALUES (?1, ?2, ?3)",
            params![group_id, key.request_id, members.len() as i64],
        ).map_err(|error| Fail::Final(sql(error)))?;
    }
    let mut promoted_refs: Vec<AssetRef> = promoted.into_values().collect();
    promoted_refs.sort_by(|left, right| left.sha256.cmp(&right.sha256));
    let fin = |error: DltError| Fail::Final(error);
    store::insert_assets(&tx, &promoted_refs, "design_system_group").map_err(fin)?;
    let mut result_members = Vec::with_capacity(members.len());
    let mut source_result = None;
    let mut member_diagnostics = Vec::new();
    for member in members {
        let row = rows.get(&member.artifact_id).expect("member row was checked above");
        let revision = row.current_revision + 1;
        let audit = member.job.as_ref().or(member.audit_job.as_ref());
        tx.execute(
            "INSERT INTO revisions(artifact_id, revision, snapshot, kind, request_id, actor, job_id, attempt_id, intents, changed_ids) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![member.artifact_id, revision as i64, member.after, member.kind.revision_kind(), key.request_id,
                member.actor, audit.map(|job| job.job_id.as_str()), audit.map(|job| job.attempt_id.as_str()),
                store::to_json(&member.intents).map_err(fin)?, store::to_json(&member.changed_ids).map_err(fin)?],
        ).map_err(|error| fin(sql(error)))?;
        tx.execute(
            "UPDATE artifacts SET current_revision = ?2, current_snapshot = ?3, title = ?4 WHERE artifact_id = ?1",
            params![member.artifact_id, revision as i64, member.after, member.title],
        ).map_err(|error| fin(sql(error)))?;
        store::touch_artifact(&tx, &member.artifact_id).map_err(fin)?;
        if let Some(job) = audit.filter(|_| !member.created_ids.is_empty()) {
            store::add_job_created_ids(&tx, &job.job_id, &member.created_ids).map_err(fin)?;
        }
        match member.kind {
            CommitKind::Change | CommitKind::Revert => {
                tx.execute("UPDATE undo_log SET state = 'discarded' WHERE artifact_id = ?1 AND state = 'undone'", params![member.artifact_id])
                    .map_err(|error| fin(sql(error)))?;
                let seq: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(seq), 0) + 1 FROM undo_log WHERE artifact_id = ?1",
                    params![member.artifact_id], |row| row.get(0),
                ).map_err(|error| fin(sql(error)))?;
                tx.execute(
                    "INSERT INTO undo_log(artifact_id, seq, change_revision, before_snapshot, after_snapshot, intents, changed_ids, state, group_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'done', ?8)",
                    params![member.artifact_id, seq, revision as i64, member.before, member.after,
                        store::to_json(&member.intents).map_err(fin)?, store::to_json(&member.changed_ids).map_err(fin)?, group_id],
                ).map_err(|error| fin(sql(error)))?;
            }
            CommitKind::Undo { seq } | CommitKind::Redo { seq } => {
                if create_history_group {
                    return Err(Fail::Final(integrity("group undo/redo cannot create a new history group")));
                }
                let (from, to) = if matches!(member.kind, CommitKind::Undo { .. }) { ("done", "undone") } else { ("undone", "done") };
                let count = tx.execute(
                    "UPDATE undo_log SET state = ?4 WHERE artifact_id = ?1 AND seq = ?2 AND group_id = ?3 AND state = ?5",
                    params![member.artifact_id, seq, group_id, to, from],
                ).map_err(|error| fin(sql(error)))?;
                if count != 1 {
                    return Err(Fail::New(err(ErrorCode::RevisionConflict, format!("group history changed for artifact {}", member.artifact_id)).at(row.current_revision)));
                }
            }
        }
        store::merge_store_features(&tx, &member.persisted_features).map_err(fin)?;
        let mut detail = json!({
            "request_id": key.request_id, "changed_ids": member.changed_ids, "actor": member.actor,
            "job_id": audit.map(|job| job.job_id.clone()), "attempt_id": audit.map(|job| job.attempt_id.clone()),
            "group_id": group_id,
        });
        if let Some(undone) = &member.undone_change { detail["undone_change"] = json!(undone); }
        // A consumer's own diagnostics (DESIGN_STYLE_ABSENT, say) stay with its event, and the group result carries
        // them too, each naming its artifact in `params.artifact_id`.
        if member.artifact_id != source_id && !member.diagnostics.is_empty() {
            detail["diagnostics"] = json!(member.diagnostics);
            for diagnostic in &member.diagnostics {
                let mut tagged = diagnostic.clone();
                tagged.params.entry("artifact_id".to_owned()).or_insert_with(|| json!(member.artifact_id));
                member_diagnostics.push(tagged);
            }
        }
        store::insert_event(&tx, Some(&member.artifact_id), Some(revision), member.kind.event(), &detail).map_err(fin)?;
        let member_result = ChangeGroupMember { artifact_id: member.artifact_id.clone(), revision, changed_ids: member.changed_ids.clone() };
        if member.artifact_id == source_id {
            source_result = Some((revision, member.changed_ids.clone(), member.diagnostics.clone()));
        }
        result_members.push(member_result);
    }
    let (revision, changed_ids, mut diagnostics) = source_result.ok_or_else(|| Fail::Final(integrity("group source result is missing")))?;
    diagnostics.extend(member_diagnostics);
    let result = ChangeResult {
        request_id: key.request_id.clone(), artifact_id: source_id.to_owned(), revision, changed_ids, diagnostics,
        group: Some(ChangeGroup { group_id: group_id.to_owned(), members: result_members }),
    };
    let mut value = serde_json::to_value(&result).map_err(|error| fin(err(ErrorCode::Internal, error.to_string())))?;
    if let Some(notes) = members.iter().find(|member| member.artifact_id == source_id).and_then(|member| member.receipt_notes.as_ref()) {
        value["notes"] = json!(notes);
    }
    store::insert_receipt(&tx, key, Some(source_id), &StoredResult::Committed { result: value }).map_err(fin)?;
    fire(hook, FaultPoint::TxnBeforeCommit).map_err(fin)?;
    tx.commit().map_err(|error| fin(sql(error)))?;
    base.add_committed(&promoted_refs);
    Ok(result)
}

mod errors { pub use crate::errors::*; }

#[cfg(test)]
mod shared_snapshot_integrity_tests {
    use super::*;

    #[test]
    fn wrapping_shared_snapshot_validation_preserves_diagnostics_and_affected_ids() {
        let diagnostic = Diagnostic {
            code: "TOKEN_VALUE_MISMATCH".into(),
            message: "resolved value differs from the pinned token".into(),
            node_id: Some("title".into()),
            blocking: true,
            params: BTreeMap::new(),
        };
        let mut source = DltError::new(ErrorCode::InvalidArgument, "bad shared snapshot");
        source.affected_ids.push("title".into());
        source.current_revision = Some(7);
        source.retryable = true;
        source.diagnostics.push(diagnostic.clone());

        let wrapped = Workspace::shared_snapshot_integrity_error("consumer", source);
        assert_eq!(wrapped.code, ErrorCode::IntegrityError);
        assert_eq!(wrapped.affected_ids, vec!["consumer", "title"]);
        assert_eq!(wrapped.current_revision, Some(7));
        assert!(wrapped.retryable);
        assert_eq!(wrapped.diagnostics.len(), 1);
        assert_eq!(wrapped.diagnostics[0].code, diagnostic.code);
        assert_eq!(wrapped.diagnostics[0].message, diagnostic.message);
        assert_eq!(wrapped.diagnostics[0].node_id, diagnostic.node_id);
    }
}

#[cfg(test)]
mod repeat_group_target_tests {
    use super::op_targets;
    use designllm_contracts::{ConditionOp, Operation, RepeatSpec, ShowCondition};

    #[test]
    fn repeat_and_condition_operations_reserve_the_group_the_data_table_and_the_component_page() {
        let spec = RepeatSpec { table_id: "table".into(), component_page_id: "card".into(), bindings: Vec::new() };
        let set = Operation::SetGroupRepeat { node_id: "list".into(), expected: None, repeat: Some(spec.clone()) };
        assert_eq!(op_targets(&set), ["list", "table", "card"]);
        // Clearing reserves what the group repeats now (`expected` equals the stored spec).
        let clear = Operation::SetGroupRepeat { node_id: "list".into(), expected: Some(spec.clone()), repeat: None };
        assert_eq!(op_targets(&clear), ["list", "table", "card"]);
        let other = RepeatSpec { table_id: "prices".into(), component_page_id: "row".into(), bindings: Vec::new() };
        let replace = Operation::SetGroupRepeat { node_id: "list".into(), expected: Some(spec), repeat: Some(other) };
        assert_eq!(op_targets(&replace), ["list", "prices", "row", "table", "card"]);
        let condition = ShowCondition { column: 0, op: ConditionOp::IsNotEmpty, value: None };
        let show = Operation::SetGroupCondition { node_id: "badge".into(), expected: None, condition: Some(condition) };
        assert_eq!(op_targets(&show), ["badge"]);
    }
}

#[cfg(test)]
mod local_dependency_scope_tests {
    use super::local_dependency_ids;
    use designllm_contracts::*;
    use serde_json::json;

    #[test]
    fn local_dependency_inventory_includes_canvas_node_style_and_figure_dataset_ids() {
        let canvas_document: CanvasDocument = serde_json::from_value(json!({
            "pages": [{ "id": "page-1", "size": { "width": 100, "height": 100 }, "nodes": [{ "id": "node-1", "kind": "text", "bounds": { "x": 0, "y": 0, "width": 100, "height": 100 }, "text": "x" }] }],
            "styles": { "Body": {}, "Title": {} }
        })).unwrap();
        let canvas = Artifact::new(ArtifactKind::Canvas, "canvas", "test-engine", ArtifactBody::Canvas(canvas_document));
        let canvas_ids = local_dependency_ids(&canvas).unwrap();
        assert!(canvas_ids.contains("page-1"));
        assert!(canvas_ids.contains("node-1"));
        assert!(canvas_ids.contains("style:Body"));
        assert!(canvas_ids.contains("style:Title"));

        let figure_document: FigureDocument = serde_json::from_value(json!({
            "size": { "width": 100, "height": 100 }, "panels": [], "caption": "",
            "datasets": [{ "id": "dataset-1", "version_hash": "v1", "original_asset_hash": "0", "origin": "user_experimental", "columns": [], "rows": [] }],
            "evidence": []
        })).unwrap();
        let figure = Artifact::new(ArtifactKind::Figure, "figure", "test-engine", ArtifactBody::Figure(figure_document));
        let figure_ids = local_dependency_ids(&figure).unwrap();
        assert!(figure_ids.contains("dataset-1"));
    }
}
