use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        mpsc::{Receiver, Sender},
        Arc, Mutex,
    },
    time::Instant,
};

use designllm_bridge::{
    catalog::Scope,
    mcp::{McpServer, Routed, ToolBackend},
    DltResult,
};
use designllm_contracts::{
    new_id, Artifact, ArtifactBody, ArtifactSummary, AssetRef, ChangeSet, Dataset, DltError, FigureDocument, Intent, Operation,
    Page, RpcRequest, Size, PROTOCOL_VERSION,
};
use eframe::egui::{epaint::ClippedShape, Context, Event, Modifiers, PointerButton, Pos2, RawInput, Rect, Vec2, ViewportId};
use serde_json::{json, Value};

use super::{CsvImportDraft, DesktopApp};
use crate::{
    notice::Severity,
    rpc::{RpcPayload, RpcReply, RpcWorker, Work},
    theme,
};

pub(super) type Request = (u64, String, Value);

pub(super) struct Harness {
    pub(super) app: DesktopApp,
    pub(super) replies: Sender<RpcReply>,
    pub(super) work: Receiver<Work>,
    pub(super) ctx: Context,
}

impl Harness {
    pub(super) fn new() -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        Self {
            app: DesktopApp::new(rpc, "작업공간".into(), None, None),
            replies,
            work,
            ctx: Context::default(),
        }
    }

    /// The requests the window sent since the last call, oldest first.
    pub(super) fn sent(&mut self) -> Vec<Request> {
        let mut batch = Vec::new();
        while let Ok(work) = self.work.try_recv() {
            if let Work::Call { ticket, method, params, .. } = work {
                batch.push((ticket, method, params));
            }
        }
        batch
    }

    /// Answers the newest request for `method` and lets the window handle the reply.
    pub(super) fn answer(&mut self, requests: &[Request], method: &str, result: Result<Value, DltError>) {
        let ticket = requests
            .iter()
            .rev()
            .find(|request| request.1 == method)
            .map(|request| request.0)
            .unwrap_or_else(|| panic!("the window sent no {method} request"));
        self.replies
            .send(RpcReply { ticket, result: result.map(RpcPayload::Json) })
            .expect("the window still listens");
        self.app.process_replies(&self.ctx);
    }

    pub(super) fn message(&self) -> Option<(Severity, String)> {
        self.app
            .notices
            .current(Instant::now())
            .map(|notice| (notice.severity, notice.text.clone()))
    }

    /// Opens one document the way the window does at start-up and answers its context read.
    /// Returns the requests of the opening and the id of the window's view.
    pub(super) fn open_document(&mut self, kind: &str, revision: u64) -> (Vec<Request>, String) {
        let startup = self.sent();
        let methods: Vec<&str> = startup.iter().map(|request| request.1.as_str()).collect();
        assert_eq!(methods, ["system.hello", "workspace.list"]);
        self.answer(
            &startup,
            "workspace.list",
            Ok(json!({ "workspace_id": "w1", "artifacts": [summary("a1", kind, revision)] })),
        );
        // The window starts on the home screen; the user opens the document from its list.
        self.app.invoke_ui("nav.open_project", json!({ "artifact_id": "a1" }));
        let opening = self.sent();
        let view_id = opening
            .iter()
            .find(|request| request.1 == "context.read")
            .and_then(|request| request.2["view_id"].as_str())
            .expect("the context read names the view")
            .to_owned();
        self.answer(&opening, "context.read", Ok(context("a1", &view_id, revision)));
        (opening, view_id)
    }
}

pub(super) fn summary(id: &str, kind: &str, revision: u64) -> Value {
    json!({ "id": id, "kind": kind, "title": "문서", "revision": revision, "engine_id": "engine", "dirty": false })
}

pub(super) fn supported() -> Value {
    json!({ "read": "supported", "render": "supported", "edit": "supported", "preserve": "supported", "write": "supported", "reason": "" })
}

