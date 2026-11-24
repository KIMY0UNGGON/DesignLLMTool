//! Slides engine: presentation pages with native text, shapes, lines, pictures, and notes, backed by the PPTX codec.
//!
//! The engine keeps the original package as an immutable asset and the page model as the artifact body. Edits
//! change the model. Saving writes the model back into a copy of the original package, which keeps every part and
//! element the model does not cover. Saved bytes are verified by re-reading them, and a blocking difference refuses
//! the save.
//!
//! Supported edits: text (whole and by range), run formatting (size, bold, italic, colour, font), shape and line
//! fill, outline, and geometry, paragraph alignment and spacing, adding and removing text boxes, preset shapes,
//! lines, and pictures, slide insertion, removal, and reordering, speaker notes, and the theme colours and fonts
//! (`SetTheme`, written into the theme parts on save). A picture can be linked to a
//! PNG figure render (`UpdateFigureLink`). The link lives in the artifact, and the saved file holds the render as a
//! normal picture. An outer shadow of a rectangle or ellipse and a vector path are read, edited, and saved in their
//! canonical PPTX forms. Anything else is refused with UNSUPPORTED_OPERATION. Unmodelled objects (tables, charts,
//! groups, SmartArt, media, and so on) are preserved and are not editable or rendered.

#![forbid(unsafe_code)]

mod guard;
mod layout;
mod ops;

use std::collections::{BTreeMap, BTreeSet};

use designllm_contracts::{
    canonical_json, sha256, Artifact, ArtifactBody, ArtifactKind, AssetRef, ContentDigest, CreateRequest, Diagnostic,
    DltError, DltResult, EncodedOutput, EngineAdapter, EngineContext, Editability, ErrorCode, ExportRequest, Feature,
    FeatureSupport, FigureLink, Id, NativeSource, Node, NodeKind, OpaqueRegion, OpenInput, Operation, Page, PreparedChange,
    PreservationManifest, PreservationVerification, RenderScene, Size, SlideDocument, Style, Support, ThemePatch,
};
use designllm_package::Package;
use designllm_pptx::{Mappings, PageMap, ShapeMap};
use serde_json::{json, Value};

pub use layout::layout_slides;

/// Engine identifier written into every artifact.
pub const ENGINE_ID: &str = "slides.pptx.v1";
/// Key of the presentation-level record in `NativeSource.preserved`.
pub const PRESERVED_PRESENTATION: &str = "presentation";
/// Persisted feature key of a deck with theme edits (`SlideDocument.theme`). A build without it refuses the deck.
pub const FEATURE_THEME: &str = "slides.theme.v1";
/// Diagnostic code of a theme edit that kept a source shape's values (defined in contracts `diag_codes`).
pub use designllm_contracts::diag_codes::SLIDES_THEME_KEPT_AS_SET;

/// The slides engine. Use `SlidesEngine::default()`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SlidesEngine;

/// The engine-facing state of a slides artifact, read from its body and native source.
#[derive(Clone, Debug)]
pub(crate) struct SlideState {
    pub pages: Vec<Page>,
    pub styles: BTreeMap<String, Style>,
    pub mappings: Mappings,
    pub opaque: Vec<OpaqueRegion>,
    pub manifest: PreservationManifest,
    pub slide_size: Size,
    /// Figure links of the artifact, one per picture, sorted by `target_node_id`.
    pub figure_links: Vec<FigureLink>,
    /// Theme edits over the theme of the original package (`SlideDocument.theme`).
    pub theme: Option<ThemePatch>,
}

pub(crate) fn state_of(artifact: &Artifact) -> DltResult<SlideState> {
    if artifact.kind != ArtifactKind::Slides {
        return Err(DltError::new(ErrorCode::InvalidArgument, "the artifact is not a slides artifact"));
    }
    let ArtifactBody::Slides(document) = &artifact.body else {
        return Err(DltError::new(ErrorCode::IntegrityError, "slides artifact has no slide body"));
    };
    // Named styles pass through as model data; slides do not render or encode their definitions.
    for style in document.styles.values() {
        designllm_pptx::guard::check_style(style, "in slides", None)?;
    }
    // A stored theme edit is checked as a new one is, so nothing unchecked reaches the theme part.
    if let Some(theme) = &document.theme {
        designllm_pptx::theme::checked_patch(theme)?;
    }
    let native = artifact
        .native
        .as_ref()
        .ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "slides artifact has no native source"))?;
    let slide_size = presentation_size(native)?;
    Ok(SlideState {
        pages: document.pages.clone(),
        styles: document.styles.clone(),
        mappings: native.mappings.clone(),
        opaque: native.preservation.opaque_regions.clone(),
        manifest: native.preservation.clone(),
        slide_size,
        figure_links: artifact.figure_links.clone(),
        theme: document.theme.clone(),
    })
}

