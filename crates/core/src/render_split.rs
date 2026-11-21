//! `render.get` and `render.pixels` in three steps, so that a host draws a page without holding the workspace (68i), as it
//! reads a PDF (`pdf_read.rs`).
//!
//! [`Workspace::dispatch_start`] checks a render request under the workspace (profile, revision, the snapshot cached for the
//! same key), lays the revision out (cached per revision) and returns a [`RenderJob`]. The job holds the laid-out scene, its
//! assets and the renderer, not the workspace. The caller releases the workspace, runs [`RenderJob::run`], and hands the
//! outcome to [`Workspace::finish_render`], which caches the snapshot as `render.get` always did. A render changes no
//! document state, so other requests run meanwhile.
//!
//! `render.pixels` is the desktop window's own preview ([`crate::CLIENT_METHODS`]; the host serves it to the gui connection
//! only). It answers what `render.get` answers, without a PNG preview, plus `pixels` (`{width, height, format, bytes}`), and
//! the job hands the raw pixels ([`RasterPixels`]) to the caller, which sends them as binary frames. Its snapshots are cached
//! apart from those of `render.get` ([`SnapshotKey::pixels`]), so an agent's `render.get` of the same page still gets a PNG
//! preview. The raw pixels of the last snapshots are held too (`RenderCache::put_pixels`, by bytes): a repeated
//! `render.pixels` answers the cached snapshot with them, or with the page drawn again once they are gone.

use crate::errors::{err, invalid, ErrExt};
use crate::routes::{check_profile, check_region, links_for_page};
use crate::views::{SceneEntry, SnapshotKey};
use crate::{files, object_params, parse, respond, store, to_value, Dispatch, Workspace, RESPONSE_BUDGET};
use designllm_contracts::*;
use serde_json::{json, Value};
use std::sync::Arc;

pub(crate) const PIXELS: &str = "render.pixels";
const GET: &str = "render.get";

/// One checked render request. It holds the laid-out scene and the renderer, not the workspace.
pub struct RenderJob {
    id: String,
    pixels: bool,
    artifact_id: String,
    revision: Revision,
    page_index: u32,
    profile: RenderProfile,
    region: Option<Rect>,
    key: SnapshotKey,
    entry: Arc<SceneEntry>,
    renderer: Arc<dyn RenderProvider>,
    /// The snapshot `render.pixels` cached for this key: answered again, with its held pixels or new ones.
    cached: Option<RenderSnapshot>,
    held: Option<Arc<RasterPixels>>,
    /// The job of a job-bound connection and the request as it came: its scope is checked again at the finish (as 34d's read).
    bound: Option<(JobRef, Value)>,
}

/// What [`RenderJob::run`] drew: the answer, the snapshot to cache, and the pixels of `render.pixels`.
pub struct RenderedPage {
    value: Value,
    fresh: Option<(RenderSnapshot, Option<(AssetRef, Arc<Vec<u8>>)>)>,
    pixels: Option<Arc<RasterPixels>>,
}

/// A render request after its checks.
pub(crate) enum Prepared {
    Cached(RenderSnapshot),
    Job(Box<RenderJob>),
}

impl RenderJob {
    /// Draws the page. It reads only what the job holds, so it runs without the workspace.
    pub fn run(&self) -> DltResult<RenderedPage> {
        if let Some(snapshot) = &self.cached {
            let pixels = match &self.held {
                Some(held) => Arc::clone(held),
                None => {
                    let one = page_scene(&self.entry.scene, self.page_index, self.region)?;
                    Arc::new(raster(&*self.renderer, &one, &self.profile, self.region, &*self.entry.assets)?)
                }
            };
            return Ok(RenderedPage { value: with_pixels(to_value(snapshot)?, &pixels), fresh: None, pixels: Some(pixels) });
        }
        let (snapshot, preview, pixels) = self.draw()?;
        let mut value = to_value(&snapshot)?;
        if let Some(pixels) = &pixels {
            value = with_pixels(value, pixels);
        }
        Ok(RenderedPage { value, fresh: Some((snapshot, preview)), pixels: pixels.map(Arc::new) })
    }

    /// The pixels this job draws, or None when it draws nothing: a `render.pixels` whose pixels are held is answered from them.
    /// A host takes its raster permits by this count (68i). A page or region that does not fit is refused here, so a host
    /// does not wait for permits to refuse it.
    pub fn draw_pixels(&self) -> DltResult<Option<u64>> {
        if self.cached.is_some() && self.held.is_some() {
            return Ok(None);
        }
        let page = self
            .entry
            .scene
            .pages
            .iter()
            .find(|p| p.page_index == self.page_index)
            .ok_or_else(|| invalid(format!("page_index {} does not exist", self.page_index)))?;
        if let Some(region) = self.region {
            check_region(region, page.size, self.page_index)?;
        }
        let (width, height) = self.region.map_or((page.size.width, page.size.height), |r| (r.width, r.height));
        let dpi = self.profile.dpi;
        Ok(Some(u64::from(pixels(width, dpi)?) * u64::from(pixels(height, dpi)?)))
    }

