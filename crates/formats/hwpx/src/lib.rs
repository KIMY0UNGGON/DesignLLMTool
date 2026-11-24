//! Native HWPX engine (`designllm-hwpx`).
//!
//! The engine opens, edits, lays out, and saves `.hwpx` packages without Hancom Office. Saves
//! patch the original bytes: only the elements whose model changed are rewritten. A save with no
//! change returns the original file byte for byte. A changed save rewrites the ZIP container, so each
//! entry keeps its payload, but archive header fields (version needed, flags, timestamps) may differ
//! from the original. Every save is reparsed and compared before it is reported as successful.
//!
//! Supported (see the capability report of a document for the exact state): paragraph text and
//! character shape (font, size, colour, bold, italic), paragraph format (alignment, spacing,
//! indent), named style definitions, page setup, block tables (cell text, column widths, new
//! tables), block pictures (new pictures, resizing, and figure link updates from PNG renders), and
//! same-format save. Not supported: removing blocks other than a picture that is the sole content of
//! its paragraph, reordering blocks, tables' structure, inline objects in new text, and floating
//! objects (kept byte for byte, never edited or drawn). A paragraph whose runs carry unmodeled markup
//! keeps its runs: text and character-style edits to it are refused.
#![forbid(unsafe_code)]

pub mod diag;
mod doc;
mod export;
mod figure;
mod guard;
mod header;
mod ops;
pub mod package;
mod section;
mod template;
pub mod text;
mod verify;
mod writer;
pub mod xmlx;

use std::collections::{BTreeMap, HashSet};

use designllm_contracts::{
    AssetRef, Artifact, ArtifactBody, ArtifactKind, CapabilityReport, ContentDigest, CreateRequest, Diagnostic,
    DltError, DltResult, Editability, EncodedOutput, EngineAdapter, EngineContext, ErrorCode, ExportRequest,
    FlowBlock, FlowDocument, Feature, FeatureSupport, Id, NativeSource, Operation, OpenInput, PreparedChange,
    PreservationVerification, RenderScene, Support, EMU_PER_HWPUNIT, json_hash,
};
use designllm_contracts::design_policy::{DirectFormattingPolicy, FEATURE_ARTIFACT_DESIGN_SYSTEM};
use designllm_contracts::{quantize, DesignSystemRef, TokenDef};
use serde_json::{json, Value};

pub use template::{A4_HEIGHT_HU, A4_WIDTH_HU, DEFAULT_MARGINS_HU};

/// Engine identifier stored in artifacts created or opened by this crate.
pub const ENGINE_ID: &str = "hwpx";

/// Native HWPX engine. Register with the core as `Arc::new(HwpxEngine::default())`.
#[derive(Debug, Default, Clone, Copy)]
pub struct HwpxEngine;

