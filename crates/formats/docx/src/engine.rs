//! `EngineAdapter` for DOCX (`designllm-docx`).
//!
//! The original package is an immutable asset. The artifact keeps the flow model and small native records.
//! Every native save writes from the original bytes and the model, and `verify_saved` re-parses the
//! result before it can be published.

use std::collections::BTreeMap;

use designllm_contracts::{
    json_hash, sha256, Artifact, ArtifactBody, ArtifactKind, AssetRef, AssetStore, CapabilityReport, ContentDigest,
    CreateRequest, Diagnostic, DltError, DltResult, EncodedOutput, EngineAdapter, EngineContext, Editability, ErrorCode,
    ExportRequest, Feature, FeatureSupport, FlowBlock, FlowDocument, FlowStyleInfo, Id, NativeSource, OpenInput, Operation, OpaqueRegion,
    PreparedChange, PreservationManifest, PreservationVerification, RenderScene, Support,
};
use designllm_contracts::design_policy::{DirectFormattingPolicy, FEATURE_ARTIFACT_DESIGN_SYSTEM};
use designllm_contracts::{DesignSystemRef, TokenDef};
use designllm_contracts::page_losses;
use designllm_flow as flow;
use designllm_flow::tree::FlowNode;
use designllm_package::opc;
use designllm_package::Package;
use serde_json::{json, Value};

use crate::diag;
use crate::guard;
use crate::model;
use crate::ops::{self, State};
use crate::parse;
use crate::styles::{FlowStyleList, Overrides, StyleTable};
use crate::template::{self, DOCX_MIME};
use crate::tree;
use crate::verify;
use crate::write::{self, MUTABLE_PARTS, REL_STYLES, STYLES_PART};

/// Engine identifier stored in `Artifact.engine_id`.
pub const ENGINE_ID: &str = "designllm-docx";

/// Native DOCX engine. Construct with `DocxEngine::default()`.
#[derive(Debug, Default, Clone, Copy)]
pub struct DocxEngine;

/// Native-state key holding style overrides (named-style edits).
const STYLE_OVERRIDES: &str = "style_overrides";
/// Native-state key holding the main document part name.
const DOCUMENT_PART: &str = "document_part";
/// Native-state key of a linked document: the `styleId` of each named style a published token sets, by the
/// style name the token gives. Written only while a design system is linked.
const DESIGN_SYSTEM_STYLES: &str = "design_system_styles";
/// Native-state key identifying the model's source for existing picture alt text.
const PICTURE_ALT_SOURCE: &str = "picture_alt_source";
/// Marker written by `open` after reading `wp:docPr descr` into each imported picture's `Node.text`.
const PICTURE_ALT_SOURCE_V1: &str = "docx.descr.v1";
/// Relationship type of a hyperlink; an external one names the URL as its target.
const REL_HYPERLINK: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PictureAltSource { DescrV1, Legacy }

/// Original package read from the asset store, with its document part and styles (no overrides).
struct Original {
    bytes: Vec<u8>,
    doc_part: String,
    styles: StyleTable,
}

fn load_original(assets: &dyn AssetStore, asset: &AssetRef) -> DltResult<Original> {
    let bytes = assets.read(asset)?;
    if sha256(&bytes) != asset.sha256 {
        return Err(DltError::new(ErrorCode::IntegrityError, "the original DOCX bytes do not match their asset hash"));
    }
    let package = guard::open_input_package(&bytes)?;
    let doc_part = write::main_document_part(&package)?;
    let styles_bytes = styles_part_bytes(&package, &doc_part)?;
    let styles = StyleTable::parse(styles_bytes.as_deref())?;
    Ok(Original { bytes, doc_part, styles })
}

/// The bytes of the styles part, found through the document's styles relationship (`word/styles.xml` when there is
/// none). `None` when the part is missing.
fn styles_part_bytes(package: &Package, doc_part: &str) -> DltResult<Option<Vec<u8>>> {
    let rels = write::read_rels(package, doc_part)?;
    let styles_part = rels
        .iter()
        .find(|r| r.rel_type == REL_STYLES && !r.external)
        .and_then(|r| opc::resolve_target(doc_part, &r.target).ok())
        .unwrap_or_else(|| STYLES_PART.to_string());
    Ok(package.part(&styles_part).ok().map(|b| b.to_vec()))
}

fn native_of(artifact: &Artifact) -> DltResult<&NativeSource> {
    artifact
        .native
        .as_ref()
        .ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "the DOCX artifact has no native source"))
}

fn document_of(artifact: &Artifact) -> DltResult<&FlowDocument> {
    match &artifact.body {
        ArtifactBody::Flow(document) => Ok(document),
        _ => Err(DltError::new(ErrorCode::IntegrityError, "the DOCX artifact body is not a flow document")),
    }
}

fn picture_alt_source(native: &NativeSource) -> DltResult<PictureAltSource> {
    match native.preserved.get(PICTURE_ALT_SOURCE) {
        None => Ok(PictureAltSource::Legacy),
        Some(Value::String(marker)) if marker == PICTURE_ALT_SOURCE_V1 => Ok(PictureAltSource::DescrV1),
        Some(value) => {
            let message = format!("unsupported DOCX picture alt-source marker {value}; the source description cannot be verified");
            let mut error = DltError::new(ErrorCode::UnsafeRoundtrip, message.clone());
            error.diagnostics.push(diag::blocking(diag::PICTURE_ALT_SOURCE_UNSUPPORTED, message, None));
            Err(error)
        }
    }
}

