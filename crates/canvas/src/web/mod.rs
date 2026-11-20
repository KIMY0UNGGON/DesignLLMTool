//! WebEngine: WebCode artifacts (HTML, CSS, and JS source files in one project).
//! Source edits are hash-checked, path-checked, and never run code. Live preview is a separate host
//! (`designllm-web-preview`) that reads a project folder. Its output is not a scene, so `layout` returns no pages.

pub(crate) mod design_system;
pub(crate) mod html;
pub(crate) mod ops;
pub(crate) mod paths;

use crate::archive;
use crate::common::{blocking, fail, info, invalid, normalize_title_text, unsupported};
use crate::digest;
use crate::native;
use crate::WEB_ENGINE_ID;
use designllm_contracts::{
    sha256, Artifact, ArtifactBody, ArtifactKind, CapabilityReport, ContentDigest, CreateRequest, Diagnostic, DltError,
    DltResult, EncodedOutput, Editability, EngineAdapter, EngineContext, ErrorCode, ExportRequest, Feature,
    DesignSystemRef, FeatureSupport, OpenInput, Operation, PreparedChange, PreservationVerification, RenderScene, Size,
    Support, TokenDef, WebDocument, WebFile,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(crate) const WEB_FORMAT: &str = "designllm.web";
pub const WEB_NATIVE_MIME: &str = "application/vnd.designllm.web+json";
const MAX_FILES: usize = 512;
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;

/// Source-project engine (`designllm_canvas::WebEngine::default()`).
#[derive(Debug, Clone, Copy, Default)]
pub struct WebEngine;

fn web_doc(artifact: &Artifact) -> DltResult<&WebDocument> {
    match &artifact.body {
        ArtifactBody::WebCode(doc) => Ok(doc),
        _ => Err(invalid("KIND_MISMATCH", "artifact body is not a web source project")),
    }
}

fn is_html(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".html") || lower.ends_with(".htm")
}

/// Structural checks: entry, paths (core's per-path rules, names equal after the Windows case fold, files that are also
/// folders), hashes, sizes, and local references. Blocking entries mean the project is invalid. Every open, save, export, and edit runs this check,
/// so a project that breaks a path rule cannot be opened, saved, or changed.
pub(crate) fn validate_project(doc: &WebDocument) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if let Err(err) = paths::validate_rel(&doc.entry) {
        out.push(blocking("WEB_ENTRY_PATH", err.message, None));
        return out;
    }
    if !is_html(&doc.entry) {
        out.push(blocking("WEB_ENTRY_TYPE", "the entry file must be an HTML file", None));
    }
    if doc.files.len() > MAX_FILES {
        out.push(blocking("WEB_TOO_MANY_FILES", "project has too many files", None));
    }
    let mut total = 0usize;
    for (path, file) in &doc.files {
        if let Err(err) = paths::validate_rel(path) {
            out.push(blocking("WEB_PATH", err.message, Some(path.clone())));
        }
        if sha256(file.content.as_bytes()) != file.sha256 {
            out.push(blocking("WEB_HASH_STALE", "stored hash does not match the file content", Some(path.clone())));
        }
        if file.content.len() > ops::MAX_FILE_BYTES {
            out.push(blocking("WEB_FILE_TOO_LARGE", "file exceeds the per-file size limit", Some(path.clone())));
        }
        total += file.content.len();
    }
    for conflict in paths::conflicts(doc.files.keys().map(String::as_str)) {
        out.push(blocking("WEB_PATH", conflict.message, Some(conflict.path)));
    }
    if total > MAX_TOTAL_BYTES {
        out.push(blocking("WEB_TOTAL_TOO_LARGE", "project exceeds the total size limit", None));
    }
    match doc.files.get(&doc.entry) {
        None => out.push(blocking("WEB_ENTRY_MISSING", "entry file is not in the project", Some(doc.entry.clone()))),
        Some(entry) => {
            if let Ok(report) = html::resource_report(&doc.entry, &entry.content, &doc.files) {
                for url in report.external {
                    out.push(info("WEB_EXTERNAL_RESOURCE", format!("external resource is not fetched by the export: {url}")));
                }
                for path in report.missing {
                    out.push(info("WEB_MISSING_RESOURCE", format!("referenced local resource is missing: {path}")));
                }
            } else {
                out.push(info("WEB_HTML_UNSCANNED", "entry HTML could not be scanned for resource references"));
            }
        }
    }
    out
}