fn presentation_size(native: &NativeSource) -> DltResult<Size> {
    let record = native
        .preserved
        .get(PRESERVED_PRESENTATION)
        .ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "slides artifact has no presentation record"))?;
    let width = record.get("width").and_then(Value::as_i64);
    let height = record.get("height").and_then(Value::as_i64);
    match (width, height) {
        (Some(width), Some(height)) => Ok(Size { width, height }),
        _ => Err(DltError::new(ErrorCode::IntegrityError, "presentation record has no size")),
    }
}

/// Replaces the body and the mapping state of an artifact. Identity, kind, revision, and engine stay as they were.
pub(crate) fn with_state(artifact: &Artifact, state: &SlideState) -> Artifact {
    let mut next = artifact.clone();
    next.body = ArtifactBody::Slides(SlideDocument { pages: state.pages.clone(), styles: state.styles.clone(), theme: state.theme.clone() });
    next.figure_links = state.figure_links.clone();
    if let Some(native) = next.native.as_mut() {
        native.mappings = state.mappings.clone();
        native.preservation.opaque_regions = state.opaque.clone();
    }
    next
}

fn presentation_record(size: Size) -> Value {
    json!({ "part": designllm_pptx::ns::PRESENTATION_PART, "width": size.width, "height": size.height })
}

/// Builds a slides artifact from a package that is already stored as an asset.
fn artifact_from_package(
    asset: AssetRef,
    bytes: &[u8],
    title: &str,
    origin: &str,
    ctx: &EngineContext<'_>,
) -> DltResult<Artifact> {
    let package = designllm_pptx::guard::open_package(bytes)?;
    let put = |data: &[u8], mime: &str| ctx.assets.put(data, mime);
    let opened = designllm_pptx::open_deck(&package, &put)?;
    let mut preserved = BTreeMap::new();
    preserved.insert(PRESERVED_PRESENTATION.to_string(), presentation_record(opened.slide_size));
    let native = NativeSource {
        asset,
        mappings: opened.mappings,
        preserved,
        preservation: PreservationManifest { parts: opened.manifest.parts, opaque_regions: opened.opaque },
    };
    let mut artifact = Artifact::new(
        ArtifactKind::Slides,
        title,
        ENGINE_ID,
        ArtifactBody::Slides(SlideDocument { pages: opened.pages, styles: BTreeMap::new(), theme: None }),
    );
    artifact.origin = origin.to_string();
    artifact.native = Some(native);
    artifact.capabilities = capability_report();
    artifact.diagnostics = opened.diagnostics;
    Ok(artifact)
}