pub(super) fn context(artifact_id: &str, view_id: &str, revision: u64) -> Value {
    let features = [
        "text", "text_style", "geometry", "pages", "paragraph_format", "native_roundtrip",
        "render_svg", "render_pdf", "render_png", "dataset", "plot", "diagram", "figure_panels",
        "evidence", "figure_links",
    ];
    let capabilities: serde_json::Map<String, Value> =
        features.iter().map(|name| ((*name).to_owned(), supported())).collect();
    json!({
        "workspace_id": "w1",
        "artifact_id": artifact_id,
        "revision": revision,
        "view_id": view_id,
        "selection": { "node_ids": [], "text_range": null },
        "visible_page": 0,
        "selected_properties": { "nodes": null, "stale_selection": null, "render_unavailable": null },
        "capabilities": capabilities,
        "render_snapshot": null,
        "view_transform": null,
    })
}

pub(super) fn methods(requests: &[Request]) -> Vec<&str> {
    requests.iter().map(|request| request.1.as_str()).collect()
}

pub(super) fn reload_methods() -> [&'static str; 4] {
    ["workspace.list", "context.read", "artifact.inspect", "render.get"]
}

pub(super) fn edit() -> (Vec<Operation>, std::collections::BTreeSet<Intent>) {
    (
        vec![Operation::SetCaption { caption: "설명".into() }],
        [Intent::Content].into_iter().collect(),
    )
}

pub(super) fn snapshot(artifact_id: &str, revision: u64) -> Value {
    json!({
        "id": "snapshot-1",
        "artifact_id": artifact_id,
        "revision": revision,
        "profile": { "dpi": 144, "transparent": false },
        "scene": {
            "pages": [{ "id": "page-1", "page_index": 0, "size": { "width": 12192000, "height": 6858000 }, "primitives": [], "source_map": [] }],
            "font_fingerprint": "",
            "diagnostics": [],
            "metadata": { "total_pages": 1 }
        },
        "preview": null,
        "preview_transform": null,
    })
}

pub(super) fn screen(scale: f32, width: f32, height: f32) -> RawInput {
    let mut input = RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(width, height))),
        ..Default::default()
    };
    input
        .viewports
        .get_mut(&ViewportId::ROOT)
        .expect("root viewport exists")
        .native_pixels_per_point = Some(scale);
    input
}

