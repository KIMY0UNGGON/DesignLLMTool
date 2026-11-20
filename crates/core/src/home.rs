//! Home screen reads (P1): the connections the host reports (`clients.list`) and template previews
//! rendered without creating an artifact (`templates.list`, `templates.preview`).
use crate::errors::{err, invalid, not_found, unsupported};
use crate::{store, to_value, Workspace};
use designllm_contracts::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

/// One host connection as the host reports it: after its `system.hello`, and again whenever it
/// learns more (an MCP client's name). The host removes it with `client_disconnected`.
#[derive(Debug, Clone)]
pub struct ClientConnection {
    pub connection_id: String,
    /// gui, bridge or cli.
    pub role: String,
    pub job: Option<JobRef>,
    pub client_name: Option<String>,
    pub client_version: Option<String>,
}

/// Params of methods that take none (unknown keys are refused).
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct NoParams {}

/// `templates.preview` params (contracts `TemplatePreviewRequest`).
pub(crate) type PreviewParams = TemplatePreviewRequest;

/// Connections by id, with the time they were first reported (Unix ms).
pub(crate) type Clients = BTreeMap<String, (ClientConnection, u64)>;
/// (kind, template id, create size (None = default), dpi): one preview recipe.
pub(crate) type PreviewKey = (ArtifactKind, String, Option<(i64, i64)>, u32);
/// Most recipes and most PNGs held at once.
pub(crate) const TEMPLATE_PREVIEW_CACHE: usize = 32;
/// PNG bytes held at once.
pub(crate) const TEMPLATE_PREVIEW_CACHE_BYTES: usize = 32 << 20;
/// Largest PNG of a template preview: the bridge inlines a preview up to 8 MiB (bridge mcp.rs INLINE_IMAGE_LIMIT).
pub(crate) const TEMPLATE_PREVIEW_BYTES: usize = 8 << 20;
/// Largest raster of a template preview, in pixels. Checked after layout, before the page is rendered.
pub(crate) const TEMPLATE_PREVIEW_PIXELS: u64 = 4096 * 4096;
/// Template previews one job attempt may hold; another one is refused.
pub(crate) const JOB_TEMPLATE_PREVIEWS: usize = 16;

/// Template previews held in memory, as render previews are: `asset.read` and `asset.lookup` serve them, and nothing is
/// stored in `assets`. A recipe is answered while its PNG is held. Past the bounds the least recently used PNG goes
/// first, with its recipes.
#[derive(Default)]
pub(crate) struct TemplatePreviewCache {
    recipes: BTreeMap<PreviewKey, (AssetRef, Size)>,
    /// sha256 -> (mime, PNG bytes).
    pngs: BTreeMap<String, (String, Arc<Vec<u8>>)>,
    /// sha256s of `pngs`, least recently used first.
    order: VecDeque<String>,
    bytes: usize,
}

impl TemplatePreviewCache {
    /// The recipe's asset and page size, while its PNG is held. A hit makes that PNG the most recent.
    fn recipe(&mut self, key: &PreviewKey) -> Option<(AssetRef, Size)> {
        let (asset, page) = self.recipes.get(key)?.clone();
        if !self.pngs.contains_key(&asset.sha256) { return None; }
        self.touch(&asset.sha256);
        Some((asset, page))
    }
    /// The PNG held under `sha256`, with its mime type. A hit makes it the most recent.
    pub(crate) fn png(&mut self, sha256: &str) -> Option<(String, Arc<Vec<u8>>)> {
        let found = self.pngs.get(sha256)?.clone();
        self.touch(sha256);
        Some(found)
    }
    fn touch(&mut self, sha256: &str) {
        if let Some(at) = self.order.iter().position(|held| held == sha256) { self.order.remove(at); }
        self.order.push_back(sha256.to_string());
    }
    fn insert(&mut self, key: PreviewKey, asset: AssetRef, page: Size, mime: String, bytes: Vec<u8>) {
        if !self.pngs.contains_key(&asset.sha256) {
            self.bytes += bytes.len();
            self.pngs.insert(asset.sha256.clone(), (mime, Arc::new(bytes)));
        }
        if self.recipes.len() >= TEMPLATE_PREVIEW_CACHE && !self.recipes.contains_key(&key) {
            // The recipe whose PNG was used least recently (every recipe's PNG is held, so it is in `order`).
            let oldest = self.order.iter().find_map(|sha| self.recipes.iter().find(|(_, (held, _))| &held.sha256 == sha).map(|(old, _)| old.clone()));
            if let Some(oldest) = oldest { self.recipes.remove(&oldest); }
        }
        self.recipes.insert(key, (asset.clone(), page));
        self.touch(&asset.sha256);
        while self.order.len() > TEMPLATE_PREVIEW_CACHE || self.bytes > TEMPLATE_PREVIEW_CACHE_BYTES {
            let Some(old) = self.order.pop_front() else { break };
            if let Some((_, data)) = self.pngs.remove(&old) { self.bytes -= data.len(); }
            self.recipes.retain(|_, (held, _)| held.sha256 != old);
        }
    }
}
/// (job id, attempt id) -> the template preview assets that attempt asked for, each with the recipes that gave it.
pub(crate) type JobPreviews = BTreeMap<(String, String), BTreeMap<String, BTreeSet<PreviewKey>>>;
/// Recipes remembered for one job attempt beyond the first of each asset (several recipes can give one PNG).
const JOB_TEMPLATE_PREVIEW_EXTRA_RECIPES: usize = 2 * JOB_TEMPLATE_PREVIEWS;

