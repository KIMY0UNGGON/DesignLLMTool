//! 68m item 4: the window's pixel path end to end against a real host in this process. `Previews` (the RPC worker's code) asks
//! `render.pixels` over the gui connection; the host sends the pixels as binary frames of at most 8 MiB, and the client
//! reassembles them. The same page drawn as a PNG (`render.get`) from the same host must show the same pixels.

use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use designllm_bridge::{Client, ClientRole};
use designllm_contracts::{new_id, AssetRef, EngineAdapter, MAX_FRAME_BYTES};
use designllm_core::Workspace;
use designllm_host::{HostMode, HostOptions, HostReady};
use eframe::egui::ColorImage;
use serde_json::json;

use super::{decode, Picture, Previews};
use crate::rpc::RpcPayload;

/// Serves the workspace at `root` in this process, with the real canvas engine and renderer, until `host.stop`. The receiver
/// gets a message once the pipe listens.
fn start_host(root: &Path) -> (std::thread::JoinHandle<()>, mpsc::Receiver<()>) {
    let fonts = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).expect("the repository root").join("assets/fonts");
    let renderer = Arc::new(designllm_render::Renderer::new(&fonts).expect("bundled fonts"));
    let engines: Vec<Arc<dyn EngineAdapter>> = vec![Arc::new(designllm_canvas::CanvasEngine::default())];
    let workspace = Workspace::open(root, engines, renderer).expect("the workspace opens");
    let (ready_tx, ready) = mpsc::channel();
    let ready_tx = Mutex::new(ready_tx);
    let options = HostOptions {
        // The application executable of the product is the test executable here: it may hold the gui role.
        gui_executables: vec![std::env::current_exe().expect("the test executable")],
        on_ready: Some(Arc::new(move |_: &HostReady| {
            let _ = ready_tx.lock().expect("the ready sender").send(());
        })),
        ..HostOptions::default()
    };
    let host = std::thread::spawn(move || designllm_host::serve_with(workspace, HostMode::Headless, options).expect("the host serves"));
    (host, ready)
}

fn image_of(payload: RpcPayload) -> ColorImage {
    match payload {
        RpcPayload::Image { image, .. } => image,
        _ => panic!("the window gets an image"),
    }
}

#[test]
fn a_page_sent_as_pixel_frames_over_the_gui_connection_is_the_page_the_png_shows() {
    let root = std::env::temp_dir().join(format!("designllm-desktop-pixels-host-{}", new_id())).join("workspace");
    std::fs::create_dir_all(&root).expect("the scratch workspace");
    let (host, ready) = start_host(&root);
    ready.recv_timeout(Duration::from_secs(20)).expect("the host listens");
    let gui = Client::gui(&root);

    let created = gui
        .call("artifact.create", json!({ "request_id": new_id(), "actor": "gui", "kind": "canvas", "title": "Pixels", "size": null, "template": null }))
        .expect("a canvas");
    let doc = created["id"].as_str().expect("the canvas id").to_string();
    // A4 at 150 dpi is about 8.7 MB of pixels: more than one 8 MiB frame.
    let params = json!({ "artifact_id": doc, "expected_revision": 0, "page_index": 0, "profile": { "dpi": 150, "transparent": false } });

    let mut previews = Previews::default();
    let answer = previews.render(&gui, 1, params.clone()).expect("render.pixels over the gui connection");
    let stand_in: AssetRef = serde_json::from_value(answer["preview"].clone()).expect("the stand-in preview");
    let (size, chunks) = match previews.picture(&gui, &stand_in).expect("the held pixels") {
        Picture::Pixels { size, chunks } => (size, chunks),
        Picture::Encoded { .. } => panic!("the window gets the pixels, not a PNG"),
    };
    assert!(chunks.len() >= 2, "the pixels arrive in {} frames", chunks.len());
    assert!(chunks.iter().all(|chunk| chunk.len() <= MAX_FRAME_BYTES), "no frame is over the frame limit");
    assert_eq!(chunks.iter().map(Vec::len).sum::<usize>(), size[0] * size[1] * 4, "every pixel arrived once");
    let from_pixels = image_of(decode(Picture::Pixels { size, chunks }).expect("the window decodes the pixels"));

    // The same page as a PNG from the same host.
    let png = gui.call("render.get", params).expect("render.get of the page");
    let png_preview: AssetRef = serde_json::from_value(png["preview"].clone()).expect("the PNG preview");
    let from_png = image_of(decode(previews.picture(&gui, &png_preview).expect("the PNG preview")).expect("the PNG decodes"));

    assert_eq!(from_pixels.size, from_png.size, "the same page size");
    assert!(from_pixels.pixels == from_png.pixels, "the reassembled pixels are the pixels the PNG shows");

    gui.close();
    Client::with_role(&root, ClientRole::Cli).call("host.stop", json!({})).expect("the host stops");
    host.join().expect("the host ends");
    let _ = std::fs::remove_dir_all(root.parent().expect("the scratch folder"));
}
