//! Home thumbnails (GUI_DESIGN §3): the first regular Canvas page (page 0 for other formats), rendered by the core at the dpi
//! that gives the tile's physical pixels, cached per artifact, revision and dpi. The requests do not
//! go through `call`: they are not scoped to the open document and never raise a message.

use std::collections::HashMap;

use designllm_contracts::{
    ArtifactKind, ArtifactSummary, AssetRef, ClientInfo, DltError, RenderProfile, RenderRequest, RenderSnapshot, Size, TemplateInfo, EMU_PER_INCH,
};
use eframe::egui::{self, Color32, Context, CornerRadius, Pos2, Rect, Ui, Vec2};
use serde_json::{json, Value};

use super::{board::default_page_size, DesktopApp, PendingAction, PendingRequest};
use crate::rpc::RpcPayload;

/// At most this many thumbnails are being made at once, so the list never floods the host.
const IN_FLIGHT: usize = 2;
const MAX_DPI: u32 = 600;
/// How often the home asks the host who is connected, and asks again for an empty template list.
const POLL: std::time::Duration = std::time::Duration::from_secs(3);

/// The stage a home read is at: an artifact's page render, a template preview, the picture of
/// either, the template list, or the host's connections. All ride on `PendingAction::Thumb`.
#[derive(Clone)]
pub(super) enum ThumbTicket {
    Inspect { artifact_id: String, revision: u64, dpi: u32 },
    Render { artifact_id: String, revision: u64, dpi: u32 },
    /// `artifact_id` is the cache key (`template_key`) for a template picture; revision 0.
    Asset { artifact_id: String, revision: u64, dpi: u32, page: Size },
    TemplatePreview { key: String, dpi: u32 },
    TemplateList,
    Clients,
    /// `proposal.preview` of a proposal row (proposals.rs); its picture is read as an `Asset` under `proposals::picture_key`.
    Proposal { proposal_id: String, dpi: u32 },
}

/// What a thumbnail shows: the first regular page of an artifact, or a template's preview.
#[derive(Clone, Copy)]
pub(super) enum ThumbSource<'a> {
    Artifact(&'a ArtifactSummary),
    Template(&'a TemplateInfo),
}

/// The cache key of a template preview.
fn template_key(template: &TemplateInfo) -> String {
    format!("template:{:?}:{}", template.kind, template.id)
}

fn first_regular_page_index(artifact: &Value) -> Option<u32> {
    let body = artifact.get("body")?;
    if body.get("type").and_then(Value::as_str) != Some("canvas") { return Some(0); }
    body.get("document")?.get("pages")?.as_array()?.iter().position(|page| {
        page.get("role").and_then(Value::as_str) != Some("component")
    }).and_then(|index| u32::try_from(index).ok())
}

impl ThumbSource<'_> {
    fn key(&self) -> String {
        match self {
            Self::Artifact(summary) => summary.id.clone(),
            Self::Template(template) => template_key(template),
        }
    }

    fn revision(&self) -> u64 {
        match self {
            Self::Artifact(summary) => summary.revision,
            Self::Template(_) => 0,
        }
    }

    fn page_hint(&self) -> Size {
        match self {
            Self::Artifact(summary) => default_page_size(summary.kind),
            Self::Template(template) => template.size.unwrap_or_else(|| default_page_size(template.kind)),
        }
    }
}

/// The home's reads of the host besides thumbnails: templates and connections.
#[derive(Default)]
pub(super) struct HomeFeeds {
    pub templates: Vec<TemplateInfo>,
    pub clients: Vec<ClientInfo>,
    templates_pending: bool,
    templates_at: Option<std::time::Instant>,
    clients_pending: bool,
    clients_at: Option<std::time::Instant>,
}

#[derive(Clone)]
struct ThumbImage {
    texture: egui::TextureHandle,
    revision: u64,
    dpi: u32,
    /// Page size in EMU.
    page: Size,
}

#[derive(Clone, Default)]
struct Thumb {
    shown: Option<ThumbImage>,
    /// (revision, dpi) of the request in flight.
    pending: Option<(u64, u32)>,
    /// (revision, dpi) that failed; not asked for again.
    failed: Option<(u64, u32)>,
    /// The page size the core reported last, so the next dpi fits the page.
    page: Option<Size>,
    /// The first regular page selected from the artifact snapshot, cached until its revision changes.
    page_index: Option<(u64, u32)>,
}