impl EngineAdapter for HwpxEngine {
    fn id(&self) -> &'static str {
        ENGINE_ID
    }

    fn kinds(&self) -> Vec<ArtifactKind> {
        vec![ArtifactKind::Hwpx]
    }

    fn create(&self, request: &CreateRequest, context: &EngineContext<'_>) -> DltResult<Artifact> {
        create_artifact(request, context)
    }

    fn open(&self, input: &OpenInput, context: &EngineContext<'_>) -> DltResult<Artifact> {
        open_artifact(input, context)
    }

    fn capabilities(&self, artifact: &Artifact) -> CapabilityReport {
        capability_report(artifact)
    }

    fn supported_persisted_features(&self) -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::from([FEATURE_ARTIFACT_DESIGN_SYSTEM.to_owned()])
    }

    /// The named styles hold the pinned tokens (SPEC.md:81); direct formatting is not checked.
    fn validate_shared_design_system_snapshot(&self, artifact: &Artifact, tokens: &BTreeMap<String, TokenDef>) -> DltResult<()> {
        let document = body(artifact)?;
        designllm_flow::design_rules::check_named_styles(tokens, |name| document.styles.get(name).cloned(), hwpx_font_size)
    }

    /// Published tokens become `SetNamedStyle` edits (`designllm_flow::design_rules`) through `prepare_change`,
    /// so the refusals of a named style edit apply unchanged: a style used by a paragraph whose runs carry
    /// unmodeled markup is refused (UNSAFE_ROUNDTRIP). `Clear` is refused, as `clear_overrides` is. A detach
    /// keeps the style values and drops only the reference.
    fn sync_design_system_in_context(
        &self,
        artifact: &Artifact,
        tokens: &BTreeMap<String, TokenDef>,
        design_system: Option<&DesignSystemRef>,
        direct_formatting: DirectFormattingPolicy,
        context: &EngineContext<'_>,
    ) -> DltResult<PreparedChange> {
        let Some(reference) = design_system else {
            let mut next = artifact.clone();
            next.design_system = None;
            return Ok(PreparedChange { artifact: next, changed_ids: Vec::new(), diagnostics: Vec::new() });
        };
        if direct_formatting == DirectFormattingPolicy::Clear {
            return Err(diag::unsupported(
                "clearing direct formatting is not supported for HWPX named styles (as clear_overrides); link with direct_formatting keep",
            ));
        }
        let document = body(artifact)?;
        // An edit of a linked document that left the governed styles alone: no package read, no prepared change.
        let style = |name: &str| document.styles.get(name).cloned();
        if designllm_flow::design_rules::already_synced(artifact, tokens, reference, direct_formatting, style, hwpx_font_size) {
            return Ok(PreparedChange { artifact: artifact.clone(), changed_ids: Vec::new(), diagnostics: Vec::new() });
        }
        let (operations, absent) =
            designllm_flow::design_rules::named_style_operations(tokens, direct_formatting, |name| document.styles.contains_key(name))?;
        let prepared = if operations.is_empty() {
            PreparedChange { artifact: artifact.clone(), changed_ids: Vec::new(), diagnostics: Vec::new() }
        } else {
            self.prepare_change(artifact, &operations, context)?
        };
        designllm_flow::design_rules::finish_sync(artifact, prepared, reference, &absent)
    }

    fn persisted_features(&self, artifact: &Artifact) -> DltResult<std::collections::BTreeSet<String>> {
        let document = body(artifact)?;
        if let Err(error) = guard::check_body(document) {
            let mut refusal = DltError::new(ErrorCode::SchemaUnsupported, "HWPX snapshot contains extension fields this engine refuses to persist");
            refusal.diagnostics = error.diagnostics;
            return Err(refusal);
        }
        Ok(std::collections::BTreeSet::new())
    }

    fn inspect(&self, artifact: &Artifact, node_ids: &[Id]) -> DltResult<Value> {
        inspect_nodes(body(artifact)?, node_ids)
    }

    fn prepare_change(
        &self,
        artifact: &Artifact,
        operations: &[Operation],
        context: &EngineContext<'_>,
    ) -> DltResult<PreparedChange> {
        guard::check_operations(operations)?;
        guard::check_body(body(artifact)?)?;
        let original = context.assets.read(&native(artifact)?.asset)?;
        let (_, loaded) = doc::load_bytes(&original, context.assets)?;
        let mut lines = native_lines(artifact)?;
        let locked = native_id_set(artifact, "regen_locked");
        let source = native_id_set(artifact, "source_blocks");
        let removable = removable_pictures(artifact);
        let inputs = ops::ChangeInputs {
            assets: context.assets,
            figure_links: &artifact.figure_links,
            locked_paragraphs: &locked,
            source_blocks: &source,
            removable_pictures: &removable,
            loaded: &loaded,
        };
        let prepared = ops::prepare(body(artifact)?, operations, &inputs, &mut lines)?;
        let mut candidate = artifact.clone();
        let preserved = &mut candidate.native.as_mut().expect("native source checked above").preserved;
        if lines.is_empty() {
            preserved.remove(LINE_OVERRIDES);
        } else {
            preserved.insert(LINE_OVERRIDES.into(), json!(lines));
        }
        candidate.body = ArtifactBody::Flow(prepared.body);
        candidate.figure_links = prepared.figure_links;
        Ok(PreparedChange { artifact: candidate, changed_ids: prepared.changed_ids, diagnostics: prepared.diagnostics })
    }

    fn validate(&self, artifact: &Artifact, context: &EngineContext<'_>) -> Vec<Diagnostic> {
        match self.encode_native(artifact, context) {
            Err(error) => error_diagnostics(error),
            Ok(output) => {
                let mut diagnostics = output.diagnostics.clone();
                match self.verify_saved(artifact, &output.bytes, context) {
                    Ok(report) => diagnostics.extend(report.diagnostics),
                    Err(error) => diagnostics.extend(error_diagnostics(error)),
                }
                diagnostics
            }
        }
    }

    /// Flow content plus the figure links: a link is a relation of the document (CONTRACTS.md §8), so changing
    /// it is a content change. Without links the digest is the flow digest itself (unlinked documents keep it).
    fn content_digest(&self, artifact: &Artifact) -> DltResult<ContentDigest> {
        let flow_digest = designllm_flow::content_digest(body(artifact)?)?;
        if artifact.figure_links.is_empty() {
            return Ok(flow_digest);
        }
        let mut links = artifact.figure_links.clone();
        links.sort_by(|a, b| a.target_node_id.cmp(&b.target_node_id));
        Ok(ContentDigest {
            content: json_hash(&json!({"flow": flow_digest.content, "figure_links": links}))?,
            data: flow_digest.data,
        })
    }

    fn layout(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<RenderScene> {
        let document = body(artifact)?;
        guard::check_body(document)?;
        // The original sections tell which page-break carriers the save folds, as in visual export.
        let Some(native) = artifact.native.as_ref() else { return export::layout(document, &[], &[], &ops::NativeLines::new(), context) };
        let original = context.assets.read(&native.asset)?;
        let (package, loaded) = doc::load_bytes(&original, context.assets)?;
        let links = export::text_link_spans(document, &package)?;
        export::layout(document, &loaded.sections, &links, &native_lines(artifact)?, context)
    }

    fn encode_native(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<EncodedOutput> {
        guard::check_body(body(artifact)?)?;
        let original = context.assets.read(&native(artifact)?.asset)?;
        let written = writer::encode(original, body(artifact)?, &native_lines(artifact)?, context.assets, &figure::figure_targets(&artifact.figure_links))?;
        Ok(EncodedOutput {
            bytes: written.bytes,
            mime: package::MIMETYPE.to_string(),
            editability: Editability::NativeData,
            diagnostics: written.diagnostics,
        })
    }

    fn verify_saved(
        &self,
        artifact: &Artifact,
        bytes: &[u8],
        context: &EngineContext<'_>,
    ) -> DltResult<PreservationVerification> {
        guard::check_body(body(artifact)?)?;
        let original = context.assets.read(&native(artifact)?.asset)?;
        Ok(verify::verify(body(artifact)?, &native_lines(artifact)?, &original, bytes, context.assets, &artifact.figure_links))
    }

    fn export(&self, artifact: &Artifact, request: &ExportRequest, context: &EngineContext<'_>) -> DltResult<EncodedOutput> {
        guard::check_body(body(artifact)?)?;
        if request.format.eq_ignore_ascii_case("hwpx") {
            designllm_flow::page_select::refuse_page_selection("hwpx", request.page_indices.as_deref())?;
            let mut output = self.encode_native(artifact, context)?;
            let report = self.verify_saved(artifact, &output.bytes, context)?;
            output.diagnostics.extend(report.diagnostics);
            return Ok(output);
        }
        let original = context.assets.read(&native(artifact)?.asset)?;
        let (package, loaded) = doc::load_bytes(&original, context.assets)?;
        let links = export::text_link_spans(body(artifact)?, &package)?;
        export::export_visual(body(artifact)?, &loaded.sections, &package, &links, &native_lines(artifact)?, request, context)
    }
}

/// A token font size on the HWPX unit grid.
fn hwpx_font_size(size: i64) -> i64 {
    quantize(size, EMU_PER_HWPUNIT).unwrap_or(size)
}

fn error_diagnostics(error: DltError) -> Vec<Diagnostic> {
    if error.diagnostics.is_empty() {
        vec![diag::blocking("HWPX_OPERATION_REFUSED", error.message, None)]
    } else {
        error.diagnostics
    }
}

fn body(artifact: &Artifact) -> DltResult<&FlowDocument> {
    if artifact.kind != ArtifactKind::Hwpx {
        return Err(DltError::invalid("artifact is not an HWPX document"));
    }
    match &artifact.body {
        ArtifactBody::Flow(document) => Ok(document),
        _ => Err(DltError::invalid("HWPX artifact has no flow body")),
    }
}

fn native(artifact: &Artifact) -> DltResult<&NativeSource> {
    artifact.native.as_ref().ok_or_else(|| DltError::invalid("HWPX artifact has no native source"))
}

const LINE_OVERRIDES: &str = "line_spacing_overrides";

fn native_lines(artifact: &Artifact) -> DltResult<ops::NativeLines> {
    match native(artifact)?.preserved.get(LINE_OVERRIDES) {
        Some(value) => serde::Deserialize::deserialize(value).map_err(|e| DltError::invalid(format!("invalid HWPX line spacing overrides: {e}"))),
        None => Ok(ops::NativeLines::new()),
    }
}

/// Native source record: the immutable original, small anchors, and part and region fingerprints.
fn native_source(asset: AssetRef, loaded: &doc::Loaded) -> NativeSource {
    let mut mappings: BTreeMap<Id, Value> = BTreeMap::new();
    for section in &loaded.sections {
        for block in &section.blocks {
            let kind = match block {
                section::PBlock::Para(_) => "paragraph",
                section::PBlock::Table(_) => "table",
                section::PBlock::Image(_) => "image",
                section::PBlock::Opaque(_) => "opaque",
            };
            let sole = matches!(block, section::PBlock::Image(picture) if picture.sole_content);
            mappings.insert(block.id().to_string(), json!({ "anchor": block.host_key(), "kind": kind, "sole": sole }));
        }
    }
    let mut preserved: BTreeMap<String, Value> = BTreeMap::new();
    preserved.insert(
        "line_cache".into(),
        json!({ "policy": "invalidate_on_reflow", "replayed": false }),
    );
    preserved.insert(
        "unmodeled_regions".into(),
        json!(loaded.regions.len()),
    );
    let locked: Vec<String> = loaded.sections.iter().flat_map(section::locked_paragraph_ids).collect();
    preserved.insert("regen_locked".into(), json!(locked));
    let source_blocks: Vec<String> = loaded.sections.iter().flat_map(section::block_ids_of).collect();
    preserved.insert("source_blocks".into(), json!(source_blocks));
    NativeSource { asset, mappings, preserved, preservation: loaded.fingerprints.clone() }
}

/// Ids listed under a key of the native source record: the paragraphs whose runs a rewrite would drop
/// (`regen_locked`) or every block of the source file (`source_blocks`).
fn native_id_set(artifact: &Artifact, key: &str) -> HashSet<String> {
    artifact
        .native
        .as_ref()
        .and_then(|native| native.preserved.get(key))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// Pictures of the source that a change may remove: each is the sole content of its paragraph, and its
/// paragraph anchor is unique among the top-level blocks, so removing it renumbers no other anchor.
fn removable_pictures(artifact: &Artifact) -> HashSet<String> {
    let Some(native) = artifact.native.as_ref() else { return HashSet::new() };
    let mut per_base: BTreeMap<String, usize> = BTreeMap::new();
    for value in native.mappings.values() {
        if let Some(anchor) = value["anchor"].as_str() {
            *per_base.entry(verify::base_anchor(anchor).to_string()).or_default() += 1;
        }
    }
    native
        .mappings
        .iter()
        .filter(|(_, value)| value["kind"] == "image" && value["sole"] == true)
        .filter(|(_, value)| value["anchor"].as_str().map(|a| per_base.get(verify::base_anchor(a)) == Some(&1)).unwrap_or(false))
        .map(|(id, _)| id.clone())
        .collect()
}

fn open_diagnostics(loaded: &doc::Loaded) -> Vec<Diagnostic> {
    let unmodeled: usize = loaded.sections.iter().map(|s| s.opaque.len()).sum::<usize>() + loaded.regions.iter().filter(|r| r.part == package::HEADER).count();
    let mut out = vec![diag::info(
        diag::UNMODELED_PRESERVED,
        format!("{unmodeled} unmodeled native element(s) are kept byte for byte"),
        None,
    )];
    let floating = loaded
        .sections
        .iter()
        .flat_map(|s| s.opaque.iter())
        .filter(|r| r.label == "pic")
        .count();
    if floating > 0 {
        out.push(diag::info(
            diag::FLOATING_NOT_RENDERED,
            format!("{floating} picture(s) without inline placement are preserved but not editable or drawn"),
            None,
        ));
    }
    let approximate: usize = loaded.sections.iter().map(|s| s.approximate_lines).sum();
    if approximate > 0 {
        out.push(diag::info(
            diag::LINE_SPACING_APPROXIMATED,
            format!("{approximate} paragraph(s) with AT_LEAST, BETWEEN_LINES, or an unknown line spacing type have an approximate line pitch; their original line spacing is kept unless it is edited"),
            None,
        ));
    }
    out
}

fn open_artifact(input: &OpenInput, context: &EngineContext<'_>) -> DltResult<Artifact> {
    if input.kind != ArtifactKind::Hwpx {
        return Err(DltError::invalid("OpenInput kind is not hwpx"));
    }
    let bytes = context.assets.read(&input.asset)?;
    let (_package, loaded) = doc::load_bytes(&bytes, context.assets)?;
    let diagnostics = open_diagnostics(&loaded);
    let native = native_source(input.asset.clone(), &loaded);
    let mut artifact = Artifact::new(ArtifactKind::Hwpx, input.title.clone(), ENGINE_ID, ArtifactBody::Flow(loaded.flow));
    artifact.origin = "imported".into();
    artifact.native = Some(native);
    artifact.capabilities = capability_report(&artifact);
    artifact.diagnostics = diagnostics;
    Ok(artifact)
}

fn hu_from_emu(emu: i64, what: &str, diagnostics: &mut Vec<Diagnostic>) -> DltResult<i64> {
    if emu <= 0 {
        return Err(DltError::invalid(format!("{what} must be positive")));
    }
    let hu = header::emu_to_hu(emu);
    if hu * EMU_PER_HWPUNIT != emu {
        diagnostics.push(diag::info(
            diag::UNIT_ROUNDING,
            format!("{what}: requested {emu} EMU, applied {} EMU (native grid {EMU_PER_HWPUNIT} EMU)", hu * EMU_PER_HWPUNIT),
            None,
        ));
    }
    Ok(hu)
}

fn create_artifact(request: &CreateRequest, context: &EngineContext<'_>) -> DltResult<Artifact> {
    if request.kind != ArtifactKind::Hwpx {
        return Err(DltError::invalid("CreateRequest kind is not hwpx"));
    }
    if let Some(template) = &request.template {
        if !template.is_empty() && template != "blank" {
            return Err(DltError::unsupported(format!("HWPX template {template} is not available")));
        }
    }
    let mut diagnostics = Vec::new();
    let (width, height) = match request.size {
        Some(size) => (
            hu_from_emu(size.width, "page width", &mut diagnostics)?,
            hu_from_emu(size.height, "page height", &mut diagnostics)?,
        ),
        None => (A4_WIDTH_HU, A4_HEIGHT_HU),
    };
    let bytes = template::blank_package(width, height, DEFAULT_MARGINS_HU)?;
    let asset = context.assets.put(&bytes, package::MIMETYPE)?;
    let (_package, loaded) = doc::load_bytes(&bytes, context.assets)?;
    let native = native_source(asset, &loaded);
    let mut artifact = Artifact::new(ArtifactKind::Hwpx, request.title.clone(), ENGINE_ID, ArtifactBody::Flow(loaded.flow));
    artifact.origin = "created".into();
    artifact.native = Some(native);
    artifact.capabilities = capability_report(&artifact);
    artifact.diagnostics = diagnostics;
    Ok(artifact)
}

fn support(read: Support, render: Support, edit: Support, preserve: Support, write: Support, reason: &str) -> FeatureSupport {
    FeatureSupport { read, render, edit, preserve, write, reason: reason.to_string() }
}

/// Capability report for every feature key. Keys not listed here are not reported as supported.
pub fn capability_report(artifact: &Artifact) -> CapabilityReport {
    let mut report: CapabilityReport = BTreeMap::new();
    use Support::{Approximate, PreserveOnly, Supported, Unsupported};
    report.insert(
        Feature::Text,
        support(Supported, Approximate, Supported, Supported, Supported, "paragraph text as grapheme ranges; inline objects are protected atoms; layout is this engine's own line breaking"),
    );
    report.insert(
        Feature::TextStyle,
        support(Supported, Approximate, Supported, Supported, Supported, "font face of the Hangul slot, size in 1/100 pt, colour, bold, italic; other style fields are not stored in HWPX text"),
    );
    report.insert(
        Feature::NamedStyles,
        support(Supported, Approximate, Supported, Supported, Supported, "named style definitions edit the referenced character shape; clear_overrides is not supported"),
    );
    report.insert(
        Feature::Geometry,
        support(Supported, Approximate, Supported, Supported, Supported, "picture width and height are editable (inline placement); floating object positions are preserved only"),
    );
    report.insert(
        Feature::PageSetup,
        support(Supported, Approximate, Supported, Supported, Supported, "page size and margins in HWPUNIT (1/7200 inch) on the native grid"),
    );
    report.insert(
        Feature::ParagraphFormat,
        support(Supported, Approximate, Supported, Supported, Supported, "alignment, spacing before and after, first-line indent, and percent line spacing"),
    );
    report.insert(
        Feature::Tables,
        support(Supported, Approximate, Supported, Supported, Supported, "block tables with resolved column grid: cell text, column widths, new tables; merged structure and tables inside text lines are preserved only"),
    );
    report.insert(
        Feature::Images,
        support(Supported, Approximate, Supported, Supported, Supported, "block pictures: insert from PNG, BMP, JPEG, GIF; resize; a figure link replaces the PNG render of a block picture; floating pictures are preserved only"),
    );
    report.insert(
        Feature::Pages,
        support(Supported, Approximate, Unsupported, PreserveOnly, Supported, "pages come from this engine's layout; a pageBreak attribute becomes the paragraph's page break"),
    );
    report.insert(
        Feature::Notes,
        support(Unsupported, Unsupported, Unsupported, PreserveOnly, Supported, "footnotes and endnotes are preserved byte for byte, not shown or edited"),
    );
    for feature in [
        Feature::Diagram,
        Feature::Plot,
        Feature::Dataset,
        Feature::Evidence,
        Feature::FigurePanels,
        Feature::CanvasLayout,
        Feature::Interactions,
        Feature::WebSource,
    ] {
        report.insert(feature, support(Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "not a HWPX document feature"));
    }
    report.insert(
        Feature::FigureLinks,
        support(Supported, Unsupported, Supported, Supported, Supported, "png only; the link is workspace state and the render is written as an ordinary embedded PNG picture; the HWPX package records no link"),
    );
    // Contract v1.2 (P0): refused by the guards in guard.rs before any change, save, render, or export.
    report.insert(
        Feature::Effects,
        support(Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "not supported: corner radius, opacity, shadows, gradients, line height, letter spacing, vertical align, font weight, stroke dash, cap and join, underline and strikethrough are refused; HWPX stores none of them"),
    );
    report.insert(
        Feature::VectorPaths,
        support(Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "not supported: vector paths (NodeKind::Path), icons, and node extension fields are refused; HWPX has no vector path model"),
    );
    report.insert(
        Feature::GridLayout,
        support(Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "not supported: layout mode grid, grid columns and span, justify, align self, padding sides, and row gap are refused"),
    );
    report.insert(
        Feature::NativeRoundtrip,
        support(Supported, Supported, Supported, Supported, Supported, "same-format save patches the original bytes and is reparsed before success; Hancom reopening is not verified in this build"),
    );
    report.insert(
        Feature::RenderSvg,
        support(Supported, Approximate, Supported, Supported, Supported, "SVG of one page through the shared renderer with this engine's layout"),
    );
    report.insert(
        Feature::RenderPdf,
        support(Supported, Approximate, Supported, Supported, Supported, "PDF of selected pages through the shared renderer; floating objects are listed as losses"),
    );
    report.insert(
        Feature::RenderPng,
        support(Supported, Approximate, Supported, Supported, Supported, "PNG of one page through the shared renderer; floating objects are listed as losses"),
    );
    report.insert(
        Feature::HitTest,
        support(Supported, Supported, Supported, Supported, Supported, "hit tests use the shared renderer on this engine's layout"),
    );
    report.insert(
        Feature::TextSelection,
        support(Supported, Supported, Supported, Supported, Supported, "selection ranges are grapheme ranges with protected atoms counted as one grapheme"),
    );
    let _ = artifact;
    report
}

// Every missing id goes to affected_ids once, in request order; the message names the first one.
fn inspect_nodes(document: &FlowDocument, node_ids: &[Id]) -> DltResult<Value> {
    let mut nodes = Vec::with_capacity(node_ids.len());
    let mut missing: Vec<Id> = Vec::new();
    for id in node_ids {
        match find_value(document, id) {
            Some(value) => nodes.push(json!({ "id": id, "value": value })),
            None if !missing.contains(id) => missing.push(id.clone()),
            None => {}
        }
    }
    if let Some(first) = missing.first() {
        let mut error = DltError::new(ErrorCode::NotFound, format!("node {first} not found"));
        error.affected_ids = missing;
        return Err(error);
    }
    Ok(json!({ "nodes": nodes }))
}

fn find_value(document: &FlowDocument, id: &str) -> Option<Value> {
    for section in &document.sections {
        if section.id == id {
            return serde_json::to_value(&section.setup).ok();
        }
        if let Some(v) = find_in_blocks(&section.blocks, id) {
            return Some(v);
        }
    }
    None
}

fn find_in_blocks(blocks: &[FlowBlock], id: &str) -> Option<Value> {
    for block in blocks {
        match block {
            FlowBlock::Paragraph(p) => {
                if p.id == id {
                    return serde_json::to_value(p).ok();
                }
                if let Some(run) = p.runs.iter().find(|r| r.id == id) {
                    return serde_json::to_value(run).ok();
                }
            }
            FlowBlock::Table(t) => {
                if t.id == id {
                    return serde_json::to_value(t).ok();
                }
                for cell in t.rows.iter().flatten() {
                    if cell.id == id {
                        return serde_json::to_value(cell).ok();
                    }
                    if let Some(v) = find_in_blocks(&cell.blocks, id) {
                        return Some(v);
                    }
                }
            }
            FlowBlock::Image(node) => {
                if node.id == id {
                    return serde_json::to_value(node).ok();
                }
            }
            FlowBlock::Opaque { id: block_id, label } => {
                if block_id == id {
                    return Some(json!({ "type": "opaque", "label": label }));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_identity_and_kind() {
        let engine = HwpxEngine::default();
        assert_eq!(engine.id(), "hwpx");
        assert_eq!(engine.kinds(), vec![ArtifactKind::Hwpx]);
    }

    #[test]
    fn capability_report_covers_the_declared_keys_without_claiming_figures() {
        let artifact = Artifact::new(
            ArtifactKind::Hwpx,
            "t",
            ENGINE_ID,
            ArtifactBody::Flow(FlowDocument::default()),
        );
        let report = capability_report(&artifact);
        assert_eq!(report[&Feature::Plot].edit, Support::Unsupported);
        assert_eq!(report[&Feature::Tables].edit, Support::Supported);
        assert!(report.contains_key(&Feature::RenderPdf));
    }

    #[test]
    fn export_of_an_unknown_format_is_refused() {
        // Format dispatch is covered by the integration tests; this only pins the error text.
        let err = DltError::unsupported("HWPX cannot export to docx");
        assert_eq!(err.code, ErrorCode::UnsupportedOperation);
    }
}