fn previews_full(job: &JobRef) -> DltError {
    invalid(format!("job {} already holds {JOB_TEMPLATE_PREVIEWS} template previews; another one is refused", job.job_id))
}

fn now_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

impl Workspace {
    /// Host API: a connection was accepted (or the host learned its client name). The first report
    /// sets `connected_at_ms`.
    pub fn client_connected(&mut self, connection: ClientConnection) {
        let at = self.clients.get(&connection.connection_id).map(|(_, at)| *at).unwrap_or_else(now_ms);
        self.clients.insert(connection.connection_id.clone(), (connection, at));
    }
    /// Host API: the connection closed.
    pub fn client_disconnected(&mut self, connection_id: &str) { self.clients.remove(connection_id); }

    /// Every connection for the app and other clients; for a job connection only the names.
    pub(crate) fn clients_list(&self, job: Option<&JobRef>) -> DltResult<Value> {
        let mut rows: Vec<(&ClientConnection, u64)> = self.clients.values().map(|(c, at)| (c, *at)).collect();
        rows.sort_by(|a, b| (a.1, &a.0.connection_id).cmp(&(b.1, &b.0.connection_id)));
        let mut out = Vec::new();
        for (conn, at) in rows {
            let job_row = conn.job.as_ref().and_then(|j| store::job_row(&self.conn, &j.job_id).ok());
            let name = conn.client_name.clone().or_else(|| job_row.as_ref().and_then(|r| r.client_name.clone()));
            if job.is_some() {
                if name.is_some() { out.push(ClientInfo { connection_id: None, role: None, client_name: name, client_version: None, job_id: None, runtime: None, connected_at_ms: None }); }
                continue;
            }
            out.push(ClientInfo {
                connection_id: Some(conn.connection_id.clone()), role: Some(conn.role.clone()), client_name: name, client_version: conn.client_version.clone(),
                job_id: conn.job.as_ref().map(|j| j.job_id.clone()), runtime: job_row.map(|r| r.request.runtime), connected_at_ms: Some(at),
            });
        }
        Ok(json!({ "clients": out }))
    }

    /// Templates of the registered engines, each listed by the engine that owns its kind.
    fn templates(&self) -> Vec<(usize, TemplateInfo)> {
        let mut all: Vec<(usize, TemplateInfo)> = self.engines.iter().enumerate()
            .flat_map(|(i, e)| e.templates().into_iter().map(move |t| (i, t)))
            .filter(|(i, t)| self.by_kind.get(&t.kind) == Some(i)).collect();
        all.sort_by(|a, b| (a.1.kind, &a.1.id).cmp(&(b.1.kind, &b.1.id)));
        all
    }
    pub(crate) fn templates_list(&self) -> DltResult<Value> {
        Ok(json!({ "templates": self.templates().into_iter().map(|(_, t)| t).collect::<Vec<_>>() }))
    }