/// The text of everything one frame drew, so a test can tell which controls were on screen.
pub(super) fn drawn_text(output: &eframe::egui::FullOutput) -> Vec<String> {
    use eframe::egui::epaint::Shape;
    fn walk(shape: &Shape, found: &mut Vec<String>) {
        match shape {
            Shape::Text(text) => found.push(text.galley.text().to_owned()),
            Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    for clipped in &output.shapes {
        walk(&clipped.shape, &mut found);
    }
    found
}

/// The repository root as a plain absolute path (the host refuses `..` components in import paths).
pub(super) fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/desktop sits two levels below the repository root")
        .to_path_buf()
}

/// The host the window talks to in these tests: a real workspace with the figure, canvas, slide,
/// Word, HWPX and HWP engines, the real store and the real renderer.
pub(super) struct Host {
    workspace: Option<designllm_core::Workspace>,
    pub(super) workspace_id: String,
    root: PathBuf,
}

impl Host {
    pub(super) fn start() -> Self {
        let root = std::env::temp_dir().join(format!("designllm-desktop-figure-{}", new_id()));
        std::fs::create_dir_all(&root).expect("create the scratch workspace");
        let workspace = Self::open_workspace(&root);
        let workspace_id = workspace.id().to_owned();
        Self { workspace: Some(workspace), workspace_id, root }
    }

    /// The host stops and a new one opens the same workspace: what it stored stays, what it kept in memory (views, the UI
    /// channel) is gone.
    pub(super) fn restart(&mut self) {
        self.workspace = None;
        self.workspace = Some(Self::open_workspace(&self.root));
    }

    fn open_workspace(root: &Path) -> designllm_core::Workspace {
        let fonts = repository_root().join("assets/fonts");
        let renderer = Arc::new(designllm_render::Renderer::new(&fonts).expect("bundled fonts"));
        let engines: Vec<Arc<dyn designllm_contracts::EngineAdapter>> = vec![
            Arc::new(designllm_figures::FigureEngine::default()),
            Arc::new(designllm_canvas::CanvasEngine::default()),
            Arc::new(designllm_canvas::WebEngine::default()),
            Arc::new(designllm_slides::SlidesEngine::default()),
            Arc::new(designllm_docx::DocxEngine::default()),
            Arc::new(designllm_hwpx::HwpxEngine::default()),
            Arc::new(designllm_hwp::HwpEngine::default()),
        ];
        let mut workspace =
            designllm_core::Workspace::open(&root.join("workspace"), engines, renderer).expect("open");
        workspace.set_source_handlers(designllm_sources::extract_pdf, designllm_figures::import_csv);
        workspace.set_csv_declared_handler(designllm_figures::import_csv_declared);
        workspace.set_dataset_editor(designllm_figures::edit_dataset);
        workspace
    }

    pub(super) fn call(&mut self, method: &str, params: Value) -> Result<Value, DltError> {
        let response = self
            .workspace
            .as_mut()
            .expect("the host is running")
            .dispatch(RpcRequest {
                protocol_version: PROTOCOL_VERSION,
                id: new_id(),
                method: method.into(),
                params,
            });
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(result), None) => Ok(result),
            (None, None) => Ok(Value::Null),
        }
    }

    /// The core itself, for what the host does around a call (49w: the review gate's preview of a history request).
    pub(super) fn workspace(&mut self) -> &mut designllm_core::Workspace {
        self.workspace.as_mut().expect("the host is running")
    }

    pub(super) fn set_job_status_for_test(
        &mut self,
        job_id: &str,
        attempt_id: &str,
        status: designllm_contracts::JobStatus,
        error: Option<DltError>,
    ) -> designllm_contracts::DltResult<designllm_contracts::Job> {
        self.workspace
            .as_mut()
            .expect("the host is running")
            .set_job_status(
                &designllm_contracts::JobRef { job_id: job_id.into(), attempt_id: attempt_id.into() },
                status,
                error,
            )
    }

    pub(super) fn create(&mut self, kind: &str, title: &str) -> String {
        let summary = self
            .call(
                "artifact.create",
                json!({ "request_id": new_id(), "actor": "desktop", "kind": kind, "title": title, "size": null, "template": null }),
            )
            .unwrap_or_else(|error| panic!("a {kind} can be created without a size: {error:?}"));
        serde_json::from_value::<ArtifactSummary>(summary).expect("summary").id
    }

    pub(super) fn create_figure(&mut self) -> String {
        self.create("figure", "그림")
    }

    pub(super) fn artifact(&mut self, artifact_id: &str) -> Artifact {
        let value = self
            .call("artifact.inspect", json!({ "artifact_id": artifact_id }))
            .expect("inspect");
        serde_json::from_value(value).expect("artifact")
    }

    pub(super) fn figure(&mut self, artifact_id: &str) -> (FigureDocument, u64) {
        let artifact = self.artifact(artifact_id);
        match artifact.body {
            ArtifactBody::Figure(figure) => (figure, artifact.revision),
            _ => panic!("not a figure"),
        }
    }

    /// Sends operations exactly as the window does and returns the new revision.
    pub(super) fn apply(
        &mut self,
        artifact_id: &str,
        operations: Vec<Operation>,
        intents: BTreeSet<Intent>,
    ) -> Result<u64, DltError> {
        let revision = self.artifact(artifact_id).revision;
        let change = change_set("desktop", &self.workspace_id, artifact_id, revision, intents, operations);
        let result = self.call("changes.apply", serde_json::to_value(change).expect("change set"))?;
        Ok(result["revision"].as_u64().expect("new revision"))
    }

    pub(super) fn import_table(&mut self, csv: &str) -> Dataset {
        let path = self.root.join("table.csv");
        std::fs::write(&path, csv).expect("write the table");
        let asset = self
            .call("asset.import", json!({ "path": path.to_string_lossy() }))
            .expect("import");
        let dataset = self
            .call("source.import_csv", json!({ "sha256": asset["sha256"] }))
            .expect("the CSV handler reads the table");
        serde_json::from_value(dataset).expect("dataset")
    }

    /// The window's own import of a table: read with inferred types, let the user declare the
    /// columns (`declare` plays the user), then read again with exactly those declarations.
    pub(super) fn declared_draft(&mut self, csv: &str, declare: impl FnOnce(&mut CsvImportDraft)) -> CsvImportDraft {
        let mut draft = CsvImportDraft::to_declare(self.import_table(csv));
        declare(&mut draft);
        let declared = self
            .call("source.import_csv", draft.declaration_request())
            .expect("the host reads the table with the declared columns");
        draft.adopt(serde_json::from_value(declared).expect("dataset"));
        draft
    }
}