/// How a page sits in its box: `Cover` fills the box and crops (list rows, top of the page kept);
/// `Contain` shows the whole page centred with a margin (grid tiles).
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Fit {
    Cover,
    Contain { margin: f32 },
}

/// The cache lives in the context's memory, by artifact id, for as long as the app runs.
type ThumbCache = HashMap<String, Thumb>;

fn with_cache<R>(ctx: &Context, change: impl FnOnce(&mut ThumbCache) -> R) -> R {
    ctx.data_mut(|data| change(data.get_temp_mut_or_default::<ThumbCache>(egui::Id::new("home-thumbs"))))
}

fn inches(emu: i64) -> f32 {
    emu as f32 / EMU_PER_INCH as f32
}

/// Logical points per page inch so that the page fits `area` as `fit` says.
fn points_per_inch(page: Size, area: Vec2, fit: Fit) -> f32 {
    let (width, height) = (inches(page.width).max(0.01), inches(page.height).max(0.01));
    match fit {
        Fit::Cover => (area.x / width).max(area.y / height),
        Fit::Contain { margin } => ((area.x - 2.0 * margin) / width).min((area.y - 2.0 * margin) / height),
    }
}

/// The dpi to ask for: the box's physical pixels per page inch, rounded up.
pub(super) fn wanted_dpi(page: Size, area: Vec2, fit: Fit, pixels_per_point: f32) -> u32 {
    let dpi = (points_per_inch(page, area, fit) * pixels_per_point).ceil();
    (dpi as u32).clamp(1, MAX_DPI)
}

/// The screen rectangle of the page and the part of the texture to show, for a page in `rect`.
pub(super) fn placement(page: Size, rect: Rect, fit: Fit) -> (Rect, Rect) {
    let scale = points_per_inch(page, rect.size(), fit);
    let shown = Vec2::new(inches(page.width) * scale, inches(page.height) * scale);
    match fit {
        Fit::Contain { .. } => (Rect::from_center_size(rect.center(), shown), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))),
        Fit::Cover => {
            // Centred across, the top of the page kept (a document's heading, a canvas's top).
            let u = ((shown.x - rect.width()) / shown.x / 2.0).max(0.0);
            let v = (rect.height() / shown.y).min(1.0);
            (rect, Rect::from_min_max(Pos2::new(u, 0.0), Pos2::new(1.0 - u, v)))
        }
    }
}

/// Paints a page's picture into `rect`, placed by `fit`; a cover picture takes `corner`, a contained page keeps square corners.
/// Returns the page's screen rectangle. Every thumbnail and proposal row paints through this.
pub(super) fn paint_picture(ui: &Ui, rect: Rect, texture: &egui::TextureHandle, page: Size, fit: Fit, corner: CornerRadius) -> Rect {
    let (screen, uv) = placement(page, rect, fit);
    let clip = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    let corner = if fit == Fit::Cover { corner } else { CornerRadius::ZERO };
    clip.add(egui::epaint::RectShape::filled(screen, corner, Color32::WHITE).with_texture(texture.id(), uv));
    screen
}

/// The picture a render snapshot shows: its PNG asset and the size of its first page (`None` when either is missing).
pub(super) fn snapshot_picture(snapshot: RenderSnapshot) -> Option<(AssetRef, Size)> {
    let page = snapshot.scene.pages.first().map(|page| page.size)?;
    Some((snapshot.preview?, page))
}

