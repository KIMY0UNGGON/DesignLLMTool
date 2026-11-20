//! FigureEngine: the EngineAdapter for figure artifacts.
//!
//! Every output goes figure -> common RenderScene -> RenderProvider::encode (SVG, PDF, PNG) or the PPTX
//! scene callback supplied by the application. The engine keeps no renderer, store, or undo of its own.

use std::collections::BTreeMap;

use designllm_contracts::{
    Artifact, ArtifactBody, ArtifactKind, AssetStore, CapabilityReport, CreateRequest, DltError, DltResult, Diagnostic, EncodedOutput,
    EngineAdapter, EngineContext, Editability, ExportRequest, FeatureSupport, Feature, Id, Intent, NativeSource, OpenInput, Operation,
    PreparedChange, PreservationManifest, PreservationVerification, RenderScene, Size, Support, ContentDigest, FigureDocument,
    canonical_json, ErrorCode, PanelBody, EMU_PER_MM, sha256,
};
use designllm_contracts::figure_math::FIGURE_MATH_FLATTENED;
use serde_json::{Value, json};

use crate::digest;
use crate::diag::{self, code, has_blocking};
use crate::evidence::UNREVIEWED_CITED;
use crate::guard;
use crate::layout::PAGE_INDEX;
use crate::native;
use crate::ops;
use crate::plot;
use crate::validate;

/// Engine id recorded on every figure artifact.
pub const ENGINE_ID: &str = "designllm-figures";

/// PPTX output is produced from the common scene by the application (designllm_pptx::encode_scene).
pub type PptxSceneExporter = fn(&RenderScene, &dyn AssetStore, &str) -> DltResult<EncodedOutput>;

/// Losses that are informational in preview and blocking at export, so the caller must list them in allowed_losses.
/// No renderer draws a rotation, so a rotated picture is a loss. The common renderer (SVG, PDF, PNG) draws a crop, so the
/// figure layout reports none; IMAGE_CROP_NOT_APPLIED stays listed so a scene diagnostic with that code is blocking at
/// export. The PPTX writer does not draw a crop and reports none yet. UNREVIEWED_CITED marks PDF evidence the user has not
/// reviewed. VISUAL_REVIEW_NOT_APPROVED marks a figure whose visual review exists and is not approved (it is reported only
/// then, so a figure without a review exports as before). VISUAL_REVIEW_STALE marks an approval that the figure's content,
/// data, or design has outgrown. Only export applies this list; validation, change, and save never refuse on these codes.
const LOSS_CODES: [&str; 5] = [
    code::IMAGE_CROP_NOT_APPLIED,
    code::IMAGE_ROTATION_NOT_APPLIED,
    UNREVIEWED_CITED,
    code::VISUAL_REVIEW_NOT_APPROVED,
    code::VISUAL_REVIEW_STALE,
];

const DEFAULT_WIDTH_MM: i64 = 170;
const DEFAULT_HEIGHT_MM: i64 = 120;

#[derive(Clone, Copy, Default)]
pub struct FigureEngine {
    pptx: Option<PptxSceneExporter>,
}

impl FigureEngine {
    /// Enables PPTX export. Without it, PPTX requests are refused as unsupported.
    pub fn with_pptx_exporter(mut self, exporter: PptxSceneExporter) -> Self {
        self.pptx = Some(exporter);
        self
    }
}

fn doc_of(artifact: &Artifact) -> DltResult<&FigureDocument> {
    match &artifact.body {
        ArtifactBody::Figure(doc) if artifact.kind == ArtifactKind::Figure => Ok(doc),
        _ => Err(DltError::invalid("artifact is not a figure")),
    }
}

fn rank(e: Editability) -> u8 {
    match e {
        Editability::None => 0,
        Editability::Vector => 1,
        Editability::Objects => 2,
        Editability::NativeData => 3,
    }
}

/// Editability reported for an output. Raster image panels lower it to Vector. PNG is never editable.
/// PPTX never reports NativeData, because plots are written as objects, not native charts.
fn cap_editability(reported: Editability, format: &str, has_raster: bool) -> Editability {
    let mut e = reported;
    if format == "pptx" && e == Editability::NativeData {
        e = Editability::Objects;
    }
    if format == "png" {
        e = Editability::None;
    }
    if has_raster && rank(e) > rank(Editability::Vector) {
        e = Editability::Vector;
    }
    e
}