/// A canvas whose only page is `page-1`, with no nodes. The page `artifact.create` makes is replaced, so canvases built this way
/// have the same ids and the same body.
pub(super) fn canvas_with_page(host: &mut Host, title: &str) -> String {
    let id = host.create("canvas", title);
    let first = match &host.artifact(&id).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("a canvas"),
    };
    let page = Page::new("page-1", Size::slide());
    let intents = [Intent::Content, Intent::Design].into_iter().collect();
    host.apply(&id, vec![Operation::InsertPage { index: 0, page }, Operation::RemovePage { page_id: first }], intents).expect("the page");
    id
}

impl Drop for Host {
    fn drop(&mut self) {
        self.workspace = None;
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The one `ToolBackend` of the desktop tests: MCP tool calls reach the real host through `Host::call`.
pub(super) struct HostBackend(Mutex<Host>);

impl HostBackend {
    pub(super) fn new(host: Host) -> Arc<Self> {
        Arc::new(Self(Mutex::new(host)))
    }

    /// Reads or changes the host between tool calls.
    pub(super) fn with<T>(&self, use_host: impl FnOnce(&mut Host) -> T) -> T {
        use_host(&mut self.0.lock().expect("the host is not poisoned"))
    }
}

impl ToolBackend for HostBackend {
    fn call(&self, method: &str, params: Value) -> DltResult<Value> {
        self.with(|host| host.call(method, params))
    }
}

/// A window whose requests the host answers at once, as its RPC worker would: a registered command then runs to the end
/// of its change set, and the reads after it are answered too.
pub(super) struct Window {
    pub(super) app: DesktopApp,
    pub(super) replies: Sender<RpcReply>,
    pub(super) work: Receiver<Work>,
    pub(super) ctx: Context,
    /// Whether the window reads its page previews back (its page textures); see `open_with_previews`.
    previews: bool,
    /// The frame clock (seconds): each frame moves it on, so two clicks in consecutive frames are a double click.
    clock: f64,
    /// Every JSON request the window sent, with the host's answer (null when the host refused it), oldest first.
    pub(super) calls: Vec<(String, Value, Value)>,
    /// The shapes of the last frame `frame` drew.
    pub(super) shapes: Vec<ClippedShape>,
    /// The RPC worker's own preview path (68i, pixels.rs) for `render.get` and preview reads, and whether the host is one without
    /// `render.pixels` (`open_with_pixel_previews`).
    pixel_previews: Option<(crate::pixels::Previews, bool)>,
}

impl Window {
    /// A window that shows the artifact `artifact_id` of `host`.
    pub(super) fn open(host: &mut Host, artifact_id: &str) -> Self {
        Self::open_with(host, artifact_id, false)
    }

    /// A window that shows `artifact_id` and reads its page previews back, so that its board holds the pages it draws and
    /// hit-tests on.
    pub(super) fn open_with_previews(host: &mut Host, artifact_id: &str) -> Self {
        Self::open_with(host, artifact_id, true)
    }

    /// A window whose previews go through the RPC worker's raw-pixel path (`render.pixels`, held pixels, decode) against
    /// `host`, or against a host without `render.pixels` (`old_host`).
    pub(super) fn open_with_pixel_previews(host: &mut Host, artifact_id: &str, old_host: bool) -> Self {
        Self::open_inner(host, artifact_id, true, Some((crate::pixels::Previews::default(), old_host)))
    }

    fn open_with(host: &mut Host, artifact_id: &str, previews: bool) -> Self {
        Self::open_inner(host, artifact_id, previews, None)
    }

    fn open_inner(host: &mut Host, artifact_id: &str, previews: bool, pixel_previews: Option<(crate::pixels::Previews, bool)>) -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let ctx = Context::default();
        theme::install(&ctx);
        // The caret blinks with the clock, and the tests read the caret of one frame: the blink is off.
        ctx.all_styles_mut(|style| style.visuals.text_cursor.blink = false);
        let mut window = Self {
            app: DesktopApp::new(rpc, "작업공간".into(), None, None),
            replies,
            work,
            ctx,
            previews,
            clock: 1.0,
            calls: Vec::new(),
            shapes: Vec::new(),
            pixel_previews,
        };
        window.settle(host);
        let list = host.call("workspace.list", json!({})).expect("the work list");
        let summary = list["artifacts"]
            .as_array()
            .and_then(|all| all.iter().find(|item| item["id"] == artifact_id))
            .cloned()
            .expect("the artifact is in the work list");
        window.app.activate(serde_json::from_value::<ArtifactSummary>(summary).expect("a summary"));
        window.settle(host);
        window
    }

    /// Answers the window's requests until it asks for nothing more.
    pub(super) fn settle(&mut self, host: &mut Host) {
        settle_work(&self.work, &mut self.app, &self.ctx, |item| match item {
            Work::Call { ticket, method, params, preview_pixels: true } if method == "render.get" && self.pixel_previews.is_some() => {
                let (previews, old) = self.pixel_previews.as_mut().expect("pixel previews");
                let backend = HostRpc::new(host, *old);
                let result = previews.render(&backend, ticket, params).map(RpcPayload::Json);
                self.calls.extend(backend.calls.into_inner());
                self.replies.send(RpcReply { ticket, result }).expect("the window listens");
            }
            Work::ReadAsset { ticket, asset } if self.pixel_previews.is_some() => {
                let (previews, old) = self.pixel_previews.as_mut().expect("pixel previews");
                let backend = HostRpc::new(host, *old);
                let result = previews.picture(&backend, &asset).and_then(crate::pixels::decode);
                self.calls.extend(backend.calls.into_inner());
                self.replies.send(RpcReply { ticket, result }).expect("the window listens");
            }
            Work::Call { ticket, method, params, .. } => {
                let answer = host.call(&method, params.clone());
                let recorded = answer.as_ref().map_or(Value::Null, Value::clone);
                self.calls.push((method, params, recorded));
                let result = answer.map(RpcPayload::Json);
                self.replies.send(RpcReply { ticket, result }).expect("the window listens");
            }
            Work::ReadAsset { ticket, asset } if self.previews => {
                let result = Ok(preview_pixels(host, &asset));
                self.replies.send(RpcReply { ticket, result }).expect("the window listens");
            }
            Work::Home { ticket, request } => {
                // The workspace queues the job, without the application's model runner.
                struct HomeHost<'a> {
                    host: std::cell::RefCell<&'a mut Host>,
                    calls: std::cell::RefCell<&'a mut Vec<(String, Value, Value)>>,
                }
                impl designllm_bridge::home::Rpc for HomeHost<'_> {
                    fn call(&self, method: &str, params: Value) -> DltResult<Value> {
                        let result = self.host.borrow_mut().call(method, params.clone());
                        let recorded = result.as_ref().map_or(Value::Null, Value::clone);
                        self.calls.borrow_mut().push((method.into(), params, recorded));
                        result
                    }
                }
                let rpc = HomeHost { host: (&mut *host).into(), calls: (&mut self.calls).into() };
                let result = designllm_bridge::home::create_from_home(&rpc, &request).map(|outcome| {
                    RpcPayload::Json(json!({ "artifact": outcome.artifact, "job": outcome.job, "sources": outcome.sources }))
                });
                self.replies.send(RpcReply { ticket, result }).expect("the window listens");
            }
            _ => {}
        });
    }

    /// An agent's run of `command` through the window's UI channel: the host queues it for the view, and the window's next
    /// poll delivers it and answers what it sends. Returns the run as the host keeps it (`status`, `error`).
    pub(super) fn agent_run(&mut self, host: &mut Host, command: &str, args: Value) -> Value {
        self.agent_run_granted(host, command, args, "change", &["content"])
    }

    pub(super) fn agent_run_granted(&mut self, host: &mut Host, command: &str, args: Value, permission: &str, intents: &[&str]) -> Value {
        self.app.publish_ui_if_due();
        self.settle(host);
        let view_id = self.app.ui.view_id.clone();
        let listing = host.call("ui.command.list", json!({ "view_id": view_id })).expect("the window's command registry");
        let queued = host
            .call(
                "ui.command.run",
                json!({
                    "view_id": view_id,
                    "expected_state_seq": listing["state_seq"],
                    "command_id": command,
                    "actor": "agent",
                    "permission": permission,
                    "allowed_intents": intents,
                    "args": args,
                }),
            )
            .expect("the agent's run is queued");
        self.app.last_event_poll = Instant::now() - std::time::Duration::from_secs(3);
        self.app.poll_events();
        self.settle(host);
        host.call("ui.command.get", json!({ "run_id": queued["run_id"] })).expect("the run's record")
    }

    /// One frame at `pixels_per_point` with `events`, then the host's answers to what the frame sent. The frame's shapes are
    /// kept in `shapes`.
    pub(super) fn frame(&mut self, host: &mut Host, pixels_per_point: f32, events: Vec<Event>) {
        self.clock += 0.05;
        let mut input = screen(pixels_per_point, 1280.0, 800.0);
        input.time = Some(self.clock);
        input.events = events;
        eframe::App::raw_input_hook(&mut self.app, &self.ctx, &mut input);
        // The order of `App::update`: the frame, then the board memo gesture is observed and finished on the release.
        let output = self.ctx.run(input, |ctx| {
            self.app.draw_frame(ctx);
            self.app.observe_board_memo_gesture(ctx);
            self.app.finish_board_memo_gesture_if_released(ctx);
        });
        self.shapes = output.shapes;
        self.settle(host);
    }

    /// Three frames with no input: the board learns its rectangle, then draws page 0 with its texture and transform.
    pub(super) fn draw(&mut self, host: &mut Host, pixels_per_point: f32) {
        for _ in 0..3 {
            self.frame(host, pixels_per_point, Vec::new());
        }
    }

    /// A click, or a double click (two clicks in consecutive frames), at `position`.
    pub(super) fn click(&mut self, host: &mut Host, pixels_per_point: f32, position: Pos2, double: bool) {
        self.frame(host, pixels_per_point, vec![Event::PointerMoved(position), primary_button(position, true), primary_button(position, false)]);
        if double {
            self.frame(host, pixels_per_point, vec![primary_button(position, true), primary_button(position, false)]);
        }
    }

    /// Answers the one request the window has queued, which must be for `method`, with `answer` in place of the host's
    /// answer, and lets the window handle the reply. The host is not asked.
    pub(super) fn answer_queued(&mut self, method: &str, answer: Value) {
        let batch: Vec<Work> = self.work.try_iter().collect();
        let [Work::Call { ticket, method: sent, .. }] = batch.as_slice() else {
            panic!("the window has {} requests queued, expected one {method}", batch.len());
        };
        assert_eq!(sent, method, "the queued request");
        self.replies.send(RpcReply { ticket: *ticket, result: Ok(RpcPayload::Json(answer)) }).expect("the window listens");
        self.app.process_replies(&self.ctx);
    }

    /// The notice the window shows now, if any.
    pub(super) fn message(&self) -> Option<String> {
        self.app.notices.current(Instant::now()).map(|notice| notice.text.clone())
    }

    /// Runs one registered command as the window does (`DesktopApp::invoke`) and answers what it sends.
    pub(super) fn step(&mut self, host: &mut Host, id: &str, args: Value) -> Result<(), String> {
        let result = self.app.invoke(id, args);
        self.settle(host);
        result
    }
}

