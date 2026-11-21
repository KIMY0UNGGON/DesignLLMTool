//! 68i (review 2): a render that draws nothing takes no raster permit. The permits are taken here by hand: a render that needs one
//! gives up at once (`stop` is true), and the cached `render.pixels` answer must still come.

use std::path::PathBuf;
use std::sync::Arc;

use designllm_contracts::*;
use designllm_core::{Dispatch, RenderJob, Workspace};
use serde_json::{json, Value};

use super::draw;
use crate::rasters::RasterPermits;

// The integration tests' test canvas engine, so the workspace has a page to draw.
#[allow(dead_code)]
#[path = "../tests/support/engine.rs"]
mod engine;

/// Every pixel opaque white, at the contract size of the page or region (as the core's test renderer draws).
struct PixelRenderer;

impl RenderProvider for PixelRenderer {
    fn shape_text(&self, _text: &str, _style: &Style) -> DltResult<ShapedText> {
        Err(DltError::unsupported("no text in this test"))
    }
    fn encode(&self, _scene: &RenderScene, _format: &str, _profile: &RenderProfile, _assets: &dyn AssetStore) -> DltResult<EncodedOutput> {
        Err(DltError::unsupported("no PNG in this test"))
    }
    fn raster_region(&self, scene: &RenderScene, profile: &RenderProfile, region: Option<Rect>, _assets: &dyn AssetStore) -> DltResult<RasterPixels> {
        let page = &scene.pages[0];
        let area = region.unwrap_or(Rect { x: 0, y: 0, width: page.size.width, height: page.size.height });
        let (width, height) = (pixels(area.width, profile.dpi)?, pixels(area.height, profile.dpi)?);
        Ok(RasterPixels { width, height, rgba_premultiplied: vec![255; width as usize * height as usize * 4], diagnostics: vec![] })
    }
    fn font_catalog_fingerprint(&self) -> String {
        "host-draw-test".into()
    }
    fn available_fonts(&self) -> Vec<String> {
        Vec::new()
    }
    fn hit_test(&self, _page: &RenderPage, _point: Point) -> DltResult<Option<HitTestResult>> {
        Err(DltError::unsupported("no hit test in this test"))
    }
    fn caret_rect(&self, _page: &RenderPage, _position: &TextPosition) -> DltResult<Rect> {
        Err(DltError::unsupported("no caret in this test"))
    }
    fn selection_rects(&self, _page: &RenderPage, _range: &TextRange) -> DltResult<Vec<Rect>> {
        Err(DltError::unsupported("no selection in this test"))
    }
}

fn request(method: &str, params: Value) -> RpcRequest {
    RpcRequest { protocol_version: PROTOCOL_VERSION, id: new_id(), method: method.into(), params }
}

/// A workspace with one test canvas artifact (its id is returned).
fn workspace(name: &str) -> (PathBuf, Workspace, String) {
    let root = std::env::temp_dir().join(format!("designllm-host-draw-{name}-{}", new_id()));
    std::fs::create_dir_all(&root).expect("the test workspace folder");
    let mut ws = Workspace::open(&root, vec![Arc::new(engine::TestCanvasEngine)], Arc::new(PixelRenderer)).expect("the workspace opens");
    let created = ws.dispatch(request("artifact.create", json!({ "request_id": new_id(), "actor": "user", "kind": "canvas", "title": "Test canvas" })));
    let artifact = created.result.as_ref().and_then(|r| r["id"].as_str()).map(str::to_owned);
    let artifact = artifact.unwrap_or_else(|| panic!("artifact.create: {:?}", created.error));
    (root, ws, artifact)
}

/// A `render.get` or `render.pixels` the way a connection gets it: checked under the workspace, and a job to draw.
fn render_job(ws: &mut Workspace, method: &str, params: Value) -> Box<RenderJob> {
    match ws.dispatch_start(request(method, params), None) {
        Dispatch::Render(job) => job,
        Dispatch::Answered(response) => panic!("a render is answered before it draws: {:?}", response.error),
        _ => panic!("a render is drawn outside the workspace"),
    }
}

#[test]
fn a_cached_pixels_answer_is_given_while_every_raster_permit_is_taken() {
    let (root, mut ws, doc) = workspace("cached");
    let params = json!({ "artifact_id": doc, "expected_revision": 0, "page_index": 0, "profile": { "dpi": 48, "transparent": false } });
    let first = render_job(&mut ws, "render.pixels", params.clone());
    let drawn = draw(&first, &RasterPermits::default(), || false).expect("the first render draws");
    let (response, _) = ws.finish_render(first, Ok(drawn));
    assert!(response.error.is_none(), "{:?}", response.error);

    // Both permits are taken. The cached answer draws nothing, so it needs no permit: it is given with `stop` already true.
    let permits = RasterPermits::default();
    let held = [permits.try_acquire(1), permits.try_acquire(1)];
    assert!(held.iter().all(Option::is_some), "both permits are taken");
    let again = render_job(&mut ws, "render.pixels", params);
    assert_eq!(again.draw_pixels().expect("the page fits"), None, "held pixels draw nothing");
    let answered = draw(&again, &permits, || true).expect("the cached answer is given without a permit");
    let (response, pixels) = ws.finish_render(again, Ok(answered));
    assert!(response.error.is_none(), "{:?}", response.error);
    assert!(pixels.is_some(), "the held pixels are sent");

    // A render that does draw needs a permit: with `stop` already true it gives up at once, as Canceled.
    let fresh = render_job(&mut ws, "render.pixels", json!({ "artifact_id": doc, "expected_revision": 0, "page_index": 0, "profile": { "dpi": 49, "transparent": false } }));
    assert!(fresh.draw_pixels().expect("the page fits").is_some(), "a render that draws has a size");
    assert_eq!(draw(&fresh, &permits, || true).err().expect("it waits for a permit").code, ErrorCode::Canceled);

    // A page that does not exist is refused before any permit is waited for: no permit is free here, and it is still refused.
    let missing = render_job(&mut ws, "render.pixels", json!({ "artifact_id": doc, "expected_revision": 0, "page_index": 7, "profile": { "dpi": 49, "transparent": false } }));
    assert_eq!(draw(&missing, &permits, || true).err().expect("refused").code, ErrorCode::InvalidArgument);
    drop(held);
    drop(ws);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_page_the_document_does_not_have_is_refused_with_the_page_count_by_both_renders() {
    let (root, mut ws, doc) = workspace("missing-page");
    let permits = RasterPermits::default();
    let held = [permits.try_acquire(1), permits.try_acquire(1)];
    for method in ["render.get", "render.pixels"] {
        let job = render_job(&mut ws, method, json!({ "artifact_id": doc, "expected_revision": 0, "page_index": 7, "profile": { "dpi": 49, "transparent": false } }));
        let error = draw(&job, &permits, || true).err().expect("refused before a permit is waited for");
        assert_eq!(error.code, ErrorCode::InvalidArgument, "{method}");
        assert!(error.message.contains("does not exist; the document has 1 page(s)"), "{method}: {}", error.message);
    }
    drop(held);
    drop(ws);
    let _ = std::fs::remove_dir_all(root);
}