fn capability_report() -> CapabilityReport {
    let all = |s: Support, reason: &str| FeatureSupport {
        read: s,
        render: s,
        edit: s,
        preserve: s,
        write: s,
        reason: reason.to_owned(),
    };
    let na = |reason: &str| all(Support::Unsupported, reason);
    let mut m: CapabilityReport = BTreeMap::new();
    m.insert(
        Feature::Diagram,
        all(Support::Supported, "rectangle and ellipse nodes; straight connectors with arrows; no port routing or obstacle avoidance"),
    );
    m.insert(
        Feature::Plot,
        all(
            Support::Supported,
            "bar, line and scatter; categorical or numeric x; explicit or automatic ranges; SD, SEM, CI and range errors; log10 y axis; one error series",
        ),
    );
    m.insert(
        Feature::Dataset,
        all(Support::Supported, "source CSV/TSV asset with declared column types; raw text kept; transforms recorded and verified"),
    );
    m.insert(
        Feature::Evidence,
        all(Support::Supported, "paper_evidence (PDF regions), table_cell and user_statement evidence; PDF extraction belongs to the sources engine"),
    );
    m.insert(Feature::FigurePanels, all(Support::Supported, "multi-panel composition: add, remove, reorder and arrange"));
    m.insert(
        Feature::FigureLinks,
        FeatureSupport {
            read: Support::Supported,
            render: Support::Supported,
            edit: Support::Unsupported,
            preserve: Support::Supported,
            write: Support::Supported,
            reason: "figure revisions export for documents; links are edited on the parent document".to_owned(),
        },
    );
    m.insert(
        Feature::Images,
        FeatureSupport {
            read: Support::Supported,
            render: Support::Approximate,
            edit: Support::Approximate,
            preserve: Support::Supported,
            write: Support::Supported,
            reason: "PNG and JPEG panels; crop is drawn by the SVG, PDF and PNG renderers, not by the PPTX writer; rotation is stored but not rendered; calibration is stored and scale bars are not generated".to_owned(),
        },
    );
    m.insert(
        Feature::NativeRoundtrip,
        all(
            Support::Supported,
            "figure project JSON reproduces datasets and evidence exactly; source assets are embedded and verified on open; format_version 1 files need their assets in the store",
        ),
    );
    m.insert(Feature::RenderSvg, all(Support::Supported, "SVG through the common renderer"));
    m.insert(Feature::RenderPdf, all(Support::Supported, "PDF through the common renderer"));
    m.insert(Feature::RenderPng, all(Support::Supported, "PNG through the common renderer at the requested DPI"));
    m.insert(Feature::HitTest, all(Support::Supported, "hit tests use the common scene source map"));
    m.insert(Feature::Text, all(Support::Supported, "panel and node labels, axis captions, tick labels, and value labels"));
    m.insert(Feature::TextStyle, all(Support::Supported, "label font family, size, bold, italic, and color through style patches"));
    m.insert(Feature::Geometry, all(Support::Supported, "panel bounds and node bounds"));
    m.insert(Feature::TextSelection, na("figure labels are not text ranges; elements are selected, not characters"));
    m.insert(Feature::NamedStyles, na("not applicable to figure artifacts"));
    m.insert(Feature::PageSetup, na("the canvas size is set at creation; page margins do not apply"));
    m.insert(Feature::ParagraphFormat, na("not applicable to figure artifacts"));
    m.insert(Feature::Tables, na("not applicable to figure artifacts; data tables are datasets"));
    m.insert(Feature::Pages, na("a figure is one page"));
    m.insert(Feature::Notes, na("not applicable to figure artifacts"));
    m.insert(Feature::CanvasLayout, na("not applicable to figure artifacts"));
    m.insert(Feature::Interactions, na("not applicable to figure artifacts"));
    m.insert(Feature::WebSource, na("not applicable to figure artifacts"));
    // Approximate: some v1.2 style fields are applied (on the element kinds the reason lists), the rest are refused.
    m.insert(Feature::Effects, all(Support::Approximate, &guard::effects_reason()));
    m.insert(
        Feature::VectorPaths,
        na("NodeKind::Path, path data and icons are refused; diagrams draw rectangles and ellipses only"),
    );
    m.insert(
        Feature::GridLayout,
        na("layout mode grid and the other v1.2 layout fields are refused; diagram nodes are placed by their bounds"),
    );
    m
}