impl DesktopApp {
    /// Paints the thumbnail of `summary` into `rect` and asks for a sharper or newer one when needed.
    /// Returns the page's screen rectangle, or None when there is no picture yet (the caller keeps
    /// the empty box).
    pub(super) fn paint_thumb(&mut self, ui: &Ui, source: ThumbSource<'_>, rect: Rect, fit: Fit, corner: CornerRadius) -> Option<Rect> {
        let ctx = ui.ctx().clone();
        let ppp = ctx.pixels_per_point();
        let key = source.key();
        let revision = source.revision();
        let (entry, in_flight) = with_cache(&ctx, |cache| {
            let in_flight = cache.values().filter(|thumb| thumb.pending.is_some()).count();
            (cache.entry(key.clone()).or_default().clone(), in_flight)
        });
        let page = entry.page.unwrap_or_else(|| source.page_hint());
        let dpi = wanted_dpi(page, rect.size(), fit, ppp);
        let up_to_date = entry.shown.as_ref().is_some_and(|shown| shown.revision == revision && shown.dpi >= dpi);
        // A revision that failed once is not asked for again at any dpi (two boxes of one source at
        // different sizes would otherwise ask in turn for ever).
        let asked = entry.pending.is_some() || entry.failed.is_some_and(|(failed, _)| failed == revision);
        let painted = entry.shown.as_ref().map(|shown| paint_picture(ui, rect, &shown.texture, shown.page, fit, corner));
        if !up_to_date && !asked && in_flight < IN_FLIGHT && ui.is_rect_visible(rect) {
            let page_index = match source {
                ThumbSource::Artifact(summary) if summary.kind != ArtifactKind::Canvas => Some(0),
                ThumbSource::Artifact(_) => entry.page_index.filter(|(cached_revision, _)| *cached_revision == revision).map(|(_, index)| index),
                ThumbSource::Template(_) => None,
            };
            self.request_thumb(&ctx, source, dpi, page_index);
        }
        painted
    }

    fn request_thumb(&mut self, ctx: &Context, source: ThumbSource<'_>, dpi: u32, page_index: Option<u32>) {
        let (key, revision) = (source.key(), source.revision());
        let sent = match source {
            ThumbSource::Artifact(summary) => match page_index {
                Some(page_index) => self.request_artifact_render(&summary.id, summary.revision, dpi, page_index),
                None => self.send_home_read(
                    "artifact.inspect",
                    json!({ "artifact_id": summary.id, "revision": summary.revision }),
                    ThumbTicket::Inspect { artifact_id: summary.id.clone(), revision: summary.revision, dpi },
                ),
            },
            ThumbSource::Template(template) => self.send_home_read(
                "templates.preview",
                json!({ "template": template.id, "kind": template.kind, "dpi": dpi }),
                ThumbTicket::TemplatePreview { key: template_key(template), dpi },
            ),
        };
        if sent {
            with_cache(ctx, |cache| cache.entry(key).or_default().pending = Some((revision, dpi)));
        } else {
            thumb_failed(ctx, &key, revision, dpi);
        }
    }

    fn request_artifact_render(&mut self, artifact_id: &str, revision: u64, dpi: u32, page_index: u32) -> bool {
        let request = RenderRequest {
            artifact_id: artifact_id.to_string(), expected_revision: revision, page_index,
            profile: RenderProfile { dpi, transparent: false }, region: None, include_node_frames: false,
        };
        let Ok(params) = serde_json::to_value(request) else { return false };
        self.send_home_read(
            "render.get", params,
            ThumbTicket::Render { artifact_id: artifact_id.to_string(), revision, dpi },
        )
    }

    /// Sends one home read to the host outside `call`: not scoped to the open document, no message.
    pub(super) fn send_home_read(&mut self, method: &str, params: Value, ticket: ThumbTicket) -> bool {
        if self.host_lost() {
            return false;
        }
        let number = self.next_ticket;
        self.next_ticket += 1;
        let sent = self.rpc.call(number, method, params).is_ok();
        if sent {
            self.pending.insert(
                number,
                PendingRequest { action: PendingAction::Thumb(ticket), method: method.into(), artifact_id: None, require_active: false, apply: None, ui_run: None },
            );
        }
        sent
    }

    pub(super) fn read_home_asset(&mut self, ctx: &Context, asset: AssetRef, key: String, revision: u64, dpi: u32, page: Size) {
        if self.host_lost() {
            return;
        }
        let number = self.next_ticket;
        self.next_ticket += 1;
        with_cache(ctx, |cache| cache.entry(key.clone()).or_default().page = Some(page));
        if self.rpc.read_asset(number, asset).is_ok() {
            let action = PendingAction::Thumb(ThumbTicket::Asset { artifact_id: key, revision, dpi, page });
            self.pending.insert(
                number,
                PendingRequest { action, method: "asset.read".into(), artifact_id: None, require_active: false, apply: None, ui_run: None },
            );
        } else {
            thumb_failed(ctx, &key, revision, dpi);
        }
    }