fn load_state(artifact: &Artifact) -> DltResult<State> {
    let native = native_of(artifact)?;
    let _ = picture_alt_source(native)?;
    let document = document_of(artifact)?.clone();
    let records = model::records_from_mappings(&native.mappings)?;
    let overrides: Overrides = match native.preserved.get(STYLE_OVERRIDES) {
        Some(value) => serde_json::from_value(value.clone()).map_err(|e| DltError::new(ErrorCode::IntegrityError, e.to_string()))?,
        None => Overrides::new(),
    };
    Ok(State { document, records, overrides, figure_links: artifact.figure_links.clone() })
}

/// Parses source picture descriptions by original paragraph path. The path is engine-owned metadata in `Rec::Image`;
/// `descr` itself is read from the immutable package bytes, never trusted from the model marker.
fn original_picture_descriptions(original: &Original) -> DltResult<BTreeMap<Vec<usize>, Option<String>>> {
    let package = guard::open_input_package(&original.bytes)?;
    let rels = write::read_rels(&package, &original.doc_part)?;
    let rels_map: BTreeMap<String, String> = rels
        .iter()
        .filter(|r| !r.external)
        .map(|r| (r.id.clone(), opc::resolve_target(&original.doc_part, &r.target).unwrap_or_default()))
        .collect();
    let document = package.part(&original.doc_part)?.to_vec();
    let read = |name: &str| package.part(name).ok().map(|bytes| bytes.to_vec());
    let parsed = parse::parse_document(&document, &original.styles, &rels_map, &read)?;
    Ok(parsed.pictures.into_iter().map(|(path, picture)| (path, picture.descr)).collect())
}

fn picture_alt_error(node_id: &Id) -> DltError {
    let message = format!(
        "picture {node_id} alt text differs from its original wp:docPr descr; the native writer preserves the source description"
    );
    let mut error = DltError::new(ErrorCode::UnsafeRoundtrip, message.clone());
    error.diagnostics.push(diag::blocking(diag::PICTURE_ALT_MISMATCH, message, Some(node_id)));
    error
}

/// Checks only imported block pictures. New pictures have no source paragraph and their text is written normally.
fn picture_alt_diagnostics(artifact: &Artifact, state: &State, original: &Original) -> DltResult<Vec<Diagnostic>> {
    fn check_blocks(
        blocks: &[FlowBlock],
        records: &model::Records,
        descriptions: &BTreeMap<Vec<usize>, Option<String>>,
        source: PictureAltSource,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> DltResult<()> {
        for block in blocks {
            match block {
                FlowBlock::Image(node) => {
                    let record = records.get(&node.id).ok_or_else(|| {
                        DltError::new(ErrorCode::IntegrityError, format!("picture {} has no native record", node.id))
                    })?;
                    let path = match record {
                        model::Rec::Image { path: Some(path) } => path,
                        model::Rec::Image { path: None } => continue,
                        _ => return Err(DltError::new(ErrorCode::IntegrityError, format!("native record for picture {} has the wrong kind", node.id))),
                    };
                    let description = descriptions
                        .get(path)
                        .ok_or_else(|| DltError::new(ErrorCode::IntegrityError, format!("picture {} has no source description record", node.id)))?
                        .as_deref()
                        .unwrap_or_default();
                    let matches_source = node.text == description;
                    match source {
                        PictureAltSource::DescrV1 if !matches_source => return Err(picture_alt_error(&node.id)),
                        PictureAltSource::Legacy if !node.text.is_empty() && !matches_source => return Err(picture_alt_error(&node.id)),
                        _ => {}
                    }
                    if source == PictureAltSource::Legacy && node.text.is_empty() && !description.is_empty() {
                        diagnostics.push(diag::info(
                            diag::LEGACY_PICTURE_ALT_PRESERVED,
                            "this legacy workspace does not model the imported picture description; the original alt text is preserved",
                            Some(&node.id),
                        ));
                    }
                }
                FlowBlock::Table(table) => {
                    for row in &table.rows {
                        for cell in row {
                            check_blocks(&cell.blocks, records, descriptions, source, diagnostics)?;
                        }
                    }
                }
                FlowBlock::Paragraph(_) | FlowBlock::Opaque { .. } => {}
            }
        }
        Ok(())
    }

    let native = native_of(artifact)?;
    let source = picture_alt_source(native)?;
    let descriptions = original_picture_descriptions(original)?;
    let mut diagnostics = Vec::new();
    for section in &state.document.sections {
        check_blocks(&section.blocks, &state.records, &descriptions, source, &mut diagnostics)?;
    }
    Ok(diagnostics)
}

fn store_state(artifact: &mut Artifact, state: State) -> DltResult<()> {
    let mappings = model::records_to_mappings(&state.records)?;
    let overrides = serde_json::to_value(&state.overrides).map_err(|e| DltError::invalid(e.to_string()))?;
    artifact.body = ArtifactBody::Flow(state.document);
    artifact.figure_links = state.figure_links;
    let native = artifact
        .native
        .as_mut()
        .ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "the DOCX artifact has no native source"))?;
    native.mappings = mappings;
    native.preserved.insert(STYLE_OVERRIDES.to_string(), overrides);
    Ok(())
}