fn default_size() -> Size {
    Size { width: DEFAULT_WIDTH_MM * EMU_PER_MM, height: DEFAULT_HEIGHT_MM * EMU_PER_MM }
}

/// Operations that add or remove a data binding without being data-intent operations.
fn touches_data_binding(op: &Operation) -> bool {
    match op {
        Operation::AddPanel { panel, .. } => matches!(panel.body, PanelBody::Plot(_)),
        Operation::RemovePanel { .. } => true,
        _ => false,
    }
}

fn non_blocking(diags: Vec<Diagnostic>) -> Vec<Diagnostic> {
    diags.into_iter().filter(|d| !d.blocking).collect()
}

fn scene_of(artifact: &Artifact, built: crate::layout::Built, digests: &ContentDigest, doc: &FigureDocument, diags: Vec<Diagnostic>) -> RenderScene {
    let crate::layout::Built { page, mut metadata, font_fingerprint, .. } = built;
    metadata.insert("figure_id".to_owned(), json!(artifact.id));
    metadata.insert("figure_revision".to_owned(), json!(artifact.revision));
    metadata.insert("content_digest".to_owned(), json!(digests.content));
    metadata.insert("data_digest".to_owned(), json!(digests.data));
    metadata.insert("datasets".to_owned(), json!(validate::dataset_versions(doc)));
    RenderScene { pages: vec![page], font_fingerprint, diagnostics: non_blocking(diags), metadata, links: vec![] }
}