    /// Keeps the template list and the connections fresh while the home is shown: the connections
    /// every few seconds, the templates once (again while the list is still empty).
    pub(super) fn poll_home_feeds(&mut self) {
        let now = std::time::Instant::now();
        // A smoke capture asks once: a capture waits for no request in flight, and polling would keep one.
        let smoke = self.smoke_output.is_some();
        let due = |at: Option<std::time::Instant>| at.is_none_or(|at| !smoke && now.duration_since(at) >= POLL);
        if !self.home.feeds.clients_pending && due(self.home.feeds.clients_at) && self.send_home_read("clients.list", json!({}), ThumbTicket::Clients) {
            self.home.feeds.clients_pending = true;
            self.home.feeds.clients_at = Some(now);
        }
        let want_templates = self.home.feeds.templates.is_empty() && due(self.home.feeds.templates_at);
        if !self.home.feeds.templates_pending && want_templates && self.send_home_read("templates.list", json!({}), ThumbTicket::TemplateList) {
            self.home.feeds.templates_pending = true;
            self.home.feeds.templates_at = Some(now);
        }
    }

    /// The reply to a thumbnail request: the render snapshot (then its picture is read), or the picture.
    pub(super) fn thumb_reply(&mut self, ctx: &Context, ticket: ThumbTicket, result: Result<RpcPayload, DltError>) {
        match (ticket, result) {
            (ThumbTicket::Inspect { artifact_id, revision, dpi }, Ok(RpcPayload::Json(value))) => {
                let page_index = (value.get("revision").and_then(Value::as_u64) == Some(revision))
                    .then(|| first_regular_page_index(&value)).flatten();
                match page_index {
                    Some(page_index) => {
                        with_cache(ctx, |cache| cache.entry(artifact_id.clone()).or_default().page_index = Some((revision, page_index)));
                        if !self.request_artifact_render(&artifact_id, revision, dpi, page_index) {
                            thumb_failed(ctx, &artifact_id, revision, dpi);
                        }
                    }
                    None => thumb_failed(ctx, &artifact_id, revision, dpi),
                }
            }
            (ThumbTicket::Inspect { artifact_id, revision, dpi }, _) => thumb_failed(ctx, &artifact_id, revision, dpi),
            (ThumbTicket::Render { artifact_id, revision, dpi }, Ok(RpcPayload::Json(value))) => {
                match serde_json::from_value::<RenderSnapshot>(value).ok().and_then(snapshot_picture) {
                    Some((asset, page)) => self.read_home_asset(ctx, asset, artifact_id, revision, dpi, page),
                    None => thumb_failed(ctx, &artifact_id, revision, dpi),
                }
            }
            (ThumbTicket::TemplatePreview { key, dpi }, Ok(RpcPayload::Json(value))) => {
                let asset = value.get("asset").cloned().and_then(|asset| serde_json::from_value::<AssetRef>(asset).ok());
                let page = value.get("page_size").cloned().and_then(|size| serde_json::from_value::<Size>(size).ok());
                match (asset, page) {
                    (Some(asset), Some(page)) => self.read_home_asset(ctx, asset, key, 0, dpi, page),
                    _ => thumb_failed(ctx, &key, 0, dpi),
                }
            }
            (ThumbTicket::TemplatePreview { key, dpi }, _) => thumb_failed(ctx, &key, 0, dpi),
            (ThumbTicket::TemplateList, result) => {
                self.home.feeds.templates_pending = false;
                if let Ok(RpcPayload::Json(value)) = result {
                    self.home.feeds.templates = parse_list::<TemplateInfo>(&value, "templates");
                    ctx.request_repaint();
                }
            }
            (ThumbTicket::Clients, result) => {
                self.home.feeds.clients_pending = false;
                if let Ok(RpcPayload::Json(value)) = result {
                    let clients = parse_list::<ClientInfo>(&value, "clients");
                    if clients != self.home.feeds.clients {
                        self.home.feeds.clients = clients;
                        ctx.request_repaint();
                    }
                }
            }
            (ThumbTicket::Asset { artifact_id, revision, dpi, page }, Ok(payload @ RpcPayload::Image { .. })) => {
                let Some(image) = payload.png_image() else {
                    thumb_failed(ctx, &artifact_id, revision, dpi);
                    return;
                };
                let texture = ctx.load_texture(format!("thumb-{artifact_id}"), image, egui::TextureOptions::LINEAR);
                with_cache(ctx, |cache| {
                    let entry = cache.entry(artifact_id).or_default();
                    entry.pending = None;
                    entry.page = Some(page);
                    entry.shown = Some(ThumbImage { texture, revision, dpi, page });
                });
                ctx.request_repaint();
            }
            (ThumbTicket::Proposal { proposal_id, dpi }, result) => self.proposal_previewed(ctx, &proposal_id, dpi, result),
            (ThumbTicket::Render { artifact_id, revision, dpi } | ThumbTicket::Asset { artifact_id, revision, dpi, .. }, _) => {
                thumb_failed(ctx, &artifact_id, revision, dpi);
            }
        }
    }
}