/// The `artifact.design_system` rules that core checks when a reference is set, reported as non-blocking notes on validate.
/// A stored artifact that already holds such a reference still opens and validates; core's `design_system.set` refuses it.
fn design_system_reference_notes(artifact: &Artifact) -> Vec<Diagnostic> {
    let Some(reference) = artifact.design_system.as_ref() else { return Vec::new() };
    let mut notes = Vec::new();
    if reference.artifact_id.trim().is_empty() {
        notes.push(info("WEB_DESIGN_SYSTEM_ID", "the design system reference names no artifact"));
    }
    if reference.artifact_id == artifact.id {
        notes.push(info("WEB_DESIGN_SYSTEM_SELF", "the design system reference names this artifact"));
    }
    if artifact.publishes_design_system {
        notes.push(info("WEB_DESIGN_SYSTEM_PUBLISHER", "a design system publisher cannot consume another design system"));
    }
    notes
}

fn template_html(title: &str) -> String {
    let escaped = html::escape_text(title);
    format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{escaped}</title>\n<style>\nbody {{ margin: 0; padding: 48px; font-family: sans-serif; }}\nh1 {{ font-size: 32px; }}\n</style>\n</head>\n<body>\n<h1 data-dlm-map=\"heading\">{escaped}</h1>\n</body>\n</html>\n"
    )
}