impl EngineAdapter for FigureEngine {
    fn id(&self) -> &'static str {
        ENGINE_ID
    }

    fn kinds(&self) -> Vec<ArtifactKind> {
        vec![ArtifactKind::Figure]
    }

    fn create(&self, request: &CreateRequest, _context: &EngineContext<'_>) -> DltResult<Artifact> {
        if request.kind != ArtifactKind::Figure {
            return Err(DltError::invalid("the figure engine creates figure artifacts only"));
        }
        let size = request.size.unwrap_or_else(default_size);
        if !size.valid() {
            return Err(DltError::invalid("canvas size must be positive and at most 10 m"));
        }
        match request.template.as_deref() {
            None | Some("blank") => {}
            Some(other) => return Err(DltError::unsupported(format!("figure template \"{other}\" is not available"))),
        }
        let doc = FigureDocument::new(size);
        let mut artifact = Artifact::new(ArtifactKind::Figure, request.title.clone(), ENGINE_ID, ArtifactBody::Figure(doc));
        artifact.capabilities = capability_report();
        Ok(artifact)
    }

    fn open(&self, input: &OpenInput, context: &EngineContext<'_>) -> DltResult<Artifact> {
        if input.kind != ArtifactKind::Figure {
            return Err(DltError::invalid("the figure engine opens figure projects only"));
        }
        if input.asset.bytes > native::MAX_NATIVE_BYTES {
            return Err(DltError::invalid(format!(
                "project file is {} bytes; the limit is {} bytes",
                input.asset.bytes,
                native::MAX_NATIVE_BYTES
            )));
        }
        let bytes = context.assets.read(&input.asset)?;
        if sha256(&bytes) != input.asset.sha256 {
            return Err(DltError::new(ErrorCode::IntegrityError, "project bytes do not match their recorded hash"));
        }
        let doc = native::decode(&bytes, context.assets)?;
        let mut artifact = Artifact::new(ArtifactKind::Figure, input.title.clone(), ENGINE_ID, ArtifactBody::Figure(doc));
        artifact.origin = "opened_native".to_owned();
        artifact.native = Some(NativeSource {
            asset: input.asset.clone(),
            mappings: BTreeMap::new(),
            preserved: BTreeMap::new(),
            preservation: PreservationManifest::default(),
        });
        let doc = doc_of(&artifact)?;
        let full = validate::full(doc, &artifact.id, context.assets, context.renderer)?;
        if has_blocking(&full.diagnostics) {
            return Err(diag::to_error(&full.diagnostics, "the project has blocking diagnostics and was not opened"));
        }
        // Kept but not rendered: reported as info, never refused (R09).
        let notes = guard::preserved_notes(doc);
        artifact.diagnostics = non_blocking(full.diagnostics);
        artifact.diagnostics.extend(notes);
        artifact.capabilities = capability_report();
        Ok(artifact)
    }

    fn capabilities(&self, _artifact: &Artifact) -> CapabilityReport {
        capability_report()
    }

    fn supported_persisted_features(&self) -> std::collections::BTreeSet<String> {
        native::PERSISTED_FEATURE_KEYS.iter().map(|key| (*key).to_owned()).collect()
    }

    fn persisted_features(&self, artifact: &Artifact) -> DltResult<std::collections::BTreeSet<String>> {
        let document = doc_of(artifact)?;
        guard::require_document(document)?;
        Ok(native::feature_keys_of(document).into_iter().map(str::to_owned).collect())
    }

    fn inspect(&self, artifact: &Artifact, node_ids: &[Id]) -> DltResult<Value> {
        let doc = doc_of(artifact)?;
        guard::require_document(doc)?;
        let mut items = Vec::new();
        for id in node_ids {
            if let Some(p) = doc.panels.iter().find(|p| &p.id == id) {
                let mut value = json!({"bounds": p.bounds, "label": p.label, "style": p.style});
                match &p.body {
                    PanelBody::Plot(spec) => {
                        let built = plot::build(doc, p, spec);
                        let axes = built.data.as_ref().map(|d| {
                            json!({
                                "kind": match d.kind { plot::Kind::Bar => "bar", plot::Kind::Line => "line", plot::Kind::Scatter => "scatter" },
                                "series": d.series,
                                "y": {"min": d.y.min.canonical(), "max": d.y.max.canonical(), "ticks": d.y.tick_labels, "explicit": d.y.explicit},
                                "x": d.x_axis.as_ref().map(|a| json!({"min": a.min.canonical(), "max": a.max.canonical(), "explicit": a.explicit})),
                            })
                        });
                        value["plot"] = json!({"spec": spec, "resolved": axes, "diagnostics": built.diagnostics});
                        items.push(json!({"id": id, "type": "plot_panel", "value": value}));
                    }
                    PanelBody::Diagram(dg) => {
                        value["diagram"] = json!({"nodes": dg.nodes, "edges": dg.edges, "evidence_ids": dg.evidence_ids, "node_origins": dg.node_origins});
                        items.push(json!({"id": id, "type": "diagram_panel", "value": value}));
                    }
                    PanelBody::Image(img) => {
                        value["image"] = json!(img);
                        items.push(json!({"id": id, "type": "image_panel", "value": value}));
                    }
                }
                continue;
            }
            let mut found = None;
            for p in &doc.panels {
                if let PanelBody::Diagram(dg) = &p.body {
                    if let Some(n) = dg.nodes.iter().find(|n| &n.id == id) {
                        found = Some(("diagram_node", json!({"panel": p.id, "node": n, "evidence_ids": dg.evidence_ids.get(id), "origin": dg.node_origins.get(id)})));
                    } else if let Some(e) = dg.edges.iter().find(|e| &e.id == id) {
                        found = Some(("diagram_connector", json!({"panel": p.id, "connector": e})));
                    }
                }
            }
            if let Some((kind, value)) = found {
                items.push(json!({"id": id, "type": kind, "value": value}));
                continue;
            }
            if let Some(ds) = doc.datasets.iter().find(|d| &d.id == id) {
                items.push(json!({"id": id, "type": "dataset", "value": ds}));
                continue;
            }
            if let Some(ev) = doc.evidence.iter().find(|e| &e.id == id) {
                items.push(json!({"id": id, "type": "evidence", "value": ev}));
                continue;
            }
            return Err(DltError::new(ErrorCode::NotFound, format!("element \"{id}\" not found in this figure")));
        }
        Ok(json!({"artifact_id": artifact.id, "revision": artifact.revision, "items": items}))
    }

    fn prepare_change(&self, artifact: &Artifact, operations: &[Operation], context: &EngineContext<'_>) -> DltResult<PreparedChange> {
        let doc = doc_of(artifact)?;
        if operations.is_empty() {
            return Err(DltError::invalid("a change needs at least one operation"));
        }
        // Every operation is checked before the first one is applied (guard.rs).
        guard::require_operations(doc, operations)?;
        validate::check_put_datasets(operations)?;
        let before = digest::digests(doc)?;
        let mut next = doc.clone();
        let mut changed: Vec<Id> = Vec::new();
        for op in operations {
            ops::apply(&mut next, op, &mut changed)?;
        }
        let full = validate::full(&next, &artifact.id, context.assets, context.renderer)?;
        if has_blocking(&full.diagnostics) {
            return Err(diag::to_error(&full.diagnostics, "change refused"));
        }
        // What the change adds to the datasets, judged against the document it replaces. A stored order that was already
        // unrecorded on open is not refused there, only a change that adds to it or moves its rows is (dataset.rs).
        let added = validate::change_findings(doc, &next, context.assets);
        if has_blocking(&added) {
            return Err(diag::to_error(&added, "change refused"));
        }
        let after = digest::digests(&next)?;
        // A digest may only change if some operation can change it. Core checks the allowed intents against
        // the same digests. A panel that carries a plot binding, or a removed panel, changes data as well.
        let content_ops = operations.iter().any(|op| op.intent() == Intent::Content);
        let data_ops = operations.iter().any(|op| op.intent() == Intent::Data || touches_data_binding(op));
        if after.content != before.content && !content_ops {
            return Err(DltError::new(ErrorCode::ContentLocked, "the change alters figure content but contains no content operation"));
        }
        if after.data != before.data && !data_ops {
            return Err(DltError::new(ErrorCode::ContentLocked, "the change alters figure data but contains no data operation"));
        }
        changed.sort();
        changed.dedup();
        let mut out = artifact.clone();
        out.body = ArtifactBody::Figure(next);
        out.capabilities = capability_report();
        Ok(PreparedChange { artifact: out, changed_ids: changed, diagnostics: non_blocking(full.diagnostics) })
    }

    fn validate(&self, artifact: &Artifact, context: &EngineContext<'_>) -> Vec<Diagnostic> {
        match doc_of(artifact) {
            Ok(doc) => {
                // Kept but not rendered: reported as info, never refused (R09). Reported with a refused figure too.
                let notes = guard::preserved_notes(doc);
                // A refused v1.2 item is reported as its own blocking diagnostics, not as a layout failure.
                if let Err(refused) = guard::require_document(doc) {
                    let mut diags = refused.diagnostics;
                    diags.extend(notes);
                    return diags;
                }
                let mut diags = match validate::run(doc, &artifact.id, context.assets, context.renderer) {
                    Ok(d) => d,
                    Err(e) => vec![diag::blocking("LAYOUT_FAILED", None, e.message)],
                };
                diags.extend(notes);
                diags
            }
            Err(e) => vec![diag::blocking("ARTIFACT_KIND_MISMATCH", None, e.message)],
        }
    }

    fn content_digest(&self, artifact: &Artifact) -> DltResult<ContentDigest> {
        let doc = doc_of(artifact)?;
        guard::require_document(doc)?;
        digest::digests(doc)
    }

    /// Snapshots are read back on commit and recovery. A snapshot that holds a refused item does not restore.
    fn restore(&self, bytes: &[u8]) -> DltResult<Artifact> {
        let artifact: Artifact = serde_json::from_slice(bytes).map_err(|e| DltError::invalid(e.to_string()))?;
        if let ArtifactBody::Figure(doc) = &artifact.body {
            guard::require_document(doc)?;
        }
        Ok(artifact)
    }

    fn layout(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<RenderScene> {
        let doc = doc_of(artifact)?;
        let full = validate::full(doc, &artifact.id, context.assets, context.renderer)?;
        let digests = digest::digests(doc)?;
        let Some(built) = full.built else {
            return Err(diag::to_error(&full.diagnostics, "the figure has blocking diagnostics and cannot be laid out"));
        };
        let _ = PAGE_INDEX;
        Ok(scene_of(artifact, built, &digests, doc, full.diagnostics))
    }

    fn encode_native(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<EncodedOutput> {
        let doc = doc_of(artifact)?;
        let full = validate::full(doc, &artifact.id, context.assets, context.renderer)?;
        if has_blocking(&full.diagnostics) {
            return Err(diag::to_error(&full.diagnostics, "the figure has blocking diagnostics; nothing was written"));
        }
        let bytes = native::encode(doc, context.assets)?;
        Ok(EncodedOutput {
            bytes,
            mime: native::MIME.to_owned(),
            editability: Editability::NativeData,
            diagnostics: non_blocking(full.diagnostics),
        })
    }

    fn verify_saved(&self, artifact: &Artifact, bytes: &[u8], context: &EngineContext<'_>) -> DltResult<PreservationVerification> {
        let doc = doc_of(artifact)?;
        let mut diagnostics = Vec::new();
        let mut checked_parts = 0u32;
        let mut checked_regions = 0u32;
        // Parsing checks every embedded payload without writing to the store. The saved model must equal the figure,
        // and every asset the file lists must already be in the store under its recorded reference.
        match native::parse(bytes, native::Limits::DEFAULT) {
            Ok(parsed) => {
                let saved = canonical_json(&serde_json::to_value(doc).map_err(|e| DltError::invalid(e.to_string()))?)?;
                let again = canonical_json(&serde_json::to_value(&parsed.doc).map_err(|e| DltError::invalid(e.to_string()))?)?;
                if saved != again {
                    diagnostics.push(diag::blocking(code::NATIVE_VERIFY_FAILED, None, "the reparsed project differs from the figure"));
                } else if let Err(e) = native::resolve(&parsed.assets, context.assets) {
                    diagnostics.push(diag::blocking(code::NATIVE_VERIFY_FAILED, None, e.message));
                } else {
                    checked_parts = parsed.assets.len() as u32;
                    checked_regions = (doc.panels.len() + doc.datasets.len() + doc.evidence.len()) as u32;
                }
            }
            Err(e) => diagnostics.push(diag::blocking(code::NATIVE_VERIFY_FAILED, None, e.message)),
        }
        Ok(PreservationVerification { checked_parts, checked_regions, diagnostics })
    }

    fn export(&self, artifact: &Artifact, request: &ExportRequest, context: &EngineContext<'_>) -> DltResult<EncodedOutput> {
        if request.artifact_id != artifact.id {
            return Err(DltError::invalid("export request names another artifact"));
        }
        if request.expected_revision != artifact.revision {
            return Err(DltError {
                current_revision: Some(artifact.revision),
                ..DltError::new(ErrorCode::RevisionConflict, "export request names an old revision")
            });
        }
        let format = request.format.as_str();
        if !["svg", "png", "pdf", "pptx"].contains(&format) {
            return Err(DltError::unsupported(format!("figure export format \"{format}\" is not supported")));
        }
        if format == "pptx" && self.pptx.is_none() {
            return Err(DltError::unsupported("PPTX export is not wired: the application must supply the scene exporter"));
        }
        let doc = doc_of(artifact)?;
        let full = validate::full(doc, &artifact.id, context.assets, context.renderer)?;
        if has_blocking(&full.diagnostics) {
            return Err(diag::to_error(&full.diagnostics, "the figure has blocking diagnostics; nothing was exported"));
        }
        let digests = digest::digests(doc)?;
        let built = full.built.ok_or_else(|| DltError::new(ErrorCode::Internal, "layout missing for a valid figure"))?;
        let mut scene = scene_of(artifact, built, &digests, doc, full.diagnostics);

        let pages: Vec<u32> = match &request.page_indices {
            None => vec![PAGE_INDEX],
            Some(list) if list.is_empty() => return Err(DltError::invalid("page_indices must not be empty")),
            Some(list) => list.clone(),
        };
        if pages.iter().any(|p| *p != PAGE_INDEX) {
            return Err(DltError::new(ErrorCode::NotFound, "a figure has one page (index 0)"));
        }
        scene.pages.retain(|p| pages.contains(&p.page_index));

        // Losses are blocking at export, so the caller must accept them explicitly.
        let mut scene_diags: Vec<Diagnostic> = scene
            .diagnostics
            .iter()
            .map(|d| {
                let mut d = d.clone();
                if LOSS_CODES.contains(&d.code.as_str()) {
                    d.blocking = true;
                }
                d
            })
            .collect();
        if format == "pptx" {
            for p in &doc.panels {
                match &p.body {
                    PanelBody::Plot(_) => scene_diags.push(diag::info(
                        code::PLOT_EXPORTED_AS_OBJECTS,
                        Some(p.id.as_str()),
                        "the chart is written as shapes, text and lines; it is not a native PowerPoint chart",
                    )),
                    // Non-blocking, so it is not a loss (LOSS_CODES): the characters stay editable text, the structure is gone.
                    PanelBody::Diagram(dg) => {
                        for n in dg.nodes.iter().filter(|n| n.ext.math.is_some()) {
                            scene_diags.push(diag::info(
                                FIGURE_MATH_FLATTENED,
                                Some(n.id.as_str()),
                                "the formula is written as text boxes and lines; its characters stay editable, its structure is not a PowerPoint equation",
                            ));
                        }
                    }
                    PanelBody::Image(_) => {}
                }
            }
        }
        // The caption is not drawn in any output format (05-decisions/figures.md); the export says so instead of dropping
        // it silently (R09). Non-blocking, like the other notes above.
        if !doc.caption.is_empty() {
            scene_diags.push(diag::info(
                code::FIGURE_CAPTION_NOT_RENDERED,
                None,
                "the caption is kept in the figure but not drawn in the exported file; put it in the manuscript text",
            ));
        }
        let has_raster = doc.panels.iter().any(|p| matches!(p.body, PanelBody::Image(_)));
        let mut out = match format {
            "pptx" => {
                let exporter = self.pptx.ok_or_else(|| DltError::unsupported("PPTX exporter missing"))?;
                exporter(&scene, context.assets, &artifact.title)?
            }
            other => context.renderer.encode(&scene, other, &request.profile, context.assets)?,
        };
        out.editability = cap_editability(out.editability, format, has_raster);
        out.diagnostics.extend(scene_diags);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_is_never_editable_and_rasters_cap_editability() {
        assert_eq!(cap_editability(Editability::Objects, "png", false), Editability::None);
        assert_eq!(cap_editability(Editability::Objects, "pptx", true), Editability::Vector);
        assert_eq!(cap_editability(Editability::NativeData, "pptx", false), Editability::Objects);
        assert_eq!(cap_editability(Editability::Objects, "svg", false), Editability::Objects);
    }

    #[test]
    fn capability_report_has_every_feature_key() {
        // Exhaustive on purpose: a new Feature variant stops this test from compiling until it is listed here.
        fn listed(f: Feature) -> bool {
            match f {
                Feature::Text | Feature::TextStyle | Feature::NamedStyles | Feature::Geometry | Feature::PageSetup => true,
                Feature::ParagraphFormat | Feature::Tables | Feature::Images | Feature::Pages | Feature::Notes => true,
                Feature::Diagram | Feature::Plot | Feature::Dataset | Feature::Evidence | Feature::FigurePanels => true,
                Feature::FigureLinks | Feature::CanvasLayout | Feature::Interactions | Feature::WebSource => true,
                Feature::NativeRoundtrip | Feature::RenderSvg | Feature::RenderPdf | Feature::RenderPng => true,
                Feature::HitTest | Feature::TextSelection | Feature::Effects | Feature::VectorPaths | Feature::GridLayout => true,
            }
        }
        let all = [
            Feature::Text, Feature::TextStyle, Feature::NamedStyles, Feature::Geometry, Feature::PageSetup,
            Feature::ParagraphFormat, Feature::Tables, Feature::Images, Feature::Pages, Feature::Notes,
            Feature::Diagram, Feature::Plot, Feature::Dataset, Feature::Evidence, Feature::FigurePanels,
            Feature::FigureLinks, Feature::CanvasLayout, Feature::Interactions, Feature::WebSource,
            Feature::NativeRoundtrip, Feature::RenderSvg, Feature::RenderPdf, Feature::RenderPng,
            Feature::HitTest, Feature::TextSelection, Feature::Effects, Feature::VectorPaths, Feature::GridLayout,
        ];
        assert!(all.iter().all(|f| listed(*f)));
        let report = capability_report();
        for f in all {
            assert!(report.contains_key(&f), "{f:?} has no capability entry");
        }
        assert_eq!(report.len(), all.len(), "every Feature variant must be explicit");
    }

    #[test]
    fn effects_is_approximate_with_the_accepted_fields_listed_and_the_other_v12_features_unsupported() {
        let report = capability_report();
        let effects = &report[&Feature::Effects];
        for support in [&effects.read, &effects.render, &effects.edit, &effects.preserve, &effects.write] {
            assert_eq!(*support, Support::Approximate, "{:?}", effects.reason);
        }
        assert!(effects.reason.contains("corner_radius (rectangle nodes, plot panels)"), "{}", effects.reason);
        assert!(effects.reason.contains("shadows (not used in paper figures)"), "{}", effects.reason);
        for f in [Feature::VectorPaths, Feature::GridLayout] {
            let support = &report[&f];
            assert_eq!(support.read, Support::Unsupported, "{f:?}");
            assert_eq!(support.render, Support::Unsupported, "{f:?}");
            assert_eq!(support.edit, Support::Unsupported, "{f:?}");
            assert!(!support.reason.is_empty(), "{f:?} needs a reason");
        }
    }
}