/// Capability keys for slides. Every `Feature` key is present, so a missing key is never read as supported.
pub fn capability_report() -> designllm_contracts::CapabilityReport {
    use Feature::*;
    use Support::*;
    let mut report = BTreeMap::new();
    let mut put = |feature: Feature, read: Support, render: Support, edit: Support, preserve: Support, write: Support, reason: &str| {
        report.insert(feature, FeatureSupport { read, render, edit, preserve, write, reason: reason.to_string() });
    };
    put(Text, Supported, Supported, Supported, Supported, Supported, "text in text boxes, preset shapes, and placeholders with explicit geometry");
    put(TextStyle, Supported, Supported, Supported, Supported, Supported, "size, bold, italic, colour, and font family per run");
    put(NamedStyles, Unsupported, Unsupported, Unsupported, PreserveOnly, Supported, "theme and master text styles are kept but not edited by name");
    put(Geometry, Supported, Supported, Supported, Supported, Supported, "position and size of text, shapes, pictures, and lines; rotation is kept but not rendered");
    put(PageSetup, Supported, Supported, Unsupported, Supported, Supported, "slide size is read and kept; changing it is not supported");
    put(ParagraphFormat, Supported, Supported, Supported, Supported, Supported, "alignment and space before and after; line spacing and indent are not supported");
    put(Tables, Unsupported, Unsupported, Unsupported, Supported, Supported, "tables are preserved byte for byte and not rendered or editable");
    put(Images, Supported, Supported, Supported, Supported, Supported, "PNG, JPEG, GIF, and BMP pictures; crop is kept but not rendered");
    put(Pages, Supported, Supported, Supported, Supported, Supported, "slide insertion, removal, and reordering");
    put(Notes, Supported, Unsupported, Supported, Supported, Supported, "speaker notes text; notes are not rendered on slides");
    put(Diagram, Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "diagram figures are not part of slides");
    put(Plot, Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "data plots are figures; PPTX charts are preserved, not edited");
    put(Dataset, Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "datasets belong to figures");
    put(Evidence, Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "evidence belongs to figures");
    put(FigurePanels, Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "figure panels belong to figures");
    put(FigureLinks, Supported, Supported, Supported, Supported, Unsupported, "a picture links to a PNG figure render; the link is kept in the artifact and the saved PPTX holds the render as a normal picture");
    put(CanvasLayout, Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "canvas layout is a canvas feature");
    put(Interactions, Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "slide transitions and interactions are preserved, not edited");
    put(WebSource, Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "web sources are a web feature");
    put(NativeRoundtrip, Supported, Supported, Supported, Supported, Supported, "native PPTX save, verified by re-reading the output");
    put(RenderSvg, Supported, Supported, Unsupported, Unsupported, Unsupported, "SVG export through the shared renderer, one page per file");
    put(RenderPdf, Supported, Supported, Unsupported, Unsupported, Unsupported, "PDF export through the shared renderer, all pages");
    put(RenderPng, Supported, Supported, Unsupported, Unsupported, Unsupported, "PNG export through the shared renderer, one page per file");
    put(HitTest, Supported, Supported, Unsupported, Unsupported, Unsupported, "hit testing on rendered text and shapes");
    put(TextSelection, Supported, Supported, Unsupported, Unsupported, Unsupported, "grapheme ranges and selection rectangles per paragraph");
    put(Effects, Approximate, Approximate, Approximate, Supported, Approximate, "an outer shadow of a rectangle or ellipse is read, drawn, edited, and saved: one shadow, no spread, an offset on one axis, a colour as #RRGGBB or #RRGGBBAA, and a blur saved with a loss entry; a shadow on another kind of node is refused, and clearing the shadow is allowed on any node; start and end markers of a line are read, drawn, edited, and saved, and a line end on another node is refused; other effects (corner radius, opacity, gradients, line height, letter spacing, vertical alignment, font weight, underline, strikethrough, stroke dash, cap, and join) are refused for slides in this release");
    put(VectorPaths, Approximate, Supported, Approximate, Supported, Approximate, "a vector path is read, drawn, created, and saved as custom geometry with the nonzero fill rule, a view box at the origin, a positive size, and a first moveTo command; after creation its position and style can be edited, except its shadow and text formatting, but its geometry cannot be edited; a path has no text, and an empty path, an even-odd fill rule, or a view box away from the origin is refused");
    put(GridLayout, Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "grid layout (mode grid, justify, align self, grid span, row gap, padding sides) is refused for slides in this release");
    report
}

/// Canonical digest of the content intent: the page order, page notes, node set, node kinds and text, picture
/// hashes, and every preservation region. Design-only data (geometry, colour, size) is excluded.
pub(crate) fn content_digest_of(state: &SlideState) -> DltResult<ContentDigest> {
    let mut pages = Vec::new();
    for page in &state.pages {
        let nodes: Vec<Value> = page
            .nodes
            .iter()
            .map(|node| {
                json!({
                    "id": node.id,
                    "kind": format!("{:?}", node.kind),
                    "text": node.text,
                    "asset": node.asset.as_ref().map(|a| a.sha256.clone()),
                })
            })
            .collect();
        pages.push(json!({ "id": page.id, "notes": page.notes, "nodes": nodes }));
    }
    let mut regions: Vec<String> = state
        .opaque
        .iter()
        .map(|r| format!("{}|{}|{}", r.part, r.anchor, r.sha256))
        .collect();
    regions.sort();
    let value = json!({ "pages": pages, "preserved": regions });
    let content = sha256(canonical_json(&value)?.as_bytes());
    let data = sha256(canonical_json(&json!([]))?.as_bytes());
    Ok(ContentDigest { content, data })
}