    /// Page 0 of a template as a PNG asset, without an artifact: the engine creates it in memory
    /// (at the chosen preset's size, the given size, or its default size), lays it out and the
    /// renderer encodes it. Held in memory per template, size and dpi (TemplatePreviewCache), so the
    /// same request gives the same asset.
    pub(crate) fn templates_preview(&mut self, p: PreviewParams, job: Option<&JobRef>) -> DltResult<Value> {
        self.prune_job_previews();
        // A job's call needs its active attempt (the check its writes use); the preview then joins that attempt's asset scope.
        let asker = match job {
            Some(job) => {
                let artifact_id = store::job_row(&self.conn, &job.job_id)?.artifact_id;
                store::check_job_write(&self.conn, job, &artifact_id, &BTreeSet::new())?;
                Some(job.clone())
            }
            None => None,
        };
        if p.dpi == 0 || p.dpi > 9_600 { return Err(invalid("dpi must be 1..=9600")); }
        if p.preset.is_some() && p.size.is_some() { return Err(invalid("pass preset or size, not both")); }
        if p.size.is_some_and(|s| !s.valid()) { return Err(invalid("size must be positive and at most 10 m per side")); }
        // A responsive template is listed once per preset: same id, one entry per size.
        let found: Vec<(usize, TemplateInfo)> = self.templates().into_iter().filter(|(_, t)| t.id == p.template && p.kind.is_none_or(|k| k == t.kind)).collect();
        if found.is_empty() { return Err(not_found(format!("no registered engine has template {:?}; templates.list names them", p.template))); }
        if found.iter().any(|(_, t)| t.kind != found[0].1.kind) { return Err(invalid(format!("template {:?} exists for several kinds; pass kind", p.template))); }
        let (index, info, size) = match (&p.preset, p.size) {
            (Some(preset), _) => {
                let (i, t) = found.into_iter().find(|(_, t)| t.preset.as_deref() == Some(preset.as_str()))
                    .ok_or_else(|| not_found(format!("template {:?} has no preset {preset:?}; templates.list names its presets", p.template)))?;
                let size = t.size;
                (i, t, size)
            }
            (None, size) => { let (i, t) = found.into_iter().next().expect("not empty"); (i, t, size) }
        };
        let key = (info.kind, info.id.clone(), size.map(|s| (s.width, s.height)), p.dpi);
        let result = |asset: &AssetRef, page: Size| to_value(&TemplatePreview { kind: info.kind, template: info.id.clone(), preset: p.preset.clone(), dpi: p.dpi, asset: asset.clone(), page_size: page });
        if let Some((asset, page)) = self.template_previews.recipe(&key) {
            self.admit_job_preview(asker.as_ref(), &asset.sha256)?;
            self.remember_job_preview(asker.as_ref(), &asset.sha256, &key);
            return result(&asset, page);
        }
        // A job with no room is refused here, before the render, unless it asked for this recipe before.
        self.admit_job_recipe(asker.as_ref(), &key)?;
        let engine = self.engines[index].clone();
        let staging = self.staging();
        let renderer = self.renderer.clone();
        let ctx = EngineContext { assets: &staging, renderer: &*renderer };
        let artifact = engine.create(&CreateRequest { kind: info.kind, title: info.title.clone(), size, template: Some(info.id.clone()) }, &ctx)?;
        let scene = engine.layout(&artifact, &ctx)?;
        crate::routes::check_engine_scene_links(&scene, engine.id())?;
        let page = scene.pages.iter().min_by_key(|page| page.page_index).cloned().ok_or_else(|| unsupported(format!("template {} has no page to preview", info.id)))?;
        let links = scene.links.iter().filter(|link| link.page_id == page.id).cloned().collect();
        let size = page.size;
        let (wide, high) = (pixels(size.width, p.dpi)?, pixels(size.height, p.dpi)?);
        if u64::from(wide) * u64::from(high) > TEMPLATE_PREVIEW_PIXELS { return Err(invalid(format!("the preview would be {wide} x {high} pixels, over the {TEMPLATE_PREVIEW_PIXELS}-pixel limit; choose a smaller size or a lower dpi"))); }
        let one = RenderScene { pages: vec![page], font_fingerprint: scene.font_fingerprint.clone(), diagnostics: scene.diagnostics.clone(), metadata: scene.metadata.clone(), links };
        let out = renderer.encode(&one, "png", &RenderProfile { dpi: p.dpi, transparent: false }, &staging)?;
        if out.mime != "image/png" || !out.bytes.starts_with(&media_header::PNG_SIGNATURE) { return Err(err(ErrorCode::Internal, format!("renderer returned {} instead of PNG for a template preview", out.mime))); }
        if out.bytes.len() > TEMPLATE_PREVIEW_BYTES { return Err(invalid(format!("the preview PNG is {} bytes, over the {TEMPLATE_PREVIEW_BYTES}-byte limit", out.bytes.len()))); }
        let asset = AssetRef { sha256: sha256(&out.bytes), mime: out.mime.clone(), bytes: out.bytes.len() as u64 };
        self.admit_job_preview(asker.as_ref(), &asset.sha256)?;
        self.remember_job_preview(asker.as_ref(), &asset.sha256, &key);
        self.template_previews.insert(key, asset.clone(), size, out.mime, out.bytes);
        result(&asset, size)
    }

