//! figure.dataset.edit: exact data edits computed by the registered figures editor (decimal
//! parsing, derived version hash, transform history), applied as ONE ordinary change set
//! (PutDataset + SetPlot rebinding plots of this figure) through the normal intent, revision,
//! job, reservation, engine-digest and receipt gates. Other figures are never touched.
use crate::errors::{err, invalid, not_found, unsupported};
use crate::pipeline::effective_job;
use crate::receipts::{key_for, reject_duplicate_set, RequestKey};
use crate::{parse, store, to_value, Workspace};
use designllm_contracts::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

pub type DatasetEditor = fn(&Dataset, &[DataTransform]) -> DltResult<Dataset>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DatasetEditRequest {
    pub request_id: Id,
    pub actor: String,
    pub workspace_id: Id,
    pub artifact_id: Id,
    pub base_revision: Revision,
    pub allowed_intents: BTreeSet<Intent>,
    #[serde(default)]
    pub job_ref: Option<JobRef>,
    pub dataset_id: Id,
    pub version_hash: String,
    pub edits: Vec<DataTransform>,
}

impl Workspace {
    /// Root wires designllm_figures::edit_dataset; core does not parse numbers itself.
    pub fn set_dataset_editor(&mut self, editor: DatasetEditor) { self.dataset_editor = Some(editor); }

    pub(crate) fn dataset_edit(&mut self, params: Value, bound: Option<&JobRef>) -> DltResult<Value> {
        reject_duplicate_set(&params, "allowed_intents")?;
        let mut req: DatasetEditRequest = parse(params)?;
        if req.workspace_id != self.id { return Err(invalid(format!("workspace_id {:?} does not match this workspace", req.workspace_id))); }
        if req.allowed_intents.is_empty() { return Err(invalid("allowed_intents must not be empty")); }
        if req.edits.is_empty() { return Err(invalid("edits must not be empty")); }
        // Configuration, not request content: never stored as a receipt.
        let editor = self.dataset_editor.ok_or_else(|| unsupported("no dataset editor is registered in this build"))?;
        let job = effective_job(bound, req.job_ref.as_ref(), self.caller.as_deref())?;
        req.job_ref = job.clone();
        let key = key_for("figure.dataset.edit", &req.request_id, &req)?;
        if let Some(found) = store::find_receipt(&self.conn, &key)? { return found; }
        let change = match self.derive_dataset_change(&req, editor) {
            Ok(cs) => cs,
            Err(e) => return Err(self.reject(&key, Some(&req.artifact_id), e)),
        };
        to_value(&self.apply_derived(&change, key, job)?)
    }

    fn derive_dataset_change(&self, req: &DatasetEditRequest, editor: DatasetEditor) -> DltResult<ChangeSet> {
        let row = store::artifact_row(&self.conn, &req.artifact_id)?;
        if row.kind != ArtifactKind::Figure { return Err(invalid(format!("{} is not a figure artifact", row.id))); }
        if req.base_revision != row.current_revision {
            let mut e = err(ErrorCode::RevisionConflict, format!("base_revision {} is not the current revision {}", req.base_revision, row.current_revision));
            e.current_revision = Some(row.current_revision);
            return Err(e);
        }
        // Cell, table-structure, sort and unit-scale edits are checked by the registered editor against this version. Row
        // selection is a derived view and stays outside this command.
        if let Some(other) = req.edits.iter().find(|t| matches!(t, DataTransform::SelectRows { .. })) {
            return Err(unsupported(format!("figure.dataset.edit takes edit_cell, add_column, add_rows, remove_row, remove_column, sort and unit_scale, not {other:?}")));
        }
        let engine = self.engine_by_id(&row.engine_id)?;
        let artifact = self.load(&*engine, &row.current_snapshot, row.current_revision)?;
        let ArtifactBody::Figure(doc) = &artifact.body else { return Err(invalid("figure artifact without figure body")) };
        let base = doc.datasets.iter().find(|d| d.id == req.dataset_id).ok_or_else(|| not_found(format!("dataset {} not found in figure {}", req.dataset_id, row.id)))?;
        if base.version_hash != req.version_hash {
            let mut e = err(ErrorCode::RevisionConflict, format!("dataset {} is at version {}, the edit was made on {}", base.id, base.version_hash, req.version_hash));
            e.current_revision = Some(row.current_revision);
            e.affected_ids.push(base.id.clone());
            return Err(e);
        }
        let derived = editor(base, &req.edits)?;
        if derived.version_hash.trim().is_empty() { return Err(err(ErrorCode::Internal, "dataset editor returned an empty version_hash")); }
        let old_ref = DatasetRef { dataset_id: base.id.clone(), version_hash: base.version_hash.clone() };
        let new_ref = DatasetRef { dataset_id: derived.id.clone(), version_hash: derived.version_hash.clone() };
        let mut operations = vec![Operation::PutDataset { dataset: derived }];
        for panel in &doc.panels {
            if let PanelBody::Plot(plot) = &panel.body {
                if plot.dataset_ref == old_ref {
                    let mut rebound = plot.clone();
                    rebound.dataset_ref = new_ref.clone();
                    operations.push(Operation::SetPlot { panel_id: panel.id.clone(), plot: rebound });
                }
            }
        }
        Ok(ChangeSet {
            request_id: req.request_id.clone(), actor: req.actor.clone(), workspace_id: req.workspace_id.clone(), artifact_id: req.artifact_id.clone(),
            base_revision: req.base_revision, allowed_intents: req.allowed_intents.clone(), job_ref: req.job_ref.clone(), read_dependencies: vec![], operations,
        })
    }

    /// Ordinary evaluation and commit of a core-derived change set under the caller's key.
    fn apply_derived(&mut self, cs: &ChangeSet, key: RequestKey, job: Option<JobRef>) -> DltResult<ChangeResult> {
        let staging = self.staging();
        let cand = match self.evaluate_change(cs, &key, job.as_ref(), &staging) {
            Ok(c) => c,
            Err(e) => return Err(self.reject(&key, Some(&cs.artifact_id), e)),
        };
        self.commit(cand)
    }
}
