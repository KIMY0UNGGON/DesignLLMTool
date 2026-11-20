//! Home "create + request" as one user action (GUI_DESIGN §2, §16), client side: the desktop window
//! and the application launcher share it. With a request the attachments are imported, then
//! `job.start` with `create` makes the artifact and queues its job in one core commit, so a refused
//! request leaves no empty artifact (imported attachments stay as workspace assets only). Without a
//! request it is a plain `artifact.create`.
use crate::client::Client;
use designllm_contracts::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// One RPC call: the host [`Client`] in the product, `Workspace::dispatch` in tests.
pub trait Rpc {
    fn call(&self, method: &str, params: Value) -> DltResult<Value>;
}
impl Rpc for Client {
    fn call(&self, method: &str, params: Value) -> DltResult<Value> { Client::call(self, method, params) }
}

/// The home request box: what to create, and the request for the AI.
#[derive(Debug, Clone)]
pub struct HomeRequest {
    pub actor: String,
    pub create: CreateRequest,
    /// Empty: only create (no job).
    pub prompt: String,
    pub runtime: String,
    pub model: String,
    pub effort: String,
    pub allowed_intents: BTreeSet<Intent>,
    /// Files attached to the request (absolute paths); they become the job's sources.
    pub attachments: Vec<PathBuf>,
    /// Explicit Figure identity and revision approvals for the created job.
    pub figure_refs: Vec<FigureRevisionRef>,
}
#[derive(Debug, Clone)]
pub struct HomeOutcome { pub artifact: ArtifactSummary, pub job: Option<Job>, pub sources: Vec<AssetRef> }

fn typed<T: serde::de::DeserializeOwned>(value: Value) -> DltResult<T> {
    serde_json::from_value(value).map_err(|e| DltError::new(ErrorCode::Internal, format!("unexpected host result: {e}")))
}
fn params<T: serde::Serialize>(value: &T) -> DltResult<Value> { serde_json::to_value(value).map_err(|e| DltError::invalid(e.to_string())) }

/// Home "create" as one user action; see the module doc.
pub fn create_from_home(rpc: &dyn Rpc, request: &HomeRequest) -> DltResult<HomeOutcome> {
    if request.prompt.trim().is_empty() {
        if !request.attachments.is_empty() || !request.figure_refs.is_empty() {
            return Err(DltError::invalid("attachments and figure_refs are sources of an AI request; write the request or remove them"));
        }
        let create = CreateArtifactRequest { request_id: new_id(), actor: request.actor.clone(), options: request.create.clone() };
        let artifact: ArtifactSummary = typed(rpc.call("artifact.create", params(&create)?)?)?;
        return Ok(HomeOutcome { artifact, job: None, sources: vec![] });
    }
    let mut sources = Vec::new();
    for path in &request.attachments {
        sources.push(typed::<AssetRef>(rpc.call("asset.import", json!({ "path": path.to_string_lossy() }))?)?);
    }
    let start = JobStartRequest {
        source_refs: sources.clone(), figure_refs: request.figure_refs.clone(), create: Some(JobCreate { actor: request.actor.clone(), options: request.create.clone() }),
        ..JobStartRequest::new(new_id(), String::new(), 0, request.allowed_intents.clone(), request.runtime.clone(), request.model.clone(), request.effort.clone(), request.prompt.clone())
    };
    let job: Job = typed(rpc.call("job.start", params(&start)?)?)?;
    let list = rpc.call("workspace.list", json!({}))?;
    let artifact = list["artifacts"].as_array().into_iter().flatten().find(|a| a["id"] == json!(job.artifact_id)).cloned()
        .ok_or_else(|| DltError::new(ErrorCode::Internal, format!("job {} names artifact {}, which the workspace does not list", job.id, job.artifact_id)))?;
    Ok(HomeOutcome { artifact: typed(artifact)?, job: Some(job), sources })
}