    /// A job may hold `JOB_TEMPLATE_PREVIEWS` previews; another one is refused before it is stored.
    fn admit_job_preview(&self, asker: Option<&JobRef>, sha256: &str) -> DltResult<()> {
        let Some(job) = asker else { return Ok(()) };
        let held = self.job_template_previews.get(&(job.job_id.clone(), job.attempt_id.clone()));
        if held.is_some_and(|set| set.contains_key(sha256)) || held.map_or(0, BTreeMap::len) < JOB_TEMPLATE_PREVIEWS { return Ok(()); }
        Err(previews_full(job))
    }

    /// The check of `admit_job_preview` before the render, while the asset is not known: a job with no room may ask again
    /// only for a recipe that gave one of its previews (that PNG may have left the cache meanwhile).
    fn admit_job_recipe(&self, asker: Option<&JobRef>, key: &PreviewKey) -> DltResult<()> {
        let Some(job) = asker else { return Ok(()) };
        let held = self.job_template_previews.get(&(job.job_id.clone(), job.attempt_id.clone()));
        if held.is_some_and(|set| set.values().any(|asked| asked.contains(key))) || held.map_or(0, BTreeMap::len) < JOB_TEMPLATE_PREVIEWS { return Ok(()); }
        Err(previews_full(job))
    }

    /// The preview joins the asset scope of the job attempt that asked for it (routes.rs `job_asset_scope_as`), with its
    /// recipe: the first recipe of each asset, and further ones up to `JOB_TEMPLATE_PREVIEW_EXTRA_RECIPES`.
    fn remember_job_preview(&mut self, asker: Option<&JobRef>, sha256: &str, key: &PreviewKey) {
        let Some(job) = asker else { return };
        let held = self.job_template_previews.entry((job.job_id.clone(), job.attempt_id.clone())).or_default();
        let extra = held.values().map(|recipes| recipes.len().saturating_sub(1)).sum::<usize>();
        let recipes = held.entry(sha256.to_string()).or_default();
        if recipes.is_empty() || extra < JOB_TEMPLATE_PREVIEW_EXTRA_RECIPES { recipes.insert(key.clone()); }
    }

    /// Drops the entries of attempts that are no longer active: ended, canceled or superseded. Runs before each template
    /// preview request and after each `job.cancel`. Status changes made by the runner (jobs.rs) have no hook here, so
    /// their entries go at the next request.
    pub(crate) fn prune_job_previews(&mut self) {
        let attempts: Vec<(String, String)> = self.job_template_previews.keys().cloned().collect();
        for (job_id, attempt_id) in attempts {
            let live = store::job_row(&self.conn, &job_id).is_ok_and(|row| {
                let attempt = JobRef { job_id: job_id.clone(), attempt_id: attempt_id.clone() };
                store::check_job_write(&self.conn, &attempt, &row.artifact_id, &BTreeSet::new()).is_ok()
            });
            if !live { self.job_template_previews.remove(&(job_id, attempt_id)); }
        }
    }

    /// Number of attempts that hold template previews (the cleanup tests read it).
    #[doc(hidden)]
    pub fn job_template_preview_entries(&self) -> usize { self.job_template_previews.len() }
}

impl Workspace {
    /// `flow.styles`: the engine's style list of the current revision (read-only, not in inspect).
    pub(crate) fn flow_styles(&mut self, p: crate::routes::ArtifactIdParams) -> DltResult<Value> {
        let (row, engine, artifact, _) = self.load_revision(&p.artifact_id, None)?;
        let staging = self.staging();
        let renderer = self.renderer.clone();
        let styles = engine.flow_styles(&artifact, &EngineContext { assets: &staging, renderer: &*renderer })?;
        Ok(json!({ "artifact_id": row.id, "revision": row.current_revision, "styles": styles }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(dpi: u32) -> PreviewKey { (ArtifactKind::Canvas, "t".into(), None, dpi) }
    fn put(cache: &mut TemplatePreviewCache, dpi: u32) {
        let asset = AssetRef { sha256: format!("{dpi:064x}"), mime: "image/png".into(), bytes: 1 };
        cache.insert(key(dpi), asset, Size::slide(), "image/png".into(), vec![0]);
    }

    #[test]
    fn a_full_recipe_table_drops_its_least_recently_used_recipe() {
        let mut cache = TemplatePreviewCache::default();
        for dpi in 0..TEMPLATE_PREVIEW_CACHE as u32 { put(&mut cache, dpi); }
        assert!(cache.recipe(&key(0)).is_some(), "the first recipe is now the most recently used");
        put(&mut cache, 100);
        assert!(cache.recipe(&key(0)).is_some(), "the recipe used last stays");
        assert!(cache.recipe(&key(1)).is_none(), "the least recently used recipe goes");
        assert!(cache.recipe(&key(100)).is_some());
        assert_eq!(cache.recipes.len(), TEMPLATE_PREVIEW_CACHE);
    }
}