fn linked_style_ids(native: &NativeSource) -> DltResult<BTreeMap<String, String>> {
    match native.preserved.get(DESIGN_SYSTEM_STYLES) {
        Some(value) => serde_json::from_value(value.clone()).map_err(|e| DltError::new(ErrorCode::IntegrityError, e.to_string())),
        None => Ok(BTreeMap::new()),
    }
}

/// The resolved named style a token's style name was linked to (`DESIGN_SYSTEM_STYLES`); None for a style the
/// document did not define when it was linked.
fn linked_style(document: &FlowDocument, style_ids: &BTreeMap<String, String>, name: &str) -> Option<designllm_contracts::Style> {
    style_ids.get(name).and_then(|id| document.styles.get(id)).cloned()
}

/// A token font size on the DOCX half-point grid.
fn docx_font_size(size: i64) -> i64 {
    crate::units::to_half_points(size).map_or(size, |(_, applied)| applied)
}

/// Two token style names (a display name and a `styleId`, say) that resolve to one DOCX style would pin two
/// values for it; the link is refused.
fn refuse_style_aliases(style_ids: &BTreeMap<String, String>) -> DltResult<()> {
    let mut names_by_id: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (name, id) in style_ids {
        names_by_id.entry(id.as_str()).or_default().push(name.as_str());
    }
    let Some((id, names)) = names_by_id.into_iter().find(|(_, names)| names.len() > 1) else { return Ok(()) };
    let message = format!("published token style names {} all name the DOCX style {id}; publish one of them", names.join(", "));
    let mut error = DltError::new(ErrorCode::UnsupportedOperation, message.clone());
    error.diagnostics.push(Diagnostic {
        code: flow::design_rules::DESIGN_TOKEN_UNMAPPED.into(),
        message,
        node_id: None,
        blocking: true,
        params: BTreeMap::from([("style_id".to_owned(), Value::from(id))]),
    });
    Err(error)
}

fn feature(read: Support, render: Support, edit: Support, preserve: Support, write: Support, reason: &str) -> FeatureSupport {
    FeatureSupport { read, render, edit, preserve, write, reason: reason.to_string() }
}

/// Diagnostics that describe content the preview or export does not draw. No page of the scene shows that content, so a
/// page selection cannot tell where it belongs: headers and footers, page borders and the page background are on every
/// page, and the page of a floating object is not determined by this layout. A page
/// selection keeps them all.
const UNRENDERED_LOSSES: [&str; 4] = [
    diag::HEADER_FOOTER_NOT_RENDERED,
    diag::FLOATING_OBJECT_NOT_RENDERED,
    page_losses::PAGE_BORDER_NOT_RENDERED,
    page_losses::PAGE_BACKGROUND_NOT_RENDERED,
];

fn is_unrendered_loss(code: &str) -> bool {
    UNRENDERED_LOSSES.contains(&code)
}