fn parse_list<T: serde::de::DeserializeOwned>(value: &Value, field: &str) -> Vec<T> {
    value.get(field).cloned().and_then(|list| serde_json::from_value(list).ok()).unwrap_or_default()
}

/// One agent shown in the home top bar (§5.5), from the host's connections.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ConnectedAgent {
    /// The display name of the runtime or the MCP client's own name.
    pub name: String,
    /// The runtime ID when known, for its colour and initial.
    pub runtime: String,
    /// The job of an app-started agent, when the host reported it.
    pub job_id: Option<String>,
    /// An MCP client the app did not start (plug glyph, no ring) rather than an app job (initial).
    pub outside: bool,
}

/// The agents among the connections, one per agent: the window itself and command lines are not
/// agents; connections with the same name (or runtime) are one agent; oldest first.
pub(super) fn connected_agents(clients: &[ClientInfo]) -> Vec<ConnectedAgent> {
    let mut agents: Vec<ConnectedAgent> = Vec::new();
    for client in clients {
        if matches!(client.role.as_deref(), Some("gui") | Some("cli")) {
            continue;
        }
        let raw = client.client_name.clone().unwrap_or_default();
        let lower = raw.to_lowercase();
        let runtime = match client.runtime.as_deref().filter(|runtime| !runtime.is_empty()) {
            Some(runtime) => runtime.to_owned(),
            None if lower.contains("claude") => "claude".into(),
            None if lower.contains("antigravity") => "agy".into(),
            None if lower.contains("codex") => "codex".into(),
            None => String::new(),
        };
        let name = match runtime.as_str() {
            "claude" => "Claude".to_owned(),
            "agy" => "AGY".to_owned(),
            "codex" => "Codex CLI".to_owned(),
            _ if !raw.is_empty() => raw,
            _ => continue,
        };
        if agents.iter().any(|agent| agent.name == name) {
            continue;
        }
        agents.push(ConnectedAgent { name, runtime, job_id: client.job_id.clone(), outside: client.job_id.is_none() });
    }
    agents
}

/// The picture the thumbnail cache shows under `key`, with the page it was rendered from (the proposal rows read theirs here).
pub(super) fn picture_of(ctx: &Context, key: &str) -> Option<(egui::TextureHandle, Size)> {
    with_cache(ctx, |cache| cache.get(key).and_then(|entry| entry.shown.as_ref()).map(|shown| (shown.texture.clone(), shown.page)))
}

/// Drops the cached pictures that `is_dropped` names (a list forgets the pictures of the rows it no longer holds).
pub(super) fn drop_pictures(ctx: &Context, is_dropped: impl Fn(&str) -> bool) {
    with_cache(ctx, |cache| cache.retain(|key, _| !is_dropped(key.as_str())));
}

pub(super) fn thumb_failed(ctx: &Context, artifact_id: &str, revision: u64, dpi: u32) {
    with_cache(ctx, |cache| {
        let entry = cache.entry(artifact_id.to_owned()).or_default();
        entry.pending = None;
        entry.failed = Some((revision, dpi));
    });
}

#[cfg(test)]
mod thumb_tests {
    use super::*;

    const A4: Size = Size { width: 210 * designllm_contracts::EMU_PER_MM, height: 297 * designllm_contracts::EMU_PER_MM };

