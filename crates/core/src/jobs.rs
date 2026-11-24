//! Jobs (CONTRACTS §6, G0_DECISIONS #4) and proposals. Processes are run by host/runtimes; the
//! core owns status, attempts and the commit-time job checks. All transitions run in IMMEDIATE
//! transactions on the single owner connection, the same serialization point as commits.
use crate::errors::{err, integrity, invalid, is_recorded, not_found, sql, ErrExt};
use crate::pipeline::effective_job;
use crate::receipts::{change_key, key_for, reject_duplicate_set};
use crate::store::{self, status_str, StoredResult};

use crate::{parse, to_value, Workspace};
use designllm_contracts::*;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;

const MAX_PROMPT_BYTES: usize = 1 << 20;
const MAX_TARGET_IDS: usize = 1_000;

/// `job.set_client` params (the job comes from the connection's binding).
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct SetClientParams { pub client_name: String, #[serde(default)] pub client_version: Option<String> }

/// Owner of a job's reservation, as TARGET_BUSY messages and targets.list name it.
fn job_owner(job_id: &str, runtime: &str) -> String { format!("job {job_id} ({runtime})") }

#[allow(clippy::too_many_arguments)]
fn reservation_event(tx: &Connection, event: &str, artifact_id: &str, revision: Revision, reservation_id: &str, job_id: &str, owner: &str, ids: &BTreeSet<String>) -> DltResult<()> {
    store::insert_event(tx, Some(artifact_id), Some(revision), event, &json!({ "reservation_id": reservation_id, "job_id": job_id, "owner": owner, "node_ids": ids }))?;
    Ok(())
}

/// Ends the job's reservation: the event inside `tx`; the caller releases it after commit.
fn release_event(views: &crate::views::Views, tx: &Connection, row: &store::JobRow) -> DltResult<bool> {
    let Some((rid, ids)) = views.job_reservation(&row.job_id) else { return Ok(false) };
    let revision = store::artifact_row(tx, &row.artifact_id)?.current_revision;
    reservation_event(tx, "targets.released", &row.artifact_id, revision, &rid, &row.job_id, &job_owner(&row.job_id, &row.request.runtime), &ids)?;
    Ok(true)
}

fn check_target_ids(ids: &[Id]) -> DltResult<()> {
    if ids.len() > MAX_TARGET_IDS { return Err(invalid(format!("target_ids has {} ids; at most {MAX_TARGET_IDS}", ids.len()))); }
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        if id.trim().is_empty() { return Err(invalid("target_ids contains an empty id")); }
        if !seen.insert(id.as_str()) { return Err(invalid(format!("target_ids lists {id} twice")).ids([id.clone()])); }
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ProposalIdParams { pub proposal_id: String }
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ProposalPreviewParams { pub proposal_id: String, #[serde(default)] pub page_index: u32, #[serde(default)] pub profile: RenderProfile }
/// `proposal.create` params; the change set is parsed on its own so its pointers start at /change_set.
#[derive(Debug, Deserialize, Serialize)]
struct ProposalCreateParams { change_set: Value }
#[derive(Debug, Serialize)]
pub(crate) struct ProposalDetail { pub proposal: Proposal, pub artifact_id: String, pub base_revision: Revision, pub error: Option<DltError>, pub diagnostics: Vec<Diagnostic> }

/// The proposal's artifact and the job that made it (None: the user's). A job-bound connection reads, previews and polls only
/// its own job's proposals (83).
pub(crate) fn proposal_owner(conn: &Connection, proposal_id: &str) -> DltResult<(String, Option<String>)> {
    conn.query_row("SELECT artifact_id, job_id FROM proposals WHERE proposal_id = ?1", params![proposal_id], |r| Ok((r.get(0)?, r.get(1)?))).optional().map_err(sql)?
        .ok_or_else(|| not_found(format!("proposal {proposal_id} not found")))
}

struct ProposalRow { id: String, artifact_id: String, base_revision: Revision, change_set: ChangeSet, status: String, error: Option<DltError>, diagnostics: Vec<Diagnostic> }
fn proposal_row(conn: &Connection, proposal_id: &str) -> DltResult<ProposalRow> {
    let row: Option<(String, i64, String, String, Option<String>, String)> = conn.query_row(
        "SELECT artifact_id, base_revision, change_set_json, status, error_json, diagnostics_json FROM proposals WHERE proposal_id = ?1",
        params![proposal_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
    ).optional().map_err(sql)?;
    let (artifact_id, base, cs, status, error, diags) = row.ok_or_else(|| not_found(format!("proposal {proposal_id} not found")))?;
    Ok(ProposalRow {
        id: proposal_id.to_string(), artifact_id, base_revision: base as u64, change_set: store::from_json(&cs, "proposal change set")?, status,
        error: error.map(|e| store::from_json(&e, "proposal error")).transpose()?, diagnostics: store::from_json(&diags, "proposal diagnostics")?,
    })
}
fn status_for(e: &DltError) -> &'static str { if e.code == ErrorCode::RevisionConflict { "conflict" } else { "rejected" } }

impl Workspace {
    fn validate_figure_refs(&self, refs: &[FigureRevisionRef]) -> DltResult<()> {
        let mut seen = BTreeSet::new();
        for reference in refs {
            if reference.figure_id.trim().is_empty() {
                return Err(invalid("figure_refs contains an empty figure_id"));
            }
            if !seen.insert((reference.figure_id.as_str(), reference.figure_revision)) {
                return Err(invalid(format!("figure_refs lists {} revision {} twice", reference.figure_id, reference.figure_revision)).ids([reference.figure_id.clone()]));
            }
            let (row, _, _, _) = self.load_revision(&reference.figure_id, Some(reference.figure_revision))?;
            if row.kind != ArtifactKind::Figure {
                return Err(invalid(format!("{} is not a Figure artifact", reference.figure_id)).ids([reference.figure_id.clone()]));
            }
        }
        Ok(())
    }

    /// The job's target_ids must be ids of the artifact at its current revision. The engine's `inspect` refuses the ones it
    /// does not have (NOT_FOUND). job.start and job.resume both check them here.
    fn check_target_ids_exist(&self, artifact_id: &str, target_ids: &[Id]) -> DltResult<()> {
        let (_, engine, artifact, _) = self.load_revision(artifact_id, None)?;
        self.validate_read_source(&*engine, &artifact)?;
        engine.inspect(&artifact, target_ids)?;
        Ok(())
    }

    /// job.start. With `create` the artifact is made by its engine first and both are committed in
    /// one transaction: a refused or failed start leaves no artifact behind.
    pub(crate) fn job_start(&mut self, params: Value) -> DltResult<Value> {
        reject_duplicate_set(&params, "allowed_intents")?;
        let req: JobStartRequest = parse(params)?;
        for (name, v) in [("runtime", &req.runtime), ("model", &req.model), ("effort", &req.effort)] {
            if v.trim().is_empty() { return Err(invalid(format!("{name} is empty; the runtime must receive an explicit value"))); }
        }
        if req.allowed_intents.is_empty() { return Err(invalid("allowed_intents must not be empty")); }
        if req.prompt.len() > MAX_PROMPT_BYTES { return Err(invalid(format!("prompt exceeds {MAX_PROMPT_BYTES} bytes"))); }
        check_target_ids(&req.target_ids)?;
        if req.figure_refs.len() > MAX_JOB_FIGURE_REFS {
            return Err(invalid(format!("figure_refs has {} pairs; at most {MAX_JOB_FIGURE_REFS}", req.figure_refs.len())));
        }
        if let Some(create) = &req.create {
            if !req.artifact_id.is_empty() || req.base_revision != 0 { return Err(invalid("with create, artifact_id must be empty and base_revision 0; the new artifact's id is in the resulting job")); }
            if !req.target_ids.is_empty() { return Err(invalid("target_ids name nodes of an existing artifact; they cannot be given with create")); }
            if create.actor.trim().is_empty() { return Err(invalid("create.actor is empty")); }
            if create.options.title.chars().count() > 1024 { return Err(invalid("title is longer than 1024 characters")); }
            if create.options.size.is_some_and(|s| !s.valid()) { return Err(invalid("size must be positive and at most 10 m per side")); }
        }
        let key = key_for("job.start", &req.request_id, &req)?;
        if let Some(found) = store::find_receipt(&self.conn, &key)? { return found; }
        if let Err(error) = self.validate_figure_refs(&req.figure_refs) {
            let artifact_id = req.create.is_none().then_some(req.artifact_id.as_str());
            return Err(self.reject(&key, artifact_id, error));
        }
        let missing: Vec<String> = req.source_refs.iter().filter(|a| self.base.get_ref(&a.sha256).is_none()).map(|a| a.sha256.clone()).collect();
        if let Some(create) = &req.create {
            if !missing.is_empty() { return Err(self.reject(&key, None, not_found("source_refs are not assets of this workspace").ids(missing))); }
            let engine = self.engine_by_kind(create.options.kind)?;
            let staging = self.staging();
            let renderer = self.renderer.clone();
            let artifact = engine.create(&create.options, &EngineContext { assets: &staging, renderer: &*renderer })?;
            let new = self.stage_new(&*engine, artifact, create.options.kind, &staging)?;
            return self.insert_job(&req, &key, new.artifact.id.clone(), 0, Some((new, create)));
        }
        let evaluated = (|| -> DltResult<store::ArtifactRow> {
            let row = store::artifact_row(&self.conn, &req.artifact_id)?;
            if req.base_revision != row.current_revision {
                return Err(err(ErrorCode::RevisionConflict, format!("base_revision {} is not the current revision {}", req.base_revision, row.current_revision)).at(row.current_revision));
            }
            if !missing.is_empty() { return Err(not_found("source_refs are not assets of this workspace").ids(missing.clone())); }
            if !req.target_ids.is_empty() { self.check_target_ids_exist(&row.id, &req.target_ids)?; }
            Ok(row)
        })();
        let row = match evaluated { Ok(r) => r, Err(e) => return Err(self.reject(&key, Some(&req.artifact_id), e)) };
        self.insert_job(&req, &key, row.id.clone(), row.current_revision, None)
    }

    /// The job rows, its receipt and events; with `created`, the new artifact's rows in the same
    /// transaction.
    fn insert_job(&mut self, req: &JobStartRequest, key: &crate::receipts::RequestKey, artifact_id: String, base_revision: Revision, created: Option<(crate::pipeline::NewArtifact, &JobCreate)>) -> DltResult<Value> {
        let mut stored = req.clone();
        stored.request_id = key.request_id.clone();
        // The runtime prompt names the artifact: a created one is stored with its id.
        stored.artifact_id = artifact_id.clone();
        let job = Job {
            id: new_id(), attempt_id: new_id(), artifact_id, base_revision, status: JobStatus::Queued,
            allowed_intents: req.allowed_intents.clone(), model: req.model.clone(), effort: req.effort.clone(), error: None,
            runtime: req.runtime.clone(), target_ids: req.target_ids.clone(), client_name: None, deliver_as: req.deliver_as,
            attempt_seq: Some(1),
        };
        let result = to_value(&job)?;
        // A job with target_ids reserves what they hold (TARGET_BUSY, not stored, when someone else holds any of it).
        let targets: BTreeSet<String> = req.target_ids.iter().cloned().collect();
        let reservation = if targets.is_empty() { None } else {
            let (_, _, artifact, _) = self.load_revision(&job.artifact_id, None)?;
            let held = self.holds_in(Some(&job.id), &targets, &artifact)?;
            self.views.check_free(&job.artifact_id, &held, &job.id)?;
            Some((new_id(), job_owner(&job.id, &req.runtime), held))
        };
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
        if let Some(found) = store::find_receipt(&tx, key)? { return found; }
        if !req.figure_refs.is_empty() {
            store::merge_store_features(&tx, &BTreeSet::from([store::JOB_FIGURE_REFS_FEATURE.to_owned()]))?;
        }
        // A build that drops deliver_as would let the job's changes.apply through; this key makes such a build refuse the workspace.
        if req.deliver_as == Some(DeliverAs::Proposal) {
            store::merge_store_features(&tx, &BTreeSet::from([store::JOB_DELIVER_AS_FEATURE.to_owned()]))?;
        }
        if let Some((new, create)) = &created {
            crate::pipeline::insert_new(&tx, new, create.options.kind, key, &create.actor, None)?;
            store::insert_event(&tx, Some(&job.artifact_id), Some(0), "artifact.created",
                &json!({ "request_id": key.request_id, "kind": create.options.kind, "title": new.artifact.title, "diagnostics": new.artifact.diagnostics.len(), "job_id": job.id, "actor": create.actor }))?;
        }
        tx.execute(
            "INSERT INTO jobs(job_id, artifact_id, base_revision, allowed_intents, status, current_attempt, attempt_count, request_json, error_json) VALUES (?1, ?2, ?3, ?4, 'queued', ?5, 1, ?6, NULL)",
            params![job.id, job.artifact_id, job.base_revision as i64, store::to_json(&job.allowed_intents)?, job.attempt_id, store::to_json(&stored)?],
        ).map_err(sql)?;
        tx.execute("INSERT INTO attempts(job_id, attempt_id, seq, state) VALUES (?1, ?2, 1, 'active')", params![job.id, job.attempt_id]).map_err(sql)?;
        store::insert_receipt(&tx, key, Some(&job.artifact_id), &StoredResult::Committed { result: result.clone() })?;
        let mut detail = json!({ "job_id": job.id, "attempt_id": job.attempt_id, "status": "queued", "runtime": req.runtime, "model": req.model, "effort": req.effort });
        if !req.target_ids.is_empty() { detail["target_ids"] = json!(req.target_ids); }
        store::insert_event(&tx, Some(&job.artifact_id), Some(job.base_revision), "job.status", &detail)?;
        if let Some((rid, owner, _)) = &reservation { reservation_event(&tx, "targets.reserved", &job.artifact_id, job.base_revision, rid, &job.id, owner, &targets)?; }
        if created.is_some() { crate::blobs::fire(&self.hook, crate::blobs::FaultPoint::TxnBeforeCommit)?; }
        tx.commit().map_err(sql)?;
        if let Some((new, _)) = &created { self.base.add_committed(&new.promoted); }
        if let Some((rid, owner, held)) = reservation { self.views.reserve_job(rid, &job.artifact_id, &job.id, owner, targets, held); }
        Ok(result)
    }

    /// Reservations of active jobs with target_ids (open: the reservations live in memory). Their holds are computed from
    /// their artifacts. An artifact that cannot be read leaves its jobs on their literal targets; the next commit recomputes them.
    pub(crate) fn restore_job_reservations(&mut self) -> DltResult<()> {
        let mut artifacts = BTreeSet::new();
        for id in store::job_ids_with_status(&self.conn, &["queued", "running", "validating"])? {
            let row = store::job_row(&self.conn, &id)?;
            if row.request.target_ids.is_empty() { continue; }
            let targets: BTreeSet<String> = row.request.target_ids.iter().cloned().collect();
            self.views.reserve_job(new_id(), &row.artifact_id, &row.job_id, job_owner(&row.job_id, &row.request.runtime), targets.clone(), targets);
            artifacts.insert(row.artifact_id.clone());
        }
        for artifact_id in artifacts {
            let _ = self.refresh_holds(&artifact_id);
        }
        Ok(())
    }

    /// job.set_client: the job's own bridge reports its MCP client after `initialize`.
    pub(crate) fn set_job_client(&mut self, p: SetClientParams, bound: Option<&JobRef>) -> DltResult<Value> {
        let job = bound.ok_or_else(|| invalid("job.set_client is called by the job's own MCP connection (a job-bound connection)"))?;
        let name = p.client_name.trim();
        if name.is_empty() || name.chars().count() > 128 || name.chars().any(char::is_control) { return Err(invalid("client_name must be 1..=128 characters without control characters")); }
        if let Some(v) = &p.client_version { if v.chars().count() > 64 || v.chars().any(char::is_control) { return Err(invalid("client_version must be at most 64 characters without control characters")); } }
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
        let row = store::job_row(&tx, &job.job_id)?;
        if row.current_attempt != job.attempt_id { return Err(err(ErrorCode::JobNotActive, format!("attempt {} of job {} was superseded by {}", job.attempt_id, job.job_id, row.current_attempt))); }
        store::set_job_client(&tx, job, name, p.client_version.as_deref())?;
        let revision = store::artifact_row(&tx, &row.artifact_id)?.current_revision;
        store::insert_event(&tx, Some(&row.artifact_id), Some(revision), "job.client", &json!({ "job_id": job.job_id, "attempt_id": job.attempt_id, "client_name": name }))?;
        let out = store::job_row(&tx, &job.job_id)?.job();
        tx.commit().map_err(sql)?;
        to_value(&out)
    }

    /// job.cancel: queued/running/validating/failed/conflict become canceled (no resume); the
    /// active attempt is canceled so its late writes fail with CANCELED. Completed stays.
    pub fn cancel_job(&mut self, job_id: &str) -> DltResult<Job> {
        self.cancel_job_for_attempt(job_id, None)
    }

    pub(crate) fn cancel_job_for_attempt(&mut self, job_id: &str, expected_attempt_id: Option<&str>) -> DltResult<Job> {
        let mut released = false;
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
        let row = store::job_row(&tx, job_id)?;
        if let Some(expected) = expected_attempt_id {
            if expected != row.current_attempt.as_str() {
                return Err(err(
                    ErrorCode::JobNotActive,
                    format!("attempt {expected} of job {job_id} was superseded by {}", row.current_attempt),
                ));
            }
        }
        if matches!(row.status, JobStatus::Queued | JobStatus::Running | JobStatus::Validating | JobStatus::Failed | JobStatus::Conflict) {
            tx.execute("UPDATE jobs SET status = 'canceled' WHERE job_id = ?1", params![job_id]).map_err(sql)?;
            tx.execute("UPDATE attempts SET state = 'canceled' WHERE job_id = ?1 AND state = 'active'", params![job_id]).map_err(sql)?;
            let revision = store::artifact_row(&tx, &row.artifact_id)?.current_revision;
            store::insert_event(&tx, Some(&row.artifact_id), Some(revision), "job.status", &json!({ "job_id": job_id, "attempt_id": row.current_attempt, "status": "canceled", "previous": status_str(row.status) }))?;
            released = release_event(&self.views, &tx, &row)?;
        }
        let job = store::job_row(&tx, job_id)?.job();
        tx.commit().map_err(sql)?;
        if released { self.views.release_job(job_id); }
        Ok(job)
    }

    /// job.resume: failed/conflict only; a new attempt reads the latest revision. Canceled jobs
    /// are not resumed (start a new job).
    pub(crate) fn resume_job(&mut self, job_id: &str) -> DltResult<Job> {
        // The new attempt reserves the job's targets again.
        let pre = store::job_row(&self.conn, job_id)?;
        let targets: BTreeSet<String> = pre.request.target_ids.iter().cloned().collect();
        let reserve = matches!(pre.status, JobStatus::Failed | JobStatus::Conflict) && !targets.is_empty();
        let held = if reserve {
            // A target removed since the job ended is refused here: the job keeps its state and reserves nothing.
            self.check_target_ids_exist(&pre.artifact_id, &pre.request.target_ids)?;
            let (_, _, artifact, _) = self.load_revision(&pre.artifact_id, None)?;
            let held = self.holds_in(Some(job_id), &targets, &artifact)?;
            self.views.check_free(&pre.artifact_id, &held, job_id)?;
            held
        } else { BTreeSet::new() };
        let reservation_id = new_id();
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
        let row = store::job_row(&tx, job_id)?;
        match row.status {
            JobStatus::Failed | JobStatus::Conflict => {}
            JobStatus::Canceled => return Err(err(ErrorCode::Canceled, format!("job {job_id} was canceled; start a new job instead of resuming"))),
            other => return Err(invalid(format!("job {job_id} is {}; only failed or conflict jobs can be resumed", status_str(other)))),
        }
        let attempt = new_id();
        let revision = store::artifact_row(&tx, &row.artifact_id)?.current_revision;
        tx.execute("UPDATE attempts SET state = 'superseded' WHERE job_id = ?1 AND state IN ('active', 'ended')", params![job_id]).map_err(sql)?;
        tx.execute("INSERT INTO attempts(job_id, attempt_id, seq, state) VALUES (?1, ?2, ?3, 'active')", params![job_id, attempt, row.attempt_count + 1]).map_err(sql)?;
        tx.execute(
            "UPDATE jobs SET status = 'queued', current_attempt = ?2, attempt_count = ?3, base_revision = ?4, error_json = NULL WHERE job_id = ?1",
            params![job_id, attempt, row.attempt_count + 1, revision as i64],
        ).map_err(sql)?;
        store::insert_event(&tx, Some(&row.artifact_id), Some(revision), "job.status", &json!({ "job_id": job_id, "attempt_id": attempt, "status": "queued", "resumed_from": row.current_attempt }))?;
        let owner = job_owner(job_id, &row.request.runtime);
        if reserve { reservation_event(&tx, "targets.reserved", &row.artifact_id, revision, &reservation_id, job_id, &owner, &targets)?; }
        let job = store::job_row(&tx, job_id)?.job();
        tx.commit().map_err(sql)?;
        if reserve { self.views.reserve_job(reservation_id, &row.artifact_id, job_id, owner, targets, held); }
        Ok(job)
    }

    /// Runtime callback: queued -> running -> validating -> completed, or failed/conflict/canceled.
    /// Only the current attempt may report; a canceled job never becomes anything else.
    pub fn set_job_status(&mut self, job: &JobRef, status: JobStatus, error: Option<DltError>) -> DltResult<Job> {
        if status == JobStatus::Queued { return Err(invalid("queued is set by job.start/job.resume only")); }
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
        let row = store::job_row(&tx, &job.job_id)?;
        store::attempt_state(&tx, &job.job_id, &job.attempt_id)?.ok_or_else(|| not_found(format!("attempt {} of job {} not found", job.attempt_id, job.job_id)))?;
        if row.current_attempt != job.attempt_id {
            return Err(err(ErrorCode::JobNotActive, format!("attempt {} of job {} was superseded by {}", job.attempt_id, job.job_id, row.current_attempt)));
        }
        use JobStatus::*;
        if row.status == Canceled {
            return if status == Canceled { Ok(row.job()) } else { Err(err(ErrorCode::Canceled, format!("job {} was canceled; status {} is rejected", job.job_id, status_str(status)))) };
        }
        let allowed = matches!((row.status, status),
            (Queued, Running | Failed | Conflict | Canceled)
            | (Running, Running | Validating | Completed | Failed | Conflict | Canceled)
            | (Validating, Running | Validating | Completed | Failed | Conflict | Canceled)
            | (Completed, Completed) | (Failed, Failed) | (Conflict, Conflict));
        if !allowed {
            return Err(err(ErrorCode::JobNotActive, format!("job {} is {}; it cannot become {}", job.job_id, status_str(row.status), status_str(status))));
        }
        if row.status == status && error.is_none() { return Ok(row.job()); }
        tx.execute("UPDATE jobs SET status = ?2, error_json = ?3 WHERE job_id = ?1", params![job.job_id, status_str(status), error.as_ref().map(store::to_json).transpose()?]).map_err(sql)?;
        let attempt_state = match status { Completed | Failed | Conflict => Some("ended"), Canceled => Some("canceled"), _ => None };
        if let Some(s) = attempt_state { tx.execute("UPDATE attempts SET state = ?3 WHERE job_id = ?1 AND attempt_id = ?2", params![job.job_id, job.attempt_id, s]).map_err(sql)?; }
        let revision = store::artifact_row(&tx, &row.artifact_id)?.current_revision;
        store::insert_event(&tx, Some(&row.artifact_id), Some(revision), "job.status", &json!({ "job_id": job.job_id, "attempt_id": job.attempt_id, "status": status_str(status), "error": error }))?;
        // An ended job no longer holds its targets.
        let released = attempt_state.is_some() && release_event(&self.views, &tx, &row)?;
        let out = store::job_row(&tx, &job.job_id)?.job();
        tx.commit().map_err(sql)?;
        if released { self.views.release_job(&job.job_id); }
        Ok(out)
    }

    // ------------------------------------------------------------------ proposals
    pub(crate) fn proposal_create(&mut self, params: Value, bound: Option<&JobRef>) -> DltResult<Value> {
        if params.get("change_set").is_none() { return Err(invalid("proposal.create expects {\"change_set\": ChangeSet}")); }
        let ProposalCreateParams { change_set: raw } = parse(params)?;
        reject_duplicate_set(&raw, "allowed_intents")?;
        let mut cs: ChangeSet = crate::strict::parse_at(&raw, "/change_set")?;
        self.check_change_shape(&cs, "/change_set")?;
        let job = effective_job(bound, cs.job_ref.as_ref(), self.caller.as_deref())?;
        let key = change_key("changes.apply", &cs, job.as_ref())?;
        cs.request_id = key.request_id.clone();
        cs.job_ref = job.clone();
        let existing: Option<(String, String)> = self.conn.query_row("SELECT proposal_id, content_hash FROM proposals WHERE request_id = ?1", params![key.request_id], |r| Ok((r.get(0)?, r.get(1)?))).optional().map_err(sql)?;
        if let Some((pid, hash)) = existing {
            if hash != key.content_hash { return Err(err(ErrorCode::RequestIdMismatch, format!("request_id {} belongs to another proposal", key.request_id))); }
            return to_value(&self.proposal_detail(&pid)?.proposal);
        }
        let (status, error, diagnostics) = match store::find_receipt(&self.conn, &key)? {
            Some(Ok(_)) => ("applied", None, vec![]),
            Some(Err(e)) if e.code == ErrorCode::RequestIdMismatch => return Err(e),
            Some(Err(e)) => (status_for(&e), Some(e), vec![]),
            None => {
                let staging = self.staging();
                match self.evaluate_change(&cs, &key, job.as_ref(), &staging) {
                    Ok(c) => ("validated", None, c.diagnostics),
                    Err(e) if is_recorded(&e.code) => (status_for(&e), Some(e), vec![]),
                    Err(e) => return Err(e),
                }
            }
        };
        let id = new_id();
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
        tx.execute(
            "INSERT INTO proposals(proposal_id, request_id, content_hash, artifact_id, base_revision, change_set_json, status, job_id, attempt_id, error_json, diagnostics_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![id, key.request_id, key.content_hash, cs.artifact_id, cs.base_revision as i64, store::to_json(&cs)?, status,
                    job.as_ref().map(|j| j.job_id.clone()), job.as_ref().map(|j| j.attempt_id.clone()), error.as_ref().map(store::to_json).transpose()?, store::to_json(&diagnostics)?],
        ).map_err(sql)?;
        store::insert_event(&tx, Some(&cs.artifact_id), Some(cs.base_revision), "proposal.created", &json!({ "proposal_id": id, "status": status, "job_id": job.as_ref().map(|j| j.job_id.clone()) }))?;
        tx.commit().map_err(sql)?;
        to_value(&Proposal { id, change_set: cs, status: status.into(), preview: None })
    }

    fn proposal_detail(&self, proposal_id: &str) -> DltResult<ProposalDetail> {
        let p = proposal_row(&self.conn, proposal_id)?;
        Ok(ProposalDetail { proposal: Proposal { id: p.id, change_set: p.change_set, status: p.status, preview: None }, artifact_id: p.artifact_id, base_revision: p.base_revision, error: p.error, diagnostics: p.diagnostics })
    }
    pub(crate) fn proposal_get(&mut self, p: ProposalIdParams) -> DltResult<Value> { to_value(&self.proposal_detail(&p.proposal_id)?) }

    fn set_proposal_status(&mut self, id: &str, status: &str, error: Option<&DltError>) -> DltResult<()> {
        self.conn.execute("UPDATE proposals SET status = ?2, error_json = ?3 WHERE proposal_id = ?1", params![id, status, error.map(store::to_json).transpose()?]).map_err(sql)?;
        Ok(())
    }

    /// Renders the candidate on its own base revision; the committed artifact is untouched and the
    /// snapshot is not registered for hit queries.
    pub(crate) fn proposal_preview(&mut self, p: ProposalPreviewParams, bound: Option<&JobRef>) -> DltResult<Value> {
        let prop = proposal_row(&self.conn, &p.proposal_id)?;
        let row = store::artifact_row(&self.conn, &prop.artifact_id)?;
        if prop.base_revision > row.current_revision { return Err(integrity("proposal base revision is ahead of the artifact")); }
        let base_snapshot = if prop.base_revision == row.current_revision { row.current_snapshot.clone() } else { store::snapshot_at(&self.conn, &row.id, prop.base_revision)? };
        let key = change_key("changes.apply", &prop.change_set, prop.change_set.job_ref.as_ref())?;
        let staging = self.staging();
        let cand = self.evaluate_preview(&prop.change_set, &key, &staging, &row, prop.base_revision, &base_snapshot, bound)?;
        let artifact = cand.candidate.ok_or_else(|| err(ErrorCode::Internal, "candidate missing"))?;
        let engine = self.engine_by_id(&row.engine_id)?;
        let renderer = self.renderer.clone();
        let scene = engine.layout(&artifact, &EngineContext { assets: &staging, renderer: &*renderer })?;
        crate::routes::check_engine_scene_links(&scene, engine.id())?;
        let mut snapshot = self.encode_page(&scene, &staging, &row.id, prop.base_revision, p.page_index, &p.profile, None, None)?;
        snapshot.scene.metadata.insert("proposal_id".into(), json!(prop.id));
        snapshot.scene.metadata.insert("candidate".into(), json!(true));
        snapshot.scene.metadata.insert("base_revision".into(), json!(prop.base_revision));
        if prop.base_revision != row.current_revision && prop.status == "validated" {
            let e = err(ErrorCode::RevisionConflict, format!("artifact moved to revision {} after the proposal", row.current_revision)).at(row.current_revision);
            self.set_proposal_status(&prop.id, "conflict", Some(&e))?;
        }
        let status = proposal_row(&self.conn, &prop.id)?.status;
        to_value(&Proposal { id: prop.id, change_set: prop.change_set, status, preview: Some(snapshot) })
    }

    /// Applies through the normal change pipeline. A job's proposal applied by the user keeps the
    /// job only as audit (the user's approval is the write); a canceled job's proposal is refused.
    pub(crate) fn proposal_apply(&mut self, p: ProposalIdParams) -> DltResult<Value> {
        let prop = proposal_row(&self.conn, &p.proposal_id)?;
        let audit_job = prop.change_set.job_ref.clone();
        let job_id = audit_job.as_ref().map(|j| j.job_id.clone());
        let mut cs = prop.change_set.clone();
        cs.job_ref = None;
        let key = key_for("proposal.apply", &cs.request_id, &json!({ "proposal_id": prop.id, "change_set": prop.change_set }))?;
        // Replays return the stored outcome (before any job-state check, no second event).
        if let Some(found) = store::find_receipt(&self.conn, &key)? { return found; }
        if let Some(j) = &audit_job {
            let job = store::job_row(&self.conn, &j.job_id)?;
            if job.status == JobStatus::Canceled { return Err(err(ErrorCode::Canceled, format!("proposal {} belongs to canceled job {}", prop.id, j.job_id))); }
        }
        match self.apply_keyed(&cs, key, None, audit_job) {
            Ok(result) => {
                self.set_proposal_status(&prop.id, "applied", None)?;
                let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
                store::insert_event(&tx, Some(&prop.artifact_id), Some(result.revision), "proposal.applied", &json!({ "proposal_id": prop.id, "request_id": result.request_id, "job_id": job_id }))?;
                tx.commit().map_err(sql)?;
                to_value(&result)
            }
            Err(e) => {
                if is_recorded(&e.code) { self.set_proposal_status(&prop.id, status_for(&e), Some(&e))?; }
                Err(e)
            }
        }
    }
}




