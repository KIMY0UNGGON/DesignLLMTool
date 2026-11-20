//! The renderer start of the desktop window, alone (tasks 62, 65). The executable runs this in a hidden child process
//! (`--gl-probe`, `--wgpu-probe`, `--wgpu-warp-probe`) before it opens the real window, so a driver that crashes inside
//! WGL or DirectX 12 takes down the child, not the application. It uses the same `NativeOptions` as `run_app`
//! (`renderer::options_for`, a hidden and inactive viewport), so eframe goes through the same setup: for OpenGL the
//! glutin display, pixel format, context, surface and `egui_glow::Painter`; for wgpu the DirectX 12 instance, adapter,
//! device and window surface of `egui_wgpu`.

use crate::renderer::{self, Renderer};
use designllm_contracts::{DltError, DltResult, ErrorCode};

/// Returns only if eframe refused to start (the error says why). Once the renderer is ready, eframe calls the app
/// creator, which ends the process with exit code 0: no frame is drawn.
pub fn run(renderer: Renderer) -> DltResult<()> {
    let viewport = eframe::egui::ViewportBuilder::default()
        .with_title("DesignLLMTool")
        .with_visible(false)
        .with_active(false);
    let options = renderer::options_for(renderer, viewport);
    type Created = Result<Box<dyn eframe::App>, Box<dyn std::error::Error + Send + Sync>>;
    eframe::run_native("DesignLLMTool", options, Box::new(|_cc: &eframe::CreationContext<'_>| -> Created { std::process::exit(0) }))
        .map_err(|error| DltError::new(ErrorCode::Unavailable, error.to_string()))
}