impl EngineAdapter for SlidesEngine {
    fn id(&self) -> &'static str {
        ENGINE_ID
    }

    fn kinds(&self) -> Vec<ArtifactKind> {
        vec![ArtifactKind::Slides]
    }

    fn create(&self, request: &CreateRequest, ctx: &EngineContext<'_>) -> DltResult<Artifact> {
        if request.kind != ArtifactKind::Slides {
            return Err(DltError::new(ErrorCode::InvalidArgument, "the slides engine creates only slides artifacts"));
        }
        if request.template.is_some() {
            return Err(DltError::unsupported("slide templates are not supported in this release; create a blank deck"));
        }
        let size = request.size.unwrap_or_else(Size::slide);
        if !size.valid() {
            return Err(DltError::invalid("slide size must be positive and at most 10 m on each side"));
        }
        let bytes = designllm_pptx::blank_deck(&request.title, size)?;
        let asset = ctx.assets.put(&bytes, designllm_pptx::ns::PPTX_MIME)?;
        artifact_from_package(asset, &bytes, &request.title, "created", ctx)
    }

    fn open(&self, input: &OpenInput, ctx: &EngineContext<'_>) -> DltResult<Artifact> {
        if input.kind != ArtifactKind::Slides {
            return Err(DltError::new(ErrorCode::InvalidArgument, "the slides engine opens only .pptx files"));
        }
        let bytes = ctx.assets.read(&input.asset)?;
        artifact_from_package(input.asset.clone(), &bytes, &input.title, "imported", ctx)
    }

    fn capabilities(&self, _artifact: &Artifact) -> designllm_contracts::CapabilityReport {
        capability_report()
    }

    fn supported_persisted_features(&self) -> BTreeSet<String> {
        BTreeSet::from([
            "slides.line_ends.v1".to_owned(),
            "slides.shadows.v1".to_owned(),
            "slides.vector_paths.v1".to_owned(),
            FEATURE_THEME.to_owned(),
        ])
    }

    fn persisted_features(&self, artifact: &Artifact) -> DltResult<BTreeSet<String>> {
        let ArtifactBody::Slides(document) = &artifact.body else {
            return Err(DltError::new(ErrorCode::IntegrityError, "slides artifact has no slide body"));
        };
        // Fail closed on every other P2 field or value. The shared PPTX guard excludes line
        // endpoints only on actual line nodes; named styles, variants, lists and other P2 values
        // cannot be silently classified as an empty feature set.
        for style in document.styles.values() {
            designllm_pptx::guard::check_style(style, "in slides", None)?;
        }
        guard::check_pages(&document.pages)?;
        // A key is set wherever its feature appears in the page tree, including the children of a group.
        fn any_node(nodes: &[Node], uses: &dyn Fn(&Node) -> bool) -> bool {
            nodes.iter().any(|node| uses(node) || any_node(&node.children, uses))
        }
        let in_pages = |uses: &dyn Fn(&Node) -> bool| document.pages.iter().any(|page| any_node(&page.nodes, uses));
        let mut used = BTreeSet::new();
        if in_pages(&|node: &Node| node.style.stroke_start.is_some() || node.style.stroke_end.is_some()) {
            used.insert("slides.line_ends.v1".to_owned());
        }
        if in_pages(&|node: &Node| !node.style.shadows.is_empty()) {
            used.insert("slides.shadows.v1".to_owned());
        }
        if in_pages(&|node: &Node| node.kind == NodeKind::Path) {
            used.insert("slides.vector_paths.v1".to_owned());
        }
        if document.theme.is_some() {
            used.insert(FEATURE_THEME.to_owned());
        }
        Ok(used)
    }

    fn inspect(&self, artifact: &Artifact, node_ids: &[Id]) -> DltResult<Value> {
        let state = state_of(artifact)?;
        let mut out = Vec::with_capacity(node_ids.len());
        for id in node_ids {
            let (page, node) = find_node(&state.pages, id)
                .ok_or_else(|| DltError::new(ErrorCode::NotFound, format!("node {id} does not exist")))?;
            let map: ShapeMap = designllm_pptx::model::from_value(id, state.mappings.get(id).unwrap_or(&Value::Null))?;
            out.push(json!({
                "id": node.id,
                "page_id": page.id,
                "kind": format!("{:?}", node.kind),
                "text": node.text,
                "bounds": node.bounds,
                "style": node.style,
                "geometry": map.geometry,
                "paragraphs": map.paragraphs,
                "asset": node.asset,
            }));
        }
        Ok(Value::Array(out))
    }

    fn prepare_change(&self, artifact: &Artifact, operations: &[Operation], ctx: &EngineContext<'_>) -> DltResult<PreparedChange> {
        let state = state_of(artifact)?;
        // Refused before any operation is applied. The current page nodes identify the only kind that carries line ends.
        guard::check_operations(operations, &state.pages)?;
        let package = base_package(artifact, ctx)?;
        let (next, changed, diagnostics) = ops::apply_all(state, operations, &package, ctx)?;
        guard::check_pages(&next.pages)?;
        let candidate = with_state(artifact, &next);
        Ok(PreparedChange { artifact: candidate, changed_ids: changed, diagnostics })
    }

    fn validate(&self, artifact: &Artifact, ctx: &EngineContext<'_>) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        let state = match state_of(artifact) {
            Ok(state) => state,
            Err(e) => {
                out.push(Diagnostic { code: "SLIDES_STATE".into(), message: e.message, node_id: None, blocking: true, params: Default::default() });
                return out;
            }
        };
        if let Err(e) = ops::check_model(&state) {
            out.push(Diagnostic { code: "SLIDES_MODEL".into(), message: e.message, node_id: None, blocking: true, params: Default::default() });
            return out;
        }
        // The change must survive a save and a re-read. Pre-existing findings in the original are reported as-is.
        match self.encode_native(artifact, ctx) {
            Ok(encoded) => {
                out.extend(encoded.diagnostics);
                let verification = self.verify_saved(artifact, &encoded.bytes, ctx);
                match verification {
                    Ok(v) => out.extend(v.diagnostics),
                    Err(e) => out.push(Diagnostic { code: "PPTX_VERIFY_FAILED".into(), message: e.message, node_id: None, blocking: true, params: Default::default() }),
                }
            }
            Err(e) => out.push(Diagnostic { code: "PPTX_WRITE_FAILED".into(), message: e.message, node_id: None, blocking: true, params: Default::default() }),
        }
        // The anti-slop warnings (contracts `design_quality`, the rules Canvas pages get): non-blocking, on the deck's pages.
        out.extend(designllm_contracts::design_quality::slop_deck_warnings(&state.pages));
        out
    }

    fn content_digest(&self, artifact: &Artifact) -> DltResult<ContentDigest> {
        content_digest_of(&state_of(artifact)?)
    }

    fn layout(&self, artifact: &Artifact, ctx: &EngineContext<'_>) -> DltResult<RenderScene> {
        let state = state_of(artifact)?;
        // A model value that the layout would not draw is refused, not left out of the preview.
        guard::check_pages(&state.pages)?;
        layout::layout_slides(&state.pages, &state.mappings, ctx, None)
    }

    fn encode_native(&self, artifact: &Artifact, ctx: &EngineContext<'_>) -> DltResult<EncodedOutput> {
        let state = state_of(artifact)?;
        // With theme edits the writer starts from the package with the edited theme, so a shape whose colour or font
        // comes from the theme compares equal to its model value and keeps its theme reference.
        let base = themed_package(base_package(artifact, ctx)?, state.theme.as_ref())?;
        // The losses of the written objects (a blur, an overlapping subpath) are returned with the bytes.
        let (package, diagnostics) = designllm_pptx::write_deck_reporting(&base, &state.pages, &state.mappings, ctx.assets)?;
        let bytes = package.encode()?;
        Ok(EncodedOutput {
            bytes,
            mime: designllm_pptx::ns::PPTX_MIME.to_string(),
            editability: Editability::Objects,
            diagnostics,
        })
    }

    fn verify_saved(&self, artifact: &Artifact, bytes: &[u8], ctx: &EngineContext<'_>) -> DltResult<PreservationVerification> {
        let state = state_of(artifact)?;
        guard::check_pages(&state.pages)?;
        let input = designllm_pptx::VerifyInput {
            pages: &state.pages,
            mappings: &state.mappings,
            slide_size: state.slide_size,
            manifest: &state.manifest,
            opaque: &state.opaque,
        };
        // Without theme edits every theme part keeps its original hash; with them, it may change only as the edit asks.
        Ok(match &state.theme {
            None => designllm_pptx::verify_bytes(&input, bytes),
            Some(theme) => designllm_pptx::verify::verify_bytes_with_theme(&input, &base_package(artifact, ctx)?, Some(theme), bytes),
        })
    }

    fn export(&self, artifact: &Artifact, request: &ExportRequest, ctx: &EngineContext<'_>) -> DltResult<EncodedOutput> {
        let state = state_of(artifact)?;
        guard::check_pages(&state.pages)?;
        match request.format.as_str() {
            "pptx" => {
                // PPTX writes the whole deck. A slide selection is refused rather than ignored.
                if request.page_indices.is_some() {
                    return Err(DltError::unsupported(
                        "pptx export writes the whole deck; page_indices is not supported for pptx (omit it, or export the pages as pdf, png, or svg)",
                    ));
                }
                // The exported file is read back and checked, the same as a native save.
                let encoded = self.encode_native(artifact, ctx)?;
                let verification = self.verify_saved(artifact, &encoded.bytes, ctx)?;
                let (blocking, findings): (Vec<Diagnostic>, Vec<Diagnostic>) =
                    verification.diagnostics.into_iter().partition(|d| d.blocking);
                if !blocking.is_empty() {
                    let mut err = DltError::new(ErrorCode::IntegrityError, "the exported PPTX does not match the document model");
                    err.diagnostics = blocking;
                    return Err(err);
                }
                let mut diagnostics = encoded.diagnostics;
                diagnostics.extend(findings);
                Ok(EncodedOutput { bytes: encoded.bytes, mime: encoded.mime, editability: encoded.editability, diagnostics })
            }
            "png" | "svg" | "svg-outline" | "pdf" => {
                let selected = layout::select_pages(&state.pages, request.page_indices.as_deref())?;
                let single_file = request.format != "pdf";
                if single_file && selected.len() != 1 {
                    return Err(DltError::invalid(format!(
                        "{} export is one file per page: select exactly one page, found {}",
                        request.format,
                        selected.len()
                    )));
                }
                let scene = layout::layout_slides(&state.pages, &state.mappings, ctx, Some(&selected))?;
                ctx.renderer.encode(&scene, &request.format, &request.profile, ctx.assets)
            }
            other => Err(DltError::unsupported(format!(
                "slides cannot be exported as {other}; use pptx, pdf, png, or svg"
            ))),
        }
    }
}