    fn draw(&self) -> DltResult<(RenderSnapshot, Option<(AssetRef, Arc<Vec<u8>>)>, Option<RasterPixels>)> {
        page_snapshot(&*self.renderer, &self.entry.scene, &*self.entry.assets, &self.artifact_id, self.revision, self.page_index, &self.profile, self.region, self.pixels)
    }
}

/// `render.pixels`: the answer names the pixels that follow it.
fn with_pixels(mut value: Value, pixels: &RasterPixels) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.insert("pixels".into(), json!({ "width": pixels.width, "height": pixels.height, "format": RASTER_PIXEL_FORMAT, "bytes": pixels.rgba_premultiplied.len() }));
    }
    value
}

/// The one-page scene of `page_index` that a snapshot holds and the renderer draws.
fn page_scene(scene: &RenderScene, page_index: u32, region: Option<Rect>) -> DltResult<RenderScene> {
    let total = scene.pages.len();
    let page = scene.pages.iter().find(|p| p.page_index == page_index).cloned().ok_or_else(|| {
        let indices: Vec<u32> = scene.pages.iter().map(|p| p.page_index).collect();
        invalid(format!("page_index {page_index} does not exist; the document has {total} page(s) {indices:?}"))
    })?;
    if let Some(r) = region { check_region(r, page.size, page_index)?; }
    let links = links_for_page(scene, &page);
    let mut metadata = scene.metadata.clone();
    metadata.insert("total_pages".into(), json!(total));
    metadata.insert("page_index".into(), json!(page_index));
    if let Some(r) = region { metadata.insert("region".into(), json!(r)); }
    Ok(RenderScene { pages: vec![page], font_fingerprint: scene.font_fingerprint.clone(), diagnostics: scene.diagnostics.clone(), metadata, links })
}

/// The renderer's raw pixels of `one`, checked against their stated size.
fn raster(renderer: &dyn RenderProvider, one: &RenderScene, profile: &RenderProfile, region: Option<Rect>, assets: &dyn AssetStore) -> DltResult<RasterPixels> {
    // The pixels' size is known from the contract's `pixels` rule before anything is drawn; what a client takes at most is refused here.
    let page = one.pages[0].size;
    let area = region.map_or((page.width, page.height), |r| (r.width, r.height));
    let wanted = u64::from(pixels(area.0, profile.dpi)?) * u64::from(pixels(area.1, profile.dpi)?) * 4;
    if wanted > MAX_RASTER_PIXEL_BYTES as u64 {
        return Err(invalid(format!("{PIXELS} of {wanted} bytes is over the {MAX_RASTER_PIXEL_BYTES}-byte limit; lower the dpi or render a region")));
    }
    let pixels = renderer.raster_region(one, profile, region, assets)?;
    let expected = u64::from(pixels.width) * u64::from(pixels.height) * 4;
    if pixels.width == 0 || pixels.height == 0 || pixels.rgba_premultiplied.len() as u64 != expected {
        return Err(err(ErrorCode::Internal, format!("renderer returned {} bytes for {}x{} pixels", pixels.rgba_premultiplied.len(), pixels.width, pixels.height)));
    }
    Ok(pixels)
}

/// One-page RenderSnapshot of `scene`, with its PNG preview, or (`pixels`) with its raw pixels and no preview.
#[allow(clippy::too_many_arguments)]
pub(crate) fn page_snapshot(renderer: &dyn RenderProvider, scene: &RenderScene, assets: &dyn AssetStore, artifact_id: &str, revision: Revision, page_index: u32, profile: &RenderProfile, region: Option<Rect>, pixels: bool) -> DltResult<(RenderSnapshot, Option<(AssetRef, Arc<Vec<u8>>)>, Option<RasterPixels>)> {
    check_profile(profile)?;
    let one = page_scene(scene, page_index, region)?;
    let (preview, raw) = if pixels {
        (None, Some(raster(renderer, &one, profile, region, assets)?))
    } else {
        let out = renderer.encode_region(&one, "png", profile, region, assets)?;
        if out.mime != "image/png" || !out.bytes.starts_with(&media_header::PNG_SIGNATURE) { return Err(err(ErrorCode::Internal, format!("renderer returned {} instead of PNG for a preview", out.mime))); }
        let asset = AssetRef { sha256: sha256(&out.bytes), mime: out.mime, bytes: out.bytes.len() as u64 };
        (Some((asset, Arc::new(out.bytes))), None)
    };
    let snapshot = RenderSnapshot {
        id: new_id(), artifact_id: artifact_id.to_string(), revision, profile: profile.clone(), scene: one, preview: preview.as_ref().map(|(asset, _)| asset.clone()),
        preview_transform: Some(ViewTransform { id: new_id(), dpi: profile.dpi, zoom_milli: 1000, offset_x_micropx: 0, offset_y_micropx: 0, crop: region }),
    };
    let size = serde_json::to_vec(&snapshot).map(|v| v.len()).unwrap_or(usize::MAX);
    if size > RESPONSE_BUDGET {
        return Err(invalid(format!("page {page_index} render snapshot is {size} bytes, over the frame budget; the page is too dense to transfer as one snapshot")));
    }
    Ok((snapshot, preview, raw))
}

