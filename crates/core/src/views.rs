//! Host-session state that is not part of the durable document: views (selection, visible
//! page, client transform), target reservations, cached layouts and render snapshots.
use crate::blobs::StagingAssets;
use crate::errors::{err, invalid};
use designllm_contracts::*;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const DEFAULT_RESERVATION_MS: u64 = 30_000;
pub const MAX_RESERVATION_MS: u64 = 300_000;
const SCENE_CACHE: usize = 4;
const SNAPSHOT_CACHE: usize = 96;

#[derive(Debug, Clone)]
pub struct ViewState {
    pub artifact_id: String,
    pub revision: Revision,
    pub selection: Selection,
    pub visible_page: u32,
    /// (page_index, client transform) last set by view.transform.set.
    pub transform: Option<(u32, ViewTransform)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Reservation {
    pub reservation_id: String,
    pub artifact_id: String,
    pub view_id: String,
    pub owner: String,
    pub node_ids: BTreeSet<String>,
    /// A job's reservation of its target_ids: held while the job is queued, running or
    /// validating (no ttl), passed only by that job's writes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// What the reservation holds in conflict checks: its nodes with what lies inside them (`Workspace::holds_in`), and for a
    /// job also what its commits created. Recomputed after each commit of the artifact (`Workspace::refresh_holds`).
    #[serde(skip)]
    pub held: BTreeSet<String>,
    #[serde(skip)]
    pub expires: Instant,
}

/// The ids of `ids` that a reservation holds, on its current revision or (`extra`, by reservation id) on a candidate.
fn held_hits(reservation: &Reservation, extra: &BTreeMap<String, BTreeSet<String>>, ids: &BTreeSet<String>) -> Vec<String> {
    let also = extra.get(&reservation.reservation_id);
    ids.iter().filter(|id| reservation.held.contains(*id) || also.is_some_and(|set| set.contains(*id))).cloned().collect()
}

#[derive(Default)]
pub struct Views {
    pub views: HashMap<String, ViewState>,
    /// The view that most recently called selection.set (the user's focused document).
    pub active: Option<String>,
    pub reservations: HashMap<String, Reservation>,
}
impl Views {
    pub fn prune(&mut self) { let now = Instant::now(); self.reservations.retain(|_, r| r.job_id.is_some() || r.expires > now); }
    /// Reserved ids among `ids` held by someone other than the writer: `actor` for a user write,
    /// `job` (its job id) for a job write. A user passes their own reservation, a job passes only
    /// its own job reservation; users never pass a job's and jobs never pass a user's.
    pub fn busy(&mut self, artifact_id: &str, ids: &BTreeSet<String>, actor: &str, job: Option<&str>) -> Option<(Vec<String>, String)> {
        self.busy_with(artifact_id, ids, actor, job, &BTreeMap::new())
    }
    /// `busy`, also counting `extra` (reservation id -> the ids it would hold on the candidate revision of this change).
    pub fn busy_with(&mut self, artifact_id: &str, ids: &BTreeSet<String>, actor: &str, job: Option<&str>, extra: &BTreeMap<String, BTreeSet<String>>) -> Option<(Vec<String>, String)> {
        self.prune();
        for r in self.reservations.values() {
            if r.artifact_id != artifact_id { continue; }
            let own = match (r.job_id.as_deref(), job) {
                (Some(holder), Some(writer)) => holder == writer,
                (None, None) => r.owner == actor,
                _ => false,
            };
            if own { continue; }
            let hit = held_hits(r, extra, ids);
            if !hit.is_empty() { return Some((hit, r.owner.clone())); }
        }
        None
    }
    /// TARGET_BUSY when another reservation holds one of `ids` (a job checks before it reserves).
    pub fn check_free(&mut self, artifact_id: &str, ids: &BTreeSet<String>, job_id: &str) -> DltResult<()> {
        self.check_free_with(artifact_id, ids, job_id, &BTreeMap::new())
    }
    /// `check_free`, also counting `extra` (see `busy_with`).
    pub fn check_free_with(&mut self, artifact_id: &str, ids: &BTreeSet<String>, job_id: &str, extra: &BTreeMap<String, BTreeSet<String>>) -> DltResult<()> {
        self.prune();
        for r in self.reservations.values() {
            if r.artifact_id != artifact_id || r.job_id.as_deref() == Some(job_id) { continue; }
            let hit = held_hits(r, extra, ids);
            if !hit.is_empty() {
                let mut e = err(ErrorCode::TargetBusy, format!("target_ids are reserved by {:?}", r.owner));
                e.affected_ids = hit;
                return Err(e);
            }
        }
        Ok(())
    }
    /// The job's reservation of its targets (replacing an earlier one of the same job). `held` is what it holds (`holds_in`).
    pub fn reserve_job(&mut self, reservation_id: String, artifact_id: &str, job_id: &str, owner: String, ids: BTreeSet<String>, held: BTreeSet<String>) {
        self.release_job(job_id);
        let reservation = Reservation {
            reservation_id, artifact_id: artifact_id.to_string(), view_id: String::new(), owner, node_ids: ids, held,
            job_id: Some(job_id.to_string()), expires: Instant::now(),
        };
        self.reservations.insert(reservation.reservation_id.clone(), reservation);
    }
    /// The job's reservation: (reservation_id, ids).
    pub fn job_reservation(&self, job_id: &str) -> Option<(String, BTreeSet<String>)> {
        self.reservations.values().find(|r| r.job_id.as_deref() == Some(job_id)).map(|r| (r.reservation_id.clone(), r.node_ids.clone()))
    }
    /// The reservations on an artifact (copies: their holds are read, then set with `set_held`).
    pub fn reservations_on(&self, artifact_id: &str) -> Vec<Reservation> {
        self.reservations.values().filter(|r| r.artifact_id == artifact_id).cloned().collect()
    }
    /// Whether any reservation sits on the artifact.
    pub fn reserved(&self, artifact_id: &str) -> bool {
        self.reservations.values().any(|r| r.artifact_id == artifact_id)
    }
    /// Sets what a reservation holds in conflict checks (recomputed after each commit of its artifact, see `refresh_holds`).
    pub fn set_held(&mut self, reservation_id: &str, held: BTreeSet<String>) {
        if let Some(r) = self.reservations.get_mut(reservation_id) { r.held = held; }
    }
    pub fn release_job(&mut self, job_id: &str) {
        self.reservations.retain(|_, r| r.job_id.as_deref() != Some(job_id));
    }
    /// A user's reservation of `req.node_ids`. `held` is what those nodes hold (`Workspace::reservation_holds`): it is checked
    /// against what the other reservations hold, and kept as this reservation's holds until the next commit.
    pub fn reserve(&mut self, req: &ReservationRequest, ttl_ms: u64, held: BTreeSet<String>) -> DltResult<Reservation> {
        self.prune();
        for r in self.reservations.values() {
            if r.artifact_id == req.artifact_id && (r.job_id.is_some() || r.owner != req.owner) {
                let hit: Vec<String> = r.held.intersection(&held).cloned().collect();
                if !hit.is_empty() {
                    let mut e = err(ErrorCode::TargetBusy, format!("targets are reserved by {:?}", r.owner));
                    e.affected_ids = hit;
                    return Err(e);
                }
            }
        }
        // One reservation per (artifact, view, owner): re-reserving renews and replaces it.
        self.reservations.retain(|_, r| !(r.job_id.is_none() && r.artifact_id == req.artifact_id && r.view_id == req.view_id && r.owner == req.owner));
        let reservation = Reservation {
            reservation_id: new_id(), artifact_id: req.artifact_id.clone(), view_id: req.view_id.clone(), owner: req.owner.clone(),
            node_ids: req.node_ids.iter().cloned().collect(), held, job_id: None, expires: Instant::now() + Duration::from_millis(ttl_ms),
        };
        self.reservations.insert(reservation.reservation_id.clone(), reservation.clone());
        Ok(reservation)
    }
}

pub struct SceneEntry { pub scene: Arc<RenderScene>, pub assets: Arc<StagingAssets> }
#[derive(Clone)]
pub struct SnapshotEntry { pub snapshot: RenderSnapshot, pub key: SnapshotKey }
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct SnapshotKey { pub artifact_id: String, pub revision: Revision, pub page_index: u32, pub dpi: u32, pub transparent: bool, pub region: Option<[i64; 4]>, pub node_frames: bool }
impl SnapshotKey {
    pub fn region_key(region: Option<Rect>) -> Option<[i64; 4]> { region.map(|r| [r.x, r.y, r.width, r.height]) }
}

#[derive(Default)]
pub struct RenderCache {
    /// (artifact, revision, node_frames): the scene of `layout` or of `layout_with_node_frames`.
    scenes: VecDeque<((String, Revision, bool), Arc<SceneEntry>)>,
    snapshots: HashMap<String, SnapshotEntry>,
    order: VecDeque<String>,
    by_key: HashMap<SnapshotKey, String>,
    previews: HashMap<String, (String, Arc<Vec<u8>>)>,
}
impl RenderCache {
    pub fn scene(&mut self, artifact_id: &str, revision: Revision, node_frames: bool) -> Option<Arc<SceneEntry>> {
        let pos = self.scenes.iter().position(|((a, r, f), _)| a == artifact_id && *r == revision && *f == node_frames)?;
        let item = self.scenes.remove(pos)?;
        let entry = item.1.clone();
        self.scenes.push_back(item);
        Some(entry)
    }
    pub fn put_scene(&mut self, artifact_id: &str, revision: Revision, node_frames: bool, entry: Arc<SceneEntry>) {
        self.scenes.retain(|((a, r, f), _)| !(a == artifact_id && *r == revision && *f == node_frames));
        self.scenes.push_back(((artifact_id.to_string(), revision, node_frames), entry));
        while self.scenes.len() > SCENE_CACHE { self.scenes.pop_front(); }
    }
    pub fn by_key(&self, key: &SnapshotKey) -> Option<RenderSnapshot> { self.by_key.get(key).and_then(|id| self.snapshots.get(id)).map(|e| e.snapshot.clone()) }
    pub fn snapshot(&self, id: &str) -> Option<&SnapshotEntry> { self.snapshots.get(id) }
    /// `register == false` keeps the preview readable but not queryable (proposal previews).
    pub fn insert(&mut self, snapshot: RenderSnapshot, key: Option<SnapshotKey>, preview: Option<(AssetRef, Arc<Vec<u8>>)>) {
        if let Some((r, bytes)) = preview { self.previews.insert(r.sha256.clone(), (r.mime, bytes)); }
        let id = snapshot.id.clone();
        if let Some(k) = &key { self.by_key.insert(k.clone(), id.clone()); }
        self.snapshots.insert(id.clone(), SnapshotEntry { snapshot, key: key.unwrap_or(SnapshotKey { artifact_id: String::new(), revision: 0, page_index: 0, dpi: 0, transparent: false, region: None, node_frames: false }) });
        self.order.push_back(id);
        while self.order.len() > SNAPSHOT_CACHE {
            if let Some(old) = self.order.pop_front() {
                if let Some(e) = self.snapshots.remove(&old) {
                    if self.by_key.get(&e.key) == Some(&old) { self.by_key.remove(&e.key); }
                    if let Some(p) = e.snapshot.preview { if !self.snapshots.values().any(|s| s.snapshot.preview.as_ref().map(|x| &x.sha256) == Some(&p.sha256)) { self.previews.remove(&p.sha256); } }
                }
            }
        }
    }
    pub fn preview(&self, hash: &str) -> Option<(String, Arc<Vec<u8>>)> { self.previews.get(hash).cloned() }
    /// Preview assets rendered from `artifact_id` (job-bound read scope).
    pub fn previews_of(&self, artifact_id: &str) -> Vec<String> {
        self.snapshots.values().filter(|s| s.snapshot.artifact_id == artifact_id).filter_map(|s| s.snapshot.preview.as_ref().map(|p| p.sha256.clone())).collect()
    }
}

pub fn check_transform(t: &ViewTransform) -> DltResult<()> {
    if t.id.trim().is_empty() { return Err(invalid("transform.id is empty")); }
    if t.dpi == 0 || t.dpi > 9_600 { return Err(invalid("transform.dpi must be 1..=9600")); }
    if t.zoom_milli == 0 || t.zoom_milli > 100_000 { return Err(invalid("transform.zoom_milli must be 1..=100000")); }
    if let Some(c) = t.crop { if c.width <= 0 || c.height <= 0 { return Err(invalid("transform.crop must have positive size")); } }
    Ok(())
}
/// Nearest integer, ties away from zero.
fn div_round(n: i128, d: i128) -> i128 { let q = (n.abs() * 2 + d.abs()) / (d.abs() * 2); if (n < 0) != (d < 0) { -q } else { q } }
fn to_i64(v: i128) -> DltResult<i64> { i64::try_from(v).map_err(|_| invalid("coordinate overflow")) }

/// Screen micro-pixels to page EMU: emu = crop + (screen - offset) * 914400 * 1000 / (1e6 * dpi * zoom_milli).
pub fn screen_to_emu(t: &ViewTransform, p: ScreenPoint) -> DltResult<Point> {
    check_transform(t)?;
    let den = 1_000_000i128 * i128::from(t.dpi) * i128::from(t.zoom_milli);
    let num = i128::from(EMU_PER_INCH) * 1000;
    let (cx, cy) = t.crop.map(|c| (c.x, c.y)).unwrap_or((0, 0));
    let x = i128::from(cx) + div_round((i128::from(p.x_micropx) - i128::from(t.offset_x_micropx)) * num, den);
    let y = i128::from(cy) + div_round((i128::from(p.y_micropx) - i128::from(t.offset_y_micropx)) * num, den);
    Ok(Point { x: to_i64(x)?, y: to_i64(y)? })
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ScreenRect { pub x_micropx: i64, pub y_micropx: i64, pub width_micropx: i64, pub height_micropx: i64 }

/// Page EMU rect to screen micro-pixels (inverse of [`screen_to_emu`]).
pub fn emu_to_screen(t: &ViewTransform, r: Rect) -> DltResult<ScreenRect> {
    check_transform(t)?;
    let num = 1_000_000i128 * i128::from(t.dpi) * i128::from(t.zoom_milli);
    let den = i128::from(EMU_PER_INCH) * 1000;
    let (cx, cy) = t.crop.map(|c| (c.x, c.y)).unwrap_or((0, 0));
    let x0 = i128::from(t.offset_x_micropx) + div_round((i128::from(r.x) - i128::from(cx)) * num, den);
    let y0 = i128::from(t.offset_y_micropx) + div_round((i128::from(r.y) - i128::from(cy)) * num, den);
    let x1 = i128::from(t.offset_x_micropx) + div_round((i128::from(r.x) + i128::from(r.width) - i128::from(cx)) * num, den);
    let y1 = i128::from(t.offset_y_micropx) + div_round((i128::from(r.y) + i128::from(r.height) - i128::from(cy)) * num, den);
    Ok(ScreenRect { x_micropx: to_i64(x0)?, y_micropx: to_i64(y0)?, width_micropx: to_i64(x1 - x0)?, height_micropx: to_i64(y1 - y0)? })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn t(dpi: u32, zoom: u32, ox: i64, oy: i64, crop: Option<Rect>) -> ViewTransform { ViewTransform { id: "t".into(), dpi, zoom_milli: zoom, offset_x_micropx: ox, offset_y_micropx: oy, crop } }
    #[test]
    fn transform_roundtrip_and_rounding() {
        // 144 dpi, 100%: one inch is 144 px = 144_000_000 micropx.
        let tr = t(144, 1000, 0, 0, None);
        assert_eq!(screen_to_emu(&tr, ScreenPoint { x_micropx: 144_000_000, y_micropx: 72_000_000 }).unwrap(), Point { x: EMU_PER_INCH, y: EMU_PER_INCH / 2 });
        let r = emu_to_screen(&tr, Rect { x: EMU_PER_INCH, y: 0, width: EMU_PER_INCH, height: EMU_PER_INCH }).unwrap();
        assert_eq!(r, ScreenRect { x_micropx: 144_000_000, y_micropx: 0, width_micropx: 144_000_000, height_micropx: 144_000_000 });
        // 96 dpi, 150% zoom, offset and crop.
        let tr = t(96, 1500, 10_000_000, -5_000_000, Some(Rect { x: 914_400, y: 0, width: 914_400, height: 914_400 }));
        let p = screen_to_emu(&tr, ScreenPoint { x_micropx: 10_000_000 + 144_000_000, y_micropx: -5_000_000 }).unwrap();
        assert_eq!(p, Point { x: 2 * EMU_PER_INCH, y: 0 });
        // One pixel at 144 dpi is 6350 EMU; one micro-pixel (0.00635 EMU) rounds to 0.
        let tr = t(144, 1000, 0, 0, None);
        assert_eq!(screen_to_emu(&tr, ScreenPoint { x_micropx: 1_000_000, y_micropx: -1_000_000 }).unwrap(), Point { x: 6350, y: -6350 });
        assert_eq!(screen_to_emu(&tr, ScreenPoint { x_micropx: 1, y_micropx: -1 }).unwrap(), Point { x: 0, y: 0 });
        // 100 dpi: one micro-pixel = 0.009144 EMU.
        let tr = t(100, 1000, 0, 0, None);
        assert_eq!(screen_to_emu(&tr, ScreenPoint { x_micropx: 125_000, y_micropx: 0 }).unwrap().x, 1143); // 1143.0 exactly
        assert_eq!(div_round(5, 2), 3);
        assert_eq!(div_round(-5, 2), -3);
        assert!(check_transform(&t(0, 1000, 0, 0, None)).is_err());
        assert!(check_transform(&t(96, 0, 0, 0, None)).is_err());
    }
}