/// The original package, read from the artifact's immutable asset.
pub(crate) fn base_package(artifact: &Artifact, ctx: &EngineContext<'_>) -> DltResult<Package> {
    let native = artifact
        .native
        .as_ref()
        .ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "slides artifact has no native source"))?;
    designllm_pptx::guard::open_package(&ctx.assets.read(&native.asset)?)
}

/// `package` with the theme edits written into its theme parts, or `package` itself without edits.
pub(crate) fn themed_package(package: Package, theme: Option<&ThemePatch>) -> DltResult<Package> {
    match theme {
        Some(theme) => designllm_pptx::theme::with_theme(package, theme),
        None => Ok(package),
    }
}

/// Finds a node and its page.
pub(crate) fn find_node<'a>(pages: &'a [Page], id: &str) -> Option<(&'a Page, &'a Node)> {
    pages.iter().find_map(|page| page.nodes.iter().find(|n| n.id == id).map(|n| (page, n)))
}

/// Page mapping record.
pub(crate) fn page_map(mappings: &Mappings, id: &str) -> DltResult<PageMap> {
    designllm_pptx::model::from_value(id, mappings.get(id).unwrap_or(&Value::Null))
}

/// Shape mapping record.
pub(crate) fn shape_map(mappings: &Mappings, id: &str) -> DltResult<ShapeMap> {
    designllm_pptx::model::from_value(id, mappings.get(id).unwrap_or(&Value::Null))
}

/// Set of every id in use: page ids and node ids.
pub(crate) fn ids_in_use(pages: &[Page]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for page in pages {
        out.insert(page.id.clone());
        for node in &page.nodes {
            out.insert(node.id.clone());
        }
    }
    out
}

/// Shorthand for a blocking invalid-argument error.
pub(crate) fn invalid(message: impl Into<String>) -> DltError {
    DltError::invalid(message)
}

/// Shorthand for an unsupported-operation error.
pub(crate) fn unsupported(message: impl Into<String>) -> DltError {
    DltError::unsupported(message)
}