    #[test]
    fn the_dpi_gives_the_box_its_physical_pixels() {
        // An A4 page (8.27 in wide) filling a 64 pt row thumbnail at 1x needs 64 / 8.27 = 7.7 -> 8 dpi.
        assert_eq!(wanted_dpi(A4, Vec2::new(64.0, 44.0), Fit::Cover, 1.0), 8);
        // At 2x the same box needs twice the pixels.
        assert_eq!(wanted_dpi(A4, Vec2::new(64.0, 44.0), Fit::Cover, 2.0), 16);
        // A grid tile shows the whole page: the height decides (156 pt for 11.69 in).
        assert_eq!(wanted_dpi(A4, Vec2::new(240.0, 180.0), Fit::Contain { margin: 12.0 }, 1.0), 14);
    }

    #[test]
    fn a_cover_thumbnail_keeps_the_top_of_a_portrait_page() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(64.0, 44.0));
        let (screen, uv) = placement(A4, rect, Fit::Cover);
        assert_eq!(screen, rect);
        assert!(uv.min.x.abs() < 1e-4 && uv.min.y == 0.0);
        // 64 pt wide makes the page 90.5 pt tall; 44 of it are shown.
        assert!((uv.max.y - 44.0 / 90.51).abs() < 1e-3);
    }

    #[test]
    fn home_canvas_thumbnail_selects_the_first_regular_page() {
        let artifact = json!({ "body": { "type": "canvas", "document": { "pages": [
            { "id": "component", "role": "component" },
            { "id": "first", "role": null },
            { "id": "second" }
        ] } } });
        assert_eq!(first_regular_page_index(&artifact), Some(1));
        let only_components = json!({ "body": { "type": "canvas", "document": { "pages": [
            { "id": "component-a", "role": "component" }, { "id": "component-b", "role": "component" }
        ] } } });
        assert_eq!(first_regular_page_index(&only_components), None);
        assert_eq!(first_regular_page_index(&json!({ "body": { "type": "slides", "document": { "pages": [] } } })), Some(0));
    }

    fn client(role: &str, name: Option<&str>, job: Option<&str>, runtime: Option<&str>) -> ClientInfo {
        ClientInfo {
            connection_id: Some(format!("{role}-{name:?}-{job:?}")),
            role: Some(role.into()),
            client_name: name.map(str::to_owned),
            client_version: None,
            job_id: job.map(str::to_owned),
            runtime: runtime.map(str::to_owned),
            connected_at_ms: Some(1),
        }
    }

    #[test]
    fn the_top_bar_shows_each_agent_once_by_its_proper_name() {
        let clients = [
            client("gui", None, None, None),
            client("bridge", Some("claude-code"), Some("j1"), Some("claude")),
            client("bridge", Some("claude-code"), Some("j2"), Some("claude")),
            client("bridge", Some("codex-mcp-client"), None, None),
            client("cli", None, None, None),
            client("bridge", Some("Cursor"), None, None),
            client("bridge", None, Some("j3"), Some("agy")),
            client("bridge", Some("Antigravity"), None, None),
            client("bridge", None, None, None),
        ];
        let agents = connected_agents(&clients);
        let names: Vec<&str> = agents.iter().map(|agent| agent.name.as_str()).collect();
        assert_eq!(names, ["Claude", "Codex CLI", "Cursor", "AGY"]);
        assert_eq!(agents[0].job_id.as_deref(), Some("j1"));
        assert_eq!(agents[1].runtime, "codex");
        assert_eq!(agents[3].job_id.as_deref(), Some("j3"));
        assert_eq!(super::super::home::agent_name("agy", None), "AGY");
        assert_eq!(super::super::chrome::agent_letter("agy"), "A");
        assert!(agents[1].outside && !agents[0].outside, "an outside MCP client has no job");
    }

    #[test]
    fn a_contained_page_is_centred_whole() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(240.0, 180.0));
        let (screen, uv) = placement(A4, rect, Fit::Contain { margin: 12.0 });
        assert!((screen.height() - 156.0).abs() < 0.01);
        assert!((screen.center().x - 120.0).abs() < 0.01);
        assert_eq!(uv, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)));
    }
}