/// Plays the RPC worker for a window until it asks for nothing more: `answer` takes each item of work the window queued (and
/// sends its reply, or leaves it unanswered), and the window handles the replies after each batch. A window that keeps
/// asking past 60 rounds fails the test.
pub(super) fn settle_work(work: &Receiver<Work>, app: &mut DesktopApp, ctx: &Context, mut answer: impl FnMut(Work)) {
    for _ in 0..60 {
        let batch: Vec<Work> = work.try_iter().collect();
        if batch.is_empty() {
            return;
        }
        batch.into_iter().for_each(&mut answer);
        app.process_replies(ctx);
    }
    panic!("the window keeps asking the host for more");
}

/// The preview PNG of a snapshot as the RPC worker delivers it: read back in one chunk and decoded to egui pixels.
pub(super) fn preview_pixels(host: &mut Host, asset: &AssetRef) -> RpcPayload {
    let png = super::link_flow_tests::bytes_of(host, asset);
    let rgba = image::load_from_memory_with_format(&png, image::ImageFormat::Png).expect("a PNG").to_rgba8();
    let image = eframe::egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
    RpcPayload::Image { mime: asset.mime.clone(), image }
}

/// A primary-button press or release at `position`.
pub(super) fn primary_button(position: Pos2, pressed: bool) -> Event {
    Event::PointerButton { pos: position, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
}

/// An MCP session over the host: the handshake, then tool calls through the protocol (`tools/call`).
pub(super) struct Mcp(McpServer);

impl Mcp {
    pub(super) fn connect(backend: Arc<HostBackend>) -> Self {
        // Parity tests compare engine results; the design-guide gate is tested in the bridge.
        let server = McpServer::new(backend, Scope::Workspace).without_guide_gate();
        let initialize = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": { "name": "desktop-parity", "version": "1" } }
        });
        assert!(matches!(server.route(initialize), Routed::Reply(_)), "the server answers initialize");
        assert!(matches!(server.route(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })), Routed::Nothing));
        Self(server)
    }

    /// Calls one tool and returns its structured result. A tool error fails the test: every call here must succeed.
    pub(super) fn call(&self, name: &str, arguments: Value) -> Value {
        let result = self.call_result(name, arguments);
        assert_ne!(result["isError"], true, "the tool {name} failed: {result}");
        result["structuredContent"].clone()
    }

    /// Calls one tool: its structured result, or the tool's error (`isError`) read as the host's `DltError`.
    pub(super) fn try_call(&self, name: &str, arguments: Value) -> Result<Value, DltError> {
        let mut result = self.call_result(name, arguments);
        let content = result["structuredContent"].take();
        if result["isError"] == true {
            Err(serde_json::from_value(content).unwrap_or_else(|_| panic!("the tool {name} failed without an error: {result}")))
        } else {
            Ok(content)
        }
    }

    /// Calls one tool and returns its whole result, a refusal (`isError`) included: for the refusal parity tests.
    pub(super) fn call_result(&self, name: &str, arguments: Value) -> Value {
        let request = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": name, "arguments": arguments } });
        let Routed::Call(pending) = self.0.route(request) else {
            panic!("the tool {name} is not called");
        };
        self.0.execute(&pending)
    }
}