impl Workspace {
    /// [`Workspace::dispatch_start`] of `render.get` and `render.pixels`.
    pub(crate) fn dispatch_render(&mut self, request: RpcRequest, bound_job: Option<JobRef>) -> Dispatch {
        let RpcRequest { id, method, params, .. } = request;
        let mut params = object_params(params);
        // A render reads no path; the mark of an agent's ui.command.run is taken off as `dispatch_bound` does.
        files::take_agent_path(&mut params);
        let (checked, bound) = match bound_job {
            Some(job) => (self.bound_scope(&method, params.clone(), &job), Some((job, params))),
            None => (Ok(params), None),
        };
        match checked.and_then(parse::<RenderRequest>).and_then(|req| self.prepare_render(method == PIXELS, id.clone(), req)) {
            Ok(Prepared::Cached(snapshot)) => Dispatch::Answered(respond(id, &method, to_value(&snapshot))),
            Ok(Prepared::Job(mut job)) => {
                job.bound = bound;
                Dispatch::Render(job)
            }
            Err(error) => Dispatch::Answered(RpcResponse::failure(id, error)),
        }
    }

    /// The checks of a render under the workspace, and the scene it draws.
    pub(crate) fn prepare_render(&mut self, pixels: bool, id: String, req: RenderRequest) -> DltResult<Prepared> {
        check_profile(&req.profile)?;
        let row = store::artifact_row(&self.conn, &req.artifact_id)?;
        if req.expected_revision > row.current_revision {
            return Err(err(ErrorCode::RevisionConflict, format!("revision {} does not exist yet; current is {}", req.expected_revision, row.current_revision)).at(row.current_revision));
        }
        let key = SnapshotKey {
            artifact_id: req.artifact_id.clone(), revision: req.expected_revision, page_index: req.page_index, dpi: req.profile.dpi, transparent: req.profile.transparent,
            region: SnapshotKey::region_key(req.region), node_frames: req.include_node_frames, pixels,
        };
        let cached = self.renders.by_key(&key);
        let held = cached.as_ref().filter(|_| pixels).and_then(|snapshot| self.renders.pixels_of(&snapshot.id));

        if !pixels {
            if let Some(snapshot) = cached { return Ok(Prepared::Cached(snapshot)); }
        }
        let entry = self.scene_with(&req.artifact_id, req.expected_revision, req.include_node_frames)?;
        Ok(Prepared::Job(Box::new(RenderJob {
            id, pixels, artifact_id: req.artifact_id, revision: req.expected_revision, page_index: req.page_index, profile: req.profile, region: req.region, key, entry,
            renderer: self.renderer.clone(), cached, held, bound: None,
        })))
    }

    /// A render drawn in this call (`dispatch_bound`, `context.read`), cached as a host's split render is.
    pub(crate) fn render_now(&mut self, job: &RenderJob) -> DltResult<RenderSnapshot> {
        let (snapshot, preview, _) = job.draw()?;
        self.renders.insert(snapshot.clone(), Some(job.key.clone()), preview);
        Ok(snapshot)
    }

    /// The response to a render run by [`RenderJob::run`], and the pixels of `render.pixels` to send after it. The snapshot is
    /// cached under the request's key, so later hit tests and `asset.read` of its preview find it. A job-bound request is checked
    /// again first: its job's scope may have changed while it was drawn (as `finish_pdf_read`); a refused one caches nothing.
    /// The check looks only at the job and its attempt rows and at the artifact (`bound_scope`): an attempt that was canceled
    /// while the page was drawn is not refused here (34d and 68i have the same rule).
    pub fn finish_render(&mut self, job: Box<RenderJob>, rendered: DltResult<RenderedPage>) -> (RpcResponse, Option<Arc<RasterPixels>>) {
        let RenderJob { id, key, pixels: raw, bound, .. } = *job;
        let method = if raw { PIXELS } else { GET };
        let rechecked = match bound {
            Some((job, params)) => self.bound_scope(method, params, &job).map(|_| ()),
            None => Ok(()),
        };
        match rechecked.and(rendered) {
            Ok(RenderedPage { value, fresh, pixels }) => {
                if let Some((snapshot, preview)) = fresh {
                    self.renders.insert(snapshot, Some(key), preview);
                }
                // Held again as the newest: a page asked again stays (`RenderCache::put_pixels`).
                if let (Some(drawn), Some(snapshot_id)) = (&pixels, value.get("id").and_then(Value::as_str)) {
                    self.renders.put_pixels(snapshot_id, Arc::clone(drawn));
                }
                let response = respond(id, method, Ok(value));
                let pixels = pixels.filter(|_| response.error.is_none());
                (response, pixels)
            }
            Err(error) => (RpcResponse::failure(id, error), None),
        }
    }
}

/// `render.pixels` through [`Workspace::dispatch_bound`]: raw pixels go only through a host's binary frames.
pub(crate) fn pixels_in_process() -> DltError {
    crate::errors::unsupported(format!("{PIXELS} is answered by the host to the gui connection only; call {GET}"))
}
