//! designllm-canvas: two `EngineAdapter` implementations for the shared core.
//!
//! - `CanvasEngine` (kind `Canvas`): pages of absolute, row, and column layouts with font-backed text, shapes, lines,
//!   and images. Breakpoints use the page width. Click interactions are declarative (navigate, toggle, variant, OpenUrl).
//!   Native format is a JSON project with referenced image bytes embedded and hash-checked. Export: HTML (self-contained),
//!   and PNG, SVG, or PDF through the injected renderer.
//! - `WebEngine` (kind `WebCode`): a source project of HTML, CSS, and JS files. Edits are whole-file replacement
//!   (`replace_web_source`) or mapped text and attribute values (`mapped_web_edit`), both hash-checked. Native format is
//!   a JSON project. Export: ZIP source folder and single-file HTML. Live preview is in `designllm-web-preview`.
//!
//! Neither engine executes project code, and neither renders a web page itself.

mod archive;
mod canvas;
mod common;
mod digest;
mod native;
mod web;

pub use canvas::{CanvasEngine, CANVAS_NATIVE_MIME};
pub use web::{WebEngine, WEB_NATIVE_MIME};

/// Engine ID stored in `Artifact.engine_id` for canvas artifacts.
pub const CANVAS_ENGINE_ID: &str = "designllm-canvas";
/// Engine ID stored in `Artifact.engine_id` for web source artifacts.
pub const WEB_ENGINE_ID: &str = "designllm-web";