/// The arguments of an MCP `changes_apply` call: the change set the window sends for `operations` at `revision`.
pub(super) fn change_set_arguments(workspace_id: &str, artifact_id: &str, revision: u64, intents: &[Intent], operations: Vec<Operation>) -> Value {
    let change = change_set("mcp", workspace_id, artifact_id, revision, intents.iter().copied().collect(), operations);
    serde_json::to_value(change).expect("a change set")
}

/// A change set of `actor` for `operations` at `revision`, with a new request id and no job.
fn change_set(actor: &str, workspace_id: &str, artifact_id: &str, revision: u64, intents: BTreeSet<Intent>, operations: Vec<Operation>) -> ChangeSet {
    ChangeSet {
        request_id: new_id(),
        actor: actor.into(),
        workspace_id: workspace_id.into(),
        artifact_id: artifact_id.into(),
        base_revision: revision,
        allowed_intents: intents,
        job_ref: None,
        read_dependencies: Vec::new(),
        operations,
    }
}

/// The content digest and the serialised body of an artifact. The id and the revision are left out: the two results are
/// compared by what the change made of the body.
pub(super) fn content_of(artifact: &Artifact) -> (String, String) {
    let body = serde_json::to_string(&artifact.body).expect("the body serialises");
    (designllm_contracts::sha256(body.as_bytes()), body)
}