const WEBGL_ANIMATION_TEMPLATE_ID: &str = "webgl_animation";
const WEBGL_ANIMATION_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__DLM_TITLE__</title>
<style>
:root { color-scheme: dark; font-family: Inter, "Segoe UI", sans-serif; background: #0b1020; color: #f4f6ff; }
* { box-sizing: border-box; }
body { min-height: 100vh; margin: 0; display: grid; place-items: center; padding: 32px; }
.stage { position: relative; width: min(960px, 100%); min-height: 540px; overflow: hidden; border: 1px solid rgba(148, 163, 255, .24); border-radius: 28px; background: #111936; box-shadow: 0 32px 100px rgba(0, 0, 0, .42); }
#shader { position: absolute; inset: 0; width: 100%; height: 100%; opacity: .84; }
.copy { position: relative; z-index: 1; max-width: 620px; padding: 92px 64px; pointer-events: none; }
.eyebrow { color: #9eb7ff; font: 700 12px/1.2 "Segoe UI", sans-serif; letter-spacing: .18em; }
h1 { max-width: 560px; margin: 18px 0; font-size: clamp(36px, 7vw, 76px); line-height: .98; }
.summary { max-width: 420px; color: #c0c9e5; font-size: 17px; line-height: 1.55; }
.status { display: flex; align-items: center; gap: 12px; margin-top: 38px; color: #dbe4ff; font-size: 13px; }
.pulse { width: 12px; height: 12px; border-radius: 50%; background: #65e9c3; box-shadow: 0 0 18px #65e9c3; animation: pulse 760ms ease-in-out infinite alternate; }
@keyframes pulse { from { transform: scale(.72); opacity: .52; } to { transform: scale(1.18); opacity: 1; } }
footer { position: absolute; right: 26px; bottom: 22px; z-index: 1; color: #aab5d1; font: 600 11px/1.2 "Segoe UI", sans-serif; letter-spacing: .12em; }
@media (max-width: 600px) { body { padding: 12px; } .stage { min-height: 460px; border-radius: 18px; } .copy { padding: 72px 28px; } }
</style>
</head>
<body>
<main class="stage">
  <canvas id="shader" aria-label="Animated WebGL shader background"></canvas>
  <section class="copy">
    <p class="eyebrow">WEBGL MOTION STUDY</p>
    <h1>__DLM_TITLE__</h1>
    <p class="summary">An editable WebCode template combining a live fragment shader with a restrained CSS motion cue.</p>
    <div class="status"><span id="pulse" class="pulse" aria-hidden="true"></span><span>requestAnimationFrame · WebGL 1.0</span></div>
  </section>
  <footer>LOCAL SOURCE · NO REMOTE LIBRARIES</footer>
</main>
<script defer src="./animation.js"></script>
</body>
</html>
"#;

const WEBGL_ANIMATION_SCRIPT: &str = r#"(() => {
  const webview = window.chrome && window.chrome.webview;
  const post = value => {
    if (webview && typeof webview.postMessage === 'function') webview.postMessage(value);
  };
  const canvas = document.getElementById('shader');
  const pulse = document.getElementById('pulse');
  const gl = canvas && canvas.getContext('webgl', { alpha: false, antialias: false });
  if (!gl) {
    post('designllm.webcode.webgl:error:context-unavailable');
    post('designllm.preview.ready');
    return;
  }

  try {
    const compile = (kind, source) => {
      const shader = gl.createShader(kind);
      if (!shader) throw new Error('shader-allocation-failed');
      gl.shaderSource(shader, source);
      gl.compileShader(shader);
      if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(shader) || 'shader-compile-failed');
      return shader;
    };
    const vertex = compile(gl.VERTEX_SHADER, 'attribute vec2 a_position; void main(){ gl_Position=vec4(a_position,0.0,1.0); }');
    const fragment = compile(gl.FRAGMENT_SHADER, 'precision mediump float; uniform vec2 u_resolution; uniform float u_time; void main(){ vec2 uv=gl_FragCoord.xy/u_resolution; float wave=0.5+0.5*sin(u_time*2.4+uv.x*5.0+uv.y*4.0); vec3 low=vec3(0.08,0.38,0.75); vec3 high=vec3(0.86,0.32,0.62); gl_FragColor=vec4(mix(low,high,wave),1.0); }');
    const program = gl.createProgram();
    if (!program) throw new Error('program-allocation-failed');
    gl.attachShader(program, vertex);
    gl.attachShader(program, fragment);
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(program) || 'program-link-failed');
    gl.useProgram(program);

    const position = gl.getAttribLocation(program, 'a_position');
    const resolution = gl.getUniformLocation(program, 'u_resolution');
    const time = gl.getUniformLocation(program, 'u_time');
    if (position < 0 || !resolution || !time) throw new Error('shader-binding-failed');
    const buffer = gl.createBuffer();
    if (!buffer) throw new Error('buffer-allocation-failed');
    gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
    gl.enableVertexAttribArray(position);
    gl.vertexAttribPointer(position, 2, gl.FLOAT, false, 0, 0);

    let width = 0;
    let height = 0;
    const resize = () => {
      const rect = canvas.getBoundingClientRect();
      const scale = Math.min(2, window.devicePixelRatio || 1);
      const nextWidth = Math.max(1, Math.round(rect.width * scale));
      const nextHeight = Math.max(1, Math.round(rect.height * scale));
      if (nextWidth !== width || nextHeight !== height) {
        width = canvas.width = nextWidth;
        height = canvas.height = nextHeight;
        gl.viewport(0, 0, width, height);
      }
      gl.uniform2f(resolution, width, height);
    };

    let frame40Pixel = null;
    let frame80Pixel = null;
    let firstTransform = null;
    let frames = 0;
    const started = performance.now();
    const draw = now => {
      resize();
      gl.uniform1f(time, (now - started) / 1000);
      gl.drawArrays(gl.TRIANGLES, 0, 3);
      frames += 1;
      if (frames === 1) firstTransform = getComputedStyle(pulse).transform;
      if (frames === 40 || frames === 80) {
        const pixel = new Uint8Array(4);
        gl.readPixels(1, 1, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, pixel);
        const sample = Array.from(pixel).join(',');
        if (frames === 40) frame40Pixel = sample;
        if (frames === 80) {
          frame80Pixel = sample;
          const changedTransform = getComputedStyle(pulse).transform !== firstTransform;
          post('designllm.webcode.webgl:ready');
          post(frame40Pixel !== null && frame80Pixel !== frame40Pixel
            ? 'designllm.webcode.webgl:frame-change-after-40:' + frame40Pixel + '>' + frame80Pixel
            : 'designllm.webcode.webgl:frame-static-after-40');
          post(changedTransform
            ? 'designllm.webcode.css-animation:changed'
            : 'designllm.webcode.css-animation:static');
          post('designllm.preview.ready');
        }
      }
      requestAnimationFrame(draw);
    };
    requestAnimationFrame(draw);
  } catch (error) {
    post('designllm.webcode.webgl:error:' + String(error));
    post('designllm.preview.ready');
  }
})();
"#;

fn webgl_animation_template_html(title: &str, size: Option<Size>) -> DltResult<String> {
    let mut html = WEBGL_ANIMATION_HTML.replace("__DLM_TITLE__", &html::escape_text(title));
    let Some(size) = size else { return Ok(html) };
    if !size.valid() {
        return Err(invalid("SIZE", "WebGL animation frame size must be positive and supported"));
    }
    let css_px = |emu: i64| {
        let divisor = i128::from(designllm_contracts::EMU_PER_CSS_PX);
        let micros = (i128::from(emu) * 1_000_000 + divisor / 2) / divisor;
        let whole = micros / 1_000_000;
        let fraction = micros % 1_000_000;
        if fraction == 0 {
            whole.to_string()
        } else {
            format!("{whole}.{fraction:06}").trim_end_matches('0').to_owned()
        }
    };
    let width = css_px(size.width);
    let height = css_px(size.height);
    let stage = format!(
        "<main class=\"stage\" data-artboard-width-emu=\"{}\" data-artboard-height-emu=\"{}\" style=\"width:min({width}px,100%);aspect-ratio:{width} / {height};min-height:0\">",
        size.width, size.height,
    );
    html = html.replace("<main class=\"stage\">", &stage);
    Ok(html)
}

pub(crate) fn capability_report() -> CapabilityReport {
    use Support::{Supported, Unsupported};
    let na = "not applicable to source projects";
    let mut report = BTreeMap::new();
    for key in [
        Feature::TextStyle,
        Feature::NamedStyles,
        Feature::Geometry,
        Feature::PageSetup,
        Feature::ParagraphFormat,
        Feature::Tables,
        Feature::Images,
        Feature::Pages,
        Feature::Notes,
        Feature::Diagram,
        Feature::Plot,
        Feature::Dataset,
        Feature::Evidence,
        Feature::FigurePanels,
        Feature::CanvasLayout,
        Feature::Interactions,
        Feature::RenderSvg,
        Feature::RenderPdf,
        Feature::RenderPng,
        Feature::HitTest,
        Feature::TextSelection,
        Feature::Effects,
        Feature::VectorPaths,
        Feature::GridLayout,
    ] {
        report.insert(
            key,
            FeatureSupport { read: Unsupported, render: Unsupported, edit: Unsupported, preserve: Unsupported, write: Unsupported, reason: na.into() },
        );
    }
    report.insert(
        Feature::FigureLinks,
        FeatureSupport {
            read: Unsupported,
            render: Unsupported,
            edit: Unsupported,
            preserve: Unsupported,
            write: Unsupported,
            reason: "source projects have no image nodes; figure render links apply to canvas image nodes only".into(),
        },
    );
    report.insert(
        Feature::Text,
        FeatureSupport {
            read: Supported,
            render: Unsupported,
            edit: Supported,
            preserve: Supported,
            write: Supported,
            reason: "source text is edited as text; it is not drawn as a scene (use the web preview)".into(),
        },
    );
    report.insert(
        Feature::WebSource,
        FeatureSupport {
            read: Supported,
            render: Unsupported,
            edit: Supported,
            preserve: Supported,
            write: Supported,
            reason: "files are hash-checked; edits are whole-file replacement or mapped text and attribute values".into(),
        },
    );
    report.insert(
        Feature::NativeRoundtrip,
        FeatureSupport { read: Supported, render: Unsupported, edit: Supported, preserve: Supported, write: Supported, reason: "JSON project with every file inline".into() },
    );
    report
}

fn project_from_zip(bytes: &[u8]) -> DltResult<(String, BTreeMap<String, WebFile>)> {
    let entries = archive::read_zip(bytes)?;
    let mut files = BTreeMap::new();
    let mut binary = Vec::new();
    for (name, data) in entries {
        match String::from_utf8(data) {
            Ok(text) => {
                files.insert(name.clone(), WebFile { sha256: sha256(text.as_bytes()), content: text });
            }
            Err(_) => binary.push(name),
        }
    }
    if !binary.is_empty() {
        return Err(unsupported(
            "WEB_BINARY_RESOURCE",
            format!("binary resources are not supported in source projects: {}", binary.join(", ")),
        ));
    }
    let entry = if files.contains_key("index.html") {
        "index.html".to_string()
    } else {
        files
            .keys()
            .find(|k| is_html(k))
            .cloned()
            .ok_or_else(|| invalid("WEB_NO_ENTRY", "archive has no HTML file to open"))?
    };
    Ok((entry, files))
}

fn single_html(bytes: &[u8]) -> DltResult<(String, BTreeMap<String, WebFile>)> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid("WEB_ENCODING", "HTML input must be UTF-8"))?
        .to_string();
    let mut files = BTreeMap::new();
    files.insert("index.html".to_string(), WebFile { sha256: sha256(text.as_bytes()), content: text });
    Ok(("index.html".to_string(), files))
}

impl EngineAdapter for WebEngine {
    fn id(&self) -> &'static str {
        WEB_ENGINE_ID
    }

    fn kinds(&self) -> Vec<ArtifactKind> {
        vec![ArtifactKind::WebCode]
    }

    fn templates(&self) -> Vec<designllm_contracts::TemplateInfo> {
        vec![designllm_contracts::TemplateInfo {
            id: WEBGL_ANIMATION_TEMPLATE_ID.into(),
            kind: ArtifactKind::WebCode,
            title: "WebGL animation".into(),
            preset: None,
            size: None,
        }]
    }

    fn create(&self, request: &CreateRequest, _context: &EngineContext<'_>) -> DltResult<Artifact> {
        if request.kind != ArtifactKind::WebCode {
            return Err(invalid("KIND", "WebEngine creates web source projects only"));
        }
        let title = normalize_title_text(&request.title, "Untitled page");
        let mut files = BTreeMap::new();
        match request.template.as_deref() {
            None | Some("blank") => {
                let content = template_html(&title);
                files.insert("index.html".to_string(), WebFile { sha256: sha256(content.as_bytes()), content });
            }
            Some(WEBGL_ANIMATION_TEMPLATE_ID) => {
                let html = webgl_animation_template_html(&title, request.size)?;
                files.insert("index.html".to_string(), WebFile { sha256: sha256(html.as_bytes()), content: html });
                let script = WEBGL_ANIMATION_SCRIPT.to_owned();
                files.insert("animation.js".to_string(), WebFile { sha256: sha256(script.as_bytes()), content: script });
            }
            Some(other) => return Err(unsupported("TEMPLATE", format!("template {other} is not available"))),
        }
        let doc = WebDocument { entry: "index.html".to_string(), files };
        let mut artifact = Artifact::new(ArtifactKind::WebCode, title, WEB_ENGINE_ID, ArtifactBody::WebCode(doc));
        artifact.capabilities = capability_report();
        Ok(artifact)
    }

    fn open(&self, input: &OpenInput, context: &EngineContext<'_>) -> DltResult<Artifact> {
        if input.kind != ArtifactKind::WebCode {
            return Err(invalid("KIND", "WebEngine opens web source projects only"));
        }
        let bytes = context.assets.read(&input.asset)?;
        if bytes.len() as u64 != input.asset.bytes || sha256(&bytes) != input.asset.sha256 {
            return Err(fail(ErrorCode::IntegrityError, "OPEN_ASSET_HASH", "input bytes do not match their reference"));
        }
        // First non-space byte decides the format: `{` is a DesignLLM project, `<` is an HTML file.
        let first = bytes.iter().find(|b| !b.is_ascii_whitespace()).copied();
        let artifact = if first == Some(b'{') {
            let decoded = native::decode(WEB_FORMAT, &bytes)?;
            if decoded.artifact.kind != ArtifactKind::WebCode || decoded.artifact.engine_id != WEB_ENGINE_ID {
                return Err(invalid("PROJECT_KIND", "the file is not a web project for this engine"));
            }
            let mut artifact = decoded.artifact;
            let doc = web_doc(&artifact)?.clone();
            let diagnostics = validate_project(&doc);
            crate::canvas::model::into_result(diagnostics.clone())?;
            artifact.capabilities = capability_report();
            artifact.diagnostics = diagnostics.into_iter().filter(|d| !d.blocking).collect();
            artifact.native = None;
            artifact
        } else {
            let (entry, files, origin) = if bytes.starts_with(b"PK\x03\x04") {
                let (entry, files) = project_from_zip(&bytes)?;
                (entry, files, "imported:zip")
            } else if first == Some(b'<') {
                let (entry, files) = single_html(&bytes)?;
                (entry, files, "imported:html")
            } else {
                return Err(invalid(
                    "WEB_FORMAT",
                    "unsupported web source: expected a ZIP source folder, an HTML file, or a DesignLLM project",
                ));
            };
            let doc = WebDocument { entry, files };
            let diagnostics = validate_project(&doc);
            crate::canvas::model::into_result(diagnostics.clone())?;
            let mut artifact = Artifact::new(
                ArtifactKind::WebCode,
                normalize_title_text(&input.title, "Imported page"),
                WEB_ENGINE_ID,
                ArtifactBody::WebCode(doc),
            );
            artifact.origin = origin.to_string();
            artifact.capabilities = capability_report();
            artifact.diagnostics = diagnostics.into_iter().filter(|d| !d.blocking).collect();
            artifact
        };
        Ok(artifact)
    }

    fn capabilities(&self, _artifact: &Artifact) -> CapabilityReport {
        capability_report()
    }

    fn supported_persisted_features(&self) -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::from([native::FEATURE_ARTIFACT_DESIGN_SYSTEM.to_owned()])
    }

    fn persisted_features(&self, artifact: &Artifact) -> DltResult<std::collections::BTreeSet<String>> {
        web_doc(artifact)?;
        Ok(std::collections::BTreeSet::new())
    }

    fn validate_shared_design_system_snapshot(
        &self,
        artifact: &Artifact,
        tokens: &BTreeMap<String, TokenDef>,
    ) -> DltResult<()> {
        design_system::validate_snapshot(artifact, tokens)
    }

    fn sync_design_system(
        &self,
        artifact: &Artifact,
        tokens: &BTreeMap<String, TokenDef>,
        design_system: Option<&DesignSystemRef>,
    ) -> DltResult<PreparedChange> {
        design_system::sync(artifact, tokens, design_system)
    }

    fn sync_design_system_with_policy(
        &self,
        artifact: &Artifact,
        tokens: &BTreeMap<String, TokenDef>,
        design_system: Option<&DesignSystemRef>,
        direct_formatting: designllm_contracts::design_policy::DirectFormattingPolicy,
    ) -> DltResult<PreparedChange> {
        use designllm_contracts::design_policy::DirectFormattingPolicy;
        match direct_formatting {
            DirectFormattingPolicy::Keep => self.sync_design_system(artifact, tokens, design_system),
            DirectFormattingPolicy::Clear => Err(unsupported(
                "WEB_DIRECT_FORMATTING",
                "WebCode has no direct formatting to clear; a design-system sync keeps the source as it is",
            )),
        }
    }

    fn inspect(&self, artifact: &Artifact, node_ids: &[String]) -> DltResult<Value> {
        let doc = web_doc(artifact)?;
        if node_ids.is_empty() {
            let files: Vec<Value> = doc
                .files
                .iter()
                .map(|(path, f)| json!({ "path": path, "bytes": f.content.len(), "sha256": f.sha256 }))
                .collect();
            return Ok(json!({ "artifact_id": artifact.id, "revision": artifact.revision, "entry": doc.entry, "files": files }));
        }
        let mut items = Vec::with_capacity(node_ids.len());
        for path in node_ids {
            let file = doc.files.get(path).ok_or_else(|| {
                let mut error = DltError::new(ErrorCode::NotFound, format!("{path} is not in this project"));
                error.affected_ids.push(path.clone());
                error
            })?;
            // A file that cannot be scanned has no mappings; `mapping_error` says so, so it is not read as "no mapped elements".
            let (mappings, mapping_error) = match html::mapping_ids(&file.content) {
                Ok(ids) => (ids.into_iter().map(|(id, tag)| json!({ "id": id, "tag": tag })).collect::<Vec<Value>>(), None),
                Err(err) => (Vec::new(), Some(json!({ "code": "WEB_HTML_UNSCANNED", "message": err.message }))),
            };
            let mut item = json!({
                "path": path,
                "sha256": file.sha256,
                "bytes": file.content.len(),
                "content": file.content,
                "mappings": mappings,
            });
            if let Some(error) = mapping_error {
                item["mapping_error"] = error;
            }
            items.push(item);
        }
        Ok(json!({ "artifact_id": artifact.id, "revision": artifact.revision, "items": items }))
    }

    fn prepare_change(&self, artifact: &Artifact, operations: &[Operation], _context: &EngineContext<'_>) -> DltResult<PreparedChange> {
        let original = web_doc(artifact)?;
        let mut working = original.clone();
        let changed_ids = ops::apply(&mut working, operations)?;
        let diagnostics = validate_project(&working);
        crate::canvas::model::into_result(diagnostics.clone())?;
        let mut next = artifact.clone();
        next.body = ArtifactBody::WebCode(working);
        next.capabilities = capability_report();
        next.diagnostics = diagnostics.iter().filter(|d| !d.blocking).cloned().collect();
        Ok(PreparedChange { artifact: next, changed_ids, diagnostics })
    }

    fn validate(&self, artifact: &Artifact, _context: &EngineContext<'_>) -> Vec<Diagnostic> {
        match web_doc(artifact) {
            Ok(doc) => {
                let mut diagnostics = validate_project(doc);
                diagnostics.extend(design_system_reference_notes(artifact));
                diagnostics
            }
            Err(err) => err.diagnostics,
        }
    }

    fn content_digest(&self, artifact: &Artifact) -> DltResult<ContentDigest> {
        digest::web_digest(web_doc(artifact)?)
    }

    /// Source projects have no scene. The web preview host renders them. This returns an empty scene with a
    /// `WEB_NO_SCENE` diagnostic, so the core can commit revisions without inventing pages.
    fn layout(&self, artifact: &Artifact, _context: &EngineContext<'_>) -> DltResult<RenderScene> {
        web_doc(artifact)?;
        Ok(RenderScene {
            pages: Vec::new(),
            font_fingerprint: sha256(b""),
            diagnostics: vec![info("WEB_NO_SCENE", "source projects are previewed by the web preview host, not as a scene")],
            metadata: BTreeMap::from([("preview".to_string(), json!("webview2-host"))]),
            links: vec![],
        })
    }

    fn encode_native(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<EncodedOutput> {
        let doc = web_doc(artifact)?;
        let diagnostics = validate_project(doc);
        crate::canvas::model::into_result(diagnostics.clone())?;
        let bytes = native::encode(WEB_FORMAT, artifact, &BTreeMap::new(), context.assets)?;
        Ok(EncodedOutput {
            bytes,
            mime: WEB_NATIVE_MIME.to_string(),
            editability: Editability::NativeData,
            diagnostics: diagnostics.into_iter().filter(|d| !d.blocking).collect(),
        })
    }

    fn verify_saved(&self, artifact: &Artifact, bytes: &[u8], _context: &EngineContext<'_>) -> DltResult<PreservationVerification> {
        let doc = web_doc(artifact)?;
        let decoded = match native::decode(WEB_FORMAT, bytes) {
            Ok(decoded) => decoded,
            Err(err) => {
                return Ok(PreservationVerification {
                    checked_parts: 0,
                    checked_regions: 0,
                    diagnostics: vec![blocking("SAVE_UNREADABLE", err.message, None)],
                })
            }
        };
        let mut diagnostics = Vec::new();
        if !native::same_artifact(artifact, &decoded.artifact) {
            diagnostics.push(blocking("SAVE_MISMATCH", "the re-read project differs from the artifact being saved", None));
        }
        let mut checked_parts = 0u32;
        if let ArtifactBody::WebCode(reread) = &decoded.artifact.body {
            for (path, file) in &reread.files {
                checked_parts += 1;
                if doc.files.get(path).map(|f| f.sha256.as_str()) != Some(file.sha256.as_str())
                    || sha256(file.content.as_bytes()) != file.sha256
                {
                    diagnostics.push(blocking("SAVE_FILE", "a saved file differs from the project", Some(path.clone())));
                }
            }
        }
        Ok(PreservationVerification { checked_parts, checked_regions: 0, diagnostics })
    }

    fn export(&self, artifact: &Artifact, request: &ExportRequest, _context: &EngineContext<'_>) -> DltResult<EncodedOutput> {
        if request.artifact_id != artifact.id {
            return Err(invalid("ARTIFACT_ID", "export request names another artifact"));
        }
        if request.expected_revision != artifact.revision {
            let mut error = DltError::new(ErrorCode::RevisionConflict, "export request was made against another revision");
            error.current_revision = Some(artifact.revision);
            return Err(error);
        }
        if request.page_indices.is_some() {
            return Err(invalid("PAGE_SELECTION", "source projects have no pages; omit page_indices"));
        }
        let doc = web_doc(artifact)?;
        let diagnostics = validate_project(doc);
        crate::canvas::model::into_result(diagnostics.clone())?;
        match request.format.as_str() {
            "zip" => {
                let entries: Vec<(String, Vec<u8>)> =
                    doc.files.iter().map(|(path, f)| (path.clone(), f.content.as_bytes().to_vec())).collect();
                let bytes = archive::write_zip(&entries)?;
                Ok(EncodedOutput {
                    bytes,
                    mime: "application/zip".into(),
                    editability: Editability::NativeData,
                    diagnostics: Vec::new(),
                })
            }
            "html" => {
                let entry = doc.files.get(&doc.entry).ok_or_else(|| invalid("WEB_ENTRY_MISSING", "entry is missing"))?;
                let inlined = html::inline_local(&doc.entry, &entry.content, &doc.files)?;
                let mut out_diagnostics: Vec<Diagnostic> = diagnostics.into_iter().filter(|d| !d.blocking).collect();
                for path in &inlined.inlined {
                    out_diagnostics.push(info("WEB_INLINED", format!("local file {path} was inlined into the HTML")));
                }
                Ok(EncodedOutput {
                    bytes: inlined.html.into_bytes(),
                    mime: "text/html; charset=utf-8".into(),
                    editability: Editability::NativeData,
                    diagnostics: out_diagnostics,
                })
            }
            other => Err(unsupported("EXPORT_FORMAT", format!("source export format {other} is not supported"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_title_is_escaped_in_head_and_heading() {
        let html = template_html("A <b>&</b>");
        assert!(html.contains("<title>A &lt;b&gt;&amp;&lt;/b&gt;</title>"));
        assert!(html.contains("data-dlm-map=\"heading\""));
    }

    #[test]
    fn project_validation_flags_stale_hashes_and_bad_entries() {
        let mut files = BTreeMap::new();
        files.insert("index.html".to_string(), WebFile { content: "<p>x</p>".into(), sha256: "0".repeat(64) });
        let doc = WebDocument { entry: "index.html".into(), files };
        let diagnostics = validate_project(&doc);
        assert!(diagnostics.iter().any(|d| d.blocking && d.code == "WEB_HASH_STALE"));
        let missing = WebDocument { entry: "app.html".into(), files: BTreeMap::new() };
        assert!(validate_project(&missing).iter().any(|d| d.blocking && d.code == "WEB_ENTRY_MISSING"));
    }
}