/// The hyperlinks of the text. A `w:hyperlink` is a protected atom of its paragraph: the records give the paragraph's
/// path in the original part and the atom's child index, and the element's `r:id` names an external relationship whose
/// target is the URL. Spans without such a target are left out.
fn text_link_spans(artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<Vec<flow::TextLinkSpan>> {
    let Some(native) = artifact.native.as_ref() else { return Ok(Vec::new()) };
    let document = document_of(artifact)?;
    let records = model::records_from_mappings(&native.mappings)?;
    let original = load_original(context.assets, &native.asset)?;
    let package = guard::open_input_package(&original.bytes)?;
    let targets: BTreeMap<String, String> = write::read_rels(&package, &original.doc_part)?
        .into_iter()
        .filter(|rel| rel.external && rel.rel_type == REL_HYPERLINK)
        .map(|rel| (rel.id, rel.target))
        .collect();
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let tree = tree::Tree::parse(package.part(&original.doc_part)?)?;
    let mut paragraphs = Vec::new();
    flow::tree::for_each_paragraph(document, |p| paragraphs.push(p));
    let mut spans = Vec::new();
    for p in paragraphs {
        let Some(model::Rec::Paragraph { path: Some(path), .. }) = records.get(&p.id) else { continue };
        let Some(node) = tree::node_at(tree.root(), path) else { continue };
        let children = tree::elements(node);
        let lp = flow::LogicalParagraph::new(p);
        for (run_index, run) in p.runs.iter().enumerate() {
            let Some(model::Rec::Run { origin: Some(origin), atom: true, .. }) = records.get(&run.id) else { continue };
            let Some(child) = children.get(*origin) else { continue };
            if !tree::is_w(*child, "hyperlink") {
                continue;
            }
            let Some(url) = child.attribute((tree::NS_R, "id")).and_then(|id| targets.get(id)) else { continue };
            let (start, end) = lp.run_range(run_index);
            spans.push(flow::TextLinkSpan { paragraph_id: p.id.clone(), start, end, url: url.clone() });
        }
    }
    Ok(spans)
}

fn node_json(node: FlowNode<'_>) -> Value {
    match node {
        FlowNode::Section(section) => json!({"type": "section", "id": section.id, "setup": section.setup}),
        FlowNode::Paragraph(p) => json!({
            "type": "paragraph",
            "id": p.id,
            "style_ref": p.style_ref,
            "format": p.format,
            "text": flow::logical_text(p),
            "runs": p.runs.iter().map(|r| json!({"id": r.id, "text": r.text, "protected": r.protected, "style": r.style})).collect::<Vec<_>>(),
        }),
        FlowNode::Run { paragraph, index } => {
            let run = &paragraph.runs[index];
            json!({"type": "run", "id": run.id, "paragraph": paragraph.id, "text": run.text, "protected": run.protected, "style": run.style})
        }
        FlowNode::Table(table) => json!({
            "type": "table",
            "id": table.id,
            "column_widths": table.column_widths,
            "rows": table.rows.iter().map(|row| row.iter().map(|c| json!({"id": c.id, "column_span": c.column_span, "row_span": c.row_span})).collect::<Vec<_>>()).collect::<Vec<_>>(),
        }),
        FlowNode::Cell { cell, .. } => json!({"type": "cell", "id": cell.id, "column_span": cell.column_span, "row_span": cell.row_span}),
        FlowNode::Image(node) => json!({"type": "image", "id": node.id, "bounds": node.bounds, "asset": node.asset}),
        FlowNode::Opaque { id, label } => json!({"type": "opaque", "id": id, "label": label}),
    }
}

impl DocxEngine {
    /// The `flow.styles` list of an original DOCX package: the styles in document order, and a diagnostic for each
    /// style left out. `EngineAdapter::flow_styles` gets no asset store, so it cannot reach the original bytes;
    /// callers pass them here. A package without a styles part lists none.
    pub fn list_flow_styles(original: &[u8]) -> DltResult<FlowStyleList> {
        let package = guard::open_input_package(original)?;
        let doc_part = write::main_document_part(&package)?;
        match styles_part_bytes(&package, &doc_part)? {
            Some(bytes) => crate::styles::list_flow_styles(&bytes),
            None => Ok(FlowStyleList::default()),
        }
    }

    /// The native save and the original parts it drops (the media of pictures the artifact no longer uses).
    /// A save that would lose an XML comment or processing instruction is refused.
    fn encode_with_released(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<(EncodedOutput, Vec<String>)> {
        // The writer stores none of the v1.2 values, so a body that carries one is refused instead of saved without it.
        let found = designllm_flow::guard::document_v12(document_of(artifact)?).into_iter().map(|f| (None, f)).collect();
        designllm_flow::guard::refuse_v12("DOCX documents", found)?;
        let native = native_of(artifact)?;
        let original = load_original(context.assets, &native.asset)?;
        let state = load_state(artifact)?;
        let diagnostics = picture_alt_diagnostics(artifact, &state, &original)?;
        let encoded = write::encode_detailed(&original.bytes, &state, &original.styles, context.assets)?;
        let before = verify::xml_metadata(&original.bytes)?;
        let after = verify::xml_metadata(&encoded.bytes)?;
        if let Some(difference) = verify::metadata_difference(&before, &after) {
            return Err(DltError::new(
                ErrorCode::UnsafeRoundtrip,
                format!("writing this DOCX would change its XML comments or processing instructions ({difference}); the original can be saved unchanged"),
            ));
        }
        let output = EncodedOutput { bytes: encoded.bytes, mime: DOCX_MIME.to_string(), editability: Editability::NativeData, diagnostics };
        Ok((output, encoded.released_parts))
    }
}

impl EngineAdapter for DocxEngine {
    fn id(&self) -> &'static str {
        ENGINE_ID
    }

    fn kinds(&self) -> Vec<ArtifactKind> {
        vec![ArtifactKind::Docx]
    }

    fn create(&self, request: &CreateRequest, context: &EngineContext<'_>) -> DltResult<Artifact> {
        if request.kind != ArtifactKind::Docx {
            return Err(DltError::invalid("the DOCX engine creates DOCX documents only"));
        }
        if let Some(template) = &request.template {
            if !template.is_empty() && template != "blank" {
                return Err(DltError::unsupported(format!("DOCX template {template} is not available; use a blank document")));
            }
        }
        let bytes = template::blank_package(request.size, &request.title)?;
        let asset = context.assets.put(&bytes, DOCX_MIME)?;
        let input = OpenInput { kind: ArtifactKind::Docx, title: request.title.clone(), asset };
        let mut artifact = self.open(&input, context)?;
        artifact.origin = "created".to_string();
        Ok(artifact)
    }

    fn open(&self, input: &OpenInput, context: &EngineContext<'_>) -> DltResult<Artifact> {
        if input.kind != ArtifactKind::Docx {
            return Err(DltError::invalid("the DOCX engine opens DOCX inputs only"));
        }
        let original = load_original(context.assets, &input.asset)?;
        let package = guard::open_input_package(&original.bytes)?;
        let rels = write::read_rels(&package, &original.doc_part)?;
        let rels_map: BTreeMap<String, String> = rels
            .iter()
            .filter(|r| !r.external)
            .map(|r| (r.id.clone(), opc::resolve_target(&original.doc_part, &r.target).unwrap_or_default()))
            .collect();
        let doc_bytes = package.part(&original.doc_part)?.to_vec();
        let read = |name: &str| package.part(name).ok().map(|b| b.to_vec());
        let parsed = parse::parse_document(&doc_bytes, &original.styles, &rels_map, &read)?;

        // Picture payloads go to the store so layout can draw them; the artifact keeps only references.
        for (sha, (mime, payload)) in &parsed.image_bytes {
            let stored = context.assets.put(payload, mime)?;
            if stored.sha256 != *sha {
                return Err(DltError::new(ErrorCode::IntegrityError, "a picture payload changed while it was stored"));
            }
        }

        let mut manifest: PreservationManifest = package.preservation_manifest();
        for name in MUTABLE_PARTS {
            if let Some(fingerprint) = manifest.parts.get_mut(name) {
                fingerprint.immutable = false;
            }
        }
        for (owner, atoms) in &parsed.atoms {
            for atom in atoms {
                manifest.opaque_regions.push(OpaqueRegion {
                    part: original.doc_part.clone(),
                    anchor: format!("{owner}#{}[{}]", atom.qname, atom.occ),
                    sha256: atom.sha.clone(),
                });
            }
        }
        let mut preserved = BTreeMap::new();
        preserved.insert(
            STYLE_OVERRIDES.to_string(),
            serde_json::to_value(Overrides::new()).map_err(|e| DltError::invalid(e.to_string()))?,
        );
        preserved.insert(DOCUMENT_PART.to_string(), json!(original.doc_part));
        preserved.insert(PICTURE_ALT_SOURCE.to_string(), json!(PICTURE_ALT_SOURCE_V1));
        let native = NativeSource {
            asset: input.asset.clone(),
            mappings: model::records_to_mappings(&parsed.records)?,
            preserved,
            preservation: manifest,
        };
        let mut artifact = Artifact::new(ArtifactKind::Docx, input.title.clone(), ENGINE_ID, ArtifactBody::Flow(parsed.document));
        artifact.origin = "imported".to_string();
        artifact.native = Some(native);
        artifact.diagnostics = parsed.diagnostics;
        // Opening does not depend on this count. A malformed XML part makes every save and edit refuse with the
        // parse error, which is the behaviour before this check existed.
        if let Ok(nodes) = verify::xml_metadata(&original.bytes) {
            if !nodes.is_empty() {
                artifact.diagnostics.push(diag::info(
                    diag::XML_COMMENT_EDIT_REFUSED,
                    format!(
                        "the package has {} XML comment or processing instruction(s); they are kept on an unchanged save, and an edit that would drop any of them is refused",
                        nodes.len()
                    ),
                    None,
                ));
            }
        }
        artifact.capabilities = self.capabilities(&artifact);
        Ok(artifact)
    }

    fn capabilities(&self, artifact: &Artifact) -> CapabilityReport {
        let metadata_guarded = artifact.diagnostics.iter().any(|d| d.code == diag::XML_COMMENT_EDIT_REFUSED);
        let unsupported = || feature(Support::Unsupported, Support::Unsupported, Support::Unsupported, Support::Unsupported, Support::Unsupported, "not part of DOCX");
        let mut report = CapabilityReport::new();
        report.insert(
            Feature::Text,
            feature(Support::Supported, Support::Approximate, Support::Supported, Support::Supported, Support::Supported, "paragraph and run text; protected inline content is shown but not editable"),
        );
        report.insert(
            Feature::TextStyle,
            feature(Support::Supported, Support::Approximate, Support::Supported, Support::Supported, Support::Supported, "direct character formatting: font, size, color, bold, italic"),
        );
        report.insert(
            Feature::NamedStyles,
            feature(Support::Supported, Support::Approximate, Support::Supported, Support::Supported, Support::Supported, "named paragraph and character styles with basedOn chains; alignment for paragraph styles"),
        );
        report.insert(
            Feature::Geometry,
            feature(Support::Supported, Support::Approximate, Support::Approximate, Support::Supported, Support::Supported, "picture width and height; pictures are placed by the text flow"),
        );
        report.insert(
            Feature::PageSetup,
            feature(Support::Supported, Support::Approximate, Support::Supported, Support::Supported, Support::Supported, "page size and margins per section; headers and footers are kept but not drawn"),
        );
        report.insert(
            Feature::ParagraphFormat,
            feature(Support::Supported, Support::Approximate, Support::Supported, Support::Supported, Support::Supported, "alignment, spacing before and after, first-line indent; automatic line spacing is laid out as single"),
        );
        report.insert(
            Feature::Tables,
            feature(Support::Supported, Support::Approximate, Support::Approximate, Support::Supported, Support::Supported, "grid widths and cell text; spans and vertical merges are kept; rows and columns cannot be added"),
        );
        report.insert(
            Feature::Images,
            feature(Support::Supported, Support::Approximate, Support::Approximate, Support::Supported, Support::Supported, "block pictures: read, resize, insert; inline pictures in text and floating pictures are placeholders or kept only"),
        );
        report.insert(
            Feature::Pages,
            feature(Support::Approximate, Support::Approximate, Support::Unsupported, Support::Supported, Support::Supported, "pagination by the shared flow layout"),
        );
        report.insert(
            Feature::Notes,
            feature(Support::Unsupported, Support::Unsupported, Support::Unsupported, Support::PreserveOnly, Support::PreserveOnly, "footnotes, endnotes, comments and tracked changes are kept as XML; they are not edited or drawn"),
        );
        for f in [
            Feature::Diagram,
            Feature::Plot,
            Feature::Dataset,
            Feature::Evidence,
            Feature::FigurePanels,
            Feature::CanvasLayout,
            Feature::Interactions,
            Feature::WebSource,
        ] {
            report.insert(f, unsupported());
        }
        report.insert(
            Feature::FigureLinks,
            feature(
                Support::Unsupported,
                Support::Approximate,
                Support::Supported,
                Support::Supported,
                Support::Unsupported,
                "png render only: UpdateFigureLink gives a picture the render bytes and keeps the link in the workspace, through undo and redo; the DOCX file stores the picture alone, so a file reopened outside the workspace has no link",
            ),
        );
        // v1.2 features: refused by name before any change (guard.rs). The file has no such values to keep.
        report.insert(
            Feature::Effects,
            feature(
                Support::Unsupported,
                Support::Unsupported,
                Support::Unsupported,
                Support::Unsupported,
                Support::Unsupported,
                "v1.2 effects (corner radius, opacity, shadows, gradients, line height, letter spacing, vertical align, font weight, stroke dash, cap and join, underline, strikethrough) are refused by name; not drawn or saved in DOCX yet",
            ),
        );
        report.insert(
            Feature::VectorPaths,
            feature(
                Support::Unsupported,
                Support::Unsupported,
                Support::Unsupported,
                Support::Unsupported,
                Support::Unsupported,
                "NodeKind::Path and icons are refused by name; DOCX has no vector path model",
            ),
        );
        report.insert(
            Feature::GridLayout,
            feature(
                Support::Unsupported,
                Support::Unsupported,
                Support::Unsupported,
                Support::Unsupported,
                Support::Unsupported,
                "LayoutSpec mode grid and the v1.2 layout fields (justify, padding_sides, grid_columns, row_gap, align_self, grid_span) are refused by name; DOCX lays out block flow only",
            ),
        );
        if metadata_guarded {
            report.insert(
                Feature::NativeRoundtrip,
                feature(Support::Supported, Support::Supported, Support::Approximate, Support::Supported, Support::Supported, "native DOCX save: unchanged parts stay byte-identical and the output is re-parsed before publication; XML comments and processing instructions are kept on an unchanged save, and an edit that would drop them is refused"),
            );
        } else {
            report.insert(
                Feature::NativeRoundtrip,
                feature(Support::Supported, Support::Supported, Support::Supported, Support::Supported, Support::Supported, "native DOCX save: unchanged parts stay byte-identical and the output is re-parsed before publication"),
            );
        }
        report.insert(
            Feature::RenderSvg,
            feature(Support::Unsupported, Support::Approximate, Support::Unsupported, Support::Unsupported, Support::Unsupported, "SVG of the shared layout; text positions follow the shared approximate layout"),
        );
        report.insert(
            Feature::RenderPdf,
            feature(Support::Unsupported, Support::Approximate, Support::Unsupported, Support::Unsupported, Support::Unsupported, "PDF of the shared layout; text positions follow the shared approximate layout"),
        );
        report.insert(
            Feature::RenderPng,
            feature(Support::Unsupported, Support::Approximate, Support::Unsupported, Support::Unsupported, Support::Unsupported, "PNG of the shared layout; text positions follow the shared approximate layout"),
        );
        report.insert(
            Feature::HitTest,
            feature(Support::Unsupported, Support::Approximate, Support::Unsupported, Support::Unsupported, Support::Unsupported, "hit testing on the shared layout snapshot"),
        );
        report.insert(
            Feature::TextSelection,
            feature(Support::Unsupported, Support::Approximate, Support::Unsupported, Support::Unsupported, Support::Unsupported, "text selection on the shared layout snapshot"),
        );
        report
    }

    fn supported_persisted_features(&self) -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::from([FEATURE_ARTIFACT_DESIGN_SYSTEM.to_owned()])
    }

    /// The named styles hold the pinned tokens (SPEC.md:81). Direct formatting is not checked: a `Keep` link
    /// leaves it as it is.
    fn validate_shared_design_system_snapshot(&self, artifact: &Artifact, tokens: &BTreeMap<String, TokenDef>) -> DltResult<()> {
        let document = document_of(artifact)?;
        let style_ids = linked_style_ids(native_of(artifact)?)?;
        flow::design_rules::check_named_styles(tokens, |name| linked_style(document, &style_ids, name), docx_font_size)
    }

    /// Published tokens become `SetNamedStyle` edits of the original styles (`flow::design_rules`), applied
    /// through `prepare_change`; `Clear` also clears the direct formatting of the text that uses each style.
    /// A detach keeps the style values and drops only the reference. Core calls this method; the context-free
    /// `sync_design_system` stays unsupported because the original styles live in the asset store.
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
            if let Some(native) = next.native.as_mut() {
                native.preserved.remove(DESIGN_SYSTEM_STYLES);
            }
            return Ok(PreparedChange { artifact: next, changed_ids: Vec::new(), diagnostics: Vec::new() });
        };
        // An edit of a linked document that left the governed styles alone: no package read, no prepared change.
        let linked_ids = linked_style_ids(native_of(artifact)?)?;
        let document = document_of(artifact)?;
        if flow::design_rules::already_synced(artifact, tokens, reference, direct_formatting, |name| linked_style(document, &linked_ids, name), docx_font_size) {
            return Ok(PreparedChange { artifact: artifact.clone(), changed_ids: Vec::new(), diagnostics: Vec::new() });
        }
        let original = load_original(context.assets, &native_of(artifact)?.asset)?;
        let (operations, absent) =
            flow::design_rules::named_style_operations(tokens, direct_formatting, |name| original.styles.resolve_name(name).is_some())?;
        let style_ids: BTreeMap<String, String> = operations
            .iter()
            .filter_map(|operation| match operation {
                Operation::SetNamedStyle { name, .. } => Some((name.clone(), original.styles.resolve_name(name)?)),
                _ => None,
            })
            .collect();
        refuse_style_aliases(&style_ids)?;
        let mut prepared = if operations.is_empty() {
            PreparedChange { artifact: artifact.clone(), changed_ids: Vec::new(), diagnostics: Vec::new() }
        } else {
            self.prepare_change(artifact, &operations, context)?
        };
        let native = prepared
            .artifact
            .native
            .as_mut()
            .ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "the DOCX artifact has no native source"))?;
        if style_ids.is_empty() {
            native.preserved.remove(DESIGN_SYSTEM_STYLES);
        } else {
            let value = serde_json::to_value(&style_ids).map_err(|e| DltError::invalid(e.to_string()))?;
            native.preserved.insert(DESIGN_SYSTEM_STYLES.to_string(), value);
        }
        flow::design_rules::finish_sync(artifact, prepared, reference, &absent)
    }

    fn persisted_features(&self, artifact: &Artifact) -> DltResult<std::collections::BTreeSet<String>> {
        let ArtifactBody::Flow(document) = &artifact.body else {
            return Err(DltError::new(ErrorCode::IntegrityError, "DOCX artifact has no flow body"));
        };
        let diagnostics = designllm_flow::guard::document_v12_diagnostics("DOCX documents", document);
        if !diagnostics.is_empty() {
            let mut error = DltError::new(ErrorCode::SchemaUnsupported, "DOCX snapshot contains extension fields this engine refuses to persist");
            error.diagnostics = diagnostics;
            return Err(error);
        }
        Ok(std::collections::BTreeSet::new())
    }

    fn inspect(&self, artifact: &Artifact, node_ids: &[Id]) -> DltResult<Value> {
        let document = document_of(artifact)?;
        let mut nodes = Vec::with_capacity(node_ids.len());
        for id in node_ids {
            let node = flow::tree::find_node(document, id).ok_or_else(|| {
                let mut error = DltError::new(ErrorCode::NotFound, format!("node {id} was not found"));
                error.affected_ids = vec![id.clone()];
                error
            })?;
            nodes.push(node_json(node));
        }
        Ok(json!({ "nodes": nodes }))
    }

    fn flow_styles(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<Vec<FlowStyleInfo>> {
        // The styles come from the original package. A style with an unknown w:type is left out here, and `validate`
        // reports it.
        let original = load_original(context.assets, &native_of(artifact)?.asset)?;
        Ok(Self::list_flow_styles(&original.bytes)?.styles)
    }

    fn prepare_change(&self, artifact: &Artifact, operations: &[Operation], context: &EngineContext<'_>) -> DltResult<PreparedChange> {
        // The only entry point that changes a DOCX document. P0 refusals (v1.2 fields, operations, variants) come
        // first, before the original package is read or any copy is made (docs/P0_CONTRACT_V1_2.md §4).
        crate::guard::check_operations(operations)?;
        let native = native_of(artifact)?;
        let original = load_original(context.assets, &native.asset)?;
        let mut state = load_state(artifact)?;
        let outcome = ops::apply_operations(&mut state, operations, &original.styles, context.assets)?;
        let problems: Vec<Diagnostic> = flow::tree::validate_document(&state.document).into_iter().filter(|d| d.blocking).collect();
        if let Some(first) = problems.first() {
            let mut error = DltError::invalid(first.message.clone());
            error.diagnostics = problems;
            return Err(error);
        }
        let mut next = artifact.clone();
        store_state(&mut next, state)?;
        next.capabilities = self.capabilities(&next);
        // Dry run of the native save. An edit whose written package would lose an XML comment or processing
        // instruction is refused here, before the workspace takes the change.
        let (encoded, released) = self.encode_with_released(&next, context)?;
        // Original parts that this save drops leave the preservation manifest. The immutable-part check then holds
        // for every part that stays, and the dropped ones are no longer expected.
        if let Some(native) = next.native.as_mut() {
            native.preservation.parts.retain(|name, _| !released.contains(name));
        }
        let mut diagnostics = outcome.diagnostics;
        diagnostics.extend(encoded.diagnostics);
        Ok(PreparedChange { artifact: next, changed_ids: outcome.changed, diagnostics })
    }

    fn validate(&self, artifact: &Artifact, context: &EngineContext<'_>) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        let Ok(document) = document_of(artifact) else {
            out.push(diag::blocking(diag::ENCODE_BLOCKED, "the artifact body is not a flow document", None));
            return out;
        };
        out.extend(flow::tree::validate_document(document));
        if let Err(error) = model::collect_ids(document) {
            out.push(diag::blocking(diag::ENCODE_BLOCKED, error.message, None));
        }
        out.extend(artifact.diagnostics.iter().cloned());
        // Styles that flow.styles leaves out (an unknown w:type) are reported here, without blocking.
        if let Ok(native) = native_of(artifact) {
            if let Ok(original) = load_original(context.assets, &native.asset) {
                if let Ok(list) = Self::list_flow_styles(&original.bytes) {
                    out.extend(list.diagnostics);
                }
            }
        }
        match self.encode_native(artifact, context) {
            Ok(encoded) => {
                out.extend(encoded.diagnostics);
                let verification = verify::verify_output(artifact, &encoded.bytes);
                out.extend(verification.diagnostics);
            }
            Err(error) => out.push(diag::blocking(diag::ENCODE_BLOCKED, error.message, None)),
        }
        out
    }

    fn content_digest(&self, artifact: &Artifact) -> DltResult<ContentDigest> {
        // Figure links are content of the document: re-linking a picture changes the digest.
        let flow_digest = flow::content_digest(document_of(artifact)?)?;
        let content = json_hash(&json!([flow_digest.content, json_hash(&artifact.figure_links)?]))?;
        Ok(ContentDigest { content, data: flow_digest.data })
    }

    fn layout(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<RenderScene> {
        let document = document_of(artifact)?;
        let links = text_link_spans(artifact, context)?;
        let mut scene = flow::layout_flow_with_links(document, &links, context)?;
        for diagnostic in &artifact.diagnostics {
            let present = scene
                .diagnostics
                .iter()
                .any(|d| d.code == diagnostic.code && d.node_id == diagnostic.node_id && d.message == diagnostic.message);
            if !present {
                scene.diagnostics.push(diagnostic.clone());
            }
        }
        Ok(scene)
    }

    fn encode_native(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<EncodedOutput> {
        self.encode_with_released(artifact, context).map(|(output, _)| output)
    }

    fn verify_saved(&self, artifact: &Artifact, bytes: &[u8], context: &EngineContext<'_>) -> DltResult<PreservationVerification> {
        let mut result = verify::verify_output(artifact, bytes);
        let original = load_original(context.assets, &native_of(artifact)?.asset)?;
        let before = verify::xml_metadata(&original.bytes)?;
        match verify::xml_metadata(bytes) {
            Ok(after) => {
                if let Some(difference) = verify::metadata_difference(&before, &after) {
                    let message = format!("XML comments or processing instructions differ from the original ({difference})");
                    result.diagnostics.push(diag::blocking(diag::VERIFY_MISMATCH, message, None));
                }
            }
            Err(error) => result.diagnostics.push(diag::blocking(
                diag::PACKAGE_STRUCTURE,
                format!("XML comments cannot be read from the written package: {}", error.message),
                None,
            )),
        }
        Ok(result)
    }

    fn export(&self, artifact: &Artifact, request: &ExportRequest, context: &EngineContext<'_>) -> DltResult<EncodedOutput> {
        let format = request.format.to_ascii_lowercase();
        match format.as_str() {
            "docx" => {
                flow::page_select::refuse_page_selection("docx", request.page_indices.as_deref())?;
                self.encode_native(artifact, context)
            }
            "pdf" | "png" | "svg" => {
                let mut scene = self.layout(artifact, context)?;
                let pages = request.page_indices.as_deref();
                flow::page_select::select_export_pages(&mut scene, document_of(artifact)?, &format, pages, &UNRENDERED_LOSSES)?;
                let mut output = context.renderer.encode(&scene, &format, &request.profile, context.assets)?;
                // The renderer echoes scene diagnostics; the export gate below is the only entry for these codes.
                // `layout` copies them from the artifact into the scene, and the page selection keeps those of the selected pages.
                output.diagnostics.retain(|d| !is_unrendered_loss(&d.code));
                for diagnostic in scene.diagnostics.iter().filter(|d| is_unrendered_loss(&d.code)) {
                    let blocking = !request.allowed_losses.contains(&diagnostic.code);
                    output.diagnostics.push(Diagnostic { blocking, ..diagnostic.clone() });
                }
                Ok(output)
            }
            other => Err(DltError::unsupported(format!("DOCX export to {other} is not supported"))),
        }
    }
}