/// The host as the RPC worker's backend (pixels.rs): `render.pixels` as the real host answers the gui connection
/// (`dispatch_start_gui`, the job drawn, `finish_render`, the pixels in binary frames of at most 8 MiB), or (`old`) refused
/// as an unknown method. Every call is recorded with its answer.
pub(super) struct HostRpc<'a> {
    host: std::cell::RefCell<&'a mut Host>,
    old: bool,
    pub(super) calls: std::cell::RefCell<Vec<(String, Value, Value)>>,
}

impl<'a> HostRpc<'a> {
    pub(super) fn new(host: &'a mut Host, old: bool) -> Self {
        Self { host: host.into(), old, calls: Vec::new().into() }
    }

    fn record(&self, method: &str, params: &Value, answer: &DltResult<Value>) {
        self.calls.borrow_mut().push((method.into(), params.clone(), answer.as_ref().map_or(Value::Null, Value::clone)));
    }
}

impl crate::pixels::Backend for HostRpc<'_> {
    fn call(&self, method: &str, params: Value) -> DltResult<Value> {
        let answer = self.host.borrow_mut().call(method, params.clone());
        self.record(method, &params, &answer);
        answer
    }

    fn call_binary(&self, method: &str, params: Value) -> DltResult<(Value, Vec<Vec<u8>>)> {
        if method != "render.pixels" {
            return self.call(method, params).map(|value| (value, Vec::new()));
        }
        if self.old {
            let refused = Err(DltError::unsupported("unknown method \"render.pixels\""));
            self.record(method, &params, &refused);
            return refused.map(|value| (value, Vec::new()));
        }
        let mut host = self.host.borrow_mut();
        let workspace = host.workspace();
        let request = RpcRequest { protocol_version: PROTOCOL_VERSION, id: new_id(), method: method.into(), params: params.clone() };
        let (response, pixels) = match workspace.dispatch_start_gui(request) {
            designllm_core::Dispatch::Render(job) => {
                let drawn = job.run();
                workspace.finish_render(job, drawn)
            }
            designllm_core::Dispatch::Answered(response) => (response, None),
            designllm_core::Dispatch::ReadPdf(_) => panic!("a render is not a PDF read"),
            designllm_core::Dispatch::OpenPdf { .. } => panic!("a render is not a PDF open"),
        };
        drop(host);
        let answer = match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (result, None) => Ok(result.unwrap_or(Value::Null)),
        };
        self.record(method, &params, &answer);
        let chunks = pixels.map(|pixels| pixels.rgba_premultiplied.chunks(designllm_contracts::MAX_FRAME_BYTES).map(<[u8]>::to_vec).collect()).unwrap_or_default();
        answer.map(|value| (value, chunks))
    }
}